//! Spec C §2.2 against the real daemon: the capability gate, the active
//! pair, the two 404s, the 413 and the 400, with `FakeWorkspace` behind.
//! Exercises both the raw `w.api.plugin`/`w.api.raw_get` routes and the
//! SDK's `Host::workspace_*` against the same daemon.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{
    ActivationState, AgentSettings, CrewSpec, FileDiff, FileStatus, FleetRequest, FleetSpec,
    GitSettings, WORKSPACE_FILE_LIMIT, WorkspaceDiff,
};
use balerix_core::plugin_id;
use balerix_plugin_sdk::{Env, Host, Plugin, bind, run};
use serde_json::json;
use support::{World, world};

struct Silent;
impl Plugin for Silent {}

async fn token(w: &World, plugin: &str) -> String {
    w.daemon
        .hook_secret(&plugin_id(&plugin.parse().unwrap()))
        .await
        .unwrap()
}

/// Starts a silent SDK plugin under `name` and says hello with its token.
async fn start_silent(w: &World, name: &str) -> Host {
    let env = Env {
        api_url: w.api.base.clone(),
        name: name.into(),
        token: token(w, name).await,
        scratch: w.dir.path().join("s"),
    };
    let (listener, listen) = bind().await.unwrap();
    let tok = env.token.clone();
    tokio::spawn(async move { run(listener, Arc::new(Silent), &tok).await });
    let host = Host::new(env).unwrap();
    host.hello("0.1.0", &listen).await.unwrap();
    host
}

fn spec(agents: &[&str]) -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "release".into(),
                git: GitSettings::default(),
                agents: agents
                    .iter()
                    .map(|n| {
                        (
                            n.to_string(),
                            AgentSettings {
                                plugins: BTreeMap::from([("web".to_string(), json!({}))]),
                                ..Default::default()
                            },
                        )
                    })
                    .collect(),
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

fn diff() -> WorkspaceDiff {
    WorkspaceDiff {
        base_ref: String::new(),
        merge_base: "m".repeat(40),
        head: "h".repeat(40),
        files: vec![FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            uncommitted: true,
            binary: false,
            patch: "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n".into(),
            truncated: false,
        }],
        truncated: false,
    }
}

async fn wait_active(w: &World, agent: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rec = w.daemon.get(&"f".parse().unwrap()).await;
            if rec.as_ref().is_some_and(|r| {
                r.status.agents.get(agent).is_some_and(|a| {
                    a.plugins
                        .get("web")
                        .is_some_and(|p| p.state == ActivationState::Active)
                })
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("web active for the agent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_routes_are_gated_by_capability_activation_and_the_path_rule() {
    let w = world().await;
    let web = start_silent(&w, "web").await;
    let flow = start_silent(&w, "flow").await;
    let (s, v) = w.api.admin(
        "POST",
        "/v1/fleets",
        Some(&json!(FleetRequest {
            spec: spec(&["a", "b"]),
            credentials: Default::default()
        })),
    );
    assert_eq!(s, 200, "{v}");
    wait_active(&w, "f/c/a").await;
    wait_active(&w, "f/c/b").await;
    w.h.workspace.set(
        &"f/c/a".parse().unwrap(),
        diff(),
        BTreeMap::from([
            ("src/lib.rs".to_string(), b"fn a() {}\n".to_vec()),
            (
                "big".to_string(),
                vec![0u8; (WORKSPACE_FILE_LIMIT + 1) as usize],
            ),
        ]),
    );

    // the diff, with the crew's ref as the base
    let got = web.workspace_diff("f/c/a").await.unwrap();
    assert_eq!(got.base_ref, "origin/release");
    assert_eq!(got.files, diff().files);
    assert!(
        w.h.workspace
            .calls()
            .contains(&"diff f/c/a origin/release".to_string())
    );
    // file and tree
    assert_eq!(
        web.workspace_file("f/c/a", "src/lib.rs").await.unwrap(),
        Some(b"fn a() {}\n".to_vec())
    );
    assert_eq!(web.workspace_file("f/c/a", "nope").await.unwrap(), None);
    let tree = web.workspace_tree("f/c/a", "").await.unwrap();
    assert_eq!(tree.entries.len(), 2);
    assert_eq!(tree.entries[1].name, "src");
    let ver = web.workspace_version("f/c/a").await.unwrap();
    assert_eq!(ver.head, "h".repeat(40));
    assert_eq!(ver.fingerprint.len(), 64);
    // the version: the diff's head and a 64-hex fingerprint over the files
    let web_tok = token(&w, "web").await;
    let flow_tok = token(&w, "flow").await;
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/version",
        None,
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["head"], "h".repeat(40));
    assert_eq!(v["fingerprint"].as_str().unwrap().len(), 64);
    assert!(
        w.h.workspace
            .calls()
            .contains(&"version f/c/a origin/release".to_string())
    );
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/b/workspace/version",
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (404, Some("no workspace for agent f/c/b"))
    );
    // the raw statuses: 403 without the capability, 404 for an active pair
    // with no worktree, 404 for a pair that is not active, 413, 400
    let (s, v) = w.api.plugin(
        &flow_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/diff",
        None,
    );
    assert_eq!(s, 403, "{v}");
    assert_eq!(
        v["error"],
        "capability \"workspace\" not declared in balerix-plugin.yaml"
    );
    let e = flow.workspace_diff("f/c/a").await.unwrap_err();
    assert!(e.to_string().starts_with("daemon: HTTP 403"), "{e}");
    let (s, v) = w.api.plugin(
        &flow_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/version",
        None,
    );
    assert_eq!(s, 403, "{v}");
    assert_eq!(
        v["error"],
        "capability \"workspace\" not declared in balerix-plugin.yaml"
    );
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/b/workspace/diff",
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (404, Some("no workspace for agent f/c/b"))
    );
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/zed/workspace/diff",
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (404, Some("plugin is not active for agent f/c/zed"))
    );
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/zed/workspace/version",
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (404, Some("plugin is not active for agent f/c/zed"))
    );
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/file?path=big",
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (413, Some("file larger than 1 MiB"))
    );
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/file?path=../x",
        None,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("workspace: invalid path: \"..\" segment"))
    );
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/tree?path=src/lib.rs",
        None,
    );
    assert_eq!((s, v["error"].as_str()), (400, Some("not a directory")));
    let (s, v) = w.api.plugin(
        &web_tok,
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/file?path=nope",
        None,
    );
    assert_eq!((s, v["error"].as_str()), (404, Some("no such path")));
    let (s, _) = w.api.plugin(
        "nope",
        "GET",
        "/v1/plugin-host/agents/f/c/a/workspace/diff",
        None,
    );
    assert_eq!(s, 401);
    // the bytes route is raw, not JSON
    let (s, bytes) = w.api.raw_get(
        &web_tok,
        "/v1/plugin-host/agents/f/c/a/workspace/file?path=src/lib.rs",
    );
    assert_eq!((s, bytes.as_slice()), (200, &b"fn a() {}\n"[..]));
}
