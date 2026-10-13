//! A real package launched under nono through `mise run` (plugins spec
//! §11 "runtime integration"): the start task runs, sees the token and its
//! scratch dir, and cannot write into the package.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::fs;
use std::process::Command;

use balerix_api::PluginManifest;
use balerix_core::{HookTarget, Materializer, ResolvedPlugin};
use balerix_runtime::{Runtime, sh_quote};

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
        fleet_defaults: serde_json::json!({}),
        digest: None,
    };
    let host = HookTarget {
        url: "http://127.0.0.1:1".into(),
        secret: "tok-secret".into(),
    };
    let rt = Runtime::new(layout.clone(), tools);
    let plan = rt.materialize_plugin(&plugin, &host).unwrap();
    let paths = layout.plugin(&plugin.name);
    let installed = || {
        fs::metadata(paths.installed_marker())
            .unwrap()
            .modified()
            .unwrap()
    };
    let first = installed();
    assert_eq!(
        fs::read_to_string(paths.installed_marker()).unwrap(),
        plugin.hash().as_str(),
        "the marker holds what render_plugin compares it with"
    );
    // #4: the marker install_plugin writes is the one render_plugin reads,
    // so the same inputs do not install again
    assert_eq!(rt.materialize_plugin(&plugin, &host).unwrap(), plan);
    assert_eq!(
        installed(),
        first,
        "same inputs: the marker is not rewritten"
    );
    let launch = fs::read_to_string(&paths.launch).unwrap();
    assert!(!launch.contains("tok-secret"), "token in launch.sh");

    // `launch.sh` starts under `balerix agent-supervise` (#119), and the
    // `balerix` slot here is this test executable, which must not run it
    // (AGENTS.md): run the script with the wrapper's three words taken
    // out, which is exactly what the wrapper would run. The wrapper in
    // front of a real plugin is `crates/balerix/tests/cli_supervise.rs`.
    let wrapper = format!("{} 'agent-supervise' '--' ", sh_quote(&plan.argv[0]));
    assert_eq!(plan.argv[1..3], ["agent-supervise", "--"]);
    assert!(launch.contains(&wrapper), "{launch}");
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(launch.replacen(&wrapper, "", 1))
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

/// The plugin's mise state and cache directories are under its `home/`,
/// which it can write: on a re-install a link at one of them is made a
/// real directory again before mise runs, and a link below them leads
/// nowhere, since the install's sandbox refuses a write that resolves
/// outside `home/` (the install fails, and the outside directory gains
/// nothing).
#[test]
fn a_plugin_reinstall_writes_nothing_through_a_link_under_home() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+nono", false));
        return;
    };
    let root = support::temp_root("plugin-links");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    fs::create_dir_all(&layout.config_root).unwrap();
    let package = root.join("pkg");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("balerix-plugin.yaml"), MANIFEST).unwrap();
    fs::write(package.join("mise.toml"), MISE_TOML).unwrap();
    let plugin = ResolvedPlugin {
        name: "probe".parse().unwrap(),
        package,
        manifest: serde_norway::from_str(MANIFEST).unwrap(),
        config: serde_json::json!({}),
        fleet_defaults: serde_json::json!({}),
        digest: None,
    };
    let host = HookTarget {
        url: "http://127.0.0.1:1".into(),
        secret: "tok-secret".into(),
    };
    let rt = Runtime::new(layout.clone(), tools);
    rt.materialize_plugin(&plugin, &host).unwrap();
    let paths = layout.plugin(&plugin.name);
    let plant = |at: &std::path::Path, n: usize| {
        let outside = root.join(format!("outside-{n}"));
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("sentinel"), "left alone").unwrap();
        fs::remove_dir_all(at).unwrap();
        std::os::unix::fs::symlink(&outside, at).unwrap();
        // a re-install, as after a changed package
        fs::remove_file(paths.installed_marker()).unwrap();
        outside
    };
    let names = |dir: &std::path::Path| -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };

    let outside = plant(&paths.mise_state_dir(), 0);
    rt.materialize_plugin(&plugin, &host).unwrap();
    assert_eq!(names(&outside), ["sentinel"]);
    assert!(
        fs::symlink_metadata(paths.mise_state_dir())
            .unwrap()
            .is_dir()
    );
    assert!(paths.mise_state_dir().join("trusted-configs").is_dir());

    let below = paths.mise_state_dir().join("trusted-configs");
    let outside = plant(&below, 1);
    let err = rt.materialize_plugin(&plugin, &host).unwrap_err();
    assert_eq!(names(&outside), ["sentinel"], "{err}");
    assert!(
        err.to_string()
            .starts_with("balerix/plugins/probe: mise trust"),
        "{err}"
    );
    assert!(!paths.installed_marker().exists());

    // a link into the daemon pool, which the install writes: the trust,
    // which follows it, cannot write the pool, and the install writes
    // nothing of the plugin's
    fs::remove_file(&below).unwrap();
    let pool = layout.mise_data_dir();
    fs::create_dir_all(&pool).unwrap();
    let before = names(&pool);
    std::os::unix::fs::symlink(&pool, &below).unwrap();
    let err = rt.materialize_plugin(&plugin, &host).unwrap_err();
    assert_eq!(names(&pool), before, "{err}");
    assert!(
        err.to_string()
            .starts_with("balerix/plugins/probe: mise trust"),
        "{err}"
    );
}

/// A package that pins a tool (a release package pins its binary so):
/// the sandboxed install fetches it into the daemon pool, the trust record
/// lands in the plugin's own state directory, and the install's own home
/// is gone afterwards.
#[test]
fn a_package_tool_is_installed_into_the_daemon_pool_under_the_sandbox() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+nono", false));
        return;
    };
    let root = support::temp_root("plugin-tool");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    fs::create_dir_all(&layout.config_root).unwrap();
    let (url, hits) = support::serve_tarball(support::probe_tarball(&root));
    let package = root.join("pkg");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("balerix-plugin.yaml"), MANIFEST).unwrap();
    fs::write(
        package.join("mise.toml"),
        format!(
            "[tools]\n\"http:balerix-probe\" = {{ version = \"1.0.0\", url = \"{url}\" }}\n\n\
             [tasks.serve]\nrun = 'balerix-probe'\n"
        ),
    )
    .unwrap();
    let plugin = ResolvedPlugin {
        name: "probe".parse().unwrap(),
        package,
        manifest: serde_norway::from_str(MANIFEST).unwrap(),
        config: serde_json::json!({}),
        fleet_defaults: serde_json::json!({}),
        digest: None,
    };
    let rt = Runtime::new(layout.clone(), tools);
    rt.materialize_plugin(
        &plugin,
        &HookTarget {
            url: "http://127.0.0.1:1".into(),
            secret: "tok-secret".into(),
        },
    )
    .unwrap();
    assert!(hits.load(std::sync::atomic::Ordering::SeqCst) > 0);
    assert!(
        layout
            .mise_data_dir()
            .join("installs/http-balerix-probe/1.0.0/balerix-probe")
            .is_file(),
        "the tool is in the daemon pool"
    );
    let paths = layout.plugin(&plugin.name);
    assert!(
        fs::read_dir(paths.mise_state_dir().join("trusted-configs"))
            .unwrap()
            .next()
            .is_some(),
        "the trust record is where the plugin's mise run looks"
    );
    assert!(!paths.xdg_data().join("mise").exists(), "nothing private");
    assert!(!paths.mise_install_home().exists());
}
