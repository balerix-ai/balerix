//! The hook-event port (architecture spec §8; plugins spec §4.3, §16.1).
//! The handler is async because Spec B's chain calls plugins over HTTP;
//! it returns a boxed `std::future::Future` so `dyn EventHandler` works and
//! this crate needs no runtime.

use std::future::Future;
use std::pin::Pin;

use balerix_api::{HookEvent, PluginAction};
use serde_json::{Value, json};

/// What the daemon answers Claude with, and what it does afterwards.
/// `{}` with no actions means allow / no-op.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub response: Value,
    pub actions: Vec<PluginAction>,
}

impl Outcome {
    pub fn allow() -> Self {
        Self {
            response: json!({}),
            actions: Vec::new(),
        }
    }
}

pub type HandlerFuture<'a> = Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>;

pub trait EventHandler: Send + Sync {
    fn handle<'a>(&'a self, event: &'a HookEvent) -> HandlerFuture<'a>;
}

/// Allow everything, do nothing: the zero-plugin behaviour and the test handler.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassThrough;

impl EventHandler for PassThrough {
    fn handle<'a>(&'a self, _: &'a HookEvent) -> HandlerFuture<'a> {
        Box::pin(async { Outcome::allow() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::Timestamp;
    use serde_json::json;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    /// Enough of an executor for a handler that is immediately ready:
    /// core has no async runtime and must not grow one for a test.
    fn block_on<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    #[test]
    fn pass_through_allows_everything_with_no_actions() {
        let e = HookEvent {
            agent: "f/c/a".into(),
            name: "PreToolUse".into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({ "tool_name": "Bash" }),
        };
        let h: &dyn EventHandler = &PassThrough;
        let out = block_on(h.handle(&e));
        assert_eq!(out, Outcome::allow());
        assert_eq!(out.response, json!({}));
        assert!(out.actions.is_empty());
    }
}
