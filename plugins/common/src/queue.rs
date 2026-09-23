//! The bounded drop-oldest command queue every observer plugin feeds its
//! actor from (Spec G-11), and the health cell the actor writes and
//! `Plugin::health` reads.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use balerix_api::OBSERVER_QUEUE;
use balerix_plugin_sdk::metrics::IntCounter;
use tokio::sync::Notify;

/// Queue depth, the same as the daemon's own observer queues.
pub const QUEUE: usize = OBSERVER_QUEUE;

/// A bounded queue that drops its oldest entry rather than blocking its
/// producer: `observe` is a daemon-to-plugin HTTP call and must return.
pub struct Queue<C> {
    inner: Mutex<VecDeque<C>>,
    notify: Notify,
    dropped: IntCounter,
}

impl<C> Queue<C> {
    /// Creates an empty queue that counts every drop into `dropped`.
    pub fn new(dropped: IntCounter) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::with_capacity(QUEUE)),
            notify: Notify::new(),
            dropped,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<C>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pushes `command` onto the back of the queue, dropping and counting
    /// the oldest entry first if the queue is already at capacity.
    pub fn push(&self, command: C) {
        {
            let mut q = self.lock();
            if q.len() >= QUEUE {
                q.pop_front();
                self.dropped.inc();
            }
            q.push_back(command);
        }
        // `notify_one` stores a permit when nobody is waiting, so a pop
        // that arrives afterwards returns at once: no lost wakeups.
        self.notify.notify_one();
    }

    /// Waits for and returns the oldest entry, in FIFO order.
    pub async fn pop(&self) -> C {
        loop {
            if let Some(command) = self.lock().pop_front() {
                return command;
            }
            self.notify.notified().await;
        }
    }

    /// The number of entries currently queued.
    pub fn len(&self) -> usize {
        self.lock().len()
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

    #[tokio::test]
    async fn the_queue_is_fifo_and_wakes_a_waiting_pop() {
        let q: Arc<Queue<u32>> = Queue::new(dropped());
        assert!(q.is_empty());
        let waiter = {
            let q = q.clone();
            tokio::spawn(async move { q.pop().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        q.push(1);
        q.push(2);
        assert_eq!(waiter.await.unwrap(), 1);
        assert_eq!(q.pop().await, 2);
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_oldest_and_counts_it() {
        let counter = dropped();
        let q: Arc<Queue<usize>> = Queue::new(counter.clone());
        for i in 0..=QUEUE {
            q.push(i);
        }
        assert_eq!(q.len(), QUEUE);
        assert_eq!(counter.get(), 1);
        assert_eq!(q.pop().await, 1, "entry 0 was dropped");
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
