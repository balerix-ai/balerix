//! The ports a Kubernetes-mode daemon has no work for (Spec O §7.2): the
//! sidecar materialises, and the shared volume's daemon pool is a Job's
//! (§8.3). `NoFiles` refuses, since nothing should call it; `NoPool`
//! answers ready, so `/readyz` and the actors' gate open.

use balerix_api::CredentialBundle;
use balerix_core::{
    AgentId, AgentName, CrewRef, CrewTools, HookTarget, Keep, LaunchPlan, MaterializeError,
    Materializer, RepoRef, ResolvedAgent, ResolvedPlugin, SystemToolchain,
};

pub struct NoFiles;

fn refused(id: String) -> MaterializeError {
    MaterializeError::Invalid {
        id,
        message: "a daemon in kubernetes mode materialises nothing; the sidecar does (Spec O §7.2)"
            .into(),
    }
}

impl Materializer for NoFiles {
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        _repo: &RepoRef,
        _git_ref: &str,
        _git: &balerix_api::GitSettings,
        _creds: &CredentialBundle,
        _tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        Err(refused(crew.to_string()))
    }
    fn materialize(
        &self,
        agent: &ResolvedAgent,
        _creds: &CredentialBundle,
        _hooks: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        Err(refused(agent.id.to_string()))
    }
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        Err(refused(agent.to_string()))
    }
    fn remove_crew(&self, crew: &CrewRef, _keep: Keep) -> Result<(), MaterializeError> {
        Err(refused(crew.to_string()))
    }
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        _host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        Err(refused(plugin.name.to_string()))
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        Err(refused(name.to_string()))
    }
}

pub struct NoPool;

impl SystemToolchain for NoPool {
    fn ensure_system_pool(&self) -> Result<(), MaterializeError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_files_refuses_and_no_pool_is_ready() {
        let id: AgentId = "f/c/a".parse().unwrap();
        let err = NoFiles.remove_agent(&id).unwrap_err();
        assert_eq!(
            err.to_string(),
            "f/c/a: a daemon in kubernetes mode materialises nothing; the sidecar does (Spec O §7.2)"
        );
        assert!(NoPool.ensure_system_pool().is_ok());
    }
}
