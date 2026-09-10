#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::process::Command;

use balerix_core::AgentId;
use balerix_runtime::{agent_env, balerix_grants, render_profile, validate_profile, write_profile};

#[test]
fn generated_profile_validates_and_enforces_isolation() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    let mut dirs = vec![
        paths.home.clone(),
        paths.workspace.clone(),
        paths.nono_home.clone(),
        paths.logs.clone(),
        crew.repo.join(".git"),
    ];
    dirs.extend(layout.agent_pools(&id));
    for d in &dirs {
        std::fs::create_dir_all(d).unwrap();
    }
    let env = agent_env(
        &id,
        &paths,
        &layout,
        "http://127.0.0.1:7643",
        "s3",
        &Default::default(),
    );
    let profile = render_profile(
        &id,
        &balerix_grants(&id, &paths, &crew, &layout, &tools.balerix, &tools.mise),
        7643,
        &env,
        &serde_json::json!({}),
    )
    .unwrap();
    write_profile(&id, &paths, &profile).unwrap();
    validate_profile(&tools, &id, &paths).unwrap();

    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let script = format!(
        "echo in > \"$HOME/ok\" && echo HOME=$HOME && echo FOO=$FOO && (echo x > {}/nope 2>/dev/null && echo ESCAPED || echo denied)",
        outside.display()
    );
    let out = Command::new(&tools.nono)
        .args([
            "-s",
            "run",
            "--profile",
            &paths.profile.display().to_string(),
            "--",
            "/bin/sh",
            "-c",
            &script,
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.nono_home)
        .env("FOO", "leak")
        .current_dir(&paths.workspace)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "nono run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!("HOME={}", paths.home.display())),
        "HOME relocated via set_vars: {stdout}"
    );
    assert!(
        stdout.contains("FOO=\n"),
        "outer env stripped by deny_vars: {stdout}"
    );
    assert!(
        stdout.contains("denied"),
        "write outside grants must fail: {stdout}"
    );
    assert!(paths.home.join("ok").exists(), "write inside home succeeds");
    assert!(!outside.join("nope").exists());
}
