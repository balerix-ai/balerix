#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::path::Path;
use std::process::Command;

use balerix_core::RepoRef;
use balerix_runtime::Workspace;

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

/// A bare repo with one commit on `main`, served over file://.
fn bare_repo(root: &Path) -> RepoRef {
    let work = root.join("upstream-work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("upstream.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    RepoRef::parse(&format!("file://{}", bare.display())).unwrap()
}

/// Spec N §3–4: the cache is a `--no-checkout` clone with `gc.auto=0`;
/// the agent's workspace is a full clone borrowing objects from it, on
/// the agent's branch, with `origin` the real remote.
#[test]
fn a_private_clone_borrows_from_the_cache_and_pushes_to_origin() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };

    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    assert!(crew.repo.join(".git").is_dir());
    assert!(!crew.repo.join("README").exists(), "--no-checkout");
    assert_eq!(
        git(&crew.repo, &["config", "gc.auto"]).trim(),
        "0",
        "N-2: the cache never gcs on its own"
    );
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap(); // present → no git call
    assert_eq!(
        fetch_lines(&crew).len(),
        0,
        "a second ensure_repo must not fetch"
    );

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(
        fetch_lines(&crew).len(),
        1,
        "creating a branch fetches the cache first"
    );
    assert_no_auto_gc(&crew);
    assert!(
        paths.workspace.join(".git").is_dir(),
        "a clone, not a worktree"
    );
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "balerix/f/c/a"
    );
    let alternates =
        std::fs::read_to_string(paths.workspace.join(".git/objects/info/alternates")).unwrap();
    assert_eq!(
        alternates.trim(),
        crew.cache_objects()
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        "N-1: objects come from the cache"
    );
    assert_eq!(
        git(&paths.workspace, &["remote", "get-url", "origin"]).trim(),
        repo.clone_url(),
        "origin is the real remote, not the cache"
    );
    assert_eq!(
        std::fs::read_to_string(paths.branch_marker()).unwrap(),
        "balerix/f/c/a"
    );

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap(); // idempotent
    assert_eq!(
        fetch_lines(&crew).len(),
        1,
        "an existing clone costs no fetch"
    );

    // a push from the clone reaches origin
    std::fs::write(paths.workspace.join("work.txt"), "pushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    git(&paths.workspace, &["push", "-q", "origin", "balerix/f/c/a"]);
    let bare = root.join("upstream.git");
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/balerix/f/c/a"]), sha);

    // a second agent gets its own clone and branch from origin/main
    let b = layout.agent(&"f/c/b".parse().unwrap());
    ws.ensure_clone("f/c/b", &crew, &b, &repo, "balerix/f/c/b", "main")
        .unwrap();
    assert!(!b.workspace.join("work.txt").exists());
    assert!(b.workspace.join(".git/objects/info/alternates").exists());
}

#[test]
fn errors_name_the_id_tool_and_first_stderr_line() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-err");
    let layout = support::layout(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    let err = ws
        .ensure_repo(
            "f/c",
            &crew,
            &RepoRef::parse("file:///nonexistent/repo.git").unwrap(),
            "main",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("f/c: git clone: "), "{msg}");
    assert!(
        msg.contains("fatal") || msg.contains("does not exist") || msg.contains("not found"),
        "{msg}"
    );
}

/// Pushes a second commit on `branch` to the bare repo `bare_repo` made,
/// through the upstream working copy.
fn push_branch(root: &Path, branch: &str) -> String {
    let work = root.join("upstream-work");
    git(&work, &["checkout", "-q", "-b", branch]);
    std::fs::write(work.join("FEATURE"), "wip\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "feature work"]);
    let bare = root.join("upstream.git").display().to_string();
    git(&work, &["push", "-q", &bare, branch]);
    git(&work, &["checkout", "-q", "main"]);
    git(&work, &["rev-parse", branch]).trim().to_string()
}

/// The `$ git …` lines of the crew's git.log that run a `fetch`.
fn fetch_lines(crew: &balerix_runtime::CrewPaths) -> Vec<String> {
    std::fs::read_to_string(crew.root.join("logs").join("git.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("$ git") && l.contains(" fetch "))
        .map(str::to_string)
        .collect()
}

/// Spec N-2: every fetch the daemon runs into the cache must refuse to gc.
fn assert_no_auto_gc(crew: &balerix_runtime::CrewPaths) {
    for l in fetch_lines(crew) {
        assert!(
            l.contains("--no-auto-gc"),
            "a daemon fetch without --no-auto-gc: {l}"
        );
    }
}

/// Spec L §6 over the clone: an agent with `branch` works on that remote
/// branch — created from `origin/<branch>`, tracking it, reused across
/// passes — never on a fresh `balerix/…` branch.
#[test]
fn a_clone_on_an_existing_remote_branch_is_created_from_it_and_reused() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-branch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let tip = push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/pr".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();

    ws.ensure_clone(
        "f/c/pr",
        &crew,
        &paths,
        &repo,
        "feature/issue-12",
        "feature/issue-12",
    )
    .unwrap();
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feature/issue-12"
    );
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(paths.workspace.join("FEATURE").exists());
    assert_eq!(
        git(
            &paths.workspace,
            &["rev-parse", "--abbrev-ref", "feature/issue-12@{upstream}"]
        )
        .trim(),
        "origin/feature/issue-12",
        "a push from the clone reaches the PR's branch"
    );
    ws.ensure_clone(
        "f/c/pr",
        &crew,
        &paths,
        &repo,
        "feature/issue-12",
        "feature/issue-12",
    )
    .unwrap();
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "HEAD"]).trim(),
        tip,
        "reused"
    );
}

/// Spec L §6 (F1) over the clone: adding, changing or removing `branch`
/// on a live agent re-creates its clean clone on the new branch; the old
/// branch is harvested into the cache (Spec N §5) and seeds the clone
/// when the setting comes back.
#[test]
fn a_changed_branch_moves_a_clean_clone_and_keeps_the_old_branch() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-rebranch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let tip = push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let head = || {
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    };

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "balerix/f/c/a");
    std::fs::write(paths.workspace.join("work.txt"), "committed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);

    // `branch` added: the old branch is harvested, the clone re-created
    ws.ensure_clone(
        "f/c/a",
        &crew,
        &paths,
        &repo,
        "feature/issue-12",
        "feature/issue-12",
    )
    .unwrap();
    assert_eq!(head(), "feature/issue-12");
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(!paths.workspace.join("work.txt").exists());
    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha,
        "N-4: the old branch lives on in the cache"
    );
    assert_no_auto_gc(&crew);

    // `branch` removed: back on the per-agent branch, seeded from the cache
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "balerix/f/c/a");
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha);
    assert!(paths.workspace.join("work.txt").exists());
    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/feature/issue-12"]).trim(),
        tip,
        "the branch the clone left is harvested too"
    );
}

/// F1: a dirty clone on the old branch fails the materialize step, naming
/// both branches, and is left exactly as it was.
#[test]
fn a_changed_branch_on_a_dirty_clone_fails_and_keeps_the_tree() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-rebranch-dirty");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    std::fs::write(paths.workspace.join("README"), "edited\n").unwrap();

    let e = ws
        .ensure_clone(
            "f/c/a",
            &crew,
            &paths,
            &repo,
            "feature/issue-12",
            "feature/issue-12",
        )
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/a: "), "{e}");
    assert!(e.contains("\"balerix/f/c/a\""), "{e}");
    assert!(e.contains("\"feature/issue-12\""), "{e}");
    assert!(e.contains("local changes"), "{e}");
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "balerix/f/c/a"
    );
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "edited\n"
    );
    assert!(
        git(&crew.repo, &["branch", "--list", "balerix/f/c/a"])
            .trim()
            .is_empty(),
        "nothing was harvested from a clone that stays"
    );
}

/// Review focus 1: a `branch` the remote does not have fails the
/// materialize step with git's message, and the half-made clone is
/// removed — a `--no-checkout` clone left behind would read as an
/// existing, dirty clone on the next pass.
#[test]
fn a_missing_remote_branch_fails_the_clone_and_leaves_nothing() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-nobranch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/pr".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let e = ws
        .ensure_clone("f/c/pr", &crew, &paths, &repo, "nope", "nope")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/pr: git checkout:"), "{e}");
    assert!(e.contains("origin/nope"), "{e}");
    assert!(!paths.workspace.exists(), "the half-made clone is gone");
    assert!(!paths.branch_marker().exists());
    assert!(
        git(&crew.repo, &["branch", "--list", "nope"])
            .trim()
            .is_empty(),
        "no branch was created in the cache"
    );
    // the next pass fails the same way, not with "local changes"
    let again = ws
        .ensure_clone("f/c/pr", &crew, &paths, &repo, "nope", "nope")
        .unwrap_err()
        .to_string();
    assert!(again.starts_with("f/c/pr: git checkout:"), "{again}");
}

/// `check_branch_name` is a model of `git check-ref-format --branch`;
/// this is the only place the model meets the real thing.
#[test]
fn check_branch_name_agrees_with_git_check_ref_format() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    for name in [
        "main",
        "feature/x",
        "release-1.2",
        "a.b/c-d_e",
        "pr/12/head",
        "x@y",
        "issue#12",
        "a/b.lockfile",
        "v1.0",
        "",
        "/main",
        "main/",
        "main.",
        "a//b",
        "a..b",
        "a@{1}",
        "a b",
        "a~1",
        "a^b",
        "a:b",
        "a?b",
        "a*b",
        "a[b",
        "a\\b",
        ".hidden",
        "a/.b",
        "a.lock",
        "a.lock/b",
    ] {
        let git_ok = Command::new(&tools.git)
            .args(["check-ref-format", "--branch", name])
            .output()
            .unwrap()
            .status
            .success();
        assert_eq!(
            balerix_api::check_branch_name(name).is_ok(),
            git_ok,
            "{name:?}: git says {git_ok}"
        );
    }

    // Documented divergences (Spec L §6, plan §12): `git check-ref-format
    // --branch <name>` always validates `refs/heads/<name>`, so it accepts
    // both of these — a `name` starting with `refs/` becomes a harmless
    // double-prefixed ref, and a bare `@` is never the *whole* checked
    // refname, so the HEAD-alias rejection never fires. We refuse both
    // anyway: unprefixed elsewhere (`origin/<branch>`, revision syntax) they
    // would be read as a fully-qualified ref or as HEAD.
    assert!(
        Command::new(&tools.git)
            .args(["check-ref-format", "--branch", "refs/heads/main"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(balerix_api::check_branch_name("refs/heads/main").is_err());
    assert!(
        Command::new(&tools.git)
            .args(["check-ref-format", "--branch", "@"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(balerix_api::check_branch_name("@").is_err());
}

/// #60 over the clone: a restart re-materializes with the *same* setting.
/// An agent that checked out a branch of its own is neither moved back
/// nor failed for it, whether its tree is clean or dirty.
#[test]
fn an_agent_switching_branches_itself_is_left_alone_on_an_unchanged_setting() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-self-switch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let head = || {
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    };

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    git(&paths.workspace, &["checkout", "-q", "-b", "my-fix"]);

    // clean tree: not moved back
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "my-fix");

    // dirty tree: not failed, and the change is kept
    std::fs::write(paths.workspace.join("README"), "edited\n").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "my-fix");
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "edited\n"
    );
}

/// A clone from before the marker existed is judged by its HEAD once
/// and the marker is written then; from there on HEAD no longer counts.
#[test]
fn a_clone_without_a_marker_is_judged_by_head_once_then_recorded() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-no-marker");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let head = || {
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    };
    let ensure = |branch: &str| ws.ensure_clone("f/c/a", &crew, &paths, &repo, branch, "main");

    ensure("balerix/f/c/a").unwrap();
    std::fs::remove_file(paths.branch_marker()).unwrap();
    git(&paths.workspace, &["checkout", "-q", "-b", "my-fix"]);

    // no marker: HEAD decides, the clone is re-created, the marker appears
    ensure("balerix/f/c/a").unwrap();
    assert_eq!(head(), "balerix/f/c/a");
    assert_eq!(
        std::fs::read_to_string(paths.branch_marker()).unwrap(),
        "balerix/f/c/a"
    );
    assert!(
        git(&crew.repo, &["rev-parse", "--verify", "refs/heads/my-fix"])
            .trim()
            .len()
            == 40,
        "the branch HEAD was on is what got harvested"
    );

    // with the marker: the same self-switch is left alone
    git(&paths.workspace, &["checkout", "-q", "-b", "my-fix"]);
    ensure("balerix/f/c/a").unwrap();
    assert_eq!(head(), "my-fix");
}

/// The agent checked out the very branch the operator then configures:
/// nothing to move, even with local changes, and the record follows.
#[test]
fn a_changed_branch_the_clone_already_sits_on_is_recorded_without_a_move() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-already-there");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let head = || {
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    };
    let ensure = |branch: &str, start_ref: &str| {
        ws.ensure_clone("f/c/a", &crew, &paths, &repo, branch, start_ref)
    };

    ensure("balerix/f/c/a", "main").unwrap();
    git(
        &paths.workspace,
        &["checkout", "-q", "-b", "feature/issue-12"],
    );
    std::fs::write(paths.workspace.join("README"), "edited\n").unwrap();

    ensure("feature/issue-12", "feature/issue-12").unwrap();
    assert_eq!(head(), "feature/issue-12");
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(paths.branch_marker()).unwrap(),
        "feature/issue-12"
    );

    // recorded: a later self-switch is left alone under this setting too
    git(&paths.workspace, &["stash", "-q"]);
    git(&paths.workspace, &["checkout", "-q", "-b", "my-fix"]);
    ensure("feature/issue-12", "feature/issue-12").unwrap();
    assert_eq!(head(), "my-fix");
}

/// N-7 (#63): two agents on one `branch` both materialize; with private
/// clones there is no checkout to collide on.
#[test]
fn two_agents_on_one_branch_both_materialize() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-shared-branch");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let tip = push_branch(&root, "feature/issue-12");
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    for name in ["a", "b"] {
        let id: balerix_core::AgentId = format!("f/c/{name}").parse().unwrap();
        let paths = layout.agent(&id);
        ws.ensure_clone(
            &id.to_string(),
            &crew,
            &paths,
            &repo,
            "feature/issue-12",
            "feature/issue-12",
        )
        .unwrap();
        assert_eq!(
            git(&paths.workspace, &["rev-parse", "HEAD"]).trim(),
            tip,
            "{id}"
        );
        assert!(paths.workspace.join("FEATURE").exists(), "{id}");
    }
}

/// N-6: a workspace whose `.git` is a file is a 0.1.x worktree. Refused
/// with the remedy; nothing touched.
#[test]
fn a_worktree_from_0_1_is_refused_with_the_purge_message() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-0-1");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    // what 0.1.x's ensure_worktree made
    git(&crew.repo, &["fetch", "-q", "origin"]);
    git(
        &crew.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "balerix/f/c/a",
            &paths.workspace.display().to_string(),
            "origin/main",
        ],
    );
    assert!(paths.workspace.join(".git").is_file());

    let e = ws
        .ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/a: "), "{e}");
    assert!(e.contains("created by balerix 0.1 as a worktree"), "{e}");
    assert!(e.contains("balerix down f --purge"), "{e}");
    assert!(paths.workspace.join(".git").is_file(), "left as it was");
    assert!(!paths.branch_marker().exists());
}

/// A `workspace/` with no `.git` at all — a clone that crashed half-way —
/// holds nothing balerix values; it is replaced rather than failing
/// `git clone` (`already exists and is not an empty directory`) on every
/// pass until a purge.
#[test]
fn a_crashed_clone_directory_without_git_is_replaced() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-stray");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    std::fs::create_dir_all(&paths.workspace).unwrap();
    std::fs::write(paths.workspace.join("stray.txt"), "not a clone\n").unwrap();

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert!(paths.workspace.join(".git").is_dir());
    assert!(paths.workspace.join("README").exists());
    assert!(!paths.workspace.join("stray.txt").exists());
}

/// Review focus 5 (#62, first bullet): the clone step's `status` on a
/// branch change runs in an agent-writable repository; config the agent
/// wrote there must not run a program as the daemon.
#[test]
fn the_clone_step_runs_no_program_from_the_clone_config() {
    use std::os::unix::fs::PermissionsExt;

    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-hardened");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    push_branch(&root, "feature/issue-12");
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();

    let ran = root.join("fsmonitor-ran");
    let hook = root.join("fsmonitor.sh");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\necho\n", ran.display())).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &paths.workspace,
        &["config", "core.fsmonitor", &hook.display().to_string()],
    );
    // the fixture bites: a plain status runs it
    git(&paths.workspace, &["status", "--porcelain"]);
    assert!(
        ran.exists(),
        "the fixture's fsmonitor hook must run under plain git"
    );
    std::fs::remove_file(&ran).unwrap();

    // a branch change on a clean clone: `symbolic-ref` and `status` run
    // in the clone, then it is harvested and re-created
    ws.ensure_clone(
        "f/c/a",
        &crew,
        &paths,
        &repo,
        "feature/issue-12",
        "feature/issue-12",
    )
    .unwrap();
    assert!(
        !ran.exists(),
        "core.fsmonitor from the clone's config ran under the daemon"
    );
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "feature/issue-12"
    );
}

/// N-4: an unpushed commit survives `remove_agent` and the next
/// materialize, seeded from the cache — the exact commit — and a rebase
/// after the seed is harvested over the cache's older copy (the `+`).
#[test]
fn an_unpushed_commit_survives_removal_and_seeds_the_next_clone() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-harvest");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    // a second local branch, and an ignored file: both go with the clone
    git(&paths.workspace, &["branch", "scratch"]);
    std::fs::write(paths.workspace.join(".gitignore"), "ignored\n").unwrap();
    std::fs::write(paths.workspace.join("ignored"), "x\n").unwrap();

    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert!(!paths.workspace.exists());
    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha
    );
    assert!(
        git(&crew.repo, &["branch", "--list", "scratch"])
            .trim()
            .is_empty(),
        "only the assigned branch is harvested"
    );
    assert_no_auto_gc(&crew);

    // the marker survives in the agent root (remove_agent deletes that);
    // a re-created clone is seeded from the cache
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "HEAD"]),
        sha,
        "the exact commit"
    );
    assert!(paths.workspace.join("work.txt").exists());
    assert!(!paths.workspace.join("ignored").exists());

    // the agent rewrites history: the cache's copy is no ancestor of the
    // clone's, and the harvest must win anyway (`+`)
    git(
        &paths.workspace,
        &["commit", "-q", "--amend", "-m", "agent work, amended"],
    );
    let amended = git(&paths.workspace, &["rev-parse", "HEAD"]);
    assert_ne!(amended, sha);
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        amended,
        "N-5: the clone's state is the newer one, fast-forward or not"
    );
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), amended);

    // a plain directory where a clone used to be is removed as well
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    std::fs::create_dir_all(&paths.workspace).unwrap();
    std::fs::write(paths.workspace.join("stray.txt"), "x\n").unwrap();
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert!(!paths.workspace.exists());
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap(); // already gone
}

/// Review focus 3: without a marker the branch HEAD is on is harvested;
/// a detached HEAD, or a branch the agent deleted, harvests nothing and
/// the removal still succeeds.
#[test]
fn removal_harvests_head_without_a_marker_and_skips_what_is_not_there() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-harvest-edge");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    let make = |name: &str| {
        let id: balerix_core::AgentId = format!("f/c/{name}").parse().unwrap();
        let paths = layout.agent(&id);
        ws.ensure_clone(
            &id.to_string(),
            &crew,
            &paths,
            &repo,
            &format!("balerix/f/c/{name}"),
            "main",
        )
        .unwrap();
        std::fs::write(paths.workspace.join("w"), name).unwrap();
        git(&paths.workspace, &["add", "."]);
        git(&paths.workspace, &["commit", "-q", "-m", name]);
        (id, paths)
    };

    // no marker (a crash between clone and marker): HEAD's branch
    let (_, a) = make("a");
    git(&a.workspace, &["checkout", "-q", "-b", "my-fix"]);
    std::fs::remove_file(a.branch_marker()).unwrap();
    let sha = git(&a.workspace, &["rev-parse", "HEAD"]);
    ws.harvest_and_remove("f/c/a", &crew, &a).unwrap();
    assert_eq!(git(&crew.repo, &["rev-parse", "refs/heads/my-fix"]), sha);
    assert!(
        git(&crew.repo, &["branch", "--list", "balerix/f/c/a"])
            .trim()
            .is_empty(),
        "the marker was gone, HEAD decided"
    );

    // detached HEAD, no marker: nothing to name, nothing harvested
    let (_, b) = make("b");
    git(&b.workspace, &["checkout", "-q", "--detach"]);
    std::fs::remove_file(b.branch_marker()).unwrap();
    ws.harvest_and_remove("f/c/b", &crew, &b).unwrap();
    assert!(!b.workspace.exists());
    assert!(
        git(&crew.repo, &["branch", "--list", "balerix/f/c/b"])
            .trim()
            .is_empty()
    );

    // the agent deleted its assigned branch: skipped, not failed
    let (_, c) = make("c");
    git(&c.workspace, &["checkout", "-q", "-b", "elsewhere"]);
    git(&c.workspace, &["branch", "-D", "balerix/f/c/c"]);
    ws.harvest_and_remove("f/c/c", &crew, &c).unwrap();
    assert!(!c.workspace.exists());
    assert!(
        git(&crew.repo, &["branch", "--list", "balerix/f/c/c"])
            .trim()
            .is_empty()
    );

    // no cache at all: nothing to harvest into, the clone is still removed
    let (_, d) = make("d");
    std::fs::remove_dir_all(&crew.repo).unwrap();
    ws.harvest_and_remove("f/c/d", &crew, &d).unwrap();
    assert!(!d.workspace.exists());
}

/// A clone git cannot read fails the removal — the operator sees it
/// rather than losing work (Spec N §5).
#[test]
fn a_broken_clone_fails_the_removal_and_stays() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-harvest-broken");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    // `.git/HEAD` gone: git no longer sees a repository there
    std::fs::remove_file(paths.workspace.join(".git/HEAD")).unwrap();
    let e = ws
        .harvest_and_remove("f/c/a", &crew, &paths)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/a: git "), "{e}");
    assert!(
        paths.workspace.exists(),
        "nothing deleted on a failed harvest"
    );
}

/// Review focus 2: a 0.1.x fleet is refused with the purge message; after
/// `down --keep-repos` the old crew clone — no `gc.auto=0`, a stale
/// worktree registration — serves as the cache, and the branch it holds
/// seeds the new clone, so the unpushed work in it is not lost.
#[test]
fn a_worktree_from_0_1_is_refused_and_keep_repos_migrates_it() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-0-1-migrate");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    // what 0.1.x left behind: a clone without the gc pin, a worktree, an
    // unpushed commit on the worktree's branch, and no marker
    std::fs::create_dir_all(&crew.root).unwrap();
    git(
        &crew.root,
        &["clone", "-q", "--no-checkout", &repo.clone_url(), "repo"],
    );
    git(
        &crew.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "balerix/f/c/a",
            &paths.workspace.display().to_string(),
            "origin/main",
        ],
    );
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "0.1 work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);

    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap(); // present: left alone
    let e = ws
        .ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap_err()
        .to_string();
    assert!(e.contains("created by balerix 0.1 as a worktree"), "{e}");

    // `down --keep-repos`: the worktree goes (nothing to harvest from a
    // `.git` file — its branch already lives in the old clone)
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert!(!paths.workspace.exists());
    // `up`: a clone seeded from the old clone's branch
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert!(paths.workspace.join(".git").is_dir());
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha);
    assert!(paths.workspace.join("work.txt").exists());
    assert_no_auto_gc(&crew);
}
