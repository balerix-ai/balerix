//! The command queue every observer plugin feeds its actor from (Spec
//! G-11, #260), and the health cell the actor writes and `Plugin::health`
//! reads.
//!
//! Two lanes. Hook events and phase changes go in a bounded lane that
//! drops its oldest entry rather than block a producer. Control commands,
//! inbound items (a chat reply, a webhook) and the one pending tick go in
//! a lane that never drops: losing an `Activate` leaves an agent
//! untracked, losing a `Deactivate` leaves a stale thread routable, and
//! losing a reply loses what a person wrote. Commands leave in the order
//! they arrived, across both lanes: a `Deactivate` must not overtake its
//! agent's last events, nor a reply the event that opened the question it
//! answers.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use balerix_api::OBSERVER_QUEUE;
use balerix_plugin_sdk::metrics::IntCounter;
use tokio::sync::Notify;

/// Depth of the bounded lane, the same as the daemon's own observer queues.
pub const QUEUE: usize = OBSERVER_QUEUE;

/// The most inbound items the kept lane holds before it refuses another.
/// A stalled homeserver or API can leave the actor behind a flood of
/// replies or webhooks; past this or `KEPT_BYTES` the producer is told
/// instead, so memory stays bounded and the producer can say so where a
/// person sees it. Control commands are never refused: the daemon sends
/// one per agent change, which no flood can come from.
pub const KEPT: usize = 4096;

/// The most bytes (`Laned::bytes`) the queued inbound items may hold
/// together before another is refused: 64 MiB.
pub const KEPT_BYTES: usize = 64 << 20;

/// Which lane of a `Queue` a command travels in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Kept: never dropped and never refused. `Configure`, `Activate`,
    /// `Deactivate`, and anything else the actor's state depends on.
    Control,
    /// Kept, never dropped; refused with `Full` once `KEPT` inbound items
    /// or `KEPT_BYTES` of them are queued. A chat reply, a webhook.
    Inbound,
    /// The bounded lane: past `QUEUE` entries the oldest is dropped,
    /// counted, and marks a gap. Hook events and phase changes.
    Lossy,
    /// Kept, with at most one such entry waiting: a repeat pushed while
    /// one is queued is merged into it. A periodic tick, which carries
    /// nothing but "run the timers"; in the kept lane so that no flood of
    /// events can push it out, again and again. One kind of command per
    /// queue should use it.
    Latest,
}

/// A command that knows its `Lane`.
pub trait Laned {
    /// The lane this command travels in.
    fn lane(&self) -> Lane;

    /// Roughly the bytes a `Lane::Inbound` command holds (its text),
    /// counted against `KEPT_BYTES`. Other lanes ignore it.
    fn bytes(&self) -> usize {
        0
    }
}

/// An `Inbound` command the queue refused: the kept lane is full. Holds
/// the command, for a producer that reports it back.
pub struct Full<C>(pub C);

impl<C> fmt::Debug for Full<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Full")
    }
}

impl<C> fmt::Display for Full<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the plugin is too far behind to queue it; try again later")
    }
}

/// Both lanes, each entry stamped with its arrival.
struct Lanes<C> {
    kept: VecDeque<(u64, C)>,
    lossy: VecDeque<(u64, C)>,
    /// The `Lane::Inbound` entries in `kept`, and their `Laned::bytes`.
    inbound: usize,
    inbound_bytes: usize,
    /// Whether a `Lane::Latest` entry is waiting in `kept`.
    latest: bool,
    next: u64,
}

/// The actor's command queue: never blocks its producer, since `observe`
/// is a daemon-to-plugin HTTP call and must return. See the module docs.
pub struct Queue<C> {
    inner: Mutex<Lanes<C>>,
    notify: Notify,
    dropped: IntCounter,
    /// Set on every drop, cleared by `take_gap`.
    gap: AtomicBool,
}

impl<C: Laned> Queue<C> {
    /// Creates an empty queue that counts every dropped event into
    /// `dropped`.
    pub fn new(dropped: IntCounter) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Lanes {
                kept: VecDeque::new(),
                lossy: VecDeque::with_capacity(QUEUE),
                inbound: 0,
                inbound_bytes: 0,
                latest: false,
                next: 0,
            }),
            notify: Notify::new(),
            dropped,
            gap: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Lanes<C>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `command` in its lane. Only a `Lane::Inbound` command can
    /// be refused, past `KEPT` or `KEPT_BYTES`; a `Lane::Lossy` one may
    /// instead push out the oldest bounded entry.
    pub fn push(&self, command: C) -> Result<(), Full<C>> {
        {
            let mut q = self.lock();
            let seq = q.next;
            match command.lane() {
                Lane::Control => q.kept.push_back((seq, command)),
                Lane::Inbound => {
                    let bytes = command.bytes();
                    if q.inbound >= KEPT || q.inbound_bytes.saturating_add(bytes) > KEPT_BYTES {
                        return Err(Full(command));
                    }
                    q.inbound += 1;
                    q.inbound_bytes += bytes;
                    q.kept.push_back((seq, command));
                }
                Lane::Latest if q.latest => return Ok(()),
                Lane::Latest => {
                    q.latest = true;
                    q.kept.push_back((seq, command));
                }
                Lane::Lossy => {
                    if q.lossy.len() >= QUEUE {
                        q.lossy.pop_front();
                        self.dropped.inc();
                        self.gap.store(true, Ordering::SeqCst);
                    }
                    q.lossy.push_back((seq, command));
                }
            }
            q.next += 1;
        }
        // `notify_one` stores a permit when nobody is waiting, so a pop
        // that arrives afterwards returns at once: no lost wakeups.
        self.notify.notify_one();
        Ok(())
    }

    /// Waits for and returns the entry that arrived first, across both
    /// lanes.
    pub async fn pop(&self) -> C {
        loop {
            if let Some(command) = self.try_pop() {
                return command;
            }
            self.notify.notified().await;
        }
    }

    fn try_pop(&self) -> Option<C> {
        let mut q = self.lock();
        let kept_first = match (q.kept.front(), q.lossy.front()) {
            (Some((k, _)), Some((l, _))) => k < l,
            (k, _) => k.is_some(),
        };
        if !kept_first {
            return q.lossy.pop_front().map(|(_, c)| c);
        }
        let (_, command) = q.kept.pop_front()?;
        match command.lane() {
            Lane::Inbound => {
                q.inbound -= 1;
                q.inbound_bytes -= command.bytes();
            }
            Lane::Latest => q.latest = false,
            Lane::Control | Lane::Lossy => {}
        }
        Some(command)
    }

    /// Whether an event was dropped since the last call, clearing the
    /// mark. An actor asks after each `pop`, before it handles what it
    /// popped: what it was tracking from the events may have missed some
    /// (common's `Deliveries::lose_track`), and the command in hand may
    /// depend on it.
    pub fn take_gap(&self) -> bool {
        self.gap.swap(false, Ordering::SeqCst)
    }

    /// The number of entries currently queued, in both lanes.
    pub fn len(&self) -> usize {
        let q = self.lock();
        q.kept.len() + q.lossy.len()
    }

    /// Whether the queue currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What `Plugin::health` reports. The actor writes it; the plugin reads it.
#[derive(Debug, Clone, Default)]
pub struct Health(Arc<Mutex<Option<String>>>);

impl Health {
    /// Creates a new, healthy cell.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears any recorded failure.
    pub fn ok(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Records `message` as the current failure.
    pub fn fail(&self, message: String) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
    }

    /// The current health: `Ok(())` if healthy, or the last failure's
    /// message.
    pub fn get(&self) -> Result<(), String> {
        match self.0.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(m) => Err(m),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::Metrics;

    fn dropped() -> IntCounter {
        Metrics::new("t")
            .int_counter("events_dropped_total", "t")
            .unwrap()
    }

    /// A test command: its lane, and a number to tell entries apart.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct C(Lane, usize);

    impl Laned for C {
        fn lane(&self) -> Lane {
            self.0
        }
    }

    fn event(n: usize) -> C {
        C(Lane::Lossy, n)
    }

    #[tokio::test]
    async fn the_queue_is_fifo_and_wakes_a_waiting_pop() {
        let q: Arc<Queue<C>> = Queue::new(dropped());
        assert!(q.is_empty());
        let waiter = {
            let q = q.clone();
            tokio::spawn(async move { q.pop().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        q.push(event(1)).unwrap();
        q.push(event(2)).unwrap();
        assert_eq!(waiter.await.unwrap(), event(1));
        assert_eq!(q.pop().await, event(2));
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_oldest_and_counts_it() {
        let counter = dropped();
        let q: Arc<Queue<C>> = Queue::new(counter.clone());
        for i in 0..=QUEUE {
            q.push(event(i)).unwrap();
        }
        assert_eq!(q.len(), QUEUE);
        assert_eq!(counter.get(), 1);
        assert_eq!(q.pop().await, event(1), "entry 0 was dropped");
    }

    #[tokio::test]
    async fn a_drop_marks_a_gap_until_it_is_taken() {
        let q: Arc<Queue<C>> = Queue::new(dropped());
        q.push(C(Lane::Control, 0)).unwrap();
        for i in 0..QUEUE {
            q.push(event(i)).unwrap();
        }
        assert!(
            !q.take_gap(),
            "full is not a gap, and the kept lane does not count"
        );
        q.push(event(QUEUE)).unwrap();
        q.pop().await;
        assert!(q.take_gap());
        assert!(!q.take_gap(), "taken once");
    }

    /// #260: a flood of events used to push out the commands before it.
    #[tokio::test]
    async fn control_and_inbound_survive_a_flood_of_events_in_arrival_order() {
        let counter = dropped();
        let q: Arc<Queue<C>> = Queue::new(counter.clone());
        q.push(C(Lane::Control, 0)).unwrap(); // Configure
        q.push(C(Lane::Control, 1)).unwrap(); // Activate
        q.push(event(2)).unwrap();
        q.push(C(Lane::Inbound, 3)).unwrap(); // a reply
        for i in 4..2500 {
            q.push(event(i)).unwrap();
        }
        q.push(C(Lane::Control, 2500)).unwrap(); // Deactivate
        assert_eq!(counter.get(), 2500 - 4 + 1 - QUEUE as u64);
        assert!(q.take_gap());

        let mut got = Vec::new();
        while !q.is_empty() {
            got.push(q.pop().await);
        }
        assert_eq!(&got[..2], &[C(Lane::Control, 0), C(Lane::Control, 1)]);
        assert_eq!(
            got[2],
            C(Lane::Inbound, 3),
            "the reply is kept, and stays ahead of later events"
        );
        assert_eq!(
            got.last(),
            Some(&C(Lane::Control, 2500)),
            "nor does it overtake them"
        );
        let events: Vec<usize> = got[3..got.len() - 1].iter().map(|c| c.1).collect();
        assert_eq!(
            events,
            (2500 - QUEUE..2500).collect::<Vec<_>>(),
            "the newest events"
        );
    }

    #[tokio::test]
    async fn repeated_ticks_collapse_to_one_that_no_flood_pushes_out() {
        let counter = dropped();
        let q: Arc<Queue<C>> = Queue::new(counter.clone());
        for i in 0..10 {
            q.push(C(Lane::Latest, i)).unwrap();
        }
        assert_eq!(q.len(), 1);
        assert_eq!(q.pop().await, C(Lane::Latest, 0));
        q.push(C(Lane::Latest, 1)).unwrap();
        assert_eq!(q.len(), 1, "a tick may queue again once the last one left");

        for i in 0..2 * QUEUE {
            q.push(event(i)).unwrap();
            q.push(C(Lane::Latest, 2 + i)).unwrap();
        }
        assert_eq!(q.len(), QUEUE + 1, "the bounded lane, and the one tick");
        assert_eq!(counter.get(), QUEUE as u64, "only events were dropped");
        assert_eq!(q.pop().await, C(Lane::Latest, 1), "the tick kept its place");
    }

    #[tokio::test]
    async fn the_kept_lane_refuses_inbound_past_its_bound_but_never_control() {
        let q: Arc<Queue<C>> = Queue::new(dropped());
        for i in 0..KEPT {
            q.push(C(Lane::Inbound, i)).unwrap();
        }
        let refused = q.push(C(Lane::Inbound, KEPT)).unwrap_err();
        assert_eq!(refused.0, C(Lane::Inbound, KEPT), "handed back");
        assert!(refused.to_string().contains("behind"), "{refused}");
        q.push(C(Lane::Control, 0)).unwrap();
        q.push(C(Lane::Latest, 0)).unwrap();
        assert_eq!(q.len(), KEPT + 2);
        assert!(!q.take_gap(), "a refusal is not a silent drop");
        assert_eq!(q.pop().await, C(Lane::Inbound, 0));
        q.push(C(Lane::Inbound, KEPT + 1)).unwrap();
        assert!(q.push(C(Lane::Inbound, KEPT + 2)).is_err());
    }

    /// An inbound command whose size is its number.
    #[derive(Debug, PartialEq, Eq)]
    struct Heavy(usize);

    impl Laned for Heavy {
        fn lane(&self) -> Lane {
            Lane::Inbound
        }
        fn bytes(&self) -> usize {
            self.0
        }
    }

    #[tokio::test]
    async fn the_kept_lane_refuses_inbound_past_its_byte_bound() {
        let q: Arc<Queue<Heavy>> = Queue::new(dropped());
        let big = KEPT_BYTES / 4;
        for _ in 0..4 {
            q.push(Heavy(big)).unwrap();
        }
        assert!(q.push(Heavy(1)).is_err(), "the bytes are spent");
        assert_eq!(q.pop().await, Heavy(big));
        assert!(q.push(Heavy(big + 1)).is_err(), "only `big` came free");
        q.push(Heavy(big)).unwrap();
    }

    #[test]
    fn health_starts_ok_and_reports_the_last_failure_until_cleared() {
        let h = Health::new();
        assert_eq!(h.get(), Ok(()));
        h.fail("a".into());
        h.fail("b".into());
        assert_eq!(h.get(), Err("b".into()));
        h.ok();
        assert_eq!(h.get(), Ok(()));
    }
}
