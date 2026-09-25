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
        crew.cache_objects(),
    ];
    dirs.extend(layout.agent_pools(&id));
    for d in &dirs {
        std::fs::create_dir_all(d).unwrap();
    }
    // Spec N-3: the cache is readable and not writable from inside.
    let objects = crew.cache_objects();
    std::fs::write(objects.join("probe"), "probe-ok\n").unwrap();
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
        "echo in > \"$HOME/ok\" && echo HOME=$HOME && echo FOO=$FOO \
         && (echo x > {outside}/nope 2>/dev/null && echo ESCAPED || echo denied) \
         && (cat {objects}/probe 2>/dev/null || echo CACHE_UNREADABLE) \
         && (echo x > {objects}/nope 2>/dev/null && echo CACHE_WRITABLE || echo cache-denied)",
        outside = outside.display(),
        objects = objects.display()
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
    assert!(
        stdout.contains("probe-ok"),
        "an object under the cache reads from inside the profile: {stdout}"
    );
    assert!(
        stdout.contains("cache-denied") && !stdout.contains("CACHE_WRITABLE"),
        "a write into the cache must be denied: {stdout}"
    );
    assert!(!objects.join("nope").exists());
}

/// The user's `sandbox:` block is merged into the profile verbatim, so its
/// keys must be nono's own. The test above renders with an empty block, which
/// leaves that merge unvalidated — `examples/payments.yaml` shipped
/// `network: { mode: allow }` because nothing here ever handed real nono a
/// user block. `mode` is not a nono field (checked against 0.76 and 0.77);
/// `block` is.
#[test]
fn a_user_network_block_validates_and_a_bogus_key_does_not() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox-user-block");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    std::fs::create_dir_all(&paths.nono_home).unwrap();
    std::fs::create_dir_all(&paths.logs).unwrap();

    let render = |user| {
        render_profile(
            &id,
            &balerix_grants(&id, &paths, &crew, &layout, &tools.balerix, &tools.mise),
            7643,
            &Default::default(),
            &user,
        )
        .unwrap()
    };
    let validate = |user| {
        let profile = render(user);
        write_profile(&id, &paths, &profile).unwrap();
        validate_profile(&tools, &id, &paths)
    };

    // What the example ships: unrestricted egress, spelled nono's way.
    validate(serde_json::json!({ "network": { "block": false } }))
        .expect("`network: { block: false }` must validate");
    // And the tightened form, which must keep the daemon port open.
    let blocked = render(serde_json::json!({ "network": { "block": true } }));
    assert_eq!(blocked["network"]["open_port"], serde_json::json!([7643]));
    validate(serde_json::json!({ "network": { "block": true } }))
        .expect("`network: { block: true }` must validate");

    // Teeth: the key that shipped broken is rejected.
    validate(serde_json::json!({ "network": { "mode": "allow" } }))
        .expect_err("`mode` is not a nono network field");
    // nono names the offending key on stdout, which `CmdFailure` does not
    // carry, so the error a user sees is only "validation failed" and the
    // detail lives in the log. Asserted so a future fix that surfaces it
    // has a test to update rather than silently losing the breadcrumb.
    let log = std::fs::read_to_string(paths.logs.join("nono.validate.log")).unwrap();
    assert!(
        log.contains("unknown field `mode`"),
        "the log must keep the detail nono printed: {log}"
    );
}
