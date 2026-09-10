//! The agent's environment (spec §4 table). These become the nono profile's
//! `environment.set_vars`; `PATH` is set by nono and `mise exec`.

use std::collections::BTreeMap;

use balerix_core::AgentId;

use crate::layout::{AgentPaths, StateLayout};

pub fn agent_env(
    id: &AgentId,
    paths: &AgentPaths,
    layout: &StateLayout,
    api_url: &str,
    hook_secret: &str,
    user_env: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let s = |p: &std::path::Path| p.display().to_string();
    let mut env: BTreeMap<String, String> = BTreeMap::from([
        ("HOME".to_string(), s(&paths.home)),
        ("XDG_CONFIG_HOME".to_string(), s(&paths.xdg_config())),
        ("XDG_DATA_HOME".to_string(), s(&paths.xdg_data())),
        ("XDG_STATE_HOME".to_string(), s(&paths.xdg_state())),
        ("XDG_CACHE_HOME".to_string(), s(&paths.xdg_cache())),
        // No path under `/tmp` is granted, and claude 2.1.263 refuses to
        // start when `/tmp/claude-<uid>` is unreachable ("Temp directory …
        // is not readable … Set CLAUDE_CODE_TMPDIR"). Both names point at
        // the 0700 `home/tmp` so every tool in the sandbox shares it.
        ("TMPDIR".to_string(), s(&paths.tmp_dir())),
        ("CLAUDE_CODE_TMPDIR".to_string(), s(&paths.tmp_dir())),
        ("CLAUDE_CONFIG_DIR".to_string(), s(&paths.claude_dir())),
        ("GH_CONFIG_DIR".to_string(), s(&paths.gh_dir())),
        ("MISE_GLOBAL_CONFIG_FILE".to_string(), s(&paths.mise_toml)),
        // The in-sandbox counterpart of the `cwd=/` the daemon's `mise
        // install` uses: mise otherwise walks up from the workspace and
        // applies every `mise.toml` it finds. The repository's own file is
        // applied even though `mise trust` never covered it (mise 2026.9.1
        // honours `[tools]` from an untrusted config), which pulls in tools
        // the daemon never installed and the sandbox cannot download; an
        // Stops mise's upward config walk at the agent root, one level
        // above the worktree. The worktree's own `mise.toml` is therefore
        // discovered — an agent may `mise install` what its repository
        // declares, into the private data dir below — while nothing outside
        // the agent is: a config the profile does not grant would make mise
        // exit on the read error. Installing is never a side effect of
        // launch; `MISE_AUTO_INSTALL=false` keeps `mise exec claude`
        // resolving even when the worktree names a tool nobody installed.
        ("MISE_CEILING_PATHS".to_string(), s(&paths.root)),
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
        // The agent's own installs, writable, inside `home/`; the pools are
        // read-only fallbacks searched crew-first (Spec E §6).
        ("MISE_DATA_DIR".to_string(), s(&paths.mise_data_dir())),
        (
            "MISE_SHARED_INSTALL_DIRS".to_string(),
            layout.shared_install_dirs(id),
        ),
        ("MISE_CONFIG_DIR".to_string(), s(&paths.mise_config_dir())),
        ("MISE_STATE_DIR".to_string(), s(&paths.mise_state_dir())),
        ("MISE_CACHE_DIR".to_string(), s(&paths.mise_cache_dir())),
        ("BALERIX_FLEET".to_string(), id.fleet.to_string()),
        ("BALERIX_CREW".to_string(), id.crew.to_string()),
        ("BALERIX_AGENT".to_string(), id.agent.to_string()),
        ("BALERIX_AGENT_ID".to_string(), id.to_string()),
        ("BALERIX_API_URL".to_string(), api_url.to_string()),
        // What `balerix hook-relay` presents; the reserved `BALERIX_` prefix
        // keeps user `env` away from it.
        ("BALERIX_HOOK_SECRET".to_string(), hook_secret.to_string()),
    ]);
    // Reserved keys were rejected at config validation; `entry().or_insert`
    // keeps the isolation rows authoritative even so.
    for (k, v) in user_env {
        env.entry(k.clone()).or_insert_with(|| v.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn isolation_rows_come_first_and_user_env_cannot_override_them() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let id: AgentId = "payments/backend/alice".parse().unwrap();
        let paths = layout.agent(&id);
        let user = BTreeMap::from([
            ("RUST_LOG".to_string(), "info".to_string()),
            ("HOME".to_string(), "/evil".to_string()),
        ]);
        let env = agent_env(
            &id,
            &paths,
            &layout,
            "http://127.0.0.1:7643",
            "hook-s3",
            &user,
        );
        assert_eq!(
            env["HOME"],
            "/h/.local/state/balerix/fleets/payments/crews/backend/agents/alice/home"
        );
        assert_eq!(env["RUST_LOG"], "info");
        assert_eq!(env["BALERIX_AGENT_ID"], "payments/backend/alice");
        assert_eq!(env["BALERIX_HOOK_SECRET"], "hook-s3");
        let base = "/h/.local/state/balerix/fleets/payments/crews/backend";
        assert_eq!(
            env["MISE_DATA_DIR"],
            format!("{base}/agents/alice/home/.local/share/mise"),
            "the agent installs into its own home, never a shared pool"
        );
        assert_eq!(
            env["MISE_SHARED_INSTALL_DIRS"],
            format!(
                "{base}/mise/installs:/h/.local/state/balerix/fleets/payments/mise/installs:\
                 /h/.local/share/balerix/mise/installs"
            ),
            "crew, then fleet, then daemon"
        );
        assert_eq!(
            env["MISE_CEILING_PATHS"],
            format!("{base}/agents/alice"),
            "the walk stops above the worktree, so the repo's own mise.toml \
             is discovered and nothing outside the agent is"
        );
        assert_eq!(
            env["MISE_AUTO_INSTALL"], "false",
            "launch resolves; installing is the agent's explicit act"
        );
        assert_eq!(env.len(), 24);
        assert!(!env.contains_key("PATH"), "PATH is nono's");
        assert_eq!(
            env["TMPDIR"],
            "/h/.local/state/balerix/fleets/payments/crews/backend/agents/alice/home/tmp"
        );
        assert_eq!(
            env["CLAUDE_CODE_TMPDIR"], env["TMPDIR"],
            "claude checks its own variable before TMPDIR"
        );
    }

    /// Spec E's guarantee is "what the daemon installs is exactly what the
    /// agent resolves" (toolchain.rs's `mise_env` doc comment). The two
    /// functions build these two rows identically today, but only by
    /// construction; nothing pins them together, so a future edit to one
    /// could drift without either's own tests noticing.
    #[test]
    fn mise_env_and_agent_env_agree_on_the_agents_data_dir_and_pool_chain() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let id: AgentId = "payments/backend/alice".parse().unwrap();
        let paths = layout.agent(&id);
        let daemon = crate::toolchain::mise_env(&id, &paths, &layout);
        let sandbox = agent_env(
            &id,
            &paths,
            &layout,
            "http://127.0.0.1:7643",
            "hook-s3",
            &BTreeMap::new(),
        );
        for key in ["MISE_DATA_DIR", "MISE_SHARED_INSTALL_DIRS"] {
            assert_eq!(
                daemon[key], sandbox[key],
                "{key}: the daemon's own install must resolve exactly what the sandbox will"
            );
        }
    }
}
