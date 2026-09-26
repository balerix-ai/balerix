//! Driven adapters (Phase 2 spec §4): everything that turns a `ResolvedAgent`
//! into files and a tmux window. Every path comes from `StateLayout`, every
//! binary from `ToolPaths`; nothing here reads the process environment.

pub mod env;
pub mod fsutil;
pub mod home;
pub mod inspect;
pub mod launch;
pub mod layout;
pub mod materializer;
pub mod plugin;
pub mod quote;
pub mod sandbox;
pub mod testing;
pub mod tmux;
pub mod toolchain;
pub mod tools;
pub mod workspace;

pub use env::agent_env;
pub use home::{HOOK_EVENTS, HomeInputs, write_home};
pub use launch::{hooks_port, render_launch, wants_continue};
pub use layout::{AgentPaths, CrewPaths, FleetPaths, PluginPaths, StateLayout, shared_list};
pub use materializer::{RenderOptions, RenderOutcome, Runtime};
pub use plugin::{
    install_plugin_tools, plugin_env, plugin_grants, render_plugin_launch, write_plugin_home,
};
pub use quote::sh_quote;
pub use sandbox::{
    Grants, balerix_grants, check_conflicts, merge_profile, render_profile, validate_profile,
    validate_profile_at, write_profile, write_profile_at,
};
pub use tmux::{ANCHOR_WINDOW, ATTACH_SESSION_PREFIX, TmuxAttach, TmuxRunner};
pub use toolchain::{
    Toolchain, embedded_system_tools, level_env, mise_env, render_level_toml, render_mise_toml,
    system_tools,
};
pub use tools::{MissingTool, ToolPaths};
pub use workspace::{CloneDecision, Workspace, decide_clone};
