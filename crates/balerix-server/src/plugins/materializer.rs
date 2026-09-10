//! The synthetic-fleet bridge (plugins spec §5.2): maps agents of the
//! `balerix` fleet back to their `ResolvedPlugin` and calls the real
//! materializer's plugin methods; a `FleetStore` that stores nothing,
//! because `plugins.yaml` is the record.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use balerix_api::{CredentialBundle, GitSettings};
use balerix_core::{
    AgentId, AgentName, CrewRef, CrewTools, FleetName, FleetRecord, FleetSecrets, FleetStore,
    HookTarget, Keep, LaunchPlan, MaterializeError, Materializer, RepoRef, ResolvedAgent,
    ResolvedPlugin, StoreError,
};

pub struct PluginMaterializer {
    inner: Arc<dyn Materializer>,
    plugins: RwLock<BTreeMap<AgentName, ResolvedPlugin>>,
}

impl PluginMaterializer {
    pub fn new(inner: Arc<dyn Materializer>) -> Self {
        Self {
            inner,
            plugins: RwLock::new(BTreeMap::new()),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<AgentName, ResolvedPlugin>> {
        self.plugins.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The synced set; called before the `Apply` that reconciles to it.
    pub fn replace(&self, plugins: Vec<ResolvedPlugin>) {
        let mut w = self.plugins.write().unwrap_or_else(|e| e.into_inner());
        *w = plugins.into_iter().map(|p| (p.name.clone(), p)).collect();
    }

    pub fn get(&self, name: &AgentName) -> Option<ResolvedPlugin> {
        self.read().get(name).cloned()
    }

    /// Sorted by name.
    pub fn all(&self) -> Vec<ResolvedPlugin> {
        self.read().values().cloned().collect()
    }
}

impl Materializer for PluginMaterializer {
    fn ensure_crew(
        &self,
        _: &CrewRef,
        _: &RepoRef,
        _: &str,
        _: &GitSettings,
        _: &CredentialBundle,
        _: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        Ok(())
    }
    fn materialize(
        &self,
        agent: &ResolvedAgent,
        _: &CredentialBundle,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let plugin = self
            .get(&agent.id.agent)
            .ok_or_else(|| MaterializeError::Invalid {
                id: agent.id.to_string(),
                message: "plugin is no longer declared".into(),
            })?;
        self.inner.materialize_plugin(&plugin, host)
    }
    /// State survives removal from `plugins.yaml`; only `purge_plugin` deletes.
    fn remove_agent(&self, _: &AgentId) -> Result<(), MaterializeError> {
        Ok(())
    }
    fn remove_crew(&self, _: &CrewRef, _: Keep) -> Result<(), MaterializeError> {
        Ok(())
    }
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        self.inner.materialize_plugin(plugin, host)
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        self.inner.purge_plugin(name)
    }
}

/// `plugins.yaml` is the record; the in-memory registry is the state.
pub struct NullStore;

impl FleetStore for NullStore {
    fn load_all(&self) -> Result<Vec<(FleetRecord, FleetSecrets)>, StoreError> {
        Ok(Vec::new())
    }
    fn put(&self, _: &FleetRecord, _: &FleetSecrets) -> Result<(), StoreError> {
        Ok(())
    }
    fn purge(&self, _: &FleetName) -> Result<(), StoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::CredentialBundle;
    use balerix_core::fakes::FakeMaterializer;
    use balerix_core::{Fleet, FleetRecord, FleetSecrets, plugin_fleet};
    use serde_json::json;
    use std::path::Path;

    fn plugin(name: &str) -> ResolvedPlugin {
        ResolvedPlugin {
            name: name.parse().unwrap(),
            package: format!("/pkg/{name}").into(),
            manifest: serde_json::from_value(json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": name,
                "version": "0.1.0", "protocol": 1, "start": "serve"
            }))
            .unwrap(),
            config: json!({}),
            digest: None,
        }
    }

    #[test]
    fn materialize_maps_the_synthetic_agent_to_its_plugin() {
        let inner = Arc::new(FakeMaterializer::default());
        let m = PluginMaterializer::new(inner.clone());
        m.replace(vec![plugin("web"), plugin("flow")]);
        assert_eq!(m.all().len(), 2);
        assert_eq!(
            m.get(&"web".parse().unwrap()).unwrap().package,
            Path::new("/pkg/web")
        );
        let fleet: Fleet = plugin_fleet(&m.all());
        let agents = ResolvedAgent::from_fleet(&fleet);
        let host = HookTarget {
            url: "http://127.0.0.1:1".into(),
            secret: "t".into(),
        };
        let plan = m
            .materialize(&agents[1], &CredentialBundle::default(), &host)
            .unwrap();
        assert_eq!(plan.cwd, Path::new("/pkg/web"));
        assert_eq!(
            inner.calls(),
            vec!["materialize_plugin balerix/plugins/web"]
        );
        // crews and removals are no-ops: nothing to clone, state kept until purge
        let empty = BTreeMap::new();
        m.ensure_crew(
            &agents[0].id.crew_ref(),
            &agents[0].repo,
            "none",
            &agents[0].git,
            &CredentialBundle::default(),
            CrewTools {
                fleet: &empty,
                crew: &empty,
            },
        )
        .unwrap();
        m.remove_agent(&agents[0].id).unwrap();
        m.remove_crew(&agents[0].id.crew_ref(), Keep::default())
            .unwrap();
        assert_eq!(
            inner.calls().len(),
            1,
            "no inner call for crews or removals"
        );
        m.replace(vec![]);
        let e = m
            .materialize(&agents[1], &CredentialBundle::default(), &host)
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "balerix/plugins/web: plugin is no longer declared"
        );
    }

    #[test]
    fn the_null_store_keeps_nothing() {
        let s = NullStore;
        s.put(
            &FleetRecord::new(plugin_fleet(&[]).into()),
            &FleetSecrets::default(),
        )
        .unwrap();
        assert!(s.load_all().unwrap().is_empty());
        s.purge(&"balerix".parse().unwrap()).unwrap();
    }
}
