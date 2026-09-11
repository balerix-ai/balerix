//! The four generated files for the `payments` example agents. Paths under
//! the temp root are replaced by `<root>`; tool paths by `<tools>/name`.
//! Review `.snap.new` against the expected values in the Phase 2 plan, Task 13.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use balerix_api::{AgentSettings, CredentialBundle, CrewSpec, FleetSpec, GitSettings};
use balerix_core::{Fleet, HookTarget, ResolvedAgent};
use balerix_runtime::{RenderOptions, Runtime, StateLayout, ToolPaths};
use serde_json::json;

fn fleet() -> Fleet {
    let mut alice = AgentSettings::default();
    alice.claude.settings =
        json!({ "model": "sonnet", "permissions": { "allow": ["Bash(git *)"] } });
    alice.claude.args = vec!["--verbose".into()];
    alice.claude.resume = true;
    alice.sandbox = json!({ "network": { "block": false } });
    alice.tools = BTreeMap::from([
        ("node".to_string(), "22.11.0".to_string()),
        ("python".to_string(), "3.12.8".to_string()),
    ]);
    alice.env = BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]);
    let mut bob = alice.clone();
    bob.claude.settings["model"] = json!("opus");
    Fleet::try_from(FleetSpec {
        name: "payments".into(),
        crews: BTreeMap::from([(
            "backend".to_string(),
            CrewSpec {
                repo: "acme/payments-api".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: BTreeMap::from([("alice".to_string(), alice), ("bob".to_string(), bob)]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    })
    .unwrap()
}

fn normalize(text: &str, root: &std::path::Path) -> String {
    text.replace(&root.display().to_string(), "<root>")
}

#[test]
fn payments_agents_generate_known_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let layout = StateLayout {
        state_root: root.join("state"),
        data_root: root.join("data"),
        config_root: root.join("config"),
    };
    // fixed tool paths so launch.sh is deterministic; nothing is executed
    let tools = ToolPaths {
        git: "/tools/git".into(),
        gh: "/tools/gh".into(),
        mise: "/tools/mise".into(),
        nono: "/tools/nono".into(),
        tmux: "/tools/tmux".into(),
        balerix: "/tools/balerix".into(),
    };
    // fixed system table so the snapshot does not move with the repo's claude pin
    std::fs::create_dir_all(&layout.config_root).unwrap();
    std::fs::write(
        layout.system_mise_toml(),
        "[tools]\nclaude = \"2.1.0\"\ngh = \"2.100.0\"\n",
    )
    .unwrap();
    let rt = Runtime::new(layout.clone(), tools);
    let creds = CredentialBundle {
        claude_credentials: Some(json!({ "claudeAiOauth": { "accessToken": "sk-SECRET" } })),
        claude_account: Some(json!({ "oauthAccount": { "emailAddress": "a@b.c" } })),
        gh_token: Some("gho_SECRET".into()),
    };
    let mut out = String::new();
    for agent in ResolvedAgent::from_fleet(&fleet()) {
        let hooks = HookTarget {
            url: "http://127.0.0.1:7643".into(),
            secret: format!("secret-{}", agent.id.agent),
        };
        let plan = rt
            .render_agent(
                &agent,
                &creds,
                &hooks,
                &RenderOptions {
                    redact_credentials: true,
                },
            )
            .unwrap()
            .plan;
        let paths = layout.agent(&agent.id);
        assert_eq!(plan.script, paths.launch);
        for (label, path) in [
            ("settings.json", paths.claude_dir().join("settings.json")),
            (".gitconfig", paths.home.join(".gitconfig")),
            ("mise.toml", paths.mise_toml.clone()),
            ("nono-profile.json", paths.profile.clone()),
            ("launch.sh", paths.launch.clone()),
        ] {
            out.push_str(&format!(
                "==== {} {label} ====\n{}\n",
                agent.id,
                normalize(&std::fs::read_to_string(path).unwrap(), root)
            ));
        }
        let creds_file =
            std::fs::read_to_string(paths.claude_dir().join(".credentials.json")).unwrap();
        assert!(!creds_file.contains("SECRET"));
        assert!(
            !std::fs::read_to_string(&paths.launch)
                .unwrap()
                .contains("SECRET")
        );
    }
    insta::assert_snapshot!("payments_generated", out);
}
