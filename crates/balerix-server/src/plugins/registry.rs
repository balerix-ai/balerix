//! The daemon's view of the installed plugins (plugins spec §4, §16.2,
//! §16.3): load-list order, where each one listens, what it subscribes to
//! and may call, and the `(agent, plugin) → activation` table. The single
//! writer of activation state; the fleet actor never sees it.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use balerix_api::{ActivationState, Capability, PluginActivation, PluginManifest};
use balerix_core::{AgentId, AgentName, FleetName, FleetRecord, ResolvedPlugin};
use serde_json::Value;

/// Where a ready plugin listens and the bearer the daemon presents to it
/// (plugins spec §18.3): the plugin's own token, learned at `hello`.
#[derive(Clone, PartialEq, Eq)]
pub struct PluginAddr {
    pub listen: String,
    pub token: String,
}

impl fmt::Debug for PluginAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginAddr")
            .field("listen", &self.listen)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    /// From the last `hello`; kept while the plugin restarts.
    pub listen: Option<String>,
    /// The token the plugin presented at that `hello`: the bearer on every
    /// daemon → plugin call (§18.3). Kept with `listen`.
    pub token: Option<String>,
    /// `Ready` per the plugin fleet's record. Only ready plugins are called.
    pub ready: bool,
    /// The health poller's verdict; `hello` clears it.
    pub degraded: Option<String>,
}

impl fmt::Debug for PluginInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginInfo")
            .field("manifest", &self.manifest)
            .field("listen", &self.listen)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("ready", &self.ready)
            .field("degraded", &self.degraded)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActivationRow {
    pub config: Value,
    pub activation: PluginActivation,
}

#[derive(Default)]
struct Inner {
    order: Vec<AgentName>,
    plugins: BTreeMap<AgentName, PluginInfo>,
    rows: BTreeMap<(AgentId, AgentName), ActivationRow>,
}

#[derive(Default)]
pub struct PluginRegistry {
    inner: RwLock<Inner>,
}

impl PluginRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The synced set. `keep_listen` names the plugins whose package,
    /// manifest and config did not change; every other name starts over
    /// with no listen address and not ready.
    pub fn replace_plugins(&self, plugins: &[ResolvedPlugin], keep_listen: &[String]) {
        let mut w = self.write();
        let old = std::mem::take(&mut w.plugins);
        w.order = plugins.iter().map(|p| p.name.clone()).collect();
        for p in plugins {
            let kept = keep_listen.iter().any(|k| k == p.name.as_str());
            let prev = old.get(&p.name).filter(|_| kept);
            w.plugins.insert(
                p.name.clone(),
                PluginInfo {
                    manifest: p.manifest.clone(),
                    listen: prev.and_then(|i| i.listen.clone()),
                    token: prev.and_then(|i| i.token.clone()),
                    ready: prev.is_some_and(|i| i.ready),
                    degraded: prev.and_then(|i| i.degraded.clone()),
                },
            );
        }
        let names: Vec<AgentName> = w.plugins.keys().cloned().collect();
        w.rows.retain(|(_, p), _| names.contains(p));
    }

    pub fn set_listen(&self, name: &AgentName, listen: String, token: String) {
        if let Some(p) = self.write().plugins.get_mut(name) {
            p.listen = Some(listen);
            p.token = Some(token);
            p.ready = true;
            p.degraded = None;
        }
    }

    pub fn set_ready(&self, name: &AgentName, ready: bool) {
        if let Some(p) = self.write().plugins.get_mut(name) {
            p.ready = ready;
        }
    }

    pub fn set_degraded(&self, name: &AgentName, reason: Option<String>) {
        if let Some(p) = self.write().plugins.get_mut(name) {
            p.degraded = reason;
        }
    }

    pub fn plugin(&self, name: &AgentName) -> Option<PluginInfo> {
        self.read().plugins.get(name).cloned()
    }

    /// Load-list order (`plugins.yaml` order): the interceptor order.
    pub fn names(&self) -> Vec<AgentName> {
        self.read().order.clone()
    }

    pub fn is_installed(&self, name: &str) -> bool {
        self.read().plugins.keys().any(|n| n.as_str() == name)
    }

    pub fn has(&self, name: &AgentName, cap: Capability) -> bool {
        self.read()
            .plugins
            .get(name)
            .is_some_and(|p| p.manifest.needs.contains(&cap))
    }

    /// `Some` only while the plugin is ready: where to call it and what
    /// bearer to present.
    pub fn ready_addr(&self, name: &AgentName) -> Option<PluginAddr> {
        let r = self.read();
        let p = r.plugins.get(name)?;
        if !p.ready {
            return None;
        }
        Some(PluginAddr {
            listen: p.listen.clone()?,
            token: p.token.clone()?,
        })
    }

    pub fn set_row(&self, agent: &AgentId, plugin: &AgentName, row: ActivationRow) {
        self.write()
            .rows
            .insert((agent.clone(), plugin.clone()), row);
    }

    /// `false` when there is no such row.
    pub fn set_state(
        &self,
        agent: &AgentId,
        plugin: &AgentName,
        activation: PluginActivation,
    ) -> bool {
        match self.write().rows.get_mut(&(agent.clone(), plugin.clone())) {
            Some(row) => {
                row.activation = activation;
                true
            }
            None => false,
        }
    }

    pub fn remove_row(&self, agent: &AgentId, plugin: &AgentName) -> Option<ActivationRow> {
        self.write().rows.remove(&(agent.clone(), plugin.clone()))
    }

    /// Every pair of the fleet, removed and returned (for `deactivate`).
    pub fn remove_fleet(&self, fleet: &FleetName) -> Vec<(AgentId, AgentName)> {
        let mut w = self.write();
        let gone: Vec<(AgentId, AgentName)> = w
            .rows
            .keys()
            .filter(|(a, _)| &a.fleet == fleet)
            .cloned()
            .collect();
        for k in &gone {
            w.rows.remove(k);
        }
        gone
    }

    pub fn row(&self, agent: &AgentId, plugin: &AgentName) -> Option<ActivationRow> {
        self.read()
            .rows
            .get(&(agent.clone(), plugin.clone()))
            .cloned()
    }

    /// Every row of one fleet, sorted by agent then plugin: what a fleet
    /// currently has activated, which is what an `apply` diffs against —
    /// the stored spec would still name pairs a `down` has just dropped.
    pub fn rows_for_fleet(&self, fleet: &FleetName) -> Vec<(AgentId, AgentName, ActivationRow)> {
        self.read()
            .rows
            .iter()
            .filter(|((a, _), _)| &a.fleet == fleet)
            .map(|((a, p), row)| (a.clone(), p.clone(), row.clone()))
            .collect()
    }

    pub fn rows_for_plugin(&self, name: &AgentName) -> Vec<(AgentId, ActivationRow)> {
        self.read()
            .rows
            .iter()
            .filter(|((_, p), _)| p == name)
            .map(|((a, _), row)| (a.clone(), row.clone()))
            .collect()
    }

    pub fn is_active(&self, agent: &AgentId, plugin: &AgentName) -> bool {
        self.read()
            .rows
            .get(&(agent.clone(), plugin.clone()))
            .is_some_and(|r| r.activation.state == ActivationState::Active)
    }

    pub fn active_agents(&self, name: &AgentName) -> u32 {
        u32::try_from(
            self.read()
                .rows
                .iter()
                .filter(|((_, p), r)| p == name && r.activation.state == ActivationState::Active)
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    fn subscribed(
        &self,
        agent: &AgentId,
        pick: impl Fn(&PluginManifest) -> bool,
    ) -> Vec<(AgentName, PluginAddr)> {
        let r = self.read();
        r.order
            .iter()
            .filter_map(|name| {
                let p = r.plugins.get(name)?;
                let listen = p.listen.clone().filter(|_| p.ready)?;
                let token = p.token.clone()?;
                let active = r
                    .rows
                    .get(&(agent.clone(), name.clone()))
                    .is_some_and(|row| row.activation.state == ActivationState::Active);
                (active && pick(&p.manifest)).then(|| (name.clone(), PluginAddr { listen, token }))
            })
            .collect()
    }

    /// Ready plugins intercepting `event` and active for `agent`, in
    /// load-list order, with their listen addresses.
    pub fn interceptors(&self, agent: &AgentId, event: &str) -> Vec<(AgentName, PluginAddr)> {
        self.subscribed(agent, |m| m.hooks.intercept.contains(event))
    }

    pub fn observers(&self, agent: &AgentId, event: &str) -> Vec<(AgentName, PluginAddr)> {
        self.subscribed(agent, |m| m.hooks.observe.contains(event))
    }

    /// Copies the fleet's rows into `status.agents[*].plugins` (§16.3);
    /// agents the record does not know yet are skipped.
    pub fn overlay(&self, record: &mut FleetRecord) {
        let r = self.read();
        for (id, status) in &mut record.status.agents {
            status.plugins = r
                .rows
                .iter()
                .filter(|((a, _), _)| a.to_string() == *id)
                .map(|((_, p), row)| (p.to_string(), row.activation.clone()))
                .collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::{ActivationState, FleetSpec};
    use balerix_core::FleetRecord;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn plugin(name: &str, intercept: &[&str], observe: &[&str], needs: &[&str]) -> ResolvedPlugin {
        ResolvedPlugin {
            name: name.parse().unwrap(),
            package: format!("/pkg/{name}").into(),
            manifest: serde_json::from_value(json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": name,
                "version": "0.1.0", "protocol": 1, "start": "serve",
                "hooks": { "intercept": intercept, "observe": observe }, "needs": needs
            }))
            .unwrap(),
            config: json!({}),
            digest: None,
        }
    }

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }
    fn name(s: &str) -> AgentName {
        s.parse().unwrap()
    }

    #[test]
    fn plugins_keep_load_order_readiness_and_capabilities() {
        let r = PluginRegistry::new();
        r.replace_plugins(
            &[
                plugin("web", &[], &["SessionStart"], &["fleets"]),
                plugin("flow", &["PreToolUse", "Stop"], &[], &["actions", "kv"]),
            ],
            &[],
        );
        assert_eq!(
            r.names()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["web", "flow"]
        );
        assert!(r.is_installed("flow") && !r.is_installed("nope"));
        assert!(r.has(&name("flow"), Capability::Kv));
        assert!(!r.has(&name("flow"), Capability::Fleets));
        assert!(!r.has(&name("nope"), Capability::Fleets));
        assert_eq!(r.ready_addr(&name("flow")), None);
        r.set_listen(&name("flow"), "127.0.0.1:4000".into(), "tok".into());
        assert_eq!(
            r.ready_addr(&name("flow")).map(|a| a.listen).as_deref(),
            Some("127.0.0.1:4000")
        );
        r.set_degraded(&name("flow"), Some("HTTP 500".into()));
        assert_eq!(
            r.plugin(&name("flow")).unwrap().degraded.as_deref(),
            Some("HTTP 500")
        );
        r.set_ready(&name("flow"), false);
        assert_eq!(r.ready_addr(&name("flow")), None, "not ready: no listen");
        assert_eq!(
            r.plugin(&name("flow")).unwrap().listen.as_deref(),
            Some("127.0.0.1:4000"),
            "the address itself is kept"
        );
        r.set_listen(&name("flow"), "127.0.0.1:4001".into(), "tok".into());
        let p = r.plugin(&name("flow")).unwrap();
        assert!(p.ready && p.degraded.is_none(), "hello clears degraded");
        // a re-sync keeps listen only for the names the caller says
        r.replace_plugins(
            &[
                plugin("flow", &["PreToolUse"], &[], &[]),
                plugin("x", &[], &[], &[]),
            ],
            &["flow".into()],
        );
        assert_eq!(
            r.ready_addr(&name("flow")).map(|a| a.listen).as_deref(),
            Some("127.0.0.1:4001")
        );
        assert!(r.plugin(&name("web")).is_none());
        r.replace_plugins(&[plugin("flow", &[], &[], &[])], &[]);
        assert_eq!(
            r.ready_addr(&name("flow")),
            None,
            "changed plugin: forgotten"
        );
    }

    #[test]
    fn rows_drive_interceptors_observers_counts_and_the_overlay() {
        let r = PluginRegistry::new();
        r.replace_plugins(
            &[
                plugin("flow", &["PreToolUse", "Stop"], &["Stop"], &[]),
                plugin("web", &["PreToolUse"], &["SessionStart", "PreToolUse"], &[]),
            ],
            &[],
        );
        r.set_listen(&name("flow"), "127.0.0.1:1".into(), "tok".into());
        r.set_listen(&name("web"), "127.0.0.1:2".into(), "tok".into());
        let a = id("f/c/a");
        let b = id("f/c/b");
        let row = |state: ActivationState| ActivationRow {
            config: json!({ "k": 1 }),
            activation: PluginActivation {
                state,
                message: String::new(),
            },
        };
        r.set_row(&a, &name("flow"), row(ActivationState::Active));
        r.set_row(&a, &name("web"), row(ActivationState::Pending));
        r.set_row(&b, &name("web"), row(ActivationState::Active));
        assert_eq!(
            r.interceptors(&a, "PreToolUse"),
            vec![(
                name("flow"),
                PluginAddr {
                    listen: "127.0.0.1:1".into(),
                    token: "tok".into()
                }
            )],
            "web is pending for a"
        );
        assert_eq!(r.interceptors(&a, "Stop").len(), 1);
        assert_eq!(r.interceptors(&a, "Notification").len(), 0);
        assert_eq!(
            r.interceptors(&b, "PreToolUse"),
            vec![(
                name("web"),
                PluginAddr {
                    listen: "127.0.0.1:2".into(),
                    token: "tok".into()
                }
            )]
        );
        assert_eq!(r.observers(&b, "SessionStart").len(), 1);
        assert_eq!(
            r.observers(&a, "Stop"),
            vec![(
                name("flow"),
                PluginAddr {
                    listen: "127.0.0.1:1".into(),
                    token: "tok".into()
                }
            )]
        );
        r.set_ready(&name("flow"), false);
        assert!(
            r.interceptors(&a, "PreToolUse").is_empty(),
            "not ready: skipped"
        );
        r.set_ready(&name("flow"), true);
        assert!(r.is_active(&a, &name("flow")));
        assert!(!r.is_active(&a, &name("web")));
        assert_eq!(
            (
                r.active_agents(&name("flow")),
                r.active_agents(&name("web"))
            ),
            (1, 1)
        );
        assert!(r.set_state(&a, &name("web"), PluginActivation::active()));
        assert!(!r.set_state(&id("f/c/z"), &name("web"), PluginActivation::active()));
        assert_eq!(r.active_agents(&name("web")), 2);
        assert_eq!(r.rows_for_plugin(&name("web")).len(), 2);
        assert_eq!(r.row(&a, &name("flow")).unwrap().config["k"], 1);
        let fleet: FleetName = "f".parse().unwrap();
        assert_eq!(
            r.rows_for_fleet(&fleet)
                .iter()
                .map(|(a, p, row)| format!("{a} {p} {}", row.config))
                .collect::<Vec<_>>(),
            vec![
                "f/c/a flow {\"k\":1}",
                "f/c/a web {\"k\":1}",
                "f/c/b web {\"k\":1}"
            ],
            "one fleet's rows, sorted, with their configs"
        );
        assert!(
            r.rows_for_fleet(&"g".parse().unwrap()).is_empty(),
            "another fleet has none"
        );

        let mut record = FleetRecord::new(FleetSpec {
            name: "f".into(),
            crews: BTreeMap::new(),
            ..Default::default()
        });
        record.status.entry("f/c/a");
        record.status.entry("f/c/b");
        r.overlay(&mut record);
        assert_eq!(record.status.agents["f/c/a"].plugins.len(), 2);
        assert_eq!(
            record.status.agents["f/c/a"].plugins["flow"].state,
            ActivationState::Active
        );
        assert_eq!(record.status.agents["f/c/b"].plugins.len(), 1);

        assert_eq!(r.remove_row(&a, &name("web")).unwrap().config["k"], 1);
        assert!(r.remove_row(&a, &name("web")).is_none());
        let removed = r.remove_fleet(&"f".parse().unwrap());
        assert_eq!(removed.len(), 2);
        assert!(r.rows_for_plugin(&name("flow")).is_empty());
        assert_eq!(r.active_agents(&name("web")), 0);
    }

    #[test]
    fn hello_records_the_token_the_daemon_presents_and_never_prints_it() {
        let r = PluginRegistry::new();
        r.replace_plugins(&[plugin("flow", &["Stop"], &[], &[])], &[]);
        assert_eq!(r.ready_addr(&name("flow")), None);
        r.set_listen(
            &name("flow"),
            "127.0.0.1:4000".into(),
            "s3cret-token".into(),
        );
        let addr = r.ready_addr(&name("flow")).unwrap();
        assert_eq!(
            (addr.listen.as_str(), addr.token.as_str()),
            ("127.0.0.1:4000", "s3cret-token")
        );
        let dbg = format!("{addr:?}");
        assert!(
            dbg.contains("127.0.0.1:4000") && !dbg.contains("s3cret") && dbg.contains("<redacted>"),
            "{dbg}"
        );
        let dbg = format!("{:?}", r.plugin(&name("flow")).unwrap());
        assert!(
            !dbg.contains("s3cret") && dbg.contains("<redacted>"),
            "{dbg}"
        );
        r.set_ready(&name("flow"), false);
        assert_eq!(r.ready_addr(&name("flow")), None, "not ready: no address");
        // a re-sync that keeps the plugin keeps its token with the address
        r.set_ready(&name("flow"), true);
        r.replace_plugins(&[plugin("flow", &["Stop"], &[], &[])], &["flow".into()]);
        assert_eq!(r.ready_addr(&name("flow")).unwrap().token, "s3cret-token");
        let a = id("f/c/a");
        r.set_row(
            &a,
            &name("flow"),
            ActivationRow {
                config: json!({}),
                activation: PluginActivation::active(),
            },
        );
        assert_eq!(
            r.interceptors(&a, "Stop"),
            vec![(
                name("flow"),
                PluginAddr {
                    listen: "127.0.0.1:4000".into(),
                    token: "s3cret-token".into()
                }
            )]
        );
    }
}
