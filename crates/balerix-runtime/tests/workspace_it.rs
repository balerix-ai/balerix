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

#[test]
fn clone_worktree_reuse_and_remove() {
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
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap(); // present → no git call
    let git_log =
        || std::fs::read_to_string(crew.root.join("logs").join("git.log")).unwrap_or_default();
    let fetches = |log: &str| {
        log.lines()
            .filter(|l| l.starts_with("$ git") && l.contains(" fetch "))
            .count()
    };
    assert_eq!(
        fetches(&git_log()),
        0,
        "a second ensure_repo must not fetch"
    );

    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(fetches(&git_log()), 1, "creating a branch fetches first");
    assert_eq!(
        std::fs::read_to_string(paths.workspace.join("README")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "balerix/f/c/a"
    );
    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main")
        .unwrap(); // idempotent
    assert_eq!(
        fetches(&git_log()),
        1,
        "a registered worktree costs no fetch"
    );

    // agent commits; remove the worktree; re-adding must keep the commit (P2-6)
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    ws.remove_worktree("f/c/a", &crew, &paths.workspace)
        .unwrap();
    assert!(!paths.workspace.exists());
    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "HEAD"]),
        sha,
        "branch reused, commit preserved"
    );
    assert!(paths.workspace.join("work.txt").exists());

    // a second agent gets its own branch from origin/main, not from alice's
    let b = layout.agent(&"f/c/b".parse().unwrap());
    ws.ensure_worktree("f/c/b", &crew, &b.workspace, "balerix/f/c/b", "main")
        .unwrap();
    assert!(!b.workspace.join("work.txt").exists());
    assert!(crew.root.join("logs").join("git.log").exists());

    // An unregistered plain directory where a worktree used to be (a crashed
    // pass, or a `.git` file removed by hand): `git worktree remove` would
    // fail with "is not a working tree", so it is not called at all. Removal
    // succeeds and leaves the directory for the caller's rm -rf to take.
    let c = layout.agent(&"f/c/c".parse().unwrap());
    std::fs::create_dir_all(&c.workspace).unwrap();
    std::fs::write(c.workspace.join("stray.txt"), "not a worktree\n").unwrap();
    ws.remove_worktree("f/c/c", &crew, &c.workspace).unwrap();
    assert!(
        c.workspace.join("stray.txt").exists(),
        "an unregistered directory is left for remove_agent's rm -rf"
    );
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

/// Spec L §6: an agent with `branch` works on that remote branch — created
/// from `origin/<branch>`, tracking it, reused across passes — never on a
/// fresh `balerix/…` branch.
#[test]
fn a_worktree_on_an_existing_remote_branch_is_created_from_it_and_reused() {
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

    ws.ensure_worktree(
        "f/c/pr",
        &crew,
        &paths.workspace,
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
        "a push from the worktree reaches the PR's branch"
    );

    // the agent commits; a removed and re-added worktree keeps the branch
    std::fs::write(paths.workspace.join("more"), "x\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    ws.remove_worktree("f/c/pr", &crew, &paths.workspace)
        .unwrap();
    ws.ensure_worktree(
        "f/c/pr",
        &crew,
        &paths.workspace,
        "feature/issue-12",
        "feature/issue-12",
    )
    .unwrap();
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha);
}

/// Spec L §6 (F1): adding, changing or removing `branch` on a live agent
/// re-creates its clean worktree on the new branch; the reconciler only
/// re-materializes, so a registered worktree must not pin the old one.
#[test]
fn a_changed_branch_moves_a_clean_worktree_and_keeps_the_old_branch() {
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

    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "balerix/f/c/a");
    std::fs::write(paths.workspace.join("work.txt"), "committed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);

    // `branch` added
    ws.ensure_worktree(
        "f/c/a",
        &crew,
        &paths.workspace,
        "feature/issue-12",
        "feature/issue-12",
    )
    .unwrap();
    assert_eq!(head(), "feature/issue-12");
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]).trim(), tip);
    assert!(!paths.workspace.join("work.txt").exists());

    // `branch` removed: back on the per-agent branch, its commit intact
    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "balerix/f/c/a");
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha);
}

/// F1: a dirty tree on the old branch fails the materialize step, naming
/// both branches, and is left exactly as it was.
#[test]
fn a_changed_branch_on_a_dirty_worktree_fails_and_keeps_the_tree() {
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
    ws.ensure_worktree("f/c/a", &crew, &paths.workspace, "balerix/f/c/a", "main")
        .unwrap();
    std::fs::write(paths.workspace.join("README"), "edited\n").unwrap();

    let e = ws
        .ensure_worktree(
            "f/c/a",
            &crew,
            &paths.workspace,
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
}

/// Review focus 1: a `branch` the remote does not have fails the
/// materialize step with git's message; nothing is created from `main`.
#[test]
fn a_missing_remote_branch_fails_the_worktree() {
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
        .ensure_worktree("f/c/pr", &crew, &paths.workspace, "nope", "nope")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/pr: git worktree:"), "{e}");
    assert!(e.contains("origin/nope"), "{e}");
    assert!(!paths.workspace.join(".git").exists());
    assert!(
        git(&crew.repo, &["branch", "--list", "nope"])
            .trim()
            .is_empty(),
        "no local branch was created"
    );
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
