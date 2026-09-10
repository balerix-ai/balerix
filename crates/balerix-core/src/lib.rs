//! Domain model and ports (spec §3). No I/O lives here.

pub mod agent;
pub mod events;
pub mod fakes;
pub mod fleet;
pub mod name;
pub mod plugin;
pub mod ports;
pub mod reconcile;
pub mod repo;
pub mod store;
pub mod version;

pub use agent::{CrewRef, ResolvedAgent};
pub use events::{EventHandler, HandlerFuture, Outcome, PassThrough};
pub use fleet::{Crew, Fleet, FleetError, RESERVED_AGENT_NAME};
pub use name::{AgentId, AgentName, CrewName, FleetName, NameError};
pub use plugin::{
    ManifestError, PLUGIN_CREW, RESERVED_FLEET, ResolvedPlugin, is_reserved_fleet, plugin_fleet,
    plugin_id, reserved_fleet_reason, validate_manifest,
};
pub use ports::{
    AgentRunner, Clock, CrewTools, HookTarget, Keep, LaunchPlan, MaterializeError, Materializer,
    ObservedState, ProcessState, PtyStream, RunnerError, SystemToolchain, WorkspaceError,
    WorkspaceReader, first_line,
};
pub use reconcile::{Plan, ReconcilePolicy, Step};
pub use repo::{RepoError, RepoRef};
pub use store::{Desired, FleetRecord, FleetSecrets, FleetStore, StoreError};
pub use version::is_exact_version;
