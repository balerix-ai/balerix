//! The generated plugin files for a fixed plugin, with the temp root
//! replaced by `<root>` so the snapshot is stable.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use balerix_api::PluginManifest;
use balerix_core::{HookTarget, ResolvedPlugin};
use balerix_runtime::{Runtime, StateLayout, ToolPaths};
use serde_json::json;

#[test]
fn plugin_files_match_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().display().to_string();
    let layout = StateLayout::xdg(
        dir.path().join("state"),
        dir.path().join("data"),
        dir.path().join("config"),
    );
    // Fixed tool paths so launch.sh and the profile are deterministic;
    // nothing is executed. They must not exist: `plugin_grants`
    // canonicalizes `mise`, and a real `/usr/local/bin/mise` is a symlink
    // on a Homebrew or `mise.run` install, which would move the snapshot's
    // `filesystem.read` entry from machine to machine (cf.
    // `generated_golden.rs`).
    let tools = ToolPaths {
        git: "/tools/git".into(),
        gh: "/tools/gh".into(),
        mise: "/tools/mise".into(),
        nono: "/tools/nono".into(),
        tmux: "/tools/tmux".into(),
        balerix: "/tools/balerix".into(),
    };
    let rt = Runtime::new(layout.clone(), tools);
    let manifest: PluginManifest = serde_json::from_value(json!({
        "apiVersion": "balerix/v1", "kind": "Plugin", "name": "web", "version": "0.1.0",
        "protocol": 1, "start": "serve", "routes": true,
        "sandbox": { "network": { "block": true }, "filesystem": { "read": ["/opt/data"] } }
    }))
    .unwrap();
    let plugin = ResolvedPlugin {
        name: "web".parse().unwrap(),
        package: dir.path().join("data/plugins/web/0123456789ab"),
        manifest,
        config: json!({ "title": "t" }),
        fleet_defaults: json!({}),
        digest: Some("0123456789abcdef".into()),
    };
    let host = HookTarget {
        url: "http://127.0.0.1:7643".into(),
        secret: "plugin-token".into(),
    };
    let out = rt.render_plugin(&plugin, &host).unwrap();
    assert!(out.toolchain_changed);
    let paths = layout.plugin(&plugin.name);
    let scrub = |s: String| s.replace(&root, "<root>");
    let profile = scrub(std::fs::read_to_string(&paths.profile).unwrap());
    let launch = scrub(std::fs::read_to_string(&paths.launch).unwrap());
    insta::assert_snapshot!("plugin_profile", profile);
    insta::assert_snapshot!("plugin_launch", launch);
    // What a successful `install_plugin` leaves behind. `toolchain_changed`
    // means "the install has to run again", so it stays true until the
    // marker holds this plugin's hash.
    std::fs::write(paths.installed_marker(), plugin.hash().as_str()).unwrap();
    let again = rt.render_plugin(&plugin, &host).unwrap();
    assert!(!again.toolchain_changed, "same inputs: no reinstall");
    assert!(
        paths.installed_marker().exists(),
        "an unchanged render keeps the marker"
    );
    assert_eq!(again.plan, out.plan);
}

/// A manifest grant inside balerix's own roots is refused before a file is
/// written from it (#2): no profile and no `launch.sh` for that plugin.
#[test]
fn a_manifest_grant_under_balerixs_roots_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().display().to_string();
    let layout = StateLayout::xdg(
        dir.path().join("state"),
        dir.path().join("data"),
        dir.path().join("config"),
    );
    let tools = ToolPaths {
        git: "/tools/git".into(),
        gh: "/tools/gh".into(),
        mise: "/tools/mise".into(),
        nono: "/tools/nono".into(),
        tmux: "/tools/tmux".into(),
        balerix: "/tools/balerix".into(),
    };
    let rt = Runtime::new(layout.clone(), tools);
    let server = layout.server_dir().display().to_string();
    let manifest: PluginManifest = serde_json::from_value(json!({
        "apiVersion": "balerix/v1", "kind": "Plugin", "name": "web", "version": "0.1.0",
        "protocol": 1, "start": "serve",
        "sandbox": { "filesystem": { "read": ["/opt/data"], "allow": [server] } }
    }))
    .unwrap();
    let plugin = ResolvedPlugin {
        name: "web".parse().unwrap(),
        package: dir.path().join("data/plugins/web/0123456789ab"),
        manifest,
        config: json!({}),
        fleet_defaults: json!({}),
        digest: Some("0123456789abcdef".into()),
    };
    let host = HookTarget {
        url: "http://127.0.0.1:7643".into(),
        secret: "plugin-token".into(),
    };
    let e = rt.render_plugin(&plugin, &host).unwrap_err().to_string();
    // the temp dir may itself sit behind a symlink (macOS `/var`), and the
    // message names the root as written
    assert_eq!(
        e.replace(&root, "<root>"),
        "balerix/plugins/web: sandbox.filesystem.allow[0]: <root>/state/server is inside \
         balerix's state root <root>/state; a plugin may name only paths in its home/ or \
         scratch/ there, or read its package or the mise pool"
    );
    let paths = layout.plugin(&plugin.name);
    assert!(!paths.profile.exists(), "no profile from a refused block");
    assert!(!paths.launch.exists());
}
