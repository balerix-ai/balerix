//! The Matrix seam (Spec G §13). `MatrixPort` is what the actor is written
//! against; `fake::FakePort` is the in-memory implementation its tests use,
//! and `MatrixClient` in `client.rs` is the `matrix-sdk` one. Inbound
//! messages are not on the trait: the adapter owns a task that pushes them
//! into the actor's queue, so the trait stays a plain request-response
//! surface.

use std::future::Future;

/// The acknowledgement reactions (Spec G §9).
pub const ACK: &str = "👍";
/// A message the plugin declined to route.
pub const REFUSED: &str = "🚫";
/// A routed message the daemon rejected.
pub const FAILED: &str = "❗";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatrixError {
    #[error("rate limited, retry in {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("auth: {0}")]
    Auth(String),
    #[error("{0}")]
    Other(String),
}

/// One message the plugin saw in a room it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    pub room: String,
    pub event_id: String,
    pub sender: String,
    /// The `m.thread` relation's root, when the message has one.
    pub thread_root: Option<String>,
    pub body: String,
}

pub trait MatrixPort: Send + Sync + 'static {
    /// The account the plugin is logged in as; used for loop protection.
    fn user_id(&self) -> &str;
    /// A private, encrypted room, with everyone in `invite` invited.
    /// Returns the room id.
    fn create_room(
        &self,
        name: &str,
        invite: &[String],
    ) -> impl Future<Output = Result<String, MatrixError>> + Send;
    /// Sends markdown, as a thread reply when `thread_root` is set.
    /// Returns the new event id.
    fn send(
        &self,
        room: &str,
        thread_root: Option<&str>,
        markdown: &str,
    ) -> impl Future<Output = Result<String, MatrixError>> + Send;
    fn react(
        &self,
        room: &str,
        event_id: &str,
        key: &str,
    ) -> impl Future<Output = Result<(), MatrixError>> + Send;
}

/// An in-memory `MatrixPort` for tests. Always compiled, like
/// `balerix_plugin_sdk::testing`, because the integration test needs it.
pub mod fake {
    use std::sync::{Arc, Mutex};

    use super::{MatrixError, MatrixPort};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        CreateRoom {
            name: String,
            invite: Vec<String>,
        },
        Send {
            room: String,
            thread_root: Option<String>,
            body: String,
        },
        React {
            room: String,
            event_id: String,
            key: String,
        },
    }

    #[derive(Default)]
    struct Inner {
        calls: Vec<Call>,
        next: u64,
        fail_next: Option<MatrixError>,
    }

    #[derive(Clone)]
    pub struct FakePort {
        user_id: String,
        inner: Arc<Mutex<Inner>>,
    }

    impl FakePort {
        pub fn new(user_id: &str) -> Self {
            Self {
                user_id: user_id.to_string(),
                inner: Arc::new(Mutex::new(Inner::default())),
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.inner.lock().unwrap_or_else(|e| e.into_inner())
        }

        pub fn calls(&self) -> Vec<Call> {
            self.lock().calls.clone()
        }

        /// Clears the recorded calls; handy between phases of a test.
        pub fn take_calls(&self) -> Vec<Call> {
            std::mem::take(&mut self.lock().calls)
        }

        /// The next call, whichever it is, fails with this error once.
        pub fn fail_next(&self, error: MatrixError) {
            self.lock().fail_next = Some(error);
        }

        fn check(&self) -> Result<(), MatrixError> {
            match self.lock().fail_next.take() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }

        fn mint(&self, prefix: &str) -> String {
            let mut inner = self.lock();
            inner.next += 1;
            format!("{prefix}{}:fake", inner.next)
        }
    }

    impl MatrixPort for FakePort {
        fn user_id(&self) -> &str {
            &self.user_id
        }

        async fn create_room(&self, name: &str, invite: &[String]) -> Result<String, MatrixError> {
            self.check()?;
            self.lock().calls.push(Call::CreateRoom {
                name: name.to_string(),
                invite: invite.to_vec(),
            });
            Ok(self.mint("!room"))
        }

        async fn send(
            &self,
            room: &str,
            thread_root: Option<&str>,
            markdown: &str,
        ) -> Result<String, MatrixError> {
            self.check()?;
            self.lock().calls.push(Call::Send {
                room: room.to_string(),
                thread_root: thread_root.map(str::to_string),
                body: markdown.to_string(),
            });
            Ok(self.mint("$evt"))
        }

        async fn react(&self, room: &str, event_id: &str, key: &str) -> Result<(), MatrixError> {
            self.check()?;
            self.lock().calls.push(Call::React {
                room: room.to_string(),
                event_id: event_id.to_string(),
                key: key.to_string(),
            });
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Call, FakePort};
    use super::*;

    #[tokio::test]
    async fn the_fake_records_calls_mints_ids_and_can_be_made_to_fail() {
        let p = FakePort::new("@balerix:example.org");
        assert_eq!(p.user_id(), "@balerix:example.org");

        let room = p
            .create_room("balerix payments/backend", &["@rahul:example.org".into()])
            .await
            .unwrap();
        let root = p.send(&room, None, "root").await.unwrap();
        let child = p.send(&room, Some(&root), "child").await.unwrap();
        p.react(&room, &child, ACK).await.unwrap();
        assert_ne!(root, child, "every send mints a fresh event id");

        assert_eq!(
            p.calls(),
            vec![
                Call::CreateRoom {
                    name: "balerix payments/backend".into(),
                    invite: vec!["@rahul:example.org".into()],
                },
                Call::Send {
                    room: room.clone(),
                    thread_root: None,
                    body: "root".into(),
                },
                Call::Send {
                    room: room.clone(),
                    thread_root: Some(root.clone()),
                    body: "child".into(),
                },
                Call::React {
                    room,
                    event_id: child,
                    key: ACK.into(),
                },
            ]
        );

        p.fail_next(MatrixError::RateLimited {
            retry_after_ms: 250,
        });
        let err = p.send("!r:fake", None, "x").await.unwrap_err();
        assert_eq!(err.to_string(), "rate limited, retry in 250 ms");
        assert!(
            p.send("!r:fake", None, "x").await.is_ok(),
            "only the next call fails"
        );
    }
}
