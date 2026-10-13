#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::process::Command;

use balerix_core::AgentId;
use balerix_runtime::{
    InstallEnv, SocketPolicy, Toolchain, agent_env, agent_install_call, embedded_system_tools,
    mise_env,
};

/// Copies the host's `<tool>@<version>` install into `pool/installs`.
/// Returns false if the host does not have it.
fn seed_into(
    tools: &balerix_runtime::ToolPaths,
    pool: &std::path::Path,
    tool: &str,
    version: &str,
) -> bool {
    let out = Command::new(&tools.mise)
        .args(["where", &format!("{tool}@{version}")])
        .output()
        .unwrap();
    if !out.status.success() {
        return false;
    }
    let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dst = pool.join("installs").join(tool).join(version);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    if !Command::new("cp")
        .args(["-r", &src, &dst.display().to_string()])
        .status()
        .unwrap()
        .success()
    {
        return false;
    }
    // and the tool's backend record, as a pool mise installed has it
    let record = std::path::Path::new(&src).with_file_name(".mise.backend.toml");
    if record.is_file() {
        std::fs::copy(&record, dst.with_file_name(".mise.backend.toml")).unwrap();
    }
    // A pool mise installed also holds the tool's runtime links (`latest`,
    // the version prefixes and, from mise 2026.10, `global`): let mise
    // make them, as `install_level` would, so the pool is one mise would
    // have made. Everything is installed already, so nothing is
    // downloaded.
    let seed = pool.with_file_name(format!("seed-{tool}"));
    std::fs::create_dir_all(&seed).unwrap();
    let config = seed.join("mise.toml");
    std::fs::write(&config, format!("[tools]\n{tool} = \"{version}\"\n")).unwrap();
    let env = balerix_runtime::level_env(&config, pool, &[]);
    ["trust", "install"].iter().all(|step| {
        let mut cmd = Command::new(&tools.mise);
        cmd.arg(step);
        if *step == "trust" {
            cmd.arg(&config);
        }
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &seed)
            .envs(&env)
            .current_dir("/")
            .status()
            .unwrap()
            .success()
    })
}

/// The version this repo pins for `tool`, read from its own `mise.toml`.
fn repo_pin(tool: &str) -> String {
    let text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../mise.toml")).unwrap();
    let doc: toml::Table = text.parse().unwrap();
    doc["tools"][tool].as_str().unwrap().to_string()
}

#[test]
fn installs_nothing_when_seeded_and_exec_resolves_read_only() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+gh", false));
        return;
    };
    let root = support::temp_root("toolchain");
    // the agent's own install runs under nono
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let system = embedded_system_tools();
    let gh_version = system["gh"].clone();
    if !support::require_or_skip(
        "gh install to seed from",
        seed_into(&tools, &layout.mise_data_dir(), "gh", &gh_version),
    ) {
        return;
    }
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    // `$HOME` for the resolution below, as `write_home` makes it
    std::fs::create_dir_all(&paths.home).unwrap();
    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
        policy: SocketPolicy::Open,
        env: &InstallEnv::default(),
    };
    // only gh: claude is not seeded and must not be attempted
    let only_gh: BTreeMap<String, String> =
        BTreeMap::from([("gh".to_string(), gh_version.clone())]);
    tc.write(&id, &paths, &only_gh, &BTreeMap::new(), true)
        .unwrap();
    tc.install(&id, &paths).unwrap();

    // the pool chain: does `mise exec` still resolve `gh` from the read-only
    // daemon pool via `MISE_SHARED_INSTALL_DIRS`, with the agent's own data
    // dir empty? (spec §4.4 row 2, Spec E §6)
    let ro = |on: bool| {
        let mode = if on { "a-w" } else { "u+w" };
        assert!(
            Command::new("chmod")
                .args(["-R", mode, &layout.mise_data_dir().display().to_string()])
                .status()
                .unwrap()
                .success()
        );
    };
    ro(true);
    let out = Command::new(&tools.mise)
        .args(["exec", "--", "gh", "--version"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.home)
        .envs(&mise_env(&id, &paths, &layout))
        .current_dir("/")
        .output()
        .unwrap();
    ro(false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "mise exec failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains(&gh_version), "resolved {stdout}");
    assert!(paths.logs.join("mise.toolchain.log").exists());
}

// The brief for this test names `jq` (fleet) and `fd` (crew) as the seeded
// tools, reasoning that both are pinned in this repo's own `mise.toml`.
// Neither actually is: `mise.toml` pins `gitleaks`, `tmux` and `gh`, and only
// those three resolve with `repo_pin` and have a host install to seed from in
// both a dev checkout and CI. `gitleaks` stands in for the fleet pool's tool,
// `tmux` for the crew pool's, and `gh` for the daemon pool's, as the brief
// already has it.
#[test]
fn each_level_installs_into_its_own_pool_and_the_agent_resolves_them_all() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise", false));
        return;
    };
    let root = support::temp_root("pools");
    // the agent's own install runs under nono
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew_ref = id.crew_ref();
    let fleet_paths = layout.fleet(&id.fleet);
    let crew_paths = layout.crew(&crew_ref);
    let paths = layout.agent(&id);
    // `$HOME` for the resolution below, as `write_home` makes it
    std::fs::create_dir_all(&paths.home).unwrap();
    std::fs::create_dir_all(crew_paths.root.join("logs")).unwrap();

    // Seed one tool per shared level from the host so nothing downloads.
    let gitleaks = repo_pin("gitleaks");
    let tmux = repo_pin("tmux");
    let gh = repo_pin("gh");
    let seeded = seed_into(&tools, &layout.mise_data_dir(), "gh", &gh)
        && seed_into(&tools, &fleet_paths.mise_pool(), "gitleaks", &gitleaks)
        && seed_into(&tools, &crew_paths.mise_pool(), "tmux", &tmux);
    if !support::require_or_skip(
        "host installs of gh, gitleaks and tmux to seed from",
        seeded,
    ) {
        return;
    }

    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
        policy: SocketPolicy::Open,
        env: &InstallEnv::default(),
    };
    let log = crew_paths.root.join("logs").join("mise.pools.log");
    let daemon_pool = layout.mise_data_dir();
    let fleet_tools = BTreeMap::from([("gitleaks".to_string(), gitleaks.clone())]);
    // The crew also names the fleet's tool, at the fleet's own version: this
    // is what actually exercises "never re-download what an outer pool
    // holds" (Spec E §5). Declaring only the crew's own tool here, as the
    // brief's literal listing does, would make the "crew pool doesn't hold
    // the fleet's tool" assertion below true no matter what `install_level`
    // does — the crew's table would simply never have named it. `gitleaks`
    // was seeded only into the fleet pool, never the crew pool, so this only
    // passes without touching the network if the crew's `mise install`
    // actually resolves it through `MISE_SHARED_INSTALL_DIRS` instead of
    // installing its own copy.
    let crew_tools = BTreeMap::from([
        ("tmux".to_string(), tmux.clone()),
        ("gitleaks".to_string(), gitleaks.clone()),
    ]);

    let crew_id = crew_ref.to_string();
    tc.install_level(
        &crew_id,
        "fleet f",
        &fleet_paths.mise_toml,
        &fleet_paths.mise_pool(),
        std::slice::from_ref(&daemon_pool),
        &fleet_paths.installed_marker(),
        &fleet_tools,
        &log,
        None,
    )
    .unwrap();
    tc.install_level(
        &crew_id,
        "crew f/c",
        &crew_paths.mise_toml(),
        &crew_paths.mise_pool(),
        &[fleet_paths.mise_pool(), daemon_pool.clone()],
        &crew_paths.installed_marker(),
        &crew_tools,
        &log,
        None,
    )
    .unwrap();

    // The agent's table names all three; only what no pool holds is private.
    let agent_tools = BTreeMap::from([
        ("gitleaks".to_string(), gitleaks.clone()),
        ("tmux".to_string(), tmux.clone()),
        ("gh".to_string(), gh.clone()),
    ]);
    tc.write(&id, &paths, &BTreeMap::new(), &agent_tools, false)
        .unwrap();
    tc.install(&id, &paths).unwrap();

    let holds = |pool: &std::path::Path, tool: &str| pool.join("installs").join(tool).exists();
    assert!(holds(&fleet_paths.mise_pool(), "gitleaks"));
    assert!(
        !holds(&fleet_paths.mise_pool(), "tmux"),
        "crew tools stay out of the fleet pool"
    );
    assert!(holds(&crew_paths.mise_pool(), "tmux"));
    assert!(
        !holds(&crew_paths.mise_pool(), "gitleaks"),
        "the fleet pool already had it: the crew's own install must resolve \
         it through the pool chain instead of downloading a second copy"
    );
    assert!(
        !paths.mise_data_dir().join("installs").exists()
            || std::fs::read_dir(paths.mise_data_dir().join("installs"))
                .unwrap()
                .next()
                .is_none(),
        "every tool came from a pool, so nothing landed privately"
    );

    // Each tool resolves, and from the pool that owns it.
    for (tool, pool) in [
        ("gitleaks", fleet_paths.mise_pool()),
        ("tmux", crew_paths.mise_pool()),
        ("gh", daemon_pool.clone()),
    ] {
        let out = Command::new(&tools.mise)
            .args(["which", tool])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &paths.home)
            .envs(&mise_env(&id, &paths, &layout))
            .current_dir("/")
            .output()
            .unwrap();
        let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            out.status.success(),
            "mise which {tool}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            resolved.starts_with(&pool.display().to_string()),
            "{tool} resolved to {resolved}, expected it under {}",
            pool.display()
        );
    }

    // The marker makes a second pass a no-op: rather than compare the
    // marker's content (its sha256 of the unchanged table is byte-identical
    // whether the install ran again or was skipped, so that comparison
    // can never fail), count the `mise install` invocations the pool log
    // recorded before and after, and assert the count did not rise.
    let installs = |log: &std::path::Path| {
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("$ nono ") && l.ends_with(" install"))
            .count()
    };
    let before = installs(&log);
    tc.install_level(
        &crew_id,
        "fleet f",
        &fleet_paths.mise_toml,
        &fleet_paths.mise_pool(),
        &[daemon_pool],
        &fleet_paths.installed_marker(),
        &fleet_tools,
        &log,
        None,
    )
    .unwrap();
    assert_eq!(
        installs(&log),
        before,
        "the marker matched: no second install"
    );
}

#[test]
fn a_crew_pins_its_own_version_without_disturbing_the_fleets() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise", false));
        return;
    };
    let root = support::temp_root("pools-versions");
    // the agent's own install runs under nono
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let fleet_paths = layout.fleet(&id.fleet);
    let crew_paths = layout.crew(&id.crew_ref());
    std::fs::create_dir_all(crew_paths.root.join("logs")).unwrap();

    // Two versions of one tool: the host's pin, and a second real version
    // taken from `mise ls-remote` so nothing is invented.
    let gitleaks = repo_pin("gitleaks");
    let out = Command::new(&tools.mise)
        .args(["ls-remote", "gitleaks"])
        .output()
        .unwrap();
    let other = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .rfind(|v| !v.is_empty() && *v != gitleaks)
        .map(str::to_string);
    let Some(other) = other.filter(|_| out.status.success()) else {
        assert!(!support::require_or_skip(
            "a second gitleaks version from mise ls-remote",
            false
        ));
        return;
    };
    if !support::require_or_skip(
        "a host gitleaks install to seed from",
        seed_into(&tools, &fleet_paths.mise_pool(), "gitleaks", &gitleaks),
    ) {
        return;
    }
    // The crew's version is seeded too: this test is about placement, not
    // downloading. Reuse the same payload under the other version's name.
    let src = fleet_paths
        .mise_pool()
        .join("installs")
        .join("gitleaks")
        .join(&gitleaks);
    let dst = crew_paths
        .mise_pool()
        .join("installs")
        .join("gitleaks")
        .join(&other);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    assert!(
        Command::new("cp")
            .args(["-r", &src.display().to_string(), &dst.display().to_string()])
            .status()
            .unwrap()
            .success()
    );

    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
        policy: SocketPolicy::Open,
        env: &InstallEnv::default(),
    };
    let log = crew_paths.root.join("logs").join("mise.pools.log");
    tc.install_level(
        &id.crew_ref().to_string(),
        "crew f/c",
        &crew_paths.mise_toml(),
        &crew_paths.mise_pool(),
        &[fleet_paths.mise_pool(), layout.mise_data_dir()],
        &crew_paths.installed_marker(),
        &BTreeMap::from([("gitleaks".to_string(), other.clone())]),
        &log,
        None,
    )
    .unwrap();

    assert!(
        fleet_paths
            .mise_pool()
            .join("installs")
            .join("gitleaks")
            .join(&gitleaks)
            .exists(),
        "the fleet's version survives a crew that pins another"
    );
    // Exact contents, not just "the other version is present": the fleet's
    // version was never seeded into the crew pool and TOML cannot hold two
    // values for one key, so a negative existence check here can't fail
    // regardless of the pool-chain logic. Assert the crew pool's `gitleaks`
    // directory holds exactly one version, and it is the crew's.
    // mise also drops alias symlinks alongside the real version directory
    // (`latest`, the major `8`, the minor `8.30`, all pointing at the exact
    // version); only real directories count as an installed version here.
    let mut crew_gitleaks_versions: Vec<String> =
        std::fs::read_dir(crew_paths.mise_pool().join("installs").join("gitleaks"))
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().unwrap().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
    crew_gitleaks_versions.sort();
    assert_eq!(
        crew_gitleaks_versions,
        vec![other.clone()],
        "the crew pool holds only what the crew declared"
    );

    // Spec E §7: "the agent resolves the crew's" version. The agent's own
    // table pins `gitleaks` at the crew's version; only the crew pool holds
    // that exact version, so this proves the agent's pool chain actually
    // reaches the crew pool, not just that `install_level` placed files
    // correctly.
    let paths = layout.agent(&id);
    // `$HOME` for the resolution below, as `write_home` makes it
    std::fs::create_dir_all(&paths.home).unwrap();
    let agent_tools = BTreeMap::from([("gitleaks".to_string(), other.clone())]);
    tc.write(&id, &paths, &BTreeMap::new(), &agent_tools, false)
        .unwrap();
    tc.install(&id, &paths).unwrap();

    assert!(
        !paths.mise_data_dir().join("installs").exists()
            || std::fs::read_dir(paths.mise_data_dir().join("installs"))
                .unwrap()
                .next()
                .is_none(),
        "the crew pool already held the pinned version, so nothing landed privately"
    );

    let out = Command::new(&tools.mise)
        .args(["which", "gitleaks"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.home)
        .envs(&mise_env(&id, &paths, &layout))
        .current_dir("/")
        .output()
        .unwrap();
    let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        out.status.success(),
        "mise which gitleaks: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        resolved.starts_with(&crew_paths.mise_pool().display().to_string()),
        "gitleaks resolved to {resolved}, expected the crew's pin under {}",
        crew_paths.mise_pool().display()
    );
}

/// Every file under `root`, relative to it and sorted; empty when `root`
/// does not exist. Used to prove a pool gained nothing.
fn pool_contents(root: &std::path::Path) -> Vec<String> {
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                out.push(path.strip_prefix(base).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

// Spec E §10's second acceptance criterion: "`mise install` in the agent's
// worktree installs a repo-declared tool into its own data dir
// without touching any pool." Nothing else in the branch exercises this —
// the ceiling move and the private data dir are otherwise only checked as
// an env string (env.rs) and the golden snapshot — so this is the only
// test that would catch a regression in the
// `MISE_CEILING_PATHS`/`MISE_AUTO_INSTALL`/private-data-dir triple.
//
// The install is a real cache miss (#22): the worktree declares a tool
// through mise's `http` backend at a tarball the test builds and serves
// from a loopback listener, so `mise install` has to fetch and unpack it
// with no outside network, and the private dir is asserted empty
// beforehand. Not `file://`: CI's mise (2026.9.2) refuses that scheme.
#[test]
fn mise_install_in_the_worktree_lands_privately_and_leaves_every_pool_alone() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise", false));
        return;
    };
    let root = support::temp_root("pools-worktree");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let fleet_paths = layout.fleet(&id.fleet);
    let crew_paths = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    std::fs::create_dir_all(&paths.workspace).unwrap();

    let (url, hits) = support::serve_tarball(support::probe_tarball(&root));

    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
        policy: SocketPolicy::Open,
        env: &InstallEnv::default(),
    };
    tc.write(&id, &paths, &BTreeMap::new(), &BTreeMap::new(), false)
        .unwrap();
    // The agent's generated global table names nothing, so the only way
    // the probe can install or resolve is through the worktree's own
    // `mise.toml`, discovered via `MISE_CEILING_PATHS`.
    std::fs::write(
        paths.workspace.join("mise.toml"),
        format!("[tools]\n\"http:balerix-probe\" = {{ version = \"1.0.0\", url = \"{url}\" }}\n"),
    )
    .unwrap();

    let private = paths.mise_data_dir();
    assert_eq!(
        pool_contents(&private),
        Vec::<String>::new(),
        "the private dir starts empty, so the install below is a cache miss"
    );
    let daemon_pool = layout.mise_data_dir();
    let before = (
        pool_contents(&daemon_pool),
        pool_contents(&fleet_paths.mise_pool()),
        pool_contents(&crew_paths.mise_pool()),
    );

    // The same environment the sandbox would carry into the worktree
    // (Spec E §6): private `MISE_DATA_DIR`, the pool chain as a read-only
    // fallback, and the ceiling one level above the worktree.
    let env = agent_env(
        &id,
        &paths,
        &layout,
        "http://127.0.0.1:0",
        "s",
        &BTreeMap::new(),
    );
    let run = |args: &[&str]| {
        Command::new(&tools.mise)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &paths.home)
            .envs(&env)
            .current_dir(&paths.workspace)
            .output()
            .unwrap()
    };

    // Spec §6: mise honours an untrusted config's `[tools]` for `mise
    // install`, so no `mise trust` of the worktree file precedes this.
    let out = run(&["install"]);
    assert!(
        out.status.success(),
        "mise install: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        hits.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "mise fetched the probe from the loopback server"
    );
    let installed = pool_contents(&private.join("installs"));
    assert!(
        installed
            .iter()
            .any(|p| p.starts_with("http-balerix-probe/1.0.0/")),
        "the probe landed under the agent's private data dir: {installed:?}"
    );

    // Resolution, not just presence: no pool holds the probe, so this
    // only succeeds if `mise which` discovered the worktree's own
    // `mise.toml` and found the install in the private dir.
    let out = run(&["which", "balerix-probe"]);
    assert!(
        out.status.success(),
        "mise which balerix-probe: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        resolved.starts_with(&private.display().to_string()),
        "balerix-probe resolved to {resolved}, expected the private dir"
    );
    let out = Command::new(&resolved).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "balerix-probe\n");

    let after = (
        pool_contents(&daemon_pool),
        pool_contents(&fleet_paths.mise_pool()),
        pool_contents(&crew_paths.mise_pool()),
    );
    assert_eq!(after, before, "no pool gained anything");
}

/// The agent can write inside its four mise directories, and the daemon
/// re-installs whenever the agent's table changes. Links left there must
/// carry nothing outside the agent: the daemon makes `installs/` again
/// without following a link, and below it the install's sandbox refuses
/// any write that resolves outside the four directories, a pool the
/// agent reads included. Each case replaces a directory with a link,
/// makes the probe uninstalled again, and installs; the link's target
/// gains nothing either way.
#[test]
fn a_reinstall_writes_nothing_through_a_link_in_the_agents_mise_dirs() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+nono", false));
        return;
    };
    let root = support::temp_root("install-links");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    // `$HOME` for the resolution below, as `write_home` makes it
    std::fs::create_dir_all(&paths.home).unwrap();
    let (url, hits) = support::serve_tarball(support::probe_tarball(&root));
    // written by hand: the generated table carries versions only, and the
    // probe needs its URL
    std::fs::write(
        &paths.mise_toml,
        format!("[tools]\n\"http:balerix-probe\" = {{ version = \"1.0.0\", url = \"{url}\" }}\n"),
    )
    .unwrap();
    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
        policy: SocketPolicy::Open,
        env: &InstallEnv::default(),
    };
    tc.install(&id, &paths).unwrap();
    let installs = paths.mise_data_dir().join("installs");
    let tool_dir = installs.join("http-balerix-probe");
    // mise's http backend drops the archive's single top directory
    let probe = tool_dir.join("1.0.0").join("balerix-probe");
    assert!(
        probe.is_file(),
        "a first install lands privately: {:?}",
        pool_contents(&installs)
    );
    let target = |n: usize| {
        let outside = root.join(format!("outside-{n}"));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), "left alone").unwrap();
        outside
    };
    // whatever was there goes, so the probe is not installed any more
    let plant = |at: &std::path::Path, to: &std::path::Path| {
        std::fs::remove_dir_all(at).unwrap();
        std::os::unix::fs::symlink(to, at).unwrap();
    };
    let untouched = |dir: &std::path::Path| {
        assert_eq!(
            pool_contents(dir),
            vec!["sentinel".to_string()],
            "{} gained nothing",
            dir.display()
        );
    };

    // `installs/` is made real again before mise runs, and the install
    // goes on into it
    let before = hits.load(std::sync::atomic::Ordering::SeqCst);
    let outside = target(0);
    plant(&installs, &outside);
    tc.install(&id, &paths).unwrap();
    untouched(&outside);
    assert!(std::fs::symlink_metadata(&installs).unwrap().is_dir());
    assert!(probe.is_file());
    assert!(
        hits.load(std::sync::atomic::Ordering::SeqCst) > before,
        "the probe was installed again, not found through the link"
    );

    // below it nothing is made in advance: the sandbox refuses the writes,
    // and the install fails; the crew's pool, which the install reads, is
    // no more writable through a link than any other directory
    let crew_tool = layout
        .crew(&id.crew_ref())
        .mise_pool()
        .join("installs")
        .join("http-balerix-probe");
    std::fs::create_dir_all(&crew_tool).unwrap();
    std::fs::write(crew_tool.join("sentinel"), "left alone").unwrap();
    for (at, to) in [
        (tool_dir.clone(), target(1)),
        (paths.mise_state_dir().join("trusted-configs"), target(2)),
        (tool_dir.clone(), crew_tool.clone()),
    ] {
        plant(&at, &to);
        let err = tc.install(&id, &paths).unwrap_err();
        untouched(&to);
        let shown = err.to_string();
        assert!(
            shown.starts_with("f/c/a: mise "),
            "reported as mise's: {shown}"
        );
        assert!(
            !shown.contains("the sandbox did not start"),
            "the sandbox started; mise was refused: {shown}"
        );
        assert!(
            std::fs::symlink_metadata(&at).unwrap().is_symlink(),
            "{} is left as the agent made it",
            at.display()
        );
        std::fs::remove_file(&at).unwrap();
        std::fs::create_dir(&at).unwrap();
    }
    tc.install(&id, &paths).unwrap();
    let scratch: Vec<_> = std::fs::read_dir(&paths.root)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.starts_with(".nono-mise-").then_some(name)
        })
        .collect();
    assert!(scratch.is_empty(), "every call's scratch goes: {scratch:?}");
}

/// The agent's install profile, run by nono: what the install can reach
/// does not include the agent's `home/`, where its credentials and hook
/// secret are, while its mise directories are there to write.
#[test]
fn the_agent_install_sandbox_cannot_read_the_agents_home() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+nono", false));
        return;
    };
    let root = support::temp_root("install-home");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let secrets = [
        paths.claude_dir().join(".credentials.json"),
        paths.claude_dir().join("settings.json"),
        paths.gh_dir().join("hosts.yml"),
    ];
    for f in &secrets {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "secret").unwrap();
    }
    for d in paths.mise_dirs() {
        std::fs::create_dir_all(d).unwrap();
    }
    let scratch = root.join("scratch");
    let home = scratch.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let profile = scratch.join("profile.json");
    let call = agent_install_call(&id, &paths, &layout, &tools.mise);
    std::fs::write(
        &profile,
        serde_json::to_vec(&call.profile(&home, &InstallEnv::default())).unwrap(),
    )
    .unwrap();
    let sh = |script: &str| {
        Command::new(&tools.nono)
            .args(["-s", "run", "--no-audit", "--profile"])
            .arg(&profile)
            .args(["--", "/bin/sh", "-c", script])
            .env_clear()
            .env("HOME", &scratch)
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/")
            .output()
            .unwrap()
    };
    for f in &secrets {
        let out = sh(&format!("cat '{}'", f.display()));
        assert!(
            !out.status.success() && !String::from_utf8_lossy(&out.stdout).contains("secret"),
            "{} was readable",
            f.display()
        );
    }
    let out = sh(&format!(
        "echo x > '{0}/probe' && cat '{0}/probe'",
        paths.mise_data_dir().display()
    ));
    assert!(
        out.status.success(),
        "the data dir is writable: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A pool is written by more than the daemon's own install (a plugin's
/// writes the daemon pool), so the pool install runs sandboxed too: a link
/// planted in the pool leads nowhere, and its target gains nothing.
#[test]
fn a_pool_install_writes_nothing_through_a_link_in_its_pool() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+nono", false));
        return;
    };
    let root = support::temp_root("pool-links");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let pool = layout.mise_data_dir();
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), "left alone").unwrap();
    std::fs::create_dir_all(&pool).unwrap();
    // `level_env` keeps mise's state in the pool: `mise trust` writes there
    std::os::unix::fs::symlink(&outside, pool.join("state")).unwrap();
    let marker = layout.system_installed_marker();
    let err = Toolchain {
        tools: &tools,
        layout: &layout,
        policy: SocketPolicy::Open,
        env: &InstallEnv::default(),
    }
    .install_level(
        "system",
        "system",
        &layout.system_mise_toml_generated(),
        &pool,
        &[],
        &marker,
        &BTreeMap::new(),
        &layout.server_dir().join("logs").join("mise.system.log"),
        None,
    )
    .unwrap_err();
    assert_eq!(pool_contents(&outside), ["sentinel"], "{err}");
    assert!(err.to_string().starts_with("system: mise trust"), "{err}");
    assert!(!marker.exists());
}

/// Every name in `dir` with the target of each link, sorted: a pool
/// tool directory's runtime links, for comparing before and after.
fn links_in(dir: &std::path::Path) -> Vec<(String, Option<std::path::PathBuf>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read_link(e.path()).ok(),
            )
        })
        .collect();
    out.sort();
    out
}

/// mise rewrites a tool's runtime links in every install directory it
/// resolves through, a pool included (from mise 2026.10 a `global` link
/// that follows the installing table, which an agent's table rarely
/// matches). The agent's install cannot write a pool: those writes are
/// refused, the pool's links stay as the pool's own install made them,
/// and the install still succeeds, since every tool it names is there.
#[test]
fn an_install_leaves_the_pools_runtime_links_alone() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise+nono", false));
        return;
    };
    let root = support::temp_root("install-pool-links");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let gh = embedded_system_tools()["gh"].clone();
    if !support::require_or_skip(
        "gh install to seed from",
        seed_into(&tools, &layout.mise_data_dir(), "gh", &gh),
    ) {
        return;
    }
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    // `$HOME` for the resolution below, as `write_home` makes it
    std::fs::create_dir_all(&paths.home).unwrap();
    let (url, _) = support::serve_tarball(support::probe_tarball(&root));
    // gh is not in the agent's table: the pool's `global` link for it is
    // one the agent's install would remove
    std::fs::write(
        &paths.mise_toml,
        format!("[tools]\n\"http:balerix-probe\" = {{ version = \"1.0.0\", url = \"{url}\" }}\n"),
    )
    .unwrap();
    let pool_gh = layout.mise_data_dir().join("installs").join("gh");
    let before = links_in(&pool_gh);
    Toolchain {
        tools: &tools,
        layout: &layout,
        policy: SocketPolicy::Open,
        env: &InstallEnv::default(),
    }
    .install(&id, &paths)
    .unwrap();
    assert_eq!(links_in(&pool_gh), before, "the pool is as it was");
    assert!(
        paths
            .mise_data_dir()
            .join("installs/http-balerix-probe/1.0.0/balerix-probe")
            .is_file()
    );
}
