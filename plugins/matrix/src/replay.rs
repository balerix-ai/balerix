//! History that arrives a second time is not a reply (Spec G §9.3, #262).
//!
//! The sync token stops a restart from replaying a room, but not a rejoin:
//! after the bot is kicked and invited back, the join sync carries the
//! room's recent history, and a thread reply in it whose thread is still
//! live would be typed into the agent again. Two guards:
//!
//! - `history`: in the one sync response whose timeline carries the bot's
//!   own change to `join`, everything the timeline puts before that change
//!   is history. That is the homeserver's own order, not a clock. The
//!   adapter applies it per response and marks each `Inbound`.
//! - `Marks`: the event ids the actor has handled, the last `MAX_IDS` per
//!   room, kept in KV for the rooms still in use and within
//!   `MAX_RECORD_BYTES` in all. An id in the set is never handled again,
//!   across restarts and store purges.
//!
//! Neither guard compares a timestamp across the whole stream: the one
//! that lives past a single response matches ids, which never drops a
//! message it has not seen, whatever the sending homeserver's clock says.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use balerix_plugin_sdk::{Host, SdkError};

/// The one KV record: every room's handled ids, as one JSON object of
/// arrays, oldest first. A room id (`!opaque:server`) is not a valid KV key
/// segment, and there is one room per crew.
pub const ROUTED_KEY: &str = "routed";

/// How many handled ids a room remembers. A redelivery comes from a join
/// sync, which carries at most a timeline's worth of events (tens, with
/// the filters clients use), far below this; at about 50 bytes an id, a
/// room's list is about 13 KiB.
pub const MAX_IDS: usize = 256;

/// The most the encoded record may weigh, a quarter of the plugin-host
/// API's 1 MiB body cap. Rooms no longer in use are pruned (`retain`), so
/// this binds only past about twenty busy crews at once; then the oldest
/// ids of the longest lists go first, which keeps every room's newest.
pub const MAX_RECORD_BYTES: usize = 256 * 1024;

/// One event of one room's timeline in one sync response, reduced to what
/// `history` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry<'a> {
    /// Our own member event, changing our membership to `join` from
    /// anything else (a join on top of a join is a profile change, and is
    /// `Other`).
    OwnJoin {
        ts: u64,
    },
    /// A message, which may become an `Inbound`.
    Message {
        sender: &'a str,
        ts: u64,
    },
    Other,
}

/// Which entries of one room's timeline, in one sync response, are history
/// the join brought with it.
///
/// The rule is structural. A timeline is in the homeserver's order, so
/// when it contains our own join, everything before the last such join was
/// in the room before we were. With no join in the timeline, the join is
/// not in this response or precedes the whole timeline (a sync's state
/// section is the state at the timeline's start), and nothing is history.
///
/// One secondary check, inside the join's own timeline only: a message
/// after the join whose sender is on our homeserver and whose timestamp is
/// older than the join's. Both timestamps are then our homeserver's clock,
/// so they compare, and an event older than our join that the server only
/// ordered after it (a late arrival) is history too. A sender on any other
/// homeserver is never judged by its clock.
pub fn history(own_user_id: &str, timeline: &[Entry<'_>]) -> Vec<bool> {
    let own_server = server_of(own_user_id);
    let join = timeline
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, e)| match e {
            Entry::OwnJoin { ts } => Some((i, *ts)),
            _ => None,
        });
    let Some((at, joined)) = join else {
        return vec![false; timeline.len()];
    };
    timeline
        .iter()
        .enumerate()
        .map(|(i, entry)| match entry {
            Entry::Message { sender, ts } => {
                i < at || (server_of(sender) == own_server && *ts < joined)
            }
            _ => false,
        })
        .collect()
}

/// `@local:server` to `server`; a user id's server name starts at its first
/// colon.
fn server_of(user_id: &str) -> &str {
    user_id.split_once(':').map_or("", |(_, server)| server)
}

/// Every room's handled event ids, mirrored to KV.
#[derive(Debug, Default)]
pub struct Marks {
    rooms: BTreeMap<String, VecDeque<String>>,
}

impl Marks {
    /// Reads the record. One that no longer parses is dropped with a log
    /// line: the join guard still holds without it.
    pub async fn load(host: &Host) -> Result<Self, SdkError> {
        let Some(bytes) = host.kv_get(ROUTED_KEY).await? else {
            return Ok(Self::default());
        };
        match serde_json::from_slice(&bytes) {
            Ok(rooms) => Ok(Self { rooms }),
            Err(e) => {
                tracing::warn!("matrix: bad {ROUTED_KEY} record, starting without it: {e}");
                Ok(Self::default())
            }
        }
    }

    /// Whether `event_id` was already handled in `room`.
    pub fn seen(&self, room: &str, event_id: &str) -> bool {
        self.rooms
            .get(room)
            .is_some_and(|ids| ids.iter().any(|id| id == event_id))
    }

    /// Records `event_id` as handled in `room`, in memory first: a KV
    /// failure costs the guard across a restart, not within this process.
    pub async fn record(
        &mut self,
        host: &Host,
        room: &str,
        event_id: &str,
    ) -> Result<(), SdkError> {
        let ids = self.rooms.entry(room.to_string()).or_default();
        ids.push_back(event_id.to_string());
        while ids.len() > MAX_IDS {
            ids.pop_front();
        }
        self.store(host).await
    }

    /// Forgets every room not in `live`: the rooms the actor's threads,
    /// pinned rooms and active crews still use. A message in any other room
    /// cannot reach an agent, so its ids guard nothing. Writes only when
    /// something was dropped.
    pub async fn retain(&mut self, host: &Host, live: &BTreeSet<String>) -> Result<(), SdkError> {
        let before = self.rooms.len();
        self.rooms.retain(|room, _| live.contains(room));
        if self.rooms.len() == before {
            return Ok(());
        }
        self.store(host).await
    }

    /// Writes the record, first dropping the oldest ids of the longest
    /// lists until it fits `MAX_RECORD_BYTES`.
    async fn store(&mut self, host: &Host) -> Result<(), SdkError> {
        let encode = |rooms: &BTreeMap<String, VecDeque<String>>| {
            serde_json::to_vec(rooms)
                .map_err(|e| SdkError::Transport(format!("encode {ROUTED_KEY}: {e}")))
        };
        let mut bytes = encode(&self.rooms)?;
        while bytes.len() > MAX_RECORD_BYTES {
            // Estimate what each dropped id saves (its quotes and comma
            // too), so a large overshoot is not one encode per id; the
            // loop re-encodes to check, since escaping can make an id
            // longer than its text.
            let mut size = bytes.len();
            while size > MAX_RECORD_BYTES {
                let Some(longest) = self.rooms.values_mut().max_by_key(|ids| ids.len()) else {
                    break;
                };
                let Some(id) = longest.pop_front() else {
                    break;
                };
                size = size.saturating_sub(id.len() + 3);
            }
            let shorter = encode(&self.rooms)?;
            if shorter.len() == bytes.len() {
                break;
            }
            bytes = shorter;
        }
        host.kv_put(ROUTED_KEY, &bytes, false).await
    }

    #[cfg(test)]
    fn len(&self, room: &str) -> usize {
        self.rooms.get(room).map_or(0, VecDeque::len)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use balerix_plugin_sdk::testing::FakeHost;
    use serde_json::json;

    const ME: &str = "@balerix:home";

    fn msg(sender: &str, ts: u64) -> Entry<'_> {
        Entry::Message { sender, ts }
    }

    #[test]
    fn everything_before_our_join_in_its_timeline_is_history() {
        let timeline = [
            msg("@rahul:away", 9_000),
            Entry::Other,
            msg("@ann:home", 200),
            Entry::OwnJoin { ts: 500 },
            msg("@rahul:away", 100),
            msg("@ann:home", 600),
        ];
        assert_eq!(
            history(ME, &timeline),
            vec![true, false, true, false, false, false],
            "before the join by position, whatever the clock; after it, a \
             remote sender is never judged by its clock"
        );
    }

    #[test]
    fn a_local_message_ordered_after_the_join_but_older_than_it_is_history() {
        let timeline = [Entry::OwnJoin { ts: 500 }, msg("@ann:home", 499)];
        assert_eq!(history(ME, &timeline), vec![false, true]);
    }

    #[test]
    fn the_last_join_counts_when_a_timeline_has_two() {
        let timeline = [
            Entry::OwnJoin { ts: 100 },
            msg("@rahul:away", 150),
            Entry::Other,
            Entry::OwnJoin { ts: 300 },
            msg("@rahul:away", 350),
        ];
        assert_eq!(
            history(ME, &timeline),
            vec![false, true, false, false, false]
        );
    }

    #[test]
    fn a_timeline_without_our_join_has_no_history_whatever_its_timestamps() {
        let timeline = [msg("@rahul:away", 1_000), msg("@ann:home", 990)];
        assert_eq!(history(ME, &timeline), vec![false, false]);
    }

    #[tokio::test]
    async fn handled_ids_survive_a_reload_per_room_and_are_bounded() {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();

        let mut marks = Marks::load(&host).await.unwrap();
        marks.record(&host, "!r:h", "$one").await.unwrap();
        marks.record(&host, "!s:h", "$two").await.unwrap();
        assert_eq!(
            fake.kv_json(ROUTED_KEY),
            Some(json!({ "!r:h": ["$one"], "!s:h": ["$two"] }))
        );

        let reloaded = Marks::load(&host).await.unwrap();
        assert!(reloaded.seen("!r:h", "$one"));
        assert!(!reloaded.seen("!s:h", "$one"), "ids are per room");
        assert!(!reloaded.seen("!r:h", "$three"));

        let mut marks = reloaded;
        for i in 0..MAX_IDS {
            marks
                .record(&host, "!r:h", &format!("$n{i}"))
                .await
                .unwrap();
        }
        assert!(!marks.seen("!r:h", "$one"), "the oldest id is let go");
        assert!(marks.seen("!r:h", "$n0"));
        assert!(marks.seen("!r:h", &format!("$n{}", MAX_IDS - 1)));
    }

    #[tokio::test]
    async fn rooms_no_longer_in_use_are_pruned() {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        let mut marks = Marks::default();
        marks.record(&host, "!live:h", "$a").await.unwrap();
        marks.record(&host, "!gone:h", "$b").await.unwrap();

        let live = BTreeSet::from(["!live:h".to_string()]);
        marks.retain(&host, &live).await.unwrap();
        assert_eq!(fake.kv_json(ROUTED_KEY), Some(json!({ "!live:h": ["$a"] })));
        assert!(Marks::load(&host).await.unwrap().seen("!live:h", "$a"));
    }

    #[tokio::test]
    async fn the_record_stays_under_its_byte_cap_dropping_the_oldest_of_the_longest() {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        let mut marks = Marks::default();
        // A full list of ids of a realistic length in each of enough rooms
        // to pass the cap, built in memory and written once.
        let rooms = MAX_RECORD_BYTES / (MAX_IDS * 50) + 4;
        for r in 0..rooms {
            let ids = (0..MAX_IDS)
                .map(|i| format!("${r:03}-{i:04}-{}", "x".repeat(36)))
                .collect();
            marks.rooms.insert(format!("!room{r:03}:h"), ids);
        }
        marks.record(&host, "!room000:h", "$newest").await.unwrap();

        let stored = fake.kv().get(ROUTED_KEY).unwrap().0.len();
        assert!(stored <= MAX_RECORD_BYTES, "{stored} bytes");
        let reloaded = Marks::load(&host).await.unwrap();
        assert!(
            reloaded.seen("!room000:h", "$newest"),
            "the newest id is kept"
        );
        assert!(
            !reloaded.seen("!room001:h", &format!("$001-0000-{}", "x".repeat(36))),
            "the oldest ids went first"
        );
        let lens: Vec<usize> = (0..rooms)
            .map(|r| reloaded.len(&format!("!room{r:03}:h")))
            .collect();
        let (min, max) = (lens.iter().min().unwrap(), lens.iter().max().unwrap());
        assert!(max - min <= 1, "trimmed evenly: {lens:?}");
        assert!(*min > 0);
    }

    #[tokio::test]
    async fn an_unreadable_record_is_dropped() {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        host.kv_put(ROUTED_KEY, b"not json", false).await.unwrap();
        let marks = Marks::load(&host).await.unwrap();
        assert!(!marks.seen("!r:h", "$a"));
    }
}
