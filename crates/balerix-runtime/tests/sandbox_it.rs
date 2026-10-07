#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::os::unix::fs::PermissionsExt;
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
    // Spec N-3: the cache is readable and not writable from inside. The
    // probe sits where a loose object would, one fan-out level down.
    let objects = crew.cache_objects();
    std::fs::create_dir_all(objects.join("ab")).unwrap();
    std::fs::write(objects.join("ab/probe"), "probe-ok\n").unwrap();
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
    // Spec N amendment §13.6: the supervisor sits outside the sandbox as
    // the same user. This test process stands in for it.
    let script = format!(
        "echo in > \"$HOME/ok\" && echo HOME=$HOME && echo FOO=$FOO \
         && (echo x > {outside}/nope 2>/dev/null && echo ESCAPED || echo denied) \
         && (cat {objects}/ab/probe 2>/dev/null || echo CACHE_UNREADABLE) \
         && (echo x > {objects}/nope 2>/dev/null && echo CACHE_WRITABLE || echo cache-denied) \
         && (kill -0 {outside_pid} 2>/dev/null && echo SIGNALLED || echo signal-denied)",
        outside = outside.display(),
        objects = objects.display(),
        outside_pid = std::process::id()
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
    assert!(
        stdout.contains("signal-denied") && !stdout.contains("SIGNALLED"),
        "a sandboxed process must not be able to signal one outside: {stdout}"
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

/// #118: a user block that would loosen signal isolation is refused at
/// render, naming its config path, before any profile is written or nono
/// runs. Teeth: the same block merged by hand is one nono accepts, and
/// under it the sandboxed process does signal the one outside, so the
/// refusal is what stands between the agent and its supervisor.
#[test]
fn a_user_block_cannot_loosen_signal_isolation() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox-signal-mode");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    for d in [&paths.home, &paths.workspace, &paths.nono_home, &paths.logs] {
        std::fs::create_dir_all(d).unwrap();
    }
    let grants = balerix_grants(&id, &paths, &crew, &layout, &tools.balerix, &tools.mise);
    let loosen = serde_json::json!({ "security": { "signal_mode": "allow_all" } });

    let e = render_profile(&id, &grants, 7643, &Default::default(), &loosen).unwrap_err();
    assert_eq!(
        e.to_string(),
        "f/c/a: sandbox.security.signal_mode: balerix-owned; agents stay signal-isolated"
    );
    assert!(
        !paths.profile.exists(),
        "refused before a profile is written"
    );
    assert!(
        !paths.logs.join("nono.validate.log").exists(),
        "refused before nono runs"
    );

    let pinned = render_profile(
        &id,
        &grants,
        7643,
        &Default::default(),
        &serde_json::json!({}),
    )
    .unwrap();
    let signal = |profile: &serde_json::Value| {
        write_profile(&id, &paths, profile).unwrap();
        validate_profile(&tools, &id, &paths).unwrap();
        let out = Command::new(&tools.nono)
            .args([
                "-s",
                "run",
                "--profile",
                &paths.profile.display().to_string(),
                "--",
                "/bin/sh",
                "-c",
                &format!(
                    "kill -0 {} 2>/dev/null && echo SIGNALLED || echo signal-denied",
                    std::process::id()
                ),
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &paths.nono_home)
            .current_dir(&paths.workspace)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "nono run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(signal(&pinned), "signal-denied");
    let loosened = balerix_runtime::merge_profile(pinned, &loosen);
    assert_eq!(
        signal(&loosened),
        "SIGNALLED",
        "without the refusal the block would reach the supervisor"
    );
}

/// Spec N amendment §4: under the git profile the clone and the cache's
/// objects read; the clone cannot be written, the agent's home cannot be
/// read, nothing outside can be read, and the environment is the
/// profile's alone. The network block is not probed here; `workspace_it`
/// asserts it on the profile as written
/// (`daemon_git_in_a_clone_runs_under_the_git_profile`).
#[test]
fn the_git_profile_reads_the_clone_and_the_cache_and_writes_nothing() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox-git");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    let objects = crew.cache_objects();
    for d in [&paths.home, &paths.workspace, &objects.join("ab")] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(paths.workspace.join("tracked"), "clone-ok\n").unwrap();
    std::fs::write(objects.join("ab/probe"), "cache-ok\n").unwrap();
    std::fs::write(paths.home.join("secret"), "home\n").unwrap();
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), "outside\n").unwrap();

    balerix_runtime::write_git_profile(&tools, "f/c/a", &paths, &crew, &[]).unwrap();
    assert_eq!(
        std::fs::metadata(&paths.git_profile)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let script = format!(
        "cat {ws}/tracked && cat {objects}/ab/probe \
         && (echo x > {ws}/nope 2>/dev/null && echo CLONE_WRITABLE || echo clone-denied) \
         && (cat {home}/secret 2>/dev/null && echo HOME_READABLE || echo home-denied) \
         && (cat {outside}/secret 2>/dev/null && echo OUTSIDE_READABLE || echo outside-denied) \
         && echo LOCKS=$GIT_OPTIONAL_LOCKS LAZY=$GIT_NO_LAZY_FETCH FOO=$FOO",
        ws = paths.workspace.display(),
        objects = objects.display(),
        home = paths.home.display(),
        outside = outside.display(),
    );
    let out = Command::new(&tools.nono)
        .args([
            "-s",
            "run",
            "--profile",
            &paths.git_profile.display().to_string(),
            "--",
            "/bin/sh",
            "-c",
            &script,
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.nono_home)
        .env("FOO", "leak")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "nono run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout,
        "clone-ok\ncache-ok\nclone-denied\nhome-denied\noutside-denied\nLOCKS=0 LAZY=1 FOO=\n"
    );
    assert!(!paths.workspace.join("nope").exists());
}

/// `tools` with `git` replaced by a shell script: `#!/bin/sh` and `body`.
fn git_shim(
    root: &std::path::Path,
    tools: &balerix_runtime::ToolPaths,
    name: &str,
    body: &str,
) -> balerix_runtime::ToolPaths {
    let shim = root.join(name);
    std::fs::write(&shim, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    balerix_runtime::ToolPaths {
        git: shim,
        ..tools.clone()
    }
}

/// Spec N amendment §12.1 (#109): the profile grants the directory git
/// names as its exec-path, resolved, wherever it is. The fixture's is a
/// symlink to a directory with a space in its name, outside every other
/// grant. A harvest through such a git would not prove the grant (git
/// falls back to the host `git` on `PATH` for its helpers), so the read
/// is asserted directly.
#[test]
fn the_git_profile_grants_gits_exec_path() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox-git-exec-path");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    for d in [&paths.workspace, &crew.cache_objects()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let real = root.join("git core");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("helper"), "helper-ok\n").unwrap();
    let link = root.join("exec-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let shimmed = git_shim(
        &root,
        &tools,
        "git-exec-path.sh",
        &format!(
            "exec {} --exec-path='{}' \"$@\"",
            tools.git.display(),
            link.display()
        ),
    );

    balerix_runtime::write_git_profile(&shimmed, "f/c/a", &paths, &crew, &[]).unwrap();
    let granted = std::fs::canonicalize(&real).unwrap();
    let profile = std::fs::read_to_string(&paths.git_profile).unwrap();
    assert!(
        profile.contains(&format!("\"{}\"", granted.display())),
        "the resolved exec-path is granted: {profile}"
    );
    assert!(
        !profile.contains("exec-link"),
        "Landlock binds to what the path resolves to: {profile}"
    );

    let out = Command::new(&tools.nono)
        .args([
            "-s",
            "run",
            "--profile",
            &paths.git_profile.display().to_string(),
            "--",
            "/bin/cat",
            &granted.join("helper").display().to_string(),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.nono_home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "nono run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "helper-ok\n");
}

/// §12.1: a git that cannot name an existing exec-path directory fails the
/// step, and no profile is written without the grant (NS-5).
#[test]
fn a_git_that_cannot_name_its_exec_path_writes_no_profile() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox-git-no-exec-path");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());

    let refusing = git_shim(
        &root,
        &tools,
        "git-refusing.sh",
        "echo 'shim: no exec path' >&2\nexit 3",
    );
    let e = balerix_runtime::write_git_profile(&refusing, "f/c/a", &paths, &crew, &[])
        .unwrap_err()
        .to_string();
    assert_eq!(e, "f/c/a: git --exec-path: shim: no exec path");
    assert!(!paths.git_profile.exists());

    let missing = root.join("missing");
    let lost = git_shim(
        &root,
        &tools,
        "git-lost.sh",
        &format!("echo '{}'", missing.display()),
    );
    let e = balerix_runtime::write_git_profile(&lost, "f/c/a", &paths, &crew, &[])
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with(&format!("f/c/a: git --exec-path: {}: ", missing.display())),
        "{e}"
    );
    assert!(!paths.git_profile.exists());

    let file = root.join("a-file");
    std::fs::write(&file, "").unwrap();
    let wrong = git_shim(
        &root,
        &tools,
        "git-wrong.sh",
        &format!("echo '{}'", file.display()),
    );
    let e = balerix_runtime::write_git_profile(&wrong, "f/c/a", &paths, &crew, &[])
        .unwrap_err()
        .to_string();
    assert!(e.ends_with(": not a directory"), "{e}");
    assert!(!paths.git_profile.exists());
}

/// §12.1: the query runs from an empty environment, as the sandboxed git
/// starts. A `GIT_EXEC_PATH` the daemon inherited would otherwise make the
/// grant name a directory the sandboxed git never uses. The workspace
/// forbids `set_var`, so the shim refuses when it sees `HOME`, which this
/// test process has.
#[test]
fn the_exec_path_query_starts_from_an_empty_environment() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    assert!(
        std::env::var_os("HOME").is_some(),
        "this test proves nothing without HOME in its own environment"
    );
    let root = support::temp_root("sandbox-git-exec-path-env");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    for d in [&paths.workspace, &crew.cache_objects()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let strict = git_shim(
        &root,
        &tools,
        "git-strict-env.sh",
        &format!(
            "if [ -n \"${{HOME+x}}\" ]; then\n  echo 'shim: inherited environment' >&2\n  exit 3\nfi\nexec {} \"$@\"",
            tools.git.display()
        ),
    );
    balerix_runtime::write_git_profile(&strict, "f/c/a", &paths, &crew, &[]).unwrap();
    assert!(paths.git_profile.exists());
}
