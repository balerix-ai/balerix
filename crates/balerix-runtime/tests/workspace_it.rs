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
