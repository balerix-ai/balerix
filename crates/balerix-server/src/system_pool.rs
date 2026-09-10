//! The daemon pool's single owner (Spec F). One actor, one writer: fleet
//! actors used to install the system level from N parallel threads.

use std::sync::Arc;
use std::time::Duration;

use balerix_core::SystemToolchain;
use tokio::sync::watch;

/// What the daemon pool is doing. Read live at the top of every fleet pass —
/// never latched (Spec F, F-4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPoolState {
    Pending,
    Ready,
    Unready { reason: String },
}

#[derive(Debug, Clone, Copy)]
pub struct SystemPoolConfig {
    /// How often the pool is re-checked. A matching marker makes this a file
    /// read and a hash, so it is cheap.
    pub tick: Duration,
    /// Per-attempt budget. Carried here so the policy lives with the actor,
    /// but not yet wired anywhere: the runtime enforces a fixed 600s via its
    /// own `SYSTEM_POOL_INSTALL_TIMEOUT` constant
    /// (`balerix-runtime/src/materializer.rs`), which happens to match this
    /// field's default. Threading this value down to replace that constant
    /// is a follow-up, out of scope for this task.
    pub attempt_timeout: Duration,
}

impl Default for SystemPoolConfig {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(30),
            attempt_timeout: Duration::from_secs(600),
        }
    }
}

/// Runs until aborted. One attempt per tick, sequential, retried forever; a
/// failure is published, never fatal (F-7).
pub fn spawn(
    toolchain: Arc<dyn SystemToolchain>,
    tx: watch::Sender<SystemPoolState>,
    config: SystemPoolConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let tc = toolchain.clone();
            tracing::info!("daemon mise pool install starting");
            // The port is synchronous; a pass must not block the runtime.
            let outcome = tokio::task::spawn_blocking(move || tc.ensure_system_pool()).await;
            let next = match outcome {
                Ok(Ok(())) => SystemPoolState::Ready,
                Ok(Err(e)) => SystemPoolState::Unready {
                    reason: e.to_string(),
                },
                Err(e) => SystemPoolState::Unready {
                    reason: format!("system pool task failed: {e}"),
                },
            };
            if let SystemPoolState::Unready { reason } = &next {
                tracing::warn!(%reason, "daemon mise pool is not ready");
            }
            // `send_if_modified` keeps `changed()` meaningful: only a real
            // transition wakes the fleet actors waiting on it. Log only the
            // actual Pending/Unready → Ready transition, not every tick that
            // reconfirms an already-Ready pool.
            tx.send_if_modified(|cur| {
                if *cur == next {
                    false
                } else {
                    if next == SystemPoolState::Ready && *cur != SystemPoolState::Ready {
                        tracing::info!("daemon mise pool is ready");
                    }
                    *cur = next.clone();
                    true
                }
            });
            tokio::time::sleep(config.tick).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_core::fakes::FakeSystemToolchain;
    use std::sync::Arc;
    use std::time::Duration;

    fn fast() -> SystemPoolConfig {
        SystemPoolConfig {
            tick: Duration::from_millis(20),
            attempt_timeout: Duration::from_secs(1),
        }
    }

    /// Generous relative to the 20ms tick these tests use (hundreds of
    /// ticks), but still small next to nextest's 3-minute slow-test kill —
    /// a regression fails fast and clearly instead of hanging.
    const WAIT: Duration = Duration::from_secs(30);

    /// Awaits one `changed()` notification, failing with a clear message
    /// naming what was expected instead of hanging forever if the actor
    /// never sends it (e.g. a broken `send_if_modified` comparison or a
    /// `spawn_blocking` call that never resolves).
    async fn next_change(rx: &mut watch::Receiver<SystemPoolState>, what: &str) {
        tokio::time::timeout(WAIT, rx.changed())
            .await
            .unwrap_or_else(|_| panic!("timed out after {WAIT:?} waiting for {what}"))
            .unwrap();
    }

    /// Awaits `changed()` in a loop until `pred` holds, bounding the whole
    /// wait — not just one iteration — so a state that oscillates without
    /// ever reaching the target still fails clearly instead of hanging.
    async fn wait_until(
        rx: &mut watch::Receiver<SystemPoolState>,
        what: &str,
        pred: impl Fn(&SystemPoolState) -> bool,
    ) {
        tokio::time::timeout(WAIT, async {
            loop {
                rx.changed().await.unwrap();
                if pred(&rx.borrow()) {
                    break;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out after {WAIT:?} waiting for {what}"));
    }

    #[tokio::test]
    async fn a_healthy_pool_goes_pending_then_ready_and_stays_ready() {
        let tc = Arc::new(FakeSystemToolchain::ready());
        let (tx, mut rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        assert_eq!(*rx.borrow(), SystemPoolState::Pending);
        let handle = spawn(tc, tx, fast());

        next_change(&mut rx, "the pool to leave Pending").await;
        assert_eq!(*rx.borrow(), SystemPoolState::Ready);

        // Several more ticks must not disturb it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*rx.borrow(), SystemPoolState::Ready);
        handle.abort();
    }

    #[tokio::test]
    async fn a_failing_pool_publishes_the_reason_and_recovers_without_dying() {
        let tc = Arc::new(FakeSystemToolchain::failing(2));
        let (tx, mut rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        let handle = spawn(tc.clone(), tx, fast());

        next_change(&mut rx, "the first state change").await;
        match &*rx.borrow() {
            SystemPoolState::Unready { reason } => {
                assert!(!reason.is_empty(), "an unready state must carry a reason")
            }
            other => panic!("expected Unready first, got {other:?}"),
        }

        // It keeps trying and eventually succeeds — no fatal path (F-7).
        wait_until(&mut rx, "the pool to recover to Ready", |s| {
            *s == SystemPoolState::Ready
        })
        .await;
        assert!(tc.calls() >= 3, "one attempt per tick until it succeeds");
        assert!(!handle.is_finished(), "the actor must not exit on failure");
        handle.abort();
    }

    #[tokio::test]
    async fn readiness_drops_again_when_a_later_attempt_fails() {
        // Ready, then failing: proves readiness is live, not a latch (F-4).
        let tc = Arc::new(FakeSystemToolchain::ready_then_failing(1));
        let (tx, mut rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        let handle = spawn(tc, tx, fast());

        wait_until(&mut rx, "the pool to become Ready", |s| {
            *s == SystemPoolState::Ready
        })
        .await;
        wait_until(&mut rx, "the pool to drop back to Unready", |s| {
            matches!(s, SystemPoolState::Unready { .. })
        })
        .await;
        handle.abort();
    }

    #[tokio::test]
    async fn a_permanently_failing_pool_never_terminates_the_actor() {
        let tc = Arc::new(FakeSystemToolchain::always_failing());
        let (tx, _rx) = tokio::sync::watch::channel(SystemPoolState::Pending);
        let handle = spawn(tc.clone(), tx, fast());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!handle.is_finished(), "never fatal (F-7)");
        assert!(tc.calls() >= 3, "it keeps retrying");
        handle.abort();
    }
}
