//! The cached Matrix session (Spec G §5.2). It lives in sealed KV, which
//! the daemon encrypts at rest with its vault key, and the decision about
//! whether to use it is a pure function so "the password is read at most
//! once" is a tested property.

use std::fmt;

use balerix_plugin_sdk::{Host, SdkError};
use serde::{Deserialize, Serialize};

use crate::config::{DaemonConfig, Secret};

/// The one sealed KV key this plugin writes.
pub const AUTH_KEY: &str = "auth";

/// What the operator should do when a cached session is unusable and no
/// password is configured.
pub const REVOKED: &str = "the cached session was rejected, most likely because its device was revoked; configure a password to log in again";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub homeserver: String,
    /// The homeserver's own rendering of the account id, from the login
    /// response. This is the one `restore_session` must be given, and the
    /// one an event's `sender` is compared against.
    pub user_id: String,
    /// The `userId` in `plugins.yaml` that produced this session. A
    /// homeserver may canonicalise (Synapse lowercases a localpart), so the
    /// two can differ, and `plan` has to compare the configured spelling
    /// against the configured spelling or it would discard a perfectly good
    /// session. Defaulted, so a record stored before this field existed
    /// still reads back instead of being thrown away.
    #[serde(default)]
    pub configured_user_id: String,
    pub device_id: String,
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("homeserver", &self.homeserver)
            .field("user_id", &self.user_id)
            .field("configured_user_id", &self.configured_user_id)
            .field("device_id", &self.device_id)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

/// What to do at `configure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Reuse the cached session; the password is never read.
    Restore(Box<Session>),
    Login {
        password: Secret,
    },
}

/// A cached session is reused only when it belongs to the configured
/// homeserver and user. Otherwise a password is required, and its absence
/// is an error that says which of the two situations it is.
///
/// "the configured user" is either spelling: the homeserver may answer a
/// login with a canonicalised id, and a session whose stored id differs
/// from `config.user_id` only in the way the homeserver rewrote it is the
/// session for that user. Comparing on the stored id alone would discard
/// it, which with no password configured means the plugin never starts.
pub fn plan(cached: Option<Session>, config: &DaemonConfig) -> Result<Plan, String> {
    let usable = cached.filter(|s| {
        s.homeserver == config.homeserver
            && (s.user_id == config.user_id || s.configured_user_id == config.user_id)
    });
    if let Some(session) = usable {
        return Ok(Plan::Restore(Box::new(session)));
    }
    match &config.password {
        Some(password) => Ok(Plan::Login {
            password: password.clone(),
        }),
        None => Err(format!(
            "no cached session for {} at {}, and no password is configured: set `password` in the plugins.yaml config block, or add a `secrets.password` entry naming a 0600 file",
            config.user_id, config.homeserver
        )),
    }
}

pub async fn load(host: &Host) -> Result<Option<Session>, SdkError> {
    let Some(bytes) = host.kv_get(AUTH_KEY).await? else {
        return Ok(None);
    };
    match serde_json::from_slice(&bytes) {
        Ok(session) => Ok(Some(session)),
        Err(e) => {
            tracing::warn!("matrix: stored session is unreadable, logging in again: {e}");
            Ok(None)
        }
    }
}

pub async fn store(host: &Host, session: &Session) -> Result<(), SdkError> {
    let bytes = serde_json::to_vec(session)
        .map_err(|e| SdkError::Transport(format!("encode session: {e}")))?;
    host.kv_put(AUTH_KEY, &bytes, true).await
}

pub async fn clear(host: &Host) -> Result<(), SdkError> {
    host.kv_delete(AUTH_KEY).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_plugin_sdk::testing::FakeHost;
    use serde_json::json;

    fn config(password: Option<&str>) -> DaemonConfig {
        let mut v = json!({ "homeserver": "https://h", "userId": "@balerix:h" });
        if let Some(p) = password {
            v["password"] = json!(p);
        }
        crate::config::parse_daemon(&v).unwrap()
    }

    fn session() -> Session {
        Session {
            homeserver: "https://h".into(),
            user_id: "@balerix:h".into(),
            configured_user_id: "@balerix:h".into(),
            device_id: "balerix".into(),
            access_token: Secret::new("syt_tok"),
            refresh_token: Some(Secret::new("syr_ref")),
        }
    }

    #[test]
    fn a_matching_cached_session_is_restored_without_reading_the_password() {
        assert_eq!(
            plan(Some(session()), &config(Some("pw"))),
            Ok(Plan::Restore(Box::new(session())))
        );
        assert_eq!(
            plan(Some(session()), &config(None)),
            Ok(Plan::Restore(Box::new(session()))),
            "a cached session needs no password at all"
        );
    }

    #[test]
    fn a_session_the_homeserver_canonicalised_is_still_restored_without_a_password() {
        // The operator wrote `@Balerix:h`; the homeserver answered the login
        // with `@balerix:h`, which is the id `restore_session` needs and the
        // one an event's `sender` carries. Comparing on the stored id alone
        // would call the session foreign, and with no password there is
        // nothing to fall back to — the plugin would refuse to start, for
        // good, holding a session that works perfectly.
        let mut cached = session();
        cached.configured_user_id = "@Balerix:h".into();
        let mut c = config(None);
        c.user_id = "@Balerix:h".into();
        assert_eq!(
            plan(Some(cached.clone()), &c),
            Ok(Plan::Restore(Box::new(cached)))
        );
    }

    #[test]
    fn a_record_stored_before_the_configured_id_existed_still_reads_back() {
        let stored = json!({
            "homeserver": "https://h",
            "user_id": "@balerix:h",
            "device_id": "balerix",
            "access_token": "syt_tok",
        });
        let session: Session = serde_json::from_value(stored).unwrap();
        assert_eq!(session.configured_user_id, "");
        assert!(
            matches!(plan(Some(session), &config(None)), Ok(Plan::Restore(_))),
            "an older record still matches on the id it does have"
        );
    }

    #[test]
    fn a_changed_homeserver_or_user_discards_the_cached_session() {
        let mut c = config(Some("pw"));
        c.homeserver = "https://other".into();
        assert_eq!(
            plan(Some(session()), &c),
            Ok(Plan::Login {
                password: Secret::new("pw")
            })
        );
        let mut c = config(Some("pw"));
        c.user_id = "@other:h".into();
        assert!(matches!(plan(Some(session()), &c), Ok(Plan::Login { .. })));
    }

    #[test]
    fn no_cached_session_and_no_password_names_what_to_do() {
        let err = plan(None, &config(None)).unwrap_err();
        assert!(err.contains("no password"), "{err}");
        assert!(
            err.contains("secrets.password"),
            "names the file route: {err}"
        );

        let mut c = config(None);
        c.homeserver = "https://other".into();
        let err = plan(Some(session()), &c).unwrap_err();
        assert!(err.contains("https://other"), "names the mismatch: {err}");
        assert!(!err.contains("syt_tok"), "never the token: {err}");
    }

    #[test]
    fn debug_never_prints_a_token() {
        let text = format!("{:?}", session());
        assert!(
            !text.contains("syt_tok") && !text.contains("syr_ref"),
            "{text}"
        );
        assert!(text.contains("<redacted>"), "{text}");
        assert!(
            text.contains("balerix"),
            "the device id is readable: {text}"
        );
    }

    #[tokio::test]
    async fn the_session_round_trips_through_sealed_kv() {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        assert_eq!(load(&host).await.unwrap(), None);

        store(&host, &session()).await.unwrap();
        assert_eq!(load(&host).await.unwrap(), Some(session()));
        assert_eq!(
            fake.kv().get(AUTH_KEY).map(|(_, secret)| *secret),
            Some(true),
            "the record is stored sealed"
        );

        clear(&host).await.unwrap();
        assert_eq!(load(&host).await.unwrap(), None);
    }
}
