//! Plugins as the daemon sees them (plugins spec §3, §5.2): the resolved
//! package, manifest validation, and the synthetic fleet the reconciler
//! drives them through.

use std::collections::BTreeMap;
use std::path::PathBuf;

use balerix_api::{
    API_VERSION, AgentSettings, GitAuth, GitSettings, HOOK_EVENTS, PLUGIN_KIND, PLUGIN_PROTOCOL,
    PluginManifest, SpecHash,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::fleet::{Crew, Fleet};
use crate::name::{AgentId, AgentName, validate_name};
use crate::repo::RepoRef;
use crate::version::is_exact_version;

/// The daemon's own fleet: every plugin is an agent of its `plugins` crew.
/// User fleets may not take this name (checked at the API and client-side;
/// `FleetName` itself still parses it because the plugin actor round-trips
/// it through `Fleet::try_from` like every other fleet).
pub const RESERVED_FLEET: &str = "balerix";
pub const PLUGIN_CREW: &str = "plugins";

/// The plugin fleet's name.
pub fn is_reserved_fleet(name: &str) -> bool {
    name == RESERVED_FLEET
}

/// Why a user fleet may not take `name`, if it may not. `balerix` is the
/// plugin fleet; `watch` is a legal label that `GET
/// /v1/plugin-host/fleets/watch` would shadow, so a fleet of that name
/// could never be fetched by a plugin.
pub fn reserved_fleet_reason(name: &str) -> Option<&'static str> {
    match name {
        RESERVED_FLEET => Some("reserved for the daemon's plugins"),
        "watch" => Some("reserved: it would shadow the plugin host's fleets/watch route"),
        _ => None,
    }
}

/// `balerix/plugins/<name>`.
pub fn plugin_id(name: &AgentName) -> AgentId {
    AgentId {
        // Both literals satisfy `validate_name`; a failure here would be a
        // programming error, so fall through to the unchecked constructor.
        fleet: RESERVED_FLEET.parse().unwrap_or_else(|_| unreachable!()),
        crew: PLUGIN_CREW.parse().unwrap_or_else(|_| unreachable!()),
        agent: name.clone(),
    }
}

/// One plugin after `plugins.yaml` was synced: where its package is, what
/// its manifest says, and the daemon-level config it gets at `hello`.
#[derive(Clone, PartialEq)]
pub struct ResolvedPlugin {
    pub name: AgentName,
    /// Package root (installed tarball or a directory used in place).
    pub package: PathBuf,
    pub manifest: PluginManifest,
    pub config: Value,
    /// sha256 hex of the tarball; `None` for a directory source.
    pub digest: Option<String>,
}

/// `config` may carry a resolved secret (plugins spec G-7), so it is never
/// printed. Everything else is, because it is what a log line is for.
impl std::fmt::Debug for ResolvedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedPlugin")
            .field("name", &self.name)
            .field("package", &self.package)
            .field("manifest", &self.manifest)
            .field("config", &"<redacted>")
            .field("digest", &self.digest)
            .finish()
    }
}

#[derive(Serialize)]
struct HashInput<'a> {
    package: String,
    manifest: &'a PluginManifest,
    config: &'a Value,
    digest: &'a Option<String>,
}

impl ResolvedPlugin {
    pub fn id(&self) -> AgentId {
        plugin_id(&self.name)
    }

    /// Exactly what, when changed, must restart the plugin.
    pub fn hash(&self) -> SpecHash {
        let input = HashInput {
            package: self.package.display().to_string(),
            manifest: &self.manifest,
            config: &self.config,
            digest: &self.digest,
        };
        let bytes = serde_json::to_vec(&input).unwrap_or_default();
        SpecHash::new(hex::encode(Sha256::digest(bytes)))
    }
}

/// Why a package is not a valid plugin. Messages start with the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("balerix-plugin.yaml: {path}: {message}")]
    Manifest { path: String, message: String },
    #[error("mise.toml: {}{message}", if path.is_empty() { String::new() } else { format!("{path}: ") })]
    MiseToml { path: String, message: String },
}

/// Plugins spec §2 rules, applied to a parsed manifest and the text of the
/// package's `mise.toml`.
pub fn validate_manifest(m: &PluginManifest, mise_toml: &str) -> Result<(), ManifestError> {
    let bad = |path: &str, message: String| ManifestError::Manifest {
        path: path.to_string(),
        message,
    };
    if m.api_version != API_VERSION {
        return Err(bad(
            "apiVersion",
            format!("expected {API_VERSION:?}, got {:?}", m.api_version),
        ));
    }
    if m.kind != PLUGIN_KIND {
        return Err(bad(
            "kind",
            format!("expected {PLUGIN_KIND:?}, got {:?}", m.kind),
        ));
    }
    if let Err(reason) = validate_name(&m.name) {
        return Err(bad(
            "name",
            format!("invalid plugin name {:?}: {reason}", m.name),
        ));
    }
    if m.version.trim().is_empty() {
        return Err(bad("version", "must not be empty".into()));
    }
    if m.protocol != PLUGIN_PROTOCOL {
        return Err(bad(
            "protocol",
            format!(
                "this daemon speaks protocol {PLUGIN_PROTOCOL}, got {}",
                m.protocol
            ),
        ));
    }
    if m.start.trim().is_empty() {
        return Err(bad("start", "must not be empty".into()));
    }
    for (list, events) in [
        ("hooks.observe", &m.hooks.observe),
        ("hooks.intercept", &m.hooks.intercept),
    ] {
        if let Some(e) = events.iter().find(|e| !HOOK_EVENTS.contains(&e.as_str())) {
            return Err(bad(list, format!("unknown event {e:?}")));
        }
    }
    if !m.sandbox.is_object() {
        return Err(bad("sandbox", "expected a mapping".into()));
    }
    validate_mise_toml(mise_toml, &m.start)
}

fn validate_mise_toml(text: &str, start: &str) -> Result<(), ManifestError> {
    let doc: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| ManifestError::MiseToml {
            path: String::new(),
            message: e.message().to_string(),
        })?;
    if let Some(toml::Value::Table(tools)) = doc.get("tools") {
        for (k, v) in tools {
            let version = match v {
                toml::Value::String(s) => s.clone(),
                toml::Value::Table(t) => match t.get("version") {
                    Some(toml::Value::String(s)) => s.clone(),
                    _ => {
                        return Err(ManifestError::MiseToml {
                            path: format!("tools.{k}"),
                            message: "expected a version string".into(),
                        });
                    }
                },
                _ => {
                    return Err(ManifestError::MiseToml {
                        path: format!("tools.{k}"),
                        message: "expected a version string".into(),
                    });
                }
            };
            if !is_exact_version(&version) {
                return Err(ManifestError::MiseToml {
                    path: format!("tools.{k}"),
                    message: format!("expected an exact version, got {version:?}"),
                });
            }
        }
    }
    let defined = matches!(doc.get("tasks"), Some(toml::Value::Table(t)) if t.contains_key(start));
    if !defined {
        return Err(ManifestError::MiseToml {
            path: format!("tasks.{start}"),
            message: "the manifest's `start` task is not defined".into(),
        });
    }
    Ok(())
}

/// The synthetic fleet the reconciler drives plugins through (plugins spec
/// §5.2): fleet `balerix`, crew `plugins`, one agent per plugin. The agent's
/// settings carry nothing but the plugin's hash, so `ResolvedAgent::hash`
/// changes exactly when the plugin must restart. `PluginMaterializer` in
/// the server maps the ids back to `ResolvedPlugin`s; the repo is a
/// placeholder its `ensure_crew` never touches.
pub fn plugin_fleet(plugins: &[ResolvedPlugin]) -> Fleet {
    let agents: BTreeMap<AgentName, AgentSettings> = plugins
        .iter()
        .map(|p| {
            let mut s = AgentSettings::default();
            s.env.insert(
                "BALERIX_PLUGIN_HASH".to_string(),
                p.hash().as_str().to_string(),
            );
            (p.name.clone(), s)
        })
        .collect();
    Fleet {
        name: RESERVED_FLEET.parse().unwrap_or_else(|_| unreachable!()),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            PLUGIN_CREW.parse().unwrap_or_else(|_| unreachable!()),
            Crew {
                repo: RepoRef::Local(PathBuf::from("/dev/null")),
                git_ref: "none".to_string(),
                git: GitSettings {
                    push: false,
                    auth: GitAuth::None,
                    identity: None,
                },
                tools: BTreeMap::new(),
                agents,
            },
        )]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResolvedAgent;
    use balerix_api::{Capability, HookSubscriptions};

    use serde_json::json;

    #[test]
    fn two_fleet_names_are_reserved_and_only_one_is_the_plugin_fleet() {
        assert_eq!(
            reserved_fleet_reason("balerix"),
            Some("reserved for the daemon's plugins")
        );
        assert_eq!(
            reserved_fleet_reason("watch"),
            Some("reserved: it would shadow the plugin host's fleets/watch route")
        );
        assert_eq!(reserved_fleet_reason("payments"), None);
        assert!(is_reserved_fleet("balerix"));
        assert!(!is_reserved_fleet("watch"), "not the plugin fleet");
    }
    use std::collections::BTreeSet;

    const MISE: &str = "[tools]\nttyd = \"1.7.7\"\n\n[tasks.serve]\nrun = \"python3 plugin.py\"\n";

    fn manifest(name: &str) -> PluginManifest {
        PluginManifest {
            api_version: "balerix/v1".into(),
            kind: "Plugin".into(),
            name: name.into(),
            version: "0.1.0".into(),
            protocol: 1,
            start: "serve".into(),
            hooks: HookSubscriptions {
                observe: BTreeSet::from(["SessionStart".to_string()]),
                intercept: BTreeSet::new(),
            },
            needs: BTreeSet::from([Capability::Fleets]),
            routes: true,
            sandbox: json!({}),
        }
    }

    fn plugin(name: &str, config: serde_json::Value) -> ResolvedPlugin {
        ResolvedPlugin {
            name: name.parse().unwrap(),
            package: format!("/pkg/{name}").into(),
            manifest: manifest(name),
            config,
            digest: Some("abc".into()),
        }
    }

    fn plugin_fixture() -> ResolvedPlugin {
        plugin("web", json!({}))
    }

    #[test]
    fn debug_never_prints_the_resolved_config() {
        let mut p = plugin_fixture();
        p.config = serde_json::json!({ "password": "hunter2" });
        let text = format!("{p:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(text.contains("name"), "the rest is still readable: {text}");
    }

    #[test]
    fn the_daemon_fleet_is_reserved_and_plugin_ids_live_under_it() {
        assert!(is_reserved_fleet("balerix"));
        assert!(!is_reserved_fleet("payments"));
        assert_eq!(
            plugin_id(&"web".parse().unwrap()).to_string(),
            "balerix/plugins/web"
        );
        assert_eq!(
            plugin("web", json!({})).id().to_string(),
            "balerix/plugins/web"
        );
    }

    #[test]
    fn a_valid_manifest_passes() {
        validate_manifest(&manifest("web"), MISE).unwrap();
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn manifest_errors_carry_the_config_path() {
        let cases: Vec<(Box<dyn Fn(&mut PluginManifest)>, &str)> = vec![
            (
                Box::new(|m| m.api_version = "balerix/v2".into()),
                "balerix-plugin.yaml: apiVersion: expected \"balerix/v1\", got \"balerix/v2\"",
            ),
            (
                Box::new(|m| m.kind = "Fleet".into()),
                "balerix-plugin.yaml: kind: expected \"Plugin\", got \"Fleet\"",
            ),
            (
                Box::new(|m| m.name = "Web".into()),
                "balerix-plugin.yaml: name: invalid plugin name \"Web\": contains characters other than a-z, 0-9 and '-'",
            ),
            (
                Box::new(|m| m.version = " ".into()),
                "balerix-plugin.yaml: version: must not be empty",
            ),
            (
                Box::new(|m| m.protocol = 2),
                "balerix-plugin.yaml: protocol: this daemon speaks protocol 1, got 2",
            ),
            (
                Box::new(|m| m.start = String::new()),
                "balerix-plugin.yaml: start: must not be empty",
            ),
            (
                Box::new(|m| {
                    m.hooks.intercept.insert("Foo".into());
                }),
                "balerix-plugin.yaml: hooks.intercept: unknown event \"Foo\"",
            ),
            (
                Box::new(|m| {
                    m.hooks.observe.insert("Bar".into());
                }),
                "balerix-plugin.yaml: hooks.observe: unknown event \"Bar\"",
            ),
            (
                Box::new(|m| m.sandbox = json!([1])),
                "balerix-plugin.yaml: sandbox: expected a mapping",
            ),
        ];
        for (mutate, expected) in cases {
            let mut m = manifest("web");
            mutate(&mut m);
            assert_eq!(
                validate_manifest(&m, MISE).unwrap_err().to_string(),
                expected
            );
        }
    }

    #[test]
    fn mise_toml_must_parse_pin_exactly_and_define_the_start_task() {
        let e = validate_manifest(&manifest("web"), "[tools\n").unwrap_err();
        assert!(e.to_string().starts_with("mise.toml: "), "{e}");
        assert_eq!(
            validate_manifest(
                &manifest("web"),
                "[tools]\nttyd = \"latest\"\n[tasks.serve]\nrun = \"x\"\n"
            )
            .unwrap_err()
            .to_string(),
            "mise.toml: tools.ttyd: expected an exact version, got \"latest\""
        );
        assert_eq!(
            validate_manifest(&manifest("web"), "[tools]\n")
                .unwrap_err()
                .to_string(),
            "mise.toml: tasks.serve: the manifest's `start` task is not defined"
        );
        // inline table form of tasks is accepted too
        validate_manifest(&manifest("web"), "tasks = { serve = \"python3 p.py\" }\n").unwrap();
        // a tools entry in table form with a version key
        validate_manifest(
            &manifest("web"),
            "[tools]\nnode = { version = \"22.11.0\" }\n[tasks.serve]\nrun = \"x\"\n",
        )
        .unwrap();
    }

    #[test]
    fn hash_covers_package_manifest_config_and_digest() {
        let a = plugin("web", json!({ "title": "t" }));
        assert_eq!(a.hash(), plugin("web", json!({ "title": "t" })).hash());
        let mut b = a.clone();
        b.config = json!({ "title": "u" });
        assert_ne!(a.hash(), b.hash());
        let mut c = a.clone();
        c.package = "/elsewhere".into();
        assert_ne!(a.hash(), c.hash());
        let mut d = a.clone();
        d.digest = None;
        assert_ne!(a.hash(), d.hash());
        let mut e = a.clone();
        e.manifest.routes = false;
        assert_ne!(a.hash(), e.hash());
        assert_eq!(a.hash().as_str().len(), 64);
    }

    #[test]
    fn plugin_fleet_renders_one_agent_per_plugin_whose_hash_tracks_the_plugin() {
        let plugins = vec![plugin("web", json!({})), plugin("flow", json!({ "x": 1 }))];
        let fleet = plugin_fleet(&plugins);
        assert_eq!(fleet.name.as_str(), RESERVED_FLEET);
        let crew = &fleet.crews[&PLUGIN_CREW.parse().unwrap()];
        assert_eq!(crew.agents.len(), 2);
        let agents = ResolvedAgent::from_fleet(&fleet);
        assert_eq!(agents[0].id.to_string(), "balerix/plugins/flow");
        assert_eq!(agents[1].id.to_string(), "balerix/plugins/web");
        assert_eq!(
            agents[1].settings.env["BALERIX_PLUGIN_HASH"],
            plugins[0].hash().as_str()
        );
        // a plugin change changes the synthetic agent's hash; an unchanged one does not
        let before = agents[1].hash();
        let same = ResolvedAgent::from_fleet(&plugin_fleet(&plugins));
        assert_eq!(same[1].hash(), before);
        let mut changed = plugins.clone();
        changed[0].config = json!({ "title": "x" });
        let after = ResolvedAgent::from_fleet(&plugin_fleet(&changed));
        assert_ne!(after[1].hash(), before);
        // the empty list is a valid fleet with one empty crew
        let empty = plugin_fleet(&[]);
        assert!(empty.crews[&PLUGIN_CREW.parse().unwrap()].agents.is_empty());
    }
}
