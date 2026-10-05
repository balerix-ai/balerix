//! The Kubernetes-mode plugin source (Spec O §23.2): the operator's list
//! of plugins it runs, `hello` checked against each one's grant, and
//! readiness from the health poll. The Daemon launches nothing.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use balerix_api::{
    AgentPhase, Capability, DeclaredPlugin, HelloRequest, HelloResponse, PLUGIN_PROTOCOL,
    PluginManifest, PluginStatus,
};
use balerix_core::{AgentName, reserved_plugin_reason};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::daemon::DaemonError;
use crate::plugins::{PluginAddr, PluginRegistry, wire_label};

/// Consecutive failed polls before a ready plugin leaves the interceptor
/// chain: one slow answer (a GC pause) must not (§23.2).
pub const MISSED_POLLS: u32 = 3;

const MIN_TOKEN: usize = 32;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("plugins[{index}].{field}: {message}")]
pub struct DeclareError {
    pub index: usize,
    pub field: &'static str,
    pub message: String,
}

enum HelloState {
    Waiting,
    Refused(String),
    Accepted {
        manifest: Box<PluginManifest>,
        ready: bool,
        failed: Option<String>,
    },
}

struct Entry {
    name: AgentName,
    plugin: DeclaredPlugin,
    hash: String,
    hello: HelloState,
    misses: u32,
}

#[derive(Serialize, Deserialize)]
struct HelloFile {
    entry: String,
    manifest: PluginManifest,
}

pub struct DeclaredPlugins {
    state_dir: PathBuf,
    registry: Arc<PluginRegistry>,
    entries: Mutex<Vec<Entry>>,
}

fn hash(plugin: &DeclaredPlugin) -> String {
    // Maps and sets serialise in a stable order, so the bytes are stable.
    let bytes = serde_json::to_vec(plugin).unwrap_or_default();
    hex::encode(Sha256::digest(bytes))
}

/// The manifest's name and needs against the plugin and its grant.
fn check_manifest(plugin: &DeclaredPlugin, m: &PluginManifest) -> Result<(), String> {
    if m.name != plugin.name {
        return Err(format!(
            "hello.manifest.name: {:?} does not match the plugin {:?}",
            m.name, plugin.name
        ));
    }
    if let Some(cap) = m.needs.iter().find(|c| !plugin.grant.contains(c)) {
        return Err(format!(
            "hello.manifest.needs: {} is not granted",
            wire_label::<Capability>(*cap)
        ));
    }
    Ok(())
}

impl DeclaredPlugins {
    pub fn new(state_dir: PathBuf, registry: Arc<PluginRegistry>) -> Arc<Self> {
        Arc::new(Self {
            state_dir,
            registry,
            entries: Mutex::new(Vec::new()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn hello_path(&self, name: &str) -> PathBuf {
        self.state_dir.join(name).join("hello.json")
    }

    /// `PUT /v1/plugins`: validates the whole list, then replaces it.
    /// Returns the names that left the list.
    pub fn replace(&self, list: Vec<DeclaredPlugin>) -> Result<Vec<AgentName>, DeclareError> {
        let mut names = Vec::with_capacity(list.len());
        for (index, p) in list.iter().enumerate() {
            let err = |field, message: String| DeclareError {
                index,
                field,
                message,
            };
            let name =
                AgentName::try_from(p.name.as_str()).map_err(|e| err("name", e.to_string()))?;
            if let Some(reason) = reserved_plugin_reason(&p.name) {
                return Err(err("name", reason.to_string()));
            }
            if names.contains(&name) {
                return Err(err("name", "listed twice".into()));
            }
            if p.token.len() < MIN_TOKEN {
                return Err(err(
                    "token",
                    format!("a token is at least {MIN_TOKEN} characters"),
                ));
            }
            if !p.url.starts_with("https://") {
                return Err(err("url", "must be https://".into()));
            }
            names.push(name);
        }

        let mut entries = self.lock();
        let mut old: Vec<Entry> = std::mem::take(&mut *entries);
        let mut restored = Vec::new();
        for (name, plugin) in names.iter().zip(list) {
            let h = hash(&plugin);
            let prev = old
                .iter()
                .position(|e| e.name == *name && e.hash == h)
                .map(|i| old.remove(i));
            let (hello, misses) = match prev {
                Some(e) => (e.hello, e.misses),
                None => match self.restore(&plugin, &h) {
                    Some(manifest) => {
                        restored.push((name.clone(), manifest.clone()));
                        (
                            HelloState::Accepted {
                                manifest: Box::new(manifest),
                                ready: false,
                                failed: None,
                            },
                            0,
                        )
                    }
                    None => (HelloState::Waiting, 0),
                },
            };
            entries.push(Entry {
                name: name.clone(),
                plugin,
                hash: h,
                hello,
                misses,
            });
        }
        // What is left of `old` had a changed entry or was dropped; only
        // the dropped lose their hello file.
        let dropped: Vec<AgentName> = old
            .into_iter()
            .map(|e| e.name)
            .filter(|n| !names.contains(n))
            .collect();
        for n in &dropped {
            let _ = std::fs::remove_file(self.hello_path(n.as_str()));
        }

        let defaults: Vec<(AgentName, Value)> = entries
            .iter()
            .map(|e| (e.name.clone(), e.plugin.fleet_defaults.clone()))
            .collect();
        let keep: Vec<AgentName> = entries
            .iter()
            .filter(|e| matches!(e.hello, HelloState::Accepted { .. }))
            .map(|e| e.name.clone())
            .collect();
        self.registry.declare(&defaults, &keep);
        for (name, manifest) in restored {
            if let Some(e) = entries.iter().find(|e| e.name == name) {
                self.registry.accept_hello(
                    &name,
                    manifest,
                    e.plugin.url.clone(),
                    e.plugin.token.clone(),
                );
                self.registry.set_ready(&name, false);
            }
        }
        Ok(dropped)
    }

    /// A persisted hello counts only for the very entry it was accepted
    /// for: a new grant, config or token means a new hello (§23.2).
    fn restore(&self, plugin: &DeclaredPlugin, hash: &str) -> Option<PluginManifest> {
        let bytes = std::fs::read(self.hello_path(&plugin.name)).ok()?;
        let file: HelloFile = serde_json::from_slice(&bytes).ok()?;
        (file.entry == hash && check_manifest(plugin, &file.manifest).is_ok())
            .then_some(file.manifest)
    }

    fn persist(&self, name: &str, file: &HelloFile) {
        let path = self.hello_path(name);
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(
                &tmp,
                serde_json::to_vec(file).map_err(std::io::Error::other)?,
            )?;
            std::fs::rename(&tmp, &path)
        })();
        if let Err(e) = result {
            // Persistence only saves a re-hello after a restart.
            tracing::warn!(plugin = name, path = %path.display(), "could not persist hello: {e}");
        }
    }

    pub fn plugin_for_token(&self, token: &str) -> Option<AgentName> {
        self.lock()
            .iter()
            .find(|e| crate::auth::constant_time_eq(e.plugin.token.as_bytes(), token.as_bytes()))
            .map(|e| e.name.clone())
    }

    pub fn config(&self, name: &AgentName) -> Option<Value> {
        self.lock()
            .iter()
            .find(|e| e.name == *name)
            .map(|e| e.plugin.config.clone())
    }

    /// Every check of §23.2; on success the hello is accepted, persisted
    /// and the plugin is ready. A refusal is kept for `list`.
    pub fn hello(
        &self,
        name: &AgentName,
        req: &HelloRequest,
    ) -> Result<HelloResponse, DaemonError> {
        let mut entries = self.lock();
        let e = entries
            .iter_mut()
            .find(|e| e.name == *name)
            .ok_or(DaemonError::Unauthorized)?;
        let checked = if req.protocol != PLUGIN_PROTOCOL {
            Err(format!(
                "hello.protocol: this daemon speaks protocol {PLUGIN_PROTOCOL}, got {}",
                req.protocol
            ))
        } else {
            match &req.manifest {
                None => Err("hello.manifest: required in kubernetes mode".to_string()),
                Some(m) => check_manifest(&e.plugin, m).map(|()| m.clone()),
            }
        };
        let manifest = match checked {
            Ok(m) => m,
            Err(text) => {
                // The old hello no longer holds: out of the chain, and a
                // restart must not bring it back.
                e.hello = HelloState::Refused(text.clone());
                e.misses = 0;
                self.registry.unhello(name);
                let _ = std::fs::remove_file(self.hello_path(name.as_str()));
                return Err(DaemonError::Invalid(text));
            }
        };
        self.persist(
            name.as_str(),
            &HelloFile {
                entry: e.hash.clone(),
                manifest: manifest.clone(),
            },
        );
        self.registry.accept_hello(
            name,
            manifest.clone(),
            e.plugin.url.clone(),
            e.plugin.token.clone(),
        );
        e.hello = HelloState::Accepted {
            manifest: Box::new(manifest),
            ready: true,
            failed: None,
        };
        e.misses = 0;
        Ok(HelloResponse {
            config: e.plugin.config.clone(),
        })
    }

    /// One health result; `true` when the plugin just became ready again
    /// and the caller should re-send its activations.
    pub fn health(&self, name: &AgentName, ok: Result<(), String>) -> bool {
        let mut entries = self.lock();
        let Some(e) = entries.iter_mut().find(|e| e.name == *name) else {
            return false;
        };
        let HelloState::Accepted { ready, failed, .. } = &mut e.hello else {
            return false;
        };
        match ok {
            Ok(()) => {
                e.misses = 0;
                if *ready {
                    return false;
                }
                *ready = true;
                *failed = None;
                self.registry.set_ready(name, true);
                self.registry.set_degraded(name, None);
                true
            }
            Err(reason) => {
                e.misses += 1;
                if e.misses >= MISSED_POLLS && *ready {
                    *ready = false;
                    *failed = Some(format!("health: {reason}"));
                    self.registry.set_ready(name, false);
                    self.registry.set_degraded(name, Some(reason));
                }
                false
            }
        }
    }

    /// What to poll: every listed plugin with an accepted or restored hello.
    pub fn pollable(&self) -> Vec<(AgentName, PluginAddr)> {
        self.lock()
            .iter()
            .filter(|e| matches!(e.hello, HelloState::Accepted { .. }))
            .map(|e| {
                (
                    e.name.clone(),
                    PluginAddr {
                        listen: e.plugin.url.clone(),
                        token: e.plugin.token.clone(),
                    },
                )
            })
            .collect()
    }

    pub fn list(&self, active: impl Fn(&AgentName) -> u32) -> Vec<PluginStatus> {
        self.lock()
            .iter()
            .map(|e| {
                let (phase, message, manifest) = match &e.hello {
                    HelloState::Waiting => (AgentPhase::Starting, String::new(), None),
                    HelloState::Refused(m) => (AgentPhase::Failed, m.clone(), None),
                    HelloState::Accepted {
                        manifest,
                        ready,
                        failed,
                    } => {
                        let (phase, message) = match (ready, failed) {
                            (true, _) => (AgentPhase::Ready, String::new()),
                            (false, Some(m)) => (AgentPhase::Failed, m.clone()),
                            (false, None) => (AgentPhase::Starting, String::new()),
                        };
                        (phase, message, Some(manifest))
                    }
                };
                PluginStatus {
                    name: e.plugin.name.clone(),
                    version: manifest.map(|m| m.version.clone()).unwrap_or_default(),
                    phase,
                    listen: Some(e.plugin.url.clone()),
                    routes: manifest.is_some_and(|m| m.routes),
                    message,
                    active_agents: active(&e.name),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{AgentPhase, PLUGIN_PROTOCOL};
    use serde_json::json;

    fn entry(name: &str, grant: &[Capability]) -> DeclaredPlugin {
        DeclaredPlugin {
            name: name.into(),
            grant: grant.iter().copied().collect(),
            config: json!({ "k": name }),
            fleet_defaults: json!({}),
            token: format!("{name}-token-0123456789abcdef0123456789"),
            url: format!("https://{name}.ns.svc:7644"),
        }
    }

    fn hello(name: &str, needs: &str) -> HelloRequest {
        HelloRequest {
            name: name.into(),
            version: "1.0.0".into(),
            protocol: PLUGIN_PROTOCOL,
            listen: "0.0.0.0:7644".into(),
            manifest: Some(
                serde_norway::from_str(&format!(
                    "apiVersion: balerix/v1\nkind: Plugin\nname: {name}\nversion: 1.0.0\nprotocol: 1\nstart: serve\nneeds: [{needs}]\nhooks: {{ intercept: [Stop] }}\n"
                ))
                .unwrap(),
            ),
        }
    }

    fn n(s: &str) -> AgentName {
        s.parse().unwrap()
    }

    #[test]
    fn the_list_is_validated_whole() {
        let dir = tempfile::tempdir().unwrap();
        let d = DeclaredPlugins::new(dir.path().into(), PluginRegistry::new());
        let mut short = entry("flow", &[]);
        short.token = "short".into();
        let mut plain = entry("web", &[]);
        plain.url = "http://web:7644".into();
        let cases = [
            (
                vec![entry("flow", &[]), entry("flow", &[])],
                "plugins[1].name: listed twice",
            ),
            (
                vec![entry("kubernetes", &[])],
                "plugins[0].name: reserved: it is the owner name of the operator's fleets",
            ),
            (vec![entry("Bad Name", &[])], "plugins[0].name:"),
            (
                vec![short],
                "plugins[0].token: a token is at least 32 characters",
            ),
            (vec![plain], "plugins[0].url: must be https://"),
        ];
        for (list, want) in cases {
            let err = d.replace(list).unwrap_err().to_string();
            assert!(err.starts_with(want), "{err} / {want}");
        }
    }

    #[test]
    fn hello_is_checked_in_order_and_a_refusal_is_kept_on_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let reg = PluginRegistry::new();
        let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
        d.replace(vec![entry("flow", &[Capability::Actions, Capability::Kv])])
            .unwrap();
        assert!(reg.is_installed("flow"), "registered before hello");
        assert!(
            !reg.has(&n("flow"), Capability::Kv),
            "placeholder: no needs yet"
        );

        let mut no_manifest = hello("flow", "kv");
        no_manifest.manifest = None;
        let mut wrong_name = hello("flow", "kv");
        if let Some(m) = wrong_name.manifest.as_mut() {
            m.name = "web".into();
        }
        let mut old_protocol = hello("flow", "kv");
        old_protocol.protocol = 2;
        for (req, want) in [
            (
                old_protocol,
                "hello.protocol: this daemon speaks protocol 1, got 2",
            ),
            (no_manifest, "hello.manifest: required in kubernetes mode"),
            (
                wrong_name,
                "hello.manifest.name: \"web\" does not match the plugin \"flow\"",
            ),
            (
                hello("flow", "actions, workspace, kv"),
                "hello.manifest.needs: workspace is not granted",
            ),
        ] {
            let err = d.hello(&n("flow"), &req).unwrap_err().to_string();
            assert!(err.ends_with(want), "{err} / {want}");
            let row = &d.list(|_| 0)[0];
            assert_eq!(
                (row.phase, row.message.as_str()),
                (AgentPhase::Failed, want)
            );
        }

        let reply = d.hello(&n("flow"), &hello("flow", "actions, kv")).unwrap();
        assert_eq!(reply.config, json!({ "k": "flow" }));
        let row = &d.list(|_| 0)[0];
        assert_eq!(
            (row.phase, row.listen.as_deref()),
            (AgentPhase::Ready, Some("https://flow.ns.svc:7644"))
        );
        assert!(reg.has(&n("flow"), Capability::Kv));
        assert_eq!(
            reg.ready_addr(&n("flow")).unwrap().base(),
            "https://flow.ns.svc:7644"
        );
    }

    #[test]
    fn a_token_names_its_plugin_and_nothing_else_does() {
        let dir = tempfile::tempdir().unwrap();
        let d = DeclaredPlugins::new(dir.path().into(), PluginRegistry::new());
        d.replace(vec![entry("flow", &[]), entry("web", &[])])
            .unwrap();
        assert_eq!(d.plugin_for_token(&entry("web", &[]).token), Some(n("web")));
        assert_eq!(
            d.plugin_for_token("nope-0123456789abcdef0123456789abcdef"),
            None
        );
    }

    #[test]
    fn three_missed_polls_take_a_plugin_out_and_a_good_one_brings_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let reg = PluginRegistry::new();
        let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
        d.replace(vec![entry("flow", &[Capability::Kv])]).unwrap();
        d.hello(&n("flow"), &hello("flow", "kv")).unwrap();
        for _ in 0..2 {
            assert!(!d.health(&n("flow"), Err("timeout".into())));
            assert!(
                reg.ready_addr(&n("flow")).is_some(),
                "one or two misses keep it in the chain"
            );
        }
        assert!(!d.health(&n("flow"), Err("timeout".into())));
        assert!(reg.ready_addr(&n("flow")).is_none());
        let row = &d.list(|_| 0)[0];
        assert_eq!(
            (row.phase, row.message.as_str()),
            (AgentPhase::Failed, "health: timeout")
        );
        assert!(
            d.health(&n("flow"), Ok(())),
            "back: the caller re-activates"
        );
        assert!(reg.ready_addr(&n("flow")).is_some());
        assert!(
            !d.health(&n("flow"), Ok(())),
            "already ready: nothing to re-send"
        );
    }

    #[test]
    fn a_restart_restores_an_unchanged_entrys_hello_and_not_a_changed_one() {
        let dir = tempfile::tempdir().unwrap();
        {
            let d = DeclaredPlugins::new(dir.path().into(), PluginRegistry::new());
            d.replace(vec![
                entry("flow", &[Capability::Kv]),
                entry("web", &[Capability::Kv]),
            ])
            .unwrap();
            d.hello(&n("flow"), &hello("flow", "kv")).unwrap();
            d.hello(&n("web"), &hello("web", "kv")).unwrap();
        }
        let reg = PluginRegistry::new();
        let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
        let mut changed = entry("web", &[Capability::Kv]);
        changed.config = json!({ "k": "new" });
        d.replace(vec![entry("flow", &[Capability::Kv]), changed])
            .unwrap();
        let pollable: Vec<String> = d
            .pollable()
            .into_iter()
            .map(|(n, _)| n.to_string())
            .collect();
        assert_eq!(
            pollable,
            vec!["flow"],
            "only the unchanged entry's hello is restored"
        );
        assert!(
            reg.ready_addr(&n("flow")).is_none(),
            "restored, not ready until a poll answers"
        );
        assert!(
            reg.has(&n("flow"), Capability::Kv),
            "the restored manifest is registered"
        );
        assert!(
            d.health(&n("flow"), Ok(())),
            "first good poll: ready, re-activate"
        );
        assert_eq!(
            d.list(|_| 0)[1].phase,
            AgentPhase::Starting,
            "web waits for a new hello"
        );
    }

    #[test]
    fn a_dropped_plugin_leaves_the_registry_and_its_hello_file() {
        let dir = tempfile::tempdir().unwrap();
        let reg = PluginRegistry::new();
        let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
        d.replace(vec![entry("flow", &[Capability::Kv])]).unwrap();
        d.hello(&n("flow"), &hello("flow", "kv")).unwrap();
        assert!(dir.path().join("flow/hello.json").exists());
        assert_eq!(d.replace(vec![]).unwrap(), vec![n("flow")]);
        assert!(!reg.is_installed("flow"));
        assert!(!dir.path().join("flow/hello.json").exists());
    }

    #[test]
    fn a_refused_re_hello_leaves_the_chain_and_is_not_resurrected_by_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let reg = PluginRegistry::new();
        let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
        let e = entry("flow", &[Capability::Kv]);
        d.replace(vec![e.clone()]).unwrap();
        d.hello(&n("flow"), &hello("flow", "kv")).unwrap();
        assert!(d.hello(&n("flow"), &hello("flow", "workspace")).is_err());
        assert!(reg.ready_addr(&n("flow")).is_none(), "out of the chain");
        assert!(
            !reg.has(&n("flow"), Capability::Kv),
            "the old needs are gone"
        );
        assert!(d.pollable().is_empty());
        assert!(!dir.path().join("flow/hello.json").exists());

        let d2 = DeclaredPlugins::new(dir.path().into(), PluginRegistry::new());
        d2.replace(vec![e]).unwrap();
        assert!(
            d2.pollable().is_empty(),
            "the refused hello is not restored"
        );
    }
}
