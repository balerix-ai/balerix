//! The plugin's two config blocks (Spec M §4). The daemon has already
//! read any file-backed secret (`secrets.privateKey`,
//! `secrets.webhookSecret`), so both are plain values here.

use std::net::SocketAddr;
use std::time::Duration;

use balerix_api::DEFAULT_KEY_DELAY_MS;
pub use balerix_plugin_common::config::{
    ConfigError, DEFAULT_EVENTS, EventFilter, LIFECYCLE, Secret, deserialize, deserialize_duration,
    validate_key_delay,
};
use serde::Deserialize;
use serde_json::Value;

/// Where the webhook listener binds unless `listen` says otherwise.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8787";
/// The fleet file's path in the repository, on the default branch.
pub const DEFAULT_CONFIG_PATH: &str = ".balerix.yaml";
/// Idle sessions end after this; `0` disables.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(2 * 3600);
/// Most comments one turn may become.
pub const DEFAULT_MAX_PARTS: usize = 10;
/// GitHub's comment body limit, in characters.
pub const COMMENT_LIMIT: usize = 65_536;
/// How long a prompt may go without `UserPromptSubmit` (Spec M §8.7).
pub const DEFAULT_CONFIRM_WINDOW: Duration = Duration::from_secs(30);

/// Spec M §4.1.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DaemonConfig {
    pub app_id: u64,
    pub private_key: Secret,
    pub webhook_secret: Secret,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_config_path")]
    pub config_path: String,
    #[serde(default = "default_idle", deserialize_with = "deserialize_duration")]
    pub idle_timeout: Duration,
    #[serde(default = "default_max_parts")]
    pub max_parts: usize,
}

impl std::fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonConfig")
            .field("app_id", &self.app_id)
            .field("private_key", &"<redacted>")
            .field("webhook_secret", &"<redacted>")
            .field("listen", &self.listen)
            .field("config_path", &self.config_path)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_parts", &self.max_parts)
            .finish()
    }
}

fn default_listen() -> SocketAddr {
    // The constant is well-formed; a parse failure here is a programming error.
    DEFAULT_LISTEN
        .parse()
        .unwrap_or(SocketAddr::from(([127, 0, 0, 1], 8787)))
}
fn default_config_path() -> String {
    DEFAULT_CONFIG_PATH.to_string()
}
fn default_idle() -> Duration {
    DEFAULT_IDLE_TIMEOUT
}
fn default_max_parts() -> usize {
    DEFAULT_MAX_PARTS
}

/// Parses and checks the `hello` config.
pub fn parse_daemon(config: &Value) -> Result<DaemonConfig, ConfigError> {
    let c: DaemonConfig = deserialize(config)?;
    let invalid = |path: &str, message: &str| ConfigError {
        path: path.into(),
        message: message.into(),
    };
    if c.max_parts == 0 {
        return Err(invalid("maxParts", "must be at least 1"));
    }
    if c.config_path.is_empty() || c.config_path.starts_with('/') || c.config_path.contains("..") {
        return Err(invalid(
            "configPath",
            "a relative path inside the repository",
        ));
    }
    if c.private_key.expose().trim().is_empty() {
        return Err(invalid("privateKey", "must not be empty"));
    }
    if c.webhook_secret.expose().is_empty() {
        return Err(invalid("webhookSecret", "must not be empty"));
    }
    Ok(c)
}

/// Issue or pull request: what an agent is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Issue,
    Pr,
}

impl Kind {
    /// The agent's name in the crew (M-1): `issue-<n>` / `pr-<n>`.
    pub fn agent_name(self, number: u64) -> String {
        match self {
            Kind::Issue => format!("issue-{number}"),
            Kind::Pr => format!("pr-{number}"),
        }
    }
}

/// Spec M §4.2: the agent's `plugins.github` block. `kind` and `number`
/// are what the plugin injected when it added the agent; a hand-written
/// agent in `.balerix.yaml` lacks them and is refused.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub events: EventFilter,
    pub phases: bool,
    #[serde(rename = "keyDelayMs")]
    pub key_delay_ms: u64,
    #[serde(rename = "confirmWindow", deserialize_with = "deserialize_duration")]
    pub confirm_window: Duration,
    pub kind: Option<Kind>,
    pub number: Option<u64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            events: EventFilter::default(),
            phases: true,
            key_delay_ms: DEFAULT_KEY_DELAY_MS,
            confirm_window: DEFAULT_CONFIRM_WINDOW,
            kind: None,
            number: None,
        }
    }
}

impl AgentConfig {
    pub fn wants(&self, event: &str) -> bool {
        self.events.wants(event)
    }
}

/// Parses and checks one agent's block; the daemon prefixes the message
/// with `crews.<c>.agents.<a>.plugins.github: `.
pub fn parse_agent(config: &Value) -> Result<AgentConfig, ConfigError> {
    let c: AgentConfig = deserialize(config)?;
    c.events.validate()?;
    validate_key_delay(c.key_delay_ms)?;
    let missing = |path: &str| ConfigError {
        path: path.into(),
        message: "missing".into(),
    };
    if c.kind.is_none() {
        return Err(missing("kind"));
    }
    match c.number {
        None => return Err(missing("number")),
        Some(0) => {
            return Err(ConfigError {
                path: "number".into(),
                message: "must be at least 1".into(),
            });
        }
        Some(_) => {}
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn daemon(extra: serde_json::Value) -> serde_json::Value {
        let mut v = json!({ "appId": 12345, "privateKey": "-----BEGIN RSA PRIVATE KEY-----\nx\n-----END RSA PRIVATE KEY-----", "webhookSecret": "s3cret" });
        if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            m.extend(e.clone());
        }
        v
    }

    #[test]
    fn daemon_config_defaults_and_redacts_its_secrets() {
        let c = parse_daemon(&daemon(json!({}))).unwrap();
        assert_eq!(c.app_id, 12345);
        assert_eq!(c.listen.to_string(), "127.0.0.1:8787");
        assert_eq!(c.config_path, ".balerix.yaml");
        assert_eq!(c.idle_timeout, Duration::from_secs(7200));
        assert_eq!(c.max_parts, 10);
        assert_eq!(c.webhook_secret.expose(), "s3cret");
        let dbg = format!("{c:?}");
        assert!(
            !dbg.contains("s3cret") && !dbg.contains("BEGIN RSA"),
            "{dbg}"
        );
    }

    #[test]
    fn daemon_config_reads_every_knob_and_names_bad_ones() {
        let c = parse_daemon(&daemon(json!({
            "listen": "0.0.0.0:9000", "configPath": ".github/balerix.yaml",
            "idleTimeout": "0", "maxParts": 3
        })))
        .unwrap();
        assert_eq!(c.listen.to_string(), "0.0.0.0:9000");
        assert_eq!(c.config_path, ".github/balerix.yaml");
        assert_eq!(c.idle_timeout, Duration::ZERO);
        assert_eq!(c.max_parts, 3);
        for (extra, path) in [
            (json!({ "listen": "nowhere" }), "listen"),
            (json!({ "idleTimeout": "soon" }), "idleTimeout"),
            (json!({ "maxParts": 0 }), "maxParts"),
            (json!({ "configPath": "" }), "configPath"),
            (json!({ "configPath": "/abs" }), "configPath"),
            (json!({ "nope": 1 }), "nope"),
        ] {
            assert_eq!(parse_daemon(&daemon(extra)).unwrap_err().path, path);
        }
        let e = parse_daemon(&json!({ "appId": 1 })).unwrap_err();
        assert_eq!(e.path, "privateKey");
    }

    #[test]
    fn agent_config_requires_kind_and_number_and_defaults_the_rest() {
        let c = parse_agent(&json!({ "kind": "issue", "number": 12 })).unwrap();
        assert!(c.enabled && c.phases);
        assert_eq!(c.key_delay_ms, 100);
        assert_eq!(c.confirm_window, Duration::from_secs(30));
        assert_eq!((c.kind, c.number), (Some(Kind::Issue), Some(12)));
        assert!(c.wants("Stop") && c.wants("SessionStart") && !c.wants("PreToolUse"));
        assert_eq!(
            parse_agent(&json!({ "kind": "pr" }))
                .unwrap_err()
                .to_string(),
            "number: missing"
        );
        assert_eq!(
            parse_agent(&json!({ "number": 3 }))
                .unwrap_err()
                .to_string(),
            "kind: missing"
        );
        assert_eq!(
            parse_agent(&json!({ "kind": "gist", "number": 3 }))
                .unwrap_err()
                .path,
            "kind"
        );
        assert_eq!(
            parse_agent(&json!({ "kind": "pr", "number": 0 }))
                .unwrap_err()
                .to_string(),
            "number: must be at least 1"
        );
        assert_eq!(Kind::Issue.agent_name(12), "issue-12");
        assert_eq!(Kind::Pr.agent_name(34), "pr-34");
    }

    #[test]
    fn a_disabled_agent_still_needs_its_number() {
        assert!(parse_agent(&json!({ "enabled": false })).is_err());
    }
}
