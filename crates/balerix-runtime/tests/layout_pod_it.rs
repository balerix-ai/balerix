#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §6.1 and §8.1: every path still comes from `StateLayout`, and in
//! a pod they land on the three mounts.

use std::path::PathBuf;

use balerix_core::AgentId;
use balerix_runtime::layout::{PodMounts, StateLayout};

fn mounts() -> PodMounts {
    PodMounts {
        agent: PathBuf::from("/balerix/agent"),
        shared: PathBuf::from("/balerix/shared"),
        run: PathBuf::from("/balerix/run"),
    }
}

#[test]
fn the_agent_lives_at_the_claim_root_and_the_crew_slice_on_the_shared_mount() {
    let id: AgentId = "payments/backend/alice".parse().unwrap();
    let layout = StateLayout::pod(mounts(), &id);
    let a = layout.agent(&id);
    assert_eq!(a.root, PathBuf::from("/balerix/agent"));
    assert_eq!(a.home, PathBuf::from("/balerix/agent/home"));
    assert_eq!(a.workspace, PathBuf::from("/balerix/agent/workspace"));
    assert_eq!(a.nono_home, PathBuf::from("/balerix/agent/nono"));
    assert_eq!(a.launch, PathBuf::from("/balerix/agent/launch.sh"));
    assert_eq!(a.logs, PathBuf::from("/balerix/agent/logs"));
    assert_eq!(a.profile, PathBuf::from("/balerix/agent/nono-profile.json"));
    assert_eq!(
        a.installed_marker(),
        PathBuf::from("/balerix/agent/.installed")
    );
    let c = layout.crew(&id.crew_ref());
    assert_eq!(c.repo, PathBuf::from("/balerix/shared/repo"));
    assert_eq!(
        c.cache_objects(),
        PathBuf::from("/balerix/shared/repo/.git/objects")
    );
    assert_eq!(c.mise_pool(), PathBuf::from("/balerix/shared/crew/mise"));
    assert_eq!(
        layout.fleet(&id.fleet).mise_pool(),
        PathBuf::from("/balerix/shared/fleet/mise")
    );
    assert_eq!(
        layout.mise_data_dir(),
        PathBuf::from("/balerix/shared/daemon/mise")
    );
    assert_eq!(
        layout.shared_install_dirs(&id),
        "/balerix/shared/crew/mise/installs:/balerix/shared/fleet/mise/installs:/balerix/shared/daemon/mise/installs"
    );
    let pod = layout.pod_layout().unwrap();
    assert_eq!(pod.tmux_socket(), PathBuf::from("/balerix/run/tmux.sock"));
    assert_eq!(pod.start_marker(), PathBuf::from("/balerix/run/started"));
    assert_eq!(pod.ready_marker(), PathBuf::from("/balerix/run/ready"));
    // whatever else asks for a root lands on the claim, never on the
    // read-only root filesystem
    assert_eq!(
        layout.server_dir(),
        PathBuf::from("/balerix/agent/.balerix/state/server")
    );
    assert_eq!(
        layout.system_mise_toml(),
        PathBuf::from("/balerix/agent/.balerix/config/mise.toml")
    );
}

#[test]
fn another_agent_of_the_same_pod_layout_is_still_under_the_state_root() {
    // The layout is for one agent; a sibling's paths exist (the planner
    // may name them) but are ordinary state-root paths, never the claim.
    let id: AgentId = "payments/backend/alice".parse().unwrap();
    let other: AgentId = "payments/backend/bob".parse().unwrap();
    let layout = StateLayout::pod(mounts(), &id);
    assert_eq!(
        layout.agent(&other).root,
        PathBuf::from("/balerix/agent/.balerix/state/fleets/payments/crews/backend/agents/bob")
    );
}

#[test]
fn an_xdg_layout_has_no_pod() {
    let layout = StateLayout::xdg(
        PathBuf::from("/s"),
        PathBuf::from("/d"),
        PathBuf::from("/c"),
    );
    assert!(layout.pod_layout().is_none());
    let id: AgentId = "f/c/a".parse().unwrap();
    assert_eq!(
        layout.agent(&id).root,
        PathBuf::from("/s/fleets/f/crews/c/agents/a")
    );
    assert_eq!(layout.mise_data_dir(), PathBuf::from("/d/mise"));
}
