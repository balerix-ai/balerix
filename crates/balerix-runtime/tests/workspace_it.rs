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

/// The cache is a `--no-checkout` clone: its HEAD still sits on
/// `refs/heads/<default>`. A branch the agent is assigned that happens to
/// be the cache's default branch must be harvestable too — by HEAD (no
/// marker) and by the marker — not wedge the removal (`--update-head-ok`).
#[test]
fn a_branch_the_cache_has_checked_out_is_harvested_too() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-harvest-default-branch");
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
    assert_eq!(
        git(&crew.repo, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main",
        "the fix updated the ref under HEAD rather than detaching it"
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

    // (a) a changed `branch`: `symbolic-ref`, then `status` over a HEAD
    // whose commit is missing. It fails (`bad object HEAD`) rather than
    // fetch it, and no program runs.
    let outcome = ws.ensure_clone("f/c/p", &crew, &paths, &repo, "other", "main");
    assert!(
        !git_ok(&paths.workspace, &["cat-file", "-e", &foreign_sha]),
        "the daemon's status lazy-fetched the foreign commit into the clone"
    );
    assert!(!ran.exists(), "remote.evil.uploadpack ran as the daemon");
    let e = outcome.unwrap_err().to_string();
    assert!(e.starts_with("f/c/p: git "), "{e}");

    // (b) the harvest: the clone cannot serve a commit it does not have,
    // so the fetch fails and nothing foreign reaches the cache
    let e = ws
        .harvest_and_remove("f/c/p", &crew, &paths)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/p: git "), "{e}");
    assert!(paths.workspace.exists());
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

    // an `alternates` file in the cache itself is refused, by name
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
    assert!(paths.workspace.exists());
}
