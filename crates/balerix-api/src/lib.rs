//! Wire types shared by the balerix CLI and daemon (spec §3).
//!
//! This crate is a leaf: serde DTOs, with no logic beyond defaults,
//! secret-redacting `Debug` impls and the pure validators both sides
//! share (`branch::check_branch_name`, `workspace::check_path`).

/// The `apiVersion` every fleet file and request declares.
pub const API_VERSION: &str = "balerix/v1";

pub mod branch;
pub mod credentials;
pub mod fleet;
pub mod hook;
pub mod plugin;
pub mod protocol;
pub mod record;
pub mod request;
pub mod settings;
pub mod status;
pub mod workspace;

pub use branch::check_branch_name;
pub use credentials::CredentialBundle;
pub use fleet::{CrewSpec, FleetSpec, GitAuth, GitIdentity, GitSettings};
pub use hook::{HOOK_EVENTS, HookEvent};
pub use plugin::{
    Capability, HelloRequest, HelloResponse, HookSubscriptions, PLUGIN_KIND, PLUGIN_PROTOCOL,
    PluginEntry, PluginManifest, PluginStatus, PluginsFile, SyncReport,
};
pub use protocol::{
    ActivateRequest, CHAIN_BUDGET_MS, DEFAULT_KEY_DELAY_MS, DeactivateRequest, EventBatch,
    InterceptRequest, InterceptResponse, Key, KeyStep, KvKeys, MAX_KEY_DELAY_MS,
    MAX_KEY_SEQUENCE_MS, MAX_KEY_STEPS, MAX_KEY_TEXT, MIN_KEY_DELAY_MS, OBSERVER_BATCH,
    OBSERVER_QUEUE, PluginAction, Resize, ResizeFrame, TextFrame,
};
pub use record::{Desired, FleetRecord, Keep};
pub use request::{DownQuery, ErrorBody, FleetRequest, SessionRequest, SessionResponse};
pub use settings::{AgentSettings, ClaudeSettings, RunnerSettings};
pub use status::{
    ActivationState, AgentPhase, AgentStatus, FleetPhase, FleetStatus, FleetSummary,
    PluginActivation, SpecHash, Timestamp,
};
pub use workspace::{
    EntryKind, FileDiff, FileStatus, TreeEntry, WORKSPACE_FILE_COUNT_LIMIT, WORKSPACE_FILE_LIMIT,
    WORKSPACE_PATCH_LIMIT, WorkspaceDiff, WorkspaceTree, WorkspaceVersion, check_path,
};

#[cfg(test)]
mod tests {
    use super::API_VERSION;

    #[test]
    fn api_version_is_v1() {
        assert_eq!(API_VERSION, "balerix/v1");
    }
}
