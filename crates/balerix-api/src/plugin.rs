//! Plugin wire types (plugins spec §2, §3, §4.1): the package manifest, the
//! daemon's `plugins.yaml`, `hello`, status rows and the sync report.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AgentPhase;

/// Host protocol major this daemon speaks (plugins spec §4).
pub const PLUGIN_PROTOCOL: u32 = 1;
/// `kind` of every manifest.
pub const PLUGIN_KIND: &str = "Plugin";

/// `balerix-plugin.yaml` at a package root (plugins spec §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub version: String,
    pub protocol: u32,
    /// The mise task that starts the plugin.
    pub start: String,
    #[serde(default)]
    pub hooks: HookSubscriptions,
    #[serde(default)]
    pub needs: BTreeSet<Capability>,
    #[serde(default)]
    pub routes: bool,
    /// nono-mirroring YAML merged over balerix's base profile; passthrough.
    #[serde(default = "empty_object")]
    pub sandbox: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSubscriptions {
    #[serde(default)]
    pub observe: BTreeSet<String>,
    #[serde(default)]
    pub intercept: BTreeSet<String>,
}

/// Host capabilities a plugin may declare (plugins spec §4.1; `manage` is
/// Spec L-1: apply and down a fleet from an unresolved fleet file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    Fleets,
    Actions,
    Attach,
    Kv,
    Workspace,
    Manage,
}

/// `$XDG_CONFIG_HOME/balerix/plugins.yaml` (plugins spec §2.1). Order is
/// the interceptor order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsFile {
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEntry {
    pub name: String,
    /// `https://` URL, tarball path, or directory path (relative to the
    /// file). URL sources are part of the format but rejected until a
    /// TLS-enabled build: `ureq` here has no TLS provider.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Config keys whose values the daemon reads from a host file at load,
    /// so a secret need not be written into `plugins.yaml` (plugins spec
    /// G-7). Paths are relative to this file, like `source`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, PathBuf>,
    /// Daemon-level config, passed verbatim in the `hello` reply.
    #[serde(default = "empty_object")]
    pub config: Value,
    /// An operator-written settings layer beneath every fleet file this
    /// plugin applies (Spec M §12.1): the same shape as a fleet file's
    /// `defaults`, and the one place `claude.binary`, `claude.args`,
    /// `env` and `sandbox` may be set for a plugin's agents, since the
    /// plugin's own file may not. Layered between the host's
    /// `settings.json` and the file's `defaults`.
    #[serde(
        default = "empty_object",
        rename = "fleetDefaults",
        skip_serializing_if = "is_empty_object"
    )]
    pub fleet_defaults: Value,
}

/// Body of `POST /v1/plugin-host/hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloRequest {
    pub name: String,
    pub version: String,
    pub protocol: u32,
    /// `127.0.0.1:<port>` the plugin listens on. Ignored in Kubernetes
    /// mode, where the Daemon calls the address the operator gave (Spec O
    /// §23.1).
    pub listen: String,
    /// The plugin's own `balerix-plugin.yaml` (Spec O §23.1). Required by a
    /// Daemon in Kubernetes mode, which checks its `needs` against the
    /// grant; ignored on one machine, which reads the package. Sent only
    /// when the plugin has an authority to trust: a 0.2.0 daemon refuses
    /// unknown fields here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<PluginManifest>,
}

/// One entry of `PUT /v1/plugins` (Spec O §23.2): what the operator knows
/// of a plugin it runs. Order in the list is interceptor order.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredPlugin {
    pub name: String,
    /// The capabilities the manifest's `needs` must stay within.
    pub grant: BTreeSet<Capability>,
    /// Daemon-level config with secrets injected, handed back in `hello`.
    #[serde(default = "empty_object")]
    pub config: Value,
    #[serde(
        default = "empty_object",
        rename = "fleetDefaults",
        skip_serializing_if = "is_empty_object"
    )]
    pub fleet_defaults: Value,
    /// The plugin's token: its bearer to the Daemon and the Daemon's to it.
    pub token: String,
    /// `https://<plugin>.<namespace>.svc:7644`.
    pub url: String,
}

/// Hand-written: `token` and `config` hold secrets.
impl std::fmt::Debug for DeclaredPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclaredPlugin")
            .field("name", &self.name)
            .field("grant", &self.grant)
            .field("config", &"<redacted>")
            .field("fleet_defaults", &self.fleet_defaults)
            .field("token", &"<redacted>")
            .field("url", &self.url)
            .finish()
    }
}

/// Body of `PUT /v1/plugins`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredPlugins {
    pub plugins: Vec<DeclaredPlugin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResponse {
    pub config: Value,
}

/// One row of `GET /v1/managed-fleets` (Spec O §23.3): a plugin's
/// unresolved fleet file, for the operator to write as a Fleet. `down` is
/// the plugin's down query once it downed the fleet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedFleet {
    pub name: String,
    pub plugin: String,
    pub file: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down: Option<crate::DownQuery>,
}

/// One row of `GET /v1/plugins`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStatus {
    pub name: String,
    pub version: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub listen: Option<String>,
    pub routes: bool,
    #[serde(default)]
    pub message: String,
    /// Agents this plugin is currently `Active` for.
    #[serde(default)]
    pub active_agents: u32,
}

/// What `POST /v1/plugins/sync` did, by plugin name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub installed: Vec<String>,
    pub stopped: Vec<String>,
    pub unchanged: Vec<String>,
    /// Fleets downed because the plugin that owned them was stopped
    /// (Spec L-6). Absent from a report by an older daemon.
    #[serde(default)]
    pub downed: Vec<String>,
    /// `<fleet>: <error>` for each owned fleet the daemon could not down;
    /// the removal went on regardless.
    #[serde(default)]
    pub down_failed: Vec<String>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn is_empty_object(v: &Value) -> bool {
    v.as_object().is_some_and(|m| m.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serde_norway;

    /// `balerix-api` has no YAML dependency; the manifest is YAML on disk
    /// but JSON-shaped, so the fixture is JSON here.
    fn full_manifest() -> PluginManifest {
        serde_json::from_value(json!({
            "apiVersion": "balerix/v1", "kind": "Plugin", "name": "web", "version": "0.1.0",
            "protocol": 1, "start": "serve",
            "hooks": { "observe": ["SessionStart", "SessionEnd"], "intercept": ["PreToolUse"] },
            "needs": ["fleets", "attach"], "routes": true,
            "sandbox": { "network": { "block": true } }
        }))
        .unwrap()
    }

    #[test]
    fn manifest_parses_with_defaults_for_the_optional_blocks() {
        let m = full_manifest();
        assert_eq!(m.name, "web");
        assert_eq!(m.protocol, 1);
        assert_eq!(m.start, "serve");
        assert!(m.hooks.observe.contains("SessionEnd"));
        assert!(m.hooks.intercept.contains("PreToolUse"));
        assert!(m.needs.contains(&Capability::Fleets));
        assert!(m.needs.contains(&Capability::Attach));
        assert!(m.routes);
        assert_eq!(m.sandbox["network"]["block"], true);

        let minimal: PluginManifest = serde_json::from_value(json!({
            "apiVersion": "balerix/v1", "kind": "Plugin", "name": "x",
            "version": "0.0.1", "protocol": 1, "start": "run"
        }))
        .unwrap();
        assert!(minimal.hooks.observe.is_empty() && minimal.hooks.intercept.is_empty());
        assert!(minimal.needs.is_empty());
        assert!(!minimal.routes);
        assert_eq!(minimal.sandbox, json!({}));
    }

    #[test]
    fn manifest_rejects_unknown_fields_and_capabilities() {
        assert!(
            serde_json::from_value::<PluginManifest>(json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": "x",
                "version": "0.0.1", "protocol": 1, "start": "run", "nope": 1
            }))
            .is_err()
        );
        assert!(serde_json::from_value::<Capability>(json!("root")).is_err());
        assert_eq!(
            serde_json::to_value(Capability::Kv).unwrap(),
            json!("kv"),
            "capabilities are lowercase on the wire"
        );
        assert_eq!(
            serde_json::from_value::<Capability>(json!("workspace")).unwrap(),
            Capability::Workspace
        );
        assert_eq!(
            serde_json::from_value::<Capability>(json!("manage")).unwrap(),
            Capability::Manage,
            "Spec L-1: the capability that gates the two fleet routes"
        );
        assert_eq!(
            serde_json::to_value(Capability::Manage).unwrap(),
            json!("manage")
        );
    }

    /// A report from a daemon that predates Spec L has no `downed` lists.
    #[test]
    fn a_sync_report_without_the_downed_lists_still_loads() {
        let older: SyncReport = serde_json::from_value(json!({
            "installed": [], "stopped": ["x"], "unchanged": []
        }))
        .unwrap();
        assert!(older.downed.is_empty() && older.down_failed.is_empty());
        let r = SyncReport {
            downed: vec!["f".into()],
            down_failed: vec!["g: fleet task is gone".into()],
            ..SyncReport::default()
        };
        let back: SyncReport = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn plugins_file_round_trips_and_keeps_sources_verbatim() {
        let f: PluginsFile = serde_json::from_value(json!({
            "plugins": [
                { "name": "flow", "source": "https://x/flow.tar.gz", "sha256": "ab" },
                { "name": "web", "source": "./web", "config": { "title": "t" } }
            ]
        }))
        .unwrap();
        assert_eq!(f.plugins.len(), 2);
        assert_eq!(f.plugins[0].sha256.as_deref(), Some("ab"));
        assert_eq!(f.plugins[1].sha256, None);
        assert_eq!(f.plugins[1].config["title"], "t");
        assert_eq!(f.plugins[0].config, json!({}));
        let back = serde_json::to_value(&f).unwrap();
        assert!(back["plugins"][1].get("sha256").is_none());
        let empty: PluginsFile = serde_json::from_value(json!({})).unwrap();
        assert!(empty.plugins.is_empty());
        assert!(serde_json::from_value::<PluginsFile>(json!({ "plugin": [] })).is_err());
    }

    #[test]
    fn hello_status_and_report_round_trip() {
        let h = HelloRequest {
            name: "web".into(),
            version: "0.1.0".into(),
            protocol: PLUGIN_PROTOCOL,
            listen: "127.0.0.1:4321".into(),
            manifest: None,
        };
        let back: HelloRequest = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back, h);
        let r: HelloResponse = serde_json::from_value(json!({ "config": { "a": 1 } })).unwrap();
        assert_eq!(r.config["a"], 1);
        let s = PluginStatus {
            name: "web".into(),
            version: "0.1.0".into(),
            phase: crate::AgentPhase::Ready,
            listen: Some("127.0.0.1:4321".into()),
            routes: true,
            message: String::new(),
            active_agents: 0,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["phase"], "ready");
        assert_eq!(v["active_agents"], 0);
        let older: PluginStatus = serde_json::from_value(json!({
            "name": "web", "version": "0.1.0", "phase": "ready", "routes": false
        }))
        .unwrap();
        assert_eq!(older.active_agents, 0, "a phase 1 row still loads");
        let rep = SyncReport::default();
        assert!(rep.installed.is_empty() && rep.stopped.is_empty() && rep.unchanged.is_empty());
        assert_eq!(PLUGIN_KIND, "Plugin");
    }

    #[test]
    fn an_entry_carries_secret_file_paths_and_omits_an_empty_map() {
        let f: PluginsFile = serde_json::from_value(json!({
            "plugins": [
                { "name": "matrix", "source": "./matrix",
                  "secrets": { "password": "../secrets/matrix-password" } },
                { "name": "web", "source": "./web" }
            ]
        }))
        .unwrap();
        assert_eq!(
            f.plugins[0].secrets.get("password").map(|p| p.as_path()),
            Some(std::path::Path::new("../secrets/matrix-password"))
        );
        assert!(f.plugins[1].secrets.is_empty(), "absent means empty");
        let back = serde_json::to_value(&f).unwrap();
        assert!(
            back["plugins"][1].get("secrets").is_none(),
            "an empty map is not serialized"
        );
    }

    #[test]
    fn an_entry_without_fleet_defaults_round_trips_as_before() {
        let yaml = "name: gh\nsource: ./gh\nconfig:\n  appId: 1\n";
        let e: PluginEntry = serde_norway::from_str(yaml).unwrap();
        assert_eq!(e.fleet_defaults, json!({}));
        let out = serde_json::to_value(&e).unwrap();
        assert!(
            out.get("fleetDefaults").is_none(),
            "an empty layer is not written back: {out}"
        );
    }

    /// Review focus 5: a `plugins.yaml` in the form `plugin add` writes
    /// (`serde_norway::to_string` of the file), with no `fleetDefaults`,
    /// loads and writes back byte for byte.
    #[test]
    fn a_plugins_file_without_fleet_defaults_round_trips_byte_identical() {
        let text = "plugins:\n- name: gh\n  source: ./gh\n  sha256: abc\n  secrets:\n    key: ../k.pem\n  config:\n    appId: 1\n- name: web\n  source: ./web\n  config: {}\n";
        let f: PluginsFile = serde_norway::from_str(text).unwrap();
        assert_eq!(serde_norway::to_string(&f).unwrap(), text);
        let built = PluginsFile {
            plugins: vec![PluginEntry {
                name: "web".into(),
                source: "./web".into(),
                sha256: None,
                secrets: BTreeMap::new(),
                config: json!({}),
                fleet_defaults: json!({}),
            }],
        };
        assert_eq!(
            serde_norway::to_string(&built).unwrap(),
            "plugins:\n- name: web\n  source: ./web\n  config: {}\n"
        );
    }

    #[test]
    fn fleet_defaults_is_read_under_its_camel_case_name() {
        let yaml = "name: gh\nsource: ./gh\nfleetDefaults:\n  claude: { binary: /opt/claude }\n  sandbox: { network: { block: false } }\n";
        let e: PluginEntry = serde_norway::from_str(yaml).unwrap();
        assert_eq!(e.fleet_defaults["claude"]["binary"], "/opt/claude");
        assert_eq!(e.fleet_defaults["sandbox"]["network"]["block"], false);
        let out = serde_json::to_value(&e).unwrap();
        assert_eq!(out["fleetDefaults"]["claude"]["binary"], "/opt/claude");
        let err =
            serde_norway::from_str::<PluginEntry>("name: gh\nsource: ./gh\nfleet_defaults: {}\n")
                .unwrap_err()
                .to_string();
        assert!(err.contains("unknown field `fleet_defaults`"), "{err}");
    }

    #[test]
    fn a_hello_without_a_manifest_still_parses_and_serialises_without_the_key() {
        let old = r#"{"name":"web","version":"0.2.1","protocol":1,"listen":"127.0.0.1:9"}"#;
        let h: HelloRequest = serde_json::from_str(old).unwrap();
        assert_eq!(h.manifest, None);
        let back = serde_json::to_value(&h).unwrap();
        assert!(back.get("manifest").is_none(), "{back}");
    }

    #[test]
    fn a_hello_carries_the_manifest_when_given() {
        let m: PluginManifest = serde_norway::from_str(
            "apiVersion: balerix/v1\nkind: Plugin\nname: flow\nversion: 0.1.1\nprotocol: 1\nstart: serve\nneeds: [actions, kv]\n",
        )
        .unwrap();
        let h = HelloRequest {
            name: "flow".into(),
            version: "0.1.1".into(),
            protocol: PLUGIN_PROTOCOL,
            listen: "0.0.0.0:7644".into(),
            manifest: Some(m.clone()),
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["manifest"]["needs"], serde_json::json!(["actions", "kv"]));
        let back: HelloRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back.manifest, Some(m));
    }

    #[test]
    fn a_declared_plugin_parses_redacts_and_refuses_an_unknown_capability() {
        let body = serde_json::json!({ "plugins": [{
            "name": "flow", "grant": ["actions", "kv"], "config": { "secret": "s3cret" },
            "fleetDefaults": { "claude": { "binary": "fake-claude" } },
            "token": "t0123456789abcdef0123456789abcdef", "url": "https://flow.ns.svc:7644"
        }]});
        let d: DeclaredPlugins = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(
            d.plugins[0].grant,
            BTreeSet::from([Capability::Actions, Capability::Kv])
        );
        let dbg = format!("{:?}", d.plugins[0]);
        assert!(
            !dbg.contains("s3cret") && !dbg.contains("t0123456789"),
            "{dbg}"
        );
        let mut bad = body;
        bad["plugins"][0]["grant"] = serde_json::json!(["root"]);
        let err = serde_json::from_value::<DeclaredPlugins>(bad)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown variant `root`"), "{err}");
    }

    #[test]
    fn a_managed_fleet_row_omits_down_while_live() {
        let row = ManagedFleet {
            name: "gh-1".into(),
            plugin: "github".into(),
            file: serde_json::json!({ "crews": {} }),
            down: None,
        };
        let v = serde_json::to_value(&row).unwrap();
        assert!(v.get("down").is_none(), "{v}");
    }
}
