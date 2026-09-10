//! The room, thread and route maps (Spec G §6, §7). `threads` is the one
//! source of truth and is mirrored to the daemon's KV, so a restart
//! resumes; `routes` is derived from it and rebuilt at startup.

use std::collections::HashMap;

use balerix_plugin_sdk::{Host, SdkError};
use serde::{Deserialize, Serialize};

pub const ROOM_PREFIX: &str = "room/";
pub const THREAD_PREFIX: &str = "thread/";

/// One agent session's thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub session_id: String,
    /// The thread root's event id.
    pub root: String,
    pub room: String,
    #[serde(default)]
    pub closed: bool,
}

/// `fleet/crew/agent` to `fleet/crew`; `None` for anything else, which the
/// daemon never produces.
pub fn crew_of(agent: &str) -> Option<&str> {
    let crew = agent.rfind('/').map(|i| &agent[..i])?;
    crew.contains('/').then_some(crew)
}

pub fn room_key(crew: &str) -> String {
    format!("{ROOM_PREFIX}{crew}")
}

pub fn thread_key(agent: &str) -> String {
    format!("{THREAD_PREFIX}{agent}")
}

#[derive(Debug, Default)]
pub struct Maps {
    rooms: HashMap<String, String>,
    threads: HashMap<String, Thread>,
    routes: HashMap<(String, String), String>,
}

impl Maps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds from KV. A record that no longer parses is dropped with a
    /// log line rather than failing startup: the plugin's job is to keep
    /// posting, and the next `SessionStart` re-creates the thread.
    pub async fn load(host: &Host) -> Result<Self, SdkError> {
        let mut maps = Self::new();
        for key in host.kv_list(ROOM_PREFIX).await? {
            let Some(bytes) = host.kv_get(&key).await? else {
                continue;
            };
            match String::from_utf8(bytes) {
                Ok(room) => {
                    maps.rooms
                        .insert(key[ROOM_PREFIX.len()..].to_string(), room);
                }
                Err(e) => tracing::warn!("matrix: bad room record {key}: {e}"),
            }
        }
        for key in host.kv_list(THREAD_PREFIX).await? {
            let Some(bytes) = host.kv_get(&key).await? else {
                continue;
            };
            match serde_json::from_slice::<Thread>(&bytes) {
                Ok(thread) => {
                    let agent = key[THREAD_PREFIX.len()..].to_string();
                    maps.insert_thread(agent, thread);
                }
                Err(e) => tracing::warn!("matrix: bad thread record {key}: {e}"),
            }
        }
        Ok(maps)
    }

    fn insert_thread(&mut self, agent: String, thread: Thread) {
        if let Some(old) = self.threads.get(&agent) {
            self.routes.remove(&(old.room.clone(), old.root.clone()));
        }
        self.routes
            .insert((thread.room.clone(), thread.root.clone()), agent.clone());
        self.threads.insert(agent, thread);
    }

    pub fn room(&self, crew: &str) -> Option<&str> {
        self.rooms.get(crew).map(String::as_str)
    }

    pub async fn set_room(&mut self, host: &Host, crew: &str, room: &str) -> Result<(), SdkError> {
        host.kv_put(&room_key(crew), room.as_bytes(), false).await?;
        self.rooms.insert(crew.to_string(), room.to_string());
        Ok(())
    }

    pub fn thread(&self, agent: &str) -> Option<&Thread> {
        self.threads.get(agent)
    }

    pub async fn set_thread(
        &mut self,
        host: &Host,
        agent: &str,
        thread: Thread,
    ) -> Result<(), SdkError> {
        let bytes = serde_json::to_vec(&thread)
            .map_err(|e| SdkError::Transport(format!("encode thread: {e}")))?;
        host.kv_put(&thread_key(agent), &bytes, false).await?;
        self.insert_thread(agent.to_string(), thread);
        Ok(())
    }

    /// Marks the session ended. The record stays so a late reply can be
    /// told why it was not delivered (Spec G-12).
    pub async fn close_thread(&mut self, host: &Host, agent: &str) -> Result<(), SdkError> {
        let Some(mut thread) = self.threads.get(agent).cloned() else {
            return Ok(());
        };
        thread.closed = true;
        self.set_thread(host, agent, thread).await
    }

    /// Drops the agent entirely: `deactivate`. Deletes the store's row
    /// first, matching every other mutator here: if the delete fails, the
    /// in-memory maps still agree with what's on disk, and the caller can
    /// retry rather than the agent silently coming back on the next `load`.
    pub async fn forget(&mut self, host: &Host, agent: &str) -> Result<(), SdkError> {
        host.kv_delete(&thread_key(agent)).await?;
        if let Some(old) = self.threads.remove(agent) {
            self.routes.remove(&(old.room, old.root));
        }
        Ok(())
    }

    pub fn route(&self, room: &str, root: &str) -> Option<&str> {
        self.routes
            .get(&(room.to_string(), root.to_string()))
            .map(String::as_str)
    }

    pub fn rooms_len(&self) -> usize {
        self.rooms.len()
    }

    pub fn open_threads(&self) -> usize {
        self.threads.values().filter(|t| !t.closed).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::FakeHost;

    fn thread(session: &str, root: &str) -> Thread {
        Thread {
            session_id: session.into(),
            root: root.into(),
            room: "!r:fake".into(),
            closed: false,
        }
    }

    async fn host() -> (FakeHost, Host) {
        let fake = FakeHost::start("tok", serde_json::json!({}), Vec::new()).await;
        let env = fake.env("matrix", std::path::Path::new("scratch"));
        let host = Host::new(env).unwrap();
        (fake, host)
    }

    #[test]
    fn a_crew_is_an_agent_id_without_its_last_segment() {
        assert_eq!(crew_of("payments/backend/alice"), Some("payments/backend"));
        assert_eq!(crew_of("payments/backend"), None);
        assert_eq!(crew_of("alice"), None);
        assert_eq!(room_key("payments/backend"), "room/payments/backend");
        assert_eq!(
            thread_key("payments/backend/alice"),
            "thread/payments/backend/alice"
        );
    }

    #[tokio::test]
    async fn rooms_and_threads_survive_a_reload_and_routes_are_rebuilt() {
        let (_fake, host) = host().await;
        let mut maps = Maps::new();
        maps.set_room(&host, "payments/backend", "!r:fake")
            .await
            .unwrap();
        maps.set_thread(&host, "payments/backend/alice", thread("s1", "$root1"))
            .await
            .unwrap();
        assert_eq!(maps.room("payments/backend"), Some("!r:fake"));
        assert_eq!(
            maps.route("!r:fake", "$root1"),
            Some("payments/backend/alice")
        );
        assert_eq!(maps.rooms_len(), 1);
        assert_eq!(maps.open_threads(), 1);

        let reloaded = Maps::load(&host).await.unwrap();
        assert_eq!(reloaded.room("payments/backend"), Some("!r:fake"));
        assert_eq!(
            reloaded.route("!r:fake", "$root1"),
            Some("payments/backend/alice"),
            "routes are derived from the stored threads"
        );
        assert_eq!(
            reloaded
                .thread("payments/backend/alice")
                .map(|t| t.session_id.as_str()),
            Some("s1")
        );
    }

    #[tokio::test]
    async fn a_new_session_replaces_the_old_thread_and_its_route() {
        let (_fake, host) = host().await;
        let mut maps = Maps::new();
        maps.set_thread(&host, "f/c/a", thread("s1", "$root1"))
            .await
            .unwrap();
        maps.set_thread(&host, "f/c/a", thread("s2", "$root2"))
            .await
            .unwrap();
        assert_eq!(maps.route("!r:fake", "$root2"), Some("f/c/a"));
        assert_eq!(
            maps.route("!r:fake", "$root1"),
            None,
            "the previous session's root stops routing"
        );
        assert_eq!(maps.open_threads(), 1);
    }

    #[tokio::test]
    async fn closing_keeps_the_thread_and_forgetting_removes_it() {
        let (fake, host) = host().await;
        let mut maps = Maps::new();
        maps.set_thread(&host, "f/c/a", thread("s1", "$root1"))
            .await
            .unwrap();
        maps.close_thread(&host, "f/c/a").await.unwrap();
        assert!(maps.thread("f/c/a").is_some_and(|t| t.closed));
        assert_eq!(maps.open_threads(), 0, "a closed thread is not open");
        assert_eq!(
            maps.route("!r:fake", "$root1"),
            Some("f/c/a"),
            "still resolvable, so a reply can be told the session ended"
        );
        assert!(fake.kv().contains_key("thread/f/c/a"));

        maps.forget(&host, "f/c/a").await.unwrap();
        assert!(maps.thread("f/c/a").is_none());
        assert_eq!(maps.route("!r:fake", "$root1"), None);
        assert!(!fake.kv().contains_key("thread/f/c/a"));
    }

    #[tokio::test]
    async fn a_forget_whose_delete_fails_leaves_memory_agreeing_with_the_store() {
        let (fake, host) = host().await;
        let mut maps = Maps::new();
        maps.set_thread(&host, "f/c/a", thread("s1", "$root1"))
            .await
            .unwrap();

        // Same fake store, a token it rejects: the delete never reaches the
        // row, the same way a real store's delete can fail.
        let mut bad_env = fake.env("matrix", std::path::Path::new("scratch"));
        bad_env.token = "wrong".into();
        let bad_host = Host::new(bad_env).unwrap();

        maps.forget(&bad_host, "f/c/a")
            .await
            .expect_err("the wrong token makes the store refuse the delete");
        assert!(
            maps.thread("f/c/a").is_some(),
            "a failed delete leaves the agent in memory, agreeing with the still-present row"
        );
        assert!(fake.kv().contains_key("thread/f/c/a"));

        let reloaded = Maps::load(&host).await.unwrap();
        assert!(
            reloaded.thread("f/c/a").is_some(),
            "a later load from the same store agrees"
        );
    }
}
