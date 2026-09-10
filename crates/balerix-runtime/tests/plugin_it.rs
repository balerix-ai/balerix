//! A real package launched under nono through `mise run` (plugins spec
//! §11 "runtime integration"): the start task runs, sees the token and its
//! scratch dir, and cannot write into the package.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::fs;
use std::process::Command;

use balerix_api::PluginManifest;
use balerix_core::{HookTarget, Materializer, ResolvedPlugin};
use balerix_runtime::Runtime;

const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: probe\nversion: 0.0.1\nprotocol: 1\nstart: serve\n";
const MISE_TOML: &str = r#"[tools]

[tasks.serve]
run = 'printf "%s\n%s\n%s\n%s\n" "$BALERIX_PLUGIN_NAME" "$BALERIX_PLUGIN_TOKEN" "$HOME" "$PWD" > "$BALERIX_PLUGIN_SCRATCH/ran"; if touch "$PWD/escape" 2>/dev/null; then echo escaped >> "$BALERIX_PLUGIN_SCRATCH/ran"; fi'
"#;

#[test]
fn a_package_runs_its_start_task_inside_the_sandbox() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+nono", false));
        return;
    };
    let root = support::temp_root("plugin");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    fs::create_dir_all(&layout.config_root).unwrap();
    let package = root.join("pkg");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("balerix-plugin.yaml"), MANIFEST).unwrap();
    fs::write(package.join("mise.toml"), MISE_TOML).unwrap();
    let manifest: PluginManifest = serde_norway::from_str(MANIFEST).unwrap();
    let plugin = ResolvedPlugin {
        name: "probe".parse().unwrap(),
        package: package.clone(),
        manifest,
        config: serde_json::json!({}),
        digest: None,
    };
    let host = HookTarget {
        url: "http://127.0.0.1:1".into(),
        secret: "tok-secret".into(),
    };
    let rt = Runtime::new(layout.clone(), tools);
    let plan = rt.materialize_plugin(&plugin, &host).unwrap();
    let paths = layout.plugin(&plugin.name);
    assert!(paths.installed_marker().exists());
    let launch = fs::read_to_string(&paths.launch).unwrap();
    assert!(!launch.contains("tok-secret"), "token in launch.sh");

    let out = Command::new("/bin/sh")
        .arg(&plan.script)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "launch.sh failed: {}\nnono.log: {}",
        String::from_utf8_lossy(&out.stderr),
        fs::read_to_string(paths.logs.join("nono.log")).unwrap_or_default()
    );
    let ran = fs::read_to_string(paths.scratch.join("ran")).unwrap();
    let lines: Vec<&str> = ran.lines().collect();
    assert_eq!(lines[0], "probe");
    assert_eq!(lines[1], "tok-secret", "the token reaches the plugin");
    assert_eq!(
        lines[2],
        paths.home.display().to_string(),
        "HOME is the plugin home"
    );
    assert_eq!(
        lines[3],
        package.display().to_string(),
        "the start task runs in the package, not in $HOME"
    );
    assert_eq!(lines.len(), 4, "the package must be read-only: {ran}");
    assert!(!package.join("escape").exists());

    rt.purge_plugin(&plugin.name).unwrap();
    assert!(!paths.root.exists());
    rt.purge_plugin(&plugin.name).unwrap(); // idempotent
}
