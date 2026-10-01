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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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

    // `branch` back on the PR's branch: the cache's copy is origin's own
    // tip, so the branch comes from `origin/…` and tracks it
    let upstream = |b: &str| {
        git(
            &paths.workspace,
            &["rev-parse", "--abbrev-ref", &format!("{b}@{{upstream}}")],
        )
        .trim()
        .to_string()
    };
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
    assert_eq!(upstream("feature/issue-12"), "origin/feature/issue-12");

    // an unpushed commit on it, then away and back: seeded from the
    // cache's copy, which origin lacks, and still tracking origin's
    // branch (the seeded path's `--set-upstream-to`)
    std::fs::write(paths.workspace.join("pr.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "pr work"]);
    let pr_sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    ws.ensure_clone(
        "f/c/a",
        &crew,
        &paths,
        &repo,
        "feature/issue-12",
        "feature/issue-12",
    )
    .unwrap();
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), pr_sha);
    assert_eq!(upstream("feature/issue-12"), "origin/feature/issue-12");
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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    let git_calls = || {
        std::fs::read_to_string(crew.root.join("logs").join("git.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("$ git"))
            .count()
    };
    let before = git_calls();

    // clean tree: not moved back
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert_eq!(head(), "my-fix");
    assert_eq!(
        git_calls(),
        before,
        "Spec L §12: a matching marker costs no git call"
    );

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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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

/// A branch the agent is assigned that happens to be the remote's default
/// branch must be harvestable too — by HEAD (no marker) and by the marker —
/// not wedge the removal: the cache's HEAD is detached, so no fetch into
/// the cache meets the branch HEAD names.
#[test]
fn a_branch_the_cache_has_checked_out_is_harvested_too() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-harvest-default-branch");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();

    // route 1: no marker, HEAD decides
    let id_a: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let a = layout.agent(&id_a);
    ws.ensure_clone("f/c/a", &crew, &a, &repo, "balerix/f/c/a", "main")
        .unwrap();
    git(&a.workspace, &["checkout", "-q", "main"]);
    std::fs::write(a.workspace.join("a.txt"), "a\n").unwrap();
    git(&a.workspace, &["add", "."]);
    git(&a.workspace, &["commit", "-q", "-m", "a on main"]);
    let sha_a = git(&a.workspace, &["rev-parse", "HEAD"]);
    std::fs::remove_file(a.branch_marker()).unwrap();
    ws.harvest_and_remove("f/c/a", &crew, &a).unwrap();
    assert_eq!(git(&crew.repo, &["rev-parse", "refs/heads/main"]), sha_a);

    // route 2: a marker naming the default branch
    let id_b: balerix_core::AgentId = "f/c/b".parse().unwrap();
    let b = layout.agent(&id_b);
    ws.ensure_clone("f/c/b", &crew, &b, &repo, "balerix/f/c/b", "main")
        .unwrap();
    git(&b.workspace, &["checkout", "-q", "main"]);
    std::fs::write(b.workspace.join("b.txt"), "b\n").unwrap();
    git(&b.workspace, &["add", "."]);
    git(&b.workspace, &["commit", "-q", "-m", "b on main"]);
    let sha_b = git(&b.workspace, &["rev-parse", "HEAD"]);
    std::fs::write(b.branch_marker(), "main").unwrap();
    ws.harvest_and_remove("f/c/b", &crew, &b).unwrap();
    assert_eq!(git(&crew.repo, &["rev-parse", "refs/heads/main"]), sha_b);

    assert_no_auto_gc(&crew);
    assert!(
        !git_ok(&crew.repo, &["symbolic-ref", "-q", "HEAD"]),
        "the cache's HEAD stays detached: nothing names the harvested branch"
    );
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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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

/// `git` in `dir` without the fixture's success assertion: whether it
/// exited 0. Never lazy-fetches: an object probe in a clone with a
/// promisor remote would otherwise fetch the very object it looks for.
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap()
        .status
        .success()
}

/// Review finding 1 (Critical): the harvest must not be a confused
/// deputy. The daemon can read every repository under its uid; an agent
/// that points its clone's object store at one — through `alternates`, a
/// symlinked `.git`, a symlinked pack, `.git/commondir`, or a broken
/// `.git` that leaves `workspace/` to pass for a bare repository — must
/// not get the daemon to copy it into the crew cache its siblings read.
/// Every vector is refused before a git call, the clone stays, and the
/// foreign commit is nowhere in the cache.
#[test]
fn a_clone_pointed_at_another_repository_is_refused_and_nothing_is_harvested() {
    use std::os::unix::fs::symlink;

    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-foreign");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();

    // what the agent cannot read but the daemon can: another repository,
    // packed so that its objects live in one `.pack`/`.idx` pair
    let foreign = root.join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    git(&foreign, &["init", "-q", "-b", "main"]);
    std::fs::write(foreign.join("SECRET"), "another crew's code\n").unwrap();
    git(&foreign, &["add", "."]);
    git(&foreign, &["commit", "-q", "-m", "foreign"]);
    git(&foreign, &["repack", "-q", "-a", "-d"]);
    let foreign_sha = git(&foreign, &["rev-parse", "HEAD"]).trim().to_string();
    let foreign_git = foreign.join(".git");

    let clone = |name: &str| {
        let id = format!("f/c/{name}");
        let paths = layout.agent(&id.parse().unwrap());
        ws.ensure_clone(&id, &crew, &paths, &repo, &format!("balerix/{id}"), "main")
            .unwrap();
        (id, paths)
    };
    let refused = |id: &str, paths: &balerix_runtime::AgentPaths, names: &str| {
        let e = ws
            .harvest_and_remove(id, &crew, paths)
            .unwrap_err()
            .to_string();
        assert!(e.starts_with(&format!("{id}: ")), "{e}");
        assert!(e.contains(names), "the message names {names}: {e}");
        assert!(e.contains("--purge"), "{e}");
        assert!(
            paths.workspace.exists(),
            "a refused removal deletes nothing"
        );
        assert!(
            !git_ok(&crew.repo, &["cat-file", "-e", &foreign_sha]),
            "{id}: the foreign commit reached the cache"
        );
        assert!(
            !git_ok(
                &crew.repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/balerix/{id}")
                ]
            ),
            "{id}: the assigned branch reached the cache"
        );
        e
    };

    // (A) alternates appended, the assigned ref written by hand
    let (id, a) = clone("a");
    let alternates = a.workspace.join(".git/objects/info/alternates");
    let mut lines = std::fs::read_to_string(&alternates).unwrap();
    lines.push_str(&format!("{}\n", foreign_git.join("objects").display()));
    std::fs::write(&alternates, lines).unwrap();
    std::fs::write(
        a.workspace.join(".git/refs/heads/balerix/f/c/a"),
        format!("{foreign_sha}\n"),
    )
    .unwrap();
    assert_eq!(
        git(&a.workspace, &["cat-file", "-t", &foreign_sha]).trim(),
        "commit",
        "the fixture bites: the clone now reads the foreign commit"
    );
    refused(&id, &a, "objects/info/alternates");
    let e = ws
        .ensure_clone(&id, &crew, &a, &repo, "balerix/f/c/a", "main")
        .unwrap_err()
        .to_string();
    assert!(e.contains("objects/info/alternates"), "{e}");

    // (B) `.git` a symlink to the foreign repository's
    let (id, b) = clone("b");
    std::fs::remove_dir_all(b.workspace.join(".git")).unwrap();
    symlink(&foreign_git, b.workspace.join(".git")).unwrap();
    refused(
        &id,
        &b,
        &format!(
            "{}: not a real directory",
            b.workspace.join(".git").display()
        ),
    );
    for branch in ["balerix/f/c/b", "main"] {
        let e = ws
            .ensure_clone(&id, &crew, &b, &repo, branch, "main")
            .unwrap_err()
            .to_string();
        assert!(e.contains("/.git: not a real directory"), "{e}");
    }
    assert!(
        std::fs::symlink_metadata(b.workspace.join(".git"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "ensure_clone left the refused clone as it was"
    );

    // (B') a single pack symlinked to the foreign pack
    let (id, c) = clone("c");
    let pack_dir = foreign_git.join("objects/pack");
    for entry in std::fs::read_dir(&pack_dir).unwrap() {
        let path = entry.unwrap().path();
        symlink(
            &path,
            c.workspace
                .join(".git/objects/pack")
                .join(path.file_name().unwrap()),
        )
        .unwrap();
    }
    std::fs::write(
        c.workspace.join(".git/refs/heads/balerix/f/c/c"),
        format!("{foreign_sha}\n"),
    )
    .unwrap();
    refused(&id, &c, "a symlink");

    // (C) `.git/commondir`: objects and refs read from the directory it names
    let (id, d) = clone("d");
    std::fs::write(
        d.workspace.join(".git/commondir"),
        foreign_git.display().to_string(),
    )
    .unwrap();
    refused(&id, &d, "commondir");

    // (D) a `.git` git rejects, and `workspace/` dressed as a bare
    // repository borrowing the foreign objects: git must not fall back
    // to it (`--git-dir`, `upload-pack --strict`)
    let (id, broken) = clone("e");
    std::fs::remove_file(broken.workspace.join(".git/HEAD")).unwrap();
    std::fs::create_dir_all(broken.workspace.join("objects/info")).unwrap();
    std::fs::create_dir_all(broken.workspace.join("refs/heads/balerix/f/c")).unwrap();
    std::fs::write(
        broken.workspace.join("HEAD"),
        "ref: refs/heads/balerix/f/c/e\n",
    )
    .unwrap();
    std::fs::write(
        broken.workspace.join("objects/info/alternates"),
        format!("{}\n", foreign_git.join("objects").display()),
    )
    .unwrap();
    std::fs::write(
        broken.workspace.join("refs/heads/balerix/f/c/e"),
        format!("{foreign_sha}\n"),
    )
    .unwrap();
    let err = ws
        .harvest_and_remove(&id, &crew, &broken)
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("f/c/e: git "), "{err}");
    assert!(broken.workspace.exists());
    assert!(!git_ok(&crew.repo, &["cat-file", "-e", &foreign_sha]));

    // the happy path passes the check: an untouched clone is harvested
    let (id, f) = clone("f");
    ws.harvest_and_remove(&id, &crew, &f).unwrap();
    assert!(!f.workspace.exists());
    assert!(git_ok(
        &crew.repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/balerix/f/c/f"
        ]
    ));
}

/// #70 and #68: the checks are a verdict on the clone as it was. A
/// process the agent detached past the tmux kill can rewrite the clone
/// after they pass. Each vector the checks refuse is applied here
/// *between* the checks and the harvest (F, which the harvest's probes
/// would trip over, just before its fetch); the daemon's git runs under
/// the git profile, which cannot read the foreign repository, so nothing
/// of it reaches the cache whatever the clone says.
#[test]
fn a_clone_rewritten_after_the_checks_serves_nothing_foreign() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use balerix_runtime::testing::{
        harvest_and_remove_racing, harvest_and_remove_racing_the_fetch,
    };

    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-raced");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();

    let foreign = root.join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    git(&foreign, &["init", "-q", "-b", "main"]);
    std::fs::write(foreign.join("SECRET"), "another crew's code\n").unwrap();
    git(&foreign, &["add", "."]);
    git(&foreign, &["commit", "-q", "-m", "foreign"]);
    git(&foreign, &["repack", "-q", "-a", "-d"]);
    let foreign_sha = git(&foreign, &["rev-parse", "HEAD"]).trim().to_string();
    let foreign_git = foreign.join(".git");
    // the branches the harvest asks for in vectors B and D, where the
    // clone's refs come from the foreign repository itself: with them an
    // unsandboxed `upload-pack` would serve the foreign commit
    for name in ["b", "d"] {
        git(&foreign, &["branch", &format!("balerix/f/c/{name}")]);
    }

    let ran = root.join("uploadpack-ran");
    let script = root.join("uploadpack.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch {}\nexec git-upload-pack \"$@\"\n",
            ran.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let raced_at = |name: &str, at_fetch: bool, rewrite: &dyn Fn(&balerix_runtime::AgentPaths)| {
        let id = format!("f/c/{name}");
        let paths = layout.agent(&id.parse().unwrap());
        ws.ensure_clone(&id, &crew, &paths, &repo, &format!("balerix/{id}"), "main")
            .unwrap();
        let race = if at_fetch {
            harvest_and_remove_racing_the_fetch
        } else {
            harvest_and_remove_racing
        };
        let outcome = race(&ws, &id, &crew, &paths, &|| rewrite(&paths));
        assert!(
            !git_ok(&crew.repo, &["cat-file", "-e", &foreign_sha]),
            "{id}: the foreign commit reached the cache"
        );
        assert!(!ran.exists(), "{id}: remote.evil.uploadpack ran");
        let e = outcome.unwrap_err().to_string();
        assert!(e.starts_with(&format!("{id}: git ")), "{e}");
        assert!(
            paths.workspace.exists(),
            "{id}: a failed harvest deletes nothing"
        );
    };
    let raced =
        |name: &str, rewrite: &dyn Fn(&balerix_runtime::AgentPaths)| raced_at(name, false, rewrite);
    let point_ref = |paths: &balerix_runtime::AgentPaths, name: &str| {
        std::fs::write(
            paths
                .workspace
                .join(format!(".git/refs/heads/balerix/f/c/{name}")),
            format!("{foreign_sha}\n"),
        )
        .unwrap();
    };

    // (A) alternates appended, the assigned ref written by hand
    raced("a", &|p| {
        let alternates = p.workspace.join(".git/objects/info/alternates");
        let mut lines = std::fs::read_to_string(&alternates).unwrap();
        lines.push_str(&format!("{}\n", foreign_git.join("objects").display()));
        std::fs::write(&alternates, lines).unwrap();
        point_ref(p, "a");
    });

    // (B) `.git` swapped for a symlink to the foreign repository's
    raced("b", &|p| {
        std::fs::remove_dir_all(p.workspace.join(".git")).unwrap();
        symlink(&foreign_git, p.workspace.join(".git")).unwrap();
    });

    // (B') a pack symlinked to the foreign pack
    raced("c", &|p| {
        for entry in std::fs::read_dir(foreign_git.join("objects/pack")).unwrap() {
            let path = entry.unwrap().path();
            symlink(
                &path,
                p.workspace
                    .join(".git/objects/pack")
                    .join(path.file_name().unwrap()),
            )
            .unwrap();
        }
        point_ref(p, "c");
    });

    // (C) `.git/commondir`
    raced("d", &|p| {
        std::fs::write(
            p.workspace.join(".git/commondir"),
            foreign_git.display().to_string(),
        )
        .unwrap();
    });

    // (E) a promisor remote with its own upload-pack program
    raced("e", &|p| {
        for (k, v) in [
            ("core.repositoryformatversion", "1"),
            ("extensions.partialClone", "evil"),
            ("remote.evil.promisor", "true"),
            ("remote.evil.url", &foreign.display().to_string()),
            ("remote.evil.uploadpack", &script.display().to_string()),
        ] {
            git(&p.workspace, &["config", k, v]);
        }
        point_ref(p, "e");
    });

    // (F) `.git` swapped for a symlink to a bundle of the foreign
    // repository carrying the branch the harvest asks for: git reads a
    // local path that is a bundle file itself, as the daemon, without
    // running any `upload-pack`. The sandboxed probes cannot read such a
    // `.git` and fail first, so the swap comes after the last of them
    let bundle = root.join("foreign.bundle");
    git(&foreign, &["branch", "balerix/f/c/g"]);
    git(
        &foreign,
        &[
            "bundle",
            "create",
            "-q",
            &bundle.display().to_string(),
            "balerix/f/c/g",
        ],
    );
    raced_at("g", true, &|p| {
        std::fs::remove_dir_all(p.workspace.join(".git")).unwrap();
        symlink(&bundle, p.workspace.join(".git")).unwrap();
    });

    // an honest clone goes through the same path untouched
    let id = "f/c/f";
    let paths = layout.agent(&id.parse().unwrap());
    ws.ensure_clone(id, &crew, &paths, &repo, "balerix/f/c/f", "main")
        .unwrap();
    harvest_and_remove_racing(&ws, id, &crew, &paths, || {}).unwrap();
    assert!(!paths.workspace.exists());
    assert!(git_ok(
        &crew.repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/balerix/f/c/f"
        ]
    ));
}

/// Review finding 3: `branch: <default branch>`. A fresh `--no-checkout`
/// clone already has `main` with HEAD on it, so the create path needs
/// `checkout -B` and the seed fetch `--update-head-ok`; without them the
/// agent could never materialize, before or after a harvest.
#[test]
fn an_agent_on_the_default_branch_materializes() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-default-branch");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    // origin moves on after the cache was cloned
    let work = root.join("upstream-work");
    std::fs::write(work.join("LATER"), "later\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "later on main"]);
    let upstream = root.join("upstream.git").display().to_string();
    git(&work, &["push", "-q", &upstream, "main"]);
    let origin_tip = git(&work, &["rev-parse", "HEAD"]);

    // created: HEAD on `main` at origin's tip — not the cache's own
    // `main`, which is the tip when the cache was cloned — tracking
    // `origin/main`
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "main", "main")
        .unwrap();
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), origin_tip);
    assert_eq!(
        git(
            &paths.workspace,
            &["rev-parse", "--abbrev-ref", "main@{upstream}"]
        )
        .trim(),
        "origin/main"
    );
    assert!(paths.workspace.join("README").exists());

    // harvested, then seeded from the cache: the exact commit, even
    // after origin moved on again (the harvested copy has diverged from
    // the fresh clone's `main`: the seed fetch is forced)
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(
        &paths.workspace,
        &["commit", "-q", "-m", "agent work on main"],
    );
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert!(!paths.workspace.exists());
    std::fs::write(work.join("LATEST"), "latest\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "latest on main"]);
    git(&work, &["push", "-q", &upstream, "main"]);
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "main", "main")
        .unwrap();
    assert_eq!(git(&paths.workspace, &["rev-parse", "HEAD"]), sha);
    assert!(paths.workspace.join("work.txt").exists());
    assert!(
        git(&paths.workspace, &["status", "--porcelain"])
            .trim()
            .is_empty(),
        "the seeded checkout is clean"
    );
    assert_eq!(
        git(
            &paths.workspace,
            &["rev-parse", "--abbrev-ref", "main@{upstream}"]
        )
        .trim(),
        "origin/main"
    );

    // the agent's work reaches origin; a copy origin contains is no seed
    git(&paths.workspace, &["pull", "-q", "--rebase"]);
    git(&paths.workspace, &["push", "-q"]);
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    std::fs::write(work.join("NEWEST"), "newest\n").unwrap();
    git(&work, &["pull", "-q", "--rebase", &upstream, "main"]);
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "newest on main"]);
    git(&work, &["push", "-q", &upstream, "main"]);
    let newest = git(&work, &["rev-parse", "HEAD"]);
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "main", "main")
        .unwrap();
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "HEAD"]),
        newest,
        "origin's newer tip, not the cache's older copy"
    );
    assert_no_auto_gc(&crew);
}

/// #69: the cache's own `refs/heads/<default>` is the tip when the cache
/// was cloned, never a harvest. Once origin force-pushes its default
/// branch past it, that copy is no ancestor of `origin/main` and, read as
/// a harvest, seeds every clone on `branch: main` with history origin
/// abandoned. So the cache is made with a detached HEAD and no local
/// default branch: everything under its `refs/heads` is a harvest.
#[test]
fn a_force_pushed_default_branch_does_not_seed_from_the_cache() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-force-pushed-default");
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

    // origin rewrites `main` past the clone-time tip
    let work = root.join("upstream-work");
    git(&work, &["commit", "-q", "--amend", "-m", "init, rewritten"]);
    let upstream = root.join("upstream.git").display().to_string();
    git(&work, &["push", "-q", "--force", &upstream, "main"]);
    let rewritten = git(&work, &["rev-parse", "HEAD"]);

    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "main", "main")
        .unwrap();
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "HEAD"]),
        rewritten,
        "the new origin/main, not the cache's clone-time copy"
    );
    assert!(
        git(&crew.repo, &["branch", "--list", "main"])
            .trim()
            .is_empty(),
        "the cache keeps no clone-time default branch"
    );
    assert!(
        !git_ok(&crew.repo, &["symbolic-ref", "-q", "HEAD"]),
        "the cache's HEAD is detached"
    );
    assert_no_auto_gc(&crew);
}

/// Re-review of finding 1: a promisor remote the agent writes into its
/// clone's config (`extensions.partialClone`) must not make the daemon's
/// `status` lazy-fetch another repository's objects into the clone — the
/// next harvest would carry them into the cache — nor run the remote's
/// `uploadpack` program (`GIT_NO_LAZY_FETCH=1` in `harden_agent_git`).
/// And an `alternates` file in the cache itself, never legitimate, is
/// refused.
#[test]
fn a_promisor_remote_in_the_clone_fetches_nothing_and_runs_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-promisor");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/p".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();

    let foreign = root.join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    git(&foreign, &["init", "-q", "-b", "main"]);
    std::fs::write(foreign.join("SECRET"), "another crew's code\n").unwrap();
    git(&foreign, &["add", "."]);
    git(&foreign, &["commit", "-q", "-m", "foreign"]);
    let foreign_sha = git(&foreign, &["rev-parse", "HEAD"]).trim().to_string();

    ws.ensure_clone("f/c/p", &crew, &paths, &repo, "balerix/f/c/p", "main")
        .unwrap();
    let ran = root.join("uploadpack-ran");
    let script = root.join("uploadpack.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch {}\nexec git-upload-pack \"$@\"\n",
            ran.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (k, v) in [
        ("core.repositoryformatversion", "1"),
        ("extensions.partialClone", "evil"),
        ("remote.evil.promisor", "true"),
        ("remote.evil.url", &foreign.display().to_string()),
        ("remote.evil.uploadpack", &script.display().to_string()),
    ] {
        git(&paths.workspace, &["config", k, v]);
    }
    std::fs::write(
        paths.workspace.join(".git/refs/heads/balerix/f/c/p"),
        format!("{foreign_sha}\n"),
    )
    .unwrap();

    // The refusal (#67) comes before any git runs over the clone's
    // objects, on any git version: it names `.git/config` and the key,
    // and leaves the clone as it is. Nothing is fetched and no program
    // runs on either path.
    let refused = |key: &str| {
        // (a) a changed `branch`: `symbolic-ref`, then `status` over a
        // HEAD whose commit is missing, would otherwise fetch it
        let outcome = ws.ensure_clone("f/c/p", &crew, &paths, &repo, "other", "main");
        assert!(
            !git_ok(&paths.workspace, &["cat-file", "-e", &foreign_sha]),
            "the daemon's status lazy-fetched the foreign commit into the clone"
        );
        assert!(!ran.exists(), "remote.evil.uploadpack ran as the daemon");
        let e = outcome.unwrap_err().to_string();
        assert!(e.starts_with("f/c/p: "), "{e}");
        assert!(
            e.contains(".git/config: sets ") && e.contains(key) && e.contains("--purge"),
            "the message names the config file, {key} and the remedy: {e}"
        );

        // (b) the harvest: refused the same way, so nothing foreign can
        // reach the cache
        let e = ws
            .harvest_and_remove("f/c/p", &crew, &paths)
            .unwrap_err()
            .to_string();
        assert!(e.contains(".git/config: sets ") && e.contains(key), "{e}");
        assert!(
            paths.workspace.exists(),
            "a refused removal deletes nothing"
        );
        assert!(!ran.exists(), "remote.evil.uploadpack ran as the daemon");
        assert!(!git_ok(&crew.repo, &["cat-file", "-e", &foreign_sha]));
        assert!(!git_ok(
            &crew.repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/balerix/f/c/p"
            ]
        ));
    };
    refused("extensions.partialclone");

    // git lazy-fetches from any `remote.<x>.promisor` whether or not the
    // extension names it, so a promisor remote alone is refused too
    git(
        &paths.workspace,
        &["config", "--unset", "extensions.partialClone"],
    );
    refused("remote.evil.promisor");

    // the same keys through a file the config includes: the probe follows
    // includes, as git would
    git(
        &paths.workspace,
        &["config", "--unset", "remote.evil.promisor"],
    );
    // inside the clone: the git profile reads the clone, so this is what
    // an include the agent could use looks like; one outside it is
    // unreadable under the profile and fails the probe closed
    let included = paths.workspace.join(".git/included.config");
    std::fs::write(&included, "[remote \"inc\"]\n\tpromisor = true\n").unwrap();
    git(
        &paths.workspace,
        &["config", "include.path", &included.display().to_string()],
    );
    refused("remote.inc.promisor");
    git(&paths.workspace, &["config", "--unset", "include.path"]);

    // an include outside the clone: unreadable under the git profile, so
    // the probe fails closed, before any git reads an object
    let outside = root.join("included.config");
    std::fs::write(&outside, "[remote \"out\"]\n\tpromisor = true\n").unwrap();
    git(
        &paths.workspace,
        &["config", "include.path", &outside.display().to_string()],
    );
    let e = ws
        .harvest_and_remove("f/c/p", &crew, &paths)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/p: git config"), "{e}");
    assert!(paths.workspace.exists(), "a failed removal deletes nothing");
    assert!(!ran.exists(), "remote.evil.uploadpack ran as the daemon");
    assert!(!git_ok(&crew.repo, &["cat-file", "-e", &foreign_sha]));
    assert!(!git_ok(
        &crew.repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/balerix/f/c/p"
        ]
    ));
    git(&paths.workspace, &["config", "--unset", "include.path"]);

    // an `alternates` file in the cache itself is refused, by name: the
    // message says the cache's objects are the suspect ones, not the clone's
    let cache_alternates = crew.cache_objects().join("info/alternates");
    std::fs::write(
        &cache_alternates,
        format!("{}\n", foreign.join(".git/objects").display()),
    )
    .unwrap();
    let e = ws
        .harvest_and_remove("f/c/p", &crew, &paths)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/p: "), "{e}");
    assert!(
        e.contains(&cache_alternates.display().to_string()) && e.contains("--purge"),
        "{e}"
    );
    assert!(
        e.contains("the crew cache's objects may be another repository's"),
        "{e}"
    );
    assert!(paths.workspace.exists());
}

/// `tools` with `git` replaced by a shim that exits 128 on any call whose
/// argv holds `subcommand` as a word, and runs the real git otherwise: how
/// a real git error (not a "no" answer) is injected into one step (#74).
fn failing_git(
    root: &Path,
    tools: &balerix_runtime::ToolPaths,
    subcommand: &str,
) -> balerix_runtime::ToolPaths {
    use std::os::unix::fs::PermissionsExt;

    let shim = root.join(format!("git-failing-{subcommand}.sh"));
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = \"{subcommand}\" ]; then\n    echo \"shim: {subcommand} refused\" >&2\n    exit 128\n  fi\ndone\nexec {} \"$@\"\n",
            tools.git.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    balerix_runtime::ToolPaths {
        git: shim,
        ..tools.clone()
    }
}

/// #74: a cache whose `gc.auto` pin failed is not kept, since an existing
/// cache is never touched again; the next pass re-clones and pins it.
#[test]
fn a_cache_whose_pin_failed_is_removed_and_made_again() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-pin-failed");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let broken = failing_git(&root, &tools, "config");
    let e = Workspace {
        tools: &broken,
        gh_config_dir: None,
    }
    .ensure_repo("f/c", &crew, &repo, "main")
    .unwrap_err()
    .to_string();
    assert!(e.contains("config refused"), "{e}");
    assert!(
        !crew.repo.join(".git").exists(),
        "a half-made cache must not survive the failed pin"
    );

    Workspace {
        tools: &tools,
        gh_config_dir: None,
    }
    .ensure_repo("f/c", &crew, &repo, "main")
    .unwrap();
    assert_eq!(git(&crew.repo, &["config", "gc.auto"]).trim(), "0");
}

/// #74: a clone whose HEAD git cannot read, with no marker to fall back
/// on, is not "detached": the removal fails and keeps the clone.
#[test]
fn an_unreadable_head_without_a_marker_fails_the_removal() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-head-error");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    std::fs::remove_file(paths.branch_marker()).unwrap();

    let broken = failing_git(&root, &tools, "symbolic-ref");
    let e = Workspace {
        tools: &broken,
        gh_config_dir: None,
    }
    .harvest_and_remove("f/c/a", &crew, &paths)
    .unwrap_err()
    .to_string();
    assert!(e.contains("symbolic-ref refused"), "{e}");
    assert!(
        paths.workspace.join(".git").is_dir(),
        "nothing deleted when HEAD cannot be read"
    );
}

/// #74: the same HEAD probe on a branch change: a git error is not a
/// detached HEAD to reuse; the pass fails.
#[test]
fn an_unreadable_head_fails_a_branch_change() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-head-error-change");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    push_branch(&root, "feature/issue-74");
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

    let broken = failing_git(&root, &tools, "symbolic-ref");
    let e = Workspace {
        tools: &broken,
        gh_config_dir: None,
    }
    .ensure_clone(
        "f/c/a",
        &crew,
        &paths,
        &repo,
        "feature/issue-74",
        "feature/issue-74",
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("symbolic-ref refused"), "{e}");
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "balerix/f/c/a",
        "the clone is left as it was"
    );
}

/// #74: a git error while probing the cache for a harvested copy fails
/// the clone; it does not silently mean "no copy, build from the start
/// ref" and drop the seed.
#[test]
fn a_failed_harvest_probe_fails_the_clone() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-probe-error");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    Workspace {
        tools: &tools,
        gh_config_dir: None,
    }
    .ensure_repo("f/c", &crew, &repo, "main")
    .unwrap();

    let broken = failing_git(&root, &tools, "rev-parse");
    let e = Workspace {
        tools: &broken,
        gh_config_dir: None,
    }
    .ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
    .unwrap_err()
    .to_string();
    assert!(e.contains("rev-parse refused"), "{e}");
    assert!(!paths.workspace.exists(), "the half-made clone is removed");
}

/// Spec N amendment §5: every git call in an existing clone, and the
/// harvest's `upload-pack`, goes through `nono run --profile <git
/// profile>`; the profile is rewritten before use whatever was there.
#[test]
fn daemon_git_in_a_clone_runs_under_the_git_profile() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-sandboxed");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
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
    assert!(
        !paths.git_profile().exists(),
        "a fresh clone needs no git profile: the agent has not touched it"
    );
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);

    // whatever sits at the profile's path is replaced before use
    std::fs::write(paths.git_profile(), "{\"filesystem\":{\"allow\":[\"/\"]}}").unwrap();
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();

    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha,
        "the harvest still works through the sandbox"
    );
    let profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(paths.git_profile()).unwrap()).unwrap();
    assert!(profile["filesystem"].get("allow").is_none(), "{profile}");
    assert_eq!(profile["network"]["block"], true);

    let log = std::fs::read_to_string(crew.root.join("logs/git.log")).unwrap();
    let profile_arg = format!("run --profile {}", paths.git_profile().display());
    let in_clone: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with("$ ") && l.contains(&paths.workspace.display().to_string()))
        .filter(|l| !l.contains(" clone ") && !l.contains("checkout"))
        .collect();
    assert!(!in_clone.is_empty(), "{log}");
    for line in in_clone {
        assert!(
            line.contains(&profile_arg) || line.contains("--upload-pack=env -i "),
            "a git call in the existing clone ran outside the git profile: {line}"
        );
    }
    assert!(
        log.contains("--upload-pack=env -i ") && log.contains("upload-pack --strict"),
        "{log}"
    );
}

/// Review focus 1: the `--upload-pack` string is run by a shell, and the
/// state root is the operator's to name. The `%` is for the remote, a
/// `file://` URL git percent-decodes (#70).
#[test]
fn a_state_root_with_a_space_and_a_quote_is_harvested() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-quoting");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root.join("it's a 100%20 root"));
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
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha
    );
}

/// NS-5: no fallback. Without a nono that runs, the removal fails, the
/// clone stays and nothing is harvested.
#[test]
fn a_harvest_without_a_working_nono_fails_and_keeps_the_clone() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-no-nono");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    Workspace {
        tools: &tools,
        gh_config_dir: None,
    }
    .ensure_repo("f/c", &crew, &repo, "main")
    .unwrap();
    Workspace {
        tools: &tools,
        gh_config_dir: None,
    }
    .ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
    .unwrap();

    let no_nono = balerix_runtime::ToolPaths {
        nono: root.join("no-such-nono"),
        ..tools.clone()
    };
    let e = Workspace {
        tools: &no_nono,
        gh_config_dir: None,
    }
    .harvest_and_remove("f/c/a", &crew, &paths)
    .unwrap_err()
    .to_string();
    assert!(e.starts_with("f/c/a: "), "{e}");
    assert!(
        e.contains("no-such-nono"),
        "the message names what is missing: {e}"
    );
    assert!(paths.workspace.exists(), "a failed removal deletes nothing");
    assert!(!git_ok(
        &crew.repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/balerix/f/c/a"
        ]
    ));
}

/// NS-5, the other way a nono fails: it starts, validates the profile and
/// then exits 1 on `run`, the same status git's yes/no probes use for
/// "no". The removal must fail, not read that as "no promisor keys" and
/// "branch absent", skip the harvest and delete the clone.
#[test]
fn a_nono_that_cannot_run_fails_the_removal_and_keeps_the_clone() {
    use std::os::unix::fs::PermissionsExt;

    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-nono-exit-1");
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

    let shim = root.join("nono-cannot-run.sh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = validate ]; then exec {} \"$@\"; fi\ndone\nexit 1\n",
            tools.nono.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let broken = balerix_runtime::ToolPaths {
        nono: shim,
        ..tools.clone()
    };
    let e = Workspace {
        tools: &broken,
        gh_config_dir: None,
    }
    .harvest_and_remove("f/c/a", &crew, &paths)
    .expect_err("a nono that cannot run must fail the removal")
    .to_string();
    assert!(e.starts_with("f/c/a: git "), "{e}");
    assert!(paths.workspace.exists(), "a failed removal deletes nothing");
    assert!(!git_ok(
        &crew.repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/balerix/f/c/a"
        ]
    ));
}
