#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use balerix_api::{AgentSettings, CredentialBundle, CrewSpec, FleetSpec, GitAuth, GitSettings};
use balerix_core::{CrewRef, CrewTools, Fleet, HookTarget, Keep, Materializer, ResolvedAgent};
use balerix_runtime::{Runtime, embedded_system_tools};

fn no_tools() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        // A pre-commit hook (this crate's own, if the test suite runs from
        // one) leaks these into the environment for a linked worktree; left
        // in place they would make this fixture's `-C`-less git calls
        // operate on the real repository instead of `dir`.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn fleet(repo_url: &str) -> Fleet {
    let mut s = AgentSettings::default();
    s.claude.binary = "/bin/sh".into();
    s.tools = BTreeMap::new();
    Fleet::try_from(FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: repo_url.into(),
                git_ref: "main".into(),
                git: GitSettings {
                    push: false,
                    auth: GitAuth::None,
                    ..GitSettings::default()
                },
                agents: BTreeMap::from([("a".to_string(), s)]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn materialize_then_remove_round_trip() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize");
    let layout = support::layout(&root);
    // system table: only gh, seeded from the host so mise install is offline
    let gh_version = embedded_system_tools()["gh"].clone();
    let out = Command::new(&tools.mise)
        .args(["where", &format!("gh@{gh_version}")])
        .output()
        .unwrap();
    if !support::require_or_skip("gh install to seed from", out.status.success()) {
        return;
    }
    let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dst = layout
        .mise_data_dir()
        .join("installs")
        .join("gh")
        .join(&gh_version);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    assert!(
        Command::new("cp")
            .args(["-r", &src, &dst.display().to_string()])
            .status()
            .unwrap()
            .success()
    );
    std::fs::create_dir_all(&layout.config_root).unwrap();
    std::fs::write(
        layout.system_mise_toml(),
        format!("[tools]\ngh = \"{gh_version}\"\n"),
    )
    .unwrap();

    let work = root.join("up");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "x").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("up.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );

    let f = fleet(&format!("file://{}", bare.display()));
    let rt = Runtime::new(layout.clone(), tools);
    let agent = ResolvedAgent::from_fleet(&f).remove(0);
    let crew = agent.id.crew_ref();
    let creds = CredentialBundle::default();
    let hooks = HookTarget {
        url: "http://127.0.0.1:7643".into(),
        secret: "s".into(),
    };

    let (f_tools, c_tools) = (no_tools(), no_tools());
    rt.ensure_crew(
        &crew,
        &agent.repo,
        &agent.git_ref,
        &agent.git,
        &creds,
        CrewTools {
            fleet: &f_tools,
            crew: &c_tools,
        },
    )
    .unwrap();
    let plan = rt.materialize(&agent, &creds, &hooks).unwrap();
    let paths = layout.agent(&agent.id);
    assert!(paths.workspace.join("README").exists());
    assert!(paths.launch.exists() && paths.profile.exists() && paths.mise_toml.exists());
    assert!(paths.claude_dir().join("settings.json").exists());
    assert!(
        !paths.claude_dir().join(".credentials.json").exists(),
        "no creds in the bundle → no file"
    );
    assert_eq!(plan.cwd, paths.workspace);

    // Spec L §6: an agent with `branch` works on that remote branch. Push
    // one to the bare repo through the working copy, then resolve a
    // second agent that names it.
    git(&work, &["checkout", "-q", "-b", "feature/issue-12"]);
    std::fs::write(work.join("FEATURE"), "wip\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "feature work"]);
    git(
        &work,
        &[
            "push",
            "-q",
            &bare.display().to_string(),
            "feature/issue-12",
        ],
    );
    let mut pr = agent.clone();
    pr.id = "f/c/pr".parse().unwrap();
    pr.settings.branch = Some("feature/issue-12".into());
    rt.materialize(&pr, &creds, &hooks).unwrap();
    let pr_paths = layout.agent(&pr.id);
    assert_eq!(
        git(&pr_paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feature/issue-12",
        "the worktree is on the remote branch, not on balerix/f/c/pr"
    );
    assert!(pr_paths.workspace.join("FEATURE").exists());
    assert!(
        !paths.workspace.join("FEATURE").exists(),
        "the first agent's worktree is still on its own branch from main"
    );
    // idempotent
    rt.ensure_crew(
        &crew,
        &agent.repo,
        &agent.git_ref,
        &agent.git,
        &creds,
        CrewTools {
            fleet: &f_tools,
            crew: &c_tools,
        },
    )
    .unwrap();
    rt.materialize(&agent, &creds, &hooks).unwrap();
    assert!(paths.installed_marker().exists());
    let mise_log = std::fs::read_to_string(paths.logs.join("mise.toolchain.log")).unwrap();
    assert_eq!(
        mise_log
            .lines()
            .filter(|l| l.starts_with("$ mise install"))
            .count(),
        1,
        "unchanged table → install skipped on the second pass"
    );

    // Exercise remove_crew's per-agent harvest loop while the agent's
    // clone still exists: removing the agent first would empty `agents/`
    // and this loop would never run. An unpushed commit on the assigned
    // branch must reach the cache (Spec N §5).
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    let crew_paths = layout.crew(&crew);
    rt.remove_crew(
        &crew,
        Keep {
            repos: true,
            sessions: false,
        },
    )
    .unwrap();
    assert!(!paths.root.exists());
    assert!(crew_paths.repo.exists());
    assert_eq!(
        git(&crew_paths.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha,
        "harvested before the clone went"
    );

    rt.remove_agent(&agent.id).unwrap(); // already gone: must be a no-op
    rt.remove_crew(&crew, Keep::default()).unwrap();
    assert!(!layout.crew(&crew).root.exists());
}

#[test]
fn gh_auth_without_a_token_is_a_clear_error() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("materialize-noauth");
    let rt = Runtime::new(support::layout(&root), tools);
    let f = fleet("file:///nowhere.git");
    let agent = ResolvedAgent::from_fleet(&f).remove(0);
    let (f_tools, c_tools) = (no_tools(), no_tools());
    let err = rt
        .ensure_crew(
            &agent.id.crew_ref(),
            &agent.repo,
            "main",
            &GitSettings::default(),
            &CredentialBundle::default(),
            CrewTools {
                fleet: &f_tools,
                crew: &c_tools,
            },
        )
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "f/c: git.auth is gh but no gh token was provided (run `gh auth login` on the client)"
    );
}

#[test]
fn ensure_crew_no_longer_writes_the_daemon_pool() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise", false));
        return;
    };
    let root = support::temp_root("no-daemon-pool");
    let layout = support::layout(&root);
    let rt = Runtime::new(layout.clone(), tools);
    let daemon_pool = layout.mise_data_dir();
    std::fs::create_dir_all(&daemon_pool).unwrap();

    let before = pool_snapshot(&daemon_pool);
    let crew: CrewRef = "f/c".parse().unwrap();
    let (f, c) = (BTreeMap::new(), BTreeMap::new());
    let _ = rt.install_pools(
        &crew,
        balerix_core::CrewTools {
            fleet: &f,
            crew: &c,
        },
    );
    assert_eq!(
        pool_snapshot(&daemon_pool),
        before,
        "install_pools must not touch the daemon pool any more (Spec F §3)"
    );
}

/// Every path under `dir`, sorted, so an unchanged pool compares equal.
fn pool_snapshot(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            out.push(p.display().to_string());
            if p.is_dir() {
                stack.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Review focus 4: `down --purge` over a clone git cannot read must
/// succeed — the cache goes too, so there is nothing to harvest into.
#[test]
fn a_purge_deletes_a_broken_clone_without_harvesting() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("materialize-purge-broken");
    let layout = support::layout(&root);
    std::fs::create_dir_all(&layout.config_root).unwrap();
    std::fs::write(layout.system_mise_toml(), "[tools]\n").unwrap();
    let work = root.join("up");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "x").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("up.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let f = fleet(&format!("file://{}", bare.display()));
    let rt = Runtime::new(layout.clone(), tools);
    let agent = ResolvedAgent::from_fleet(&f).remove(0);
    let crew = agent.id.crew_ref();
    let creds = CredentialBundle::default();
    let (f_tools, c_tools) = (no_tools(), no_tools());
    rt.ensure_crew(
        &crew,
        &agent.repo,
        &agent.git_ref,
        &agent.git,
        &creds,
        CrewTools {
            fleet: &f_tools,
            crew: &c_tools,
        },
    )
    .unwrap();
    let paths = layout.agent(&agent.id);
    let ws = balerix_runtime::Workspace {
        tools: &rt.tools,
        gh_config_dir: None,
    };
    ws.ensure_clone(
        &agent.id.to_string(),
        &layout.crew(&crew),
        &paths,
        &agent.repo,
        &agent.branch(),
        agent.start_ref(),
    )
    .unwrap();
    std::fs::remove_file(paths.workspace.join(".git/HEAD")).unwrap();

    // keep-repos: the harvest runs and fails, and says so
    let e = rt
        .remove_crew(
            &crew,
            Keep {
                repos: true,
                sessions: false,
            },
        )
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/a: git "), "{e}");
    assert!(paths.workspace.exists());

    // purge: nothing to harvest into, everything goes
    rt.remove_crew(&crew, Keep::default()).unwrap();
    assert!(!layout.crew(&crew).root.exists());
}
