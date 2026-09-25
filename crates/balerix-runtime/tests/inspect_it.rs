//! Spec C §3.2 against real git: the five change kinds, the path refusals
//! and the fsmonitor control.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use balerix_api::{EntryKind, FileStatus, WORKSPACE_FILE_LIMIT};
use balerix_core::{AgentId, RepoRef, WorkspaceError, WorkspaceReader};
use balerix_runtime::{Runtime, Workspace};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
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

/// A bare repo with README and LICENSE on `main`, served over file://.
fn bare_repo(root: &Path) -> RepoRef {
    let work = root.join("upstream-work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README"), "hi\n").unwrap();
    std::fs::write(work.join("LICENSE"), "mit\n").unwrap();
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
fn the_diff_reports_every_change_kind_and_reads_stay_inside_the_worktree() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("inspect");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    let rt = Runtime::new(layout.clone(), tools.clone());

    // no worktree yet
    assert_eq!(
        rt.diff(&id, "origin/main"),
        Err(WorkspaceError::Missing("f/c/a".into()))
    );
    assert_eq!(
        rt.version(&id, "origin/main"),
        Err(WorkspaceError::Missing("f/c/a".into()))
    );

    ws.ensure_repo("f/c/a", &crew, &repo, "main").unwrap();
    ws.ensure_worktree(
        "f/c/a",
        &crew,
        &paths.workspace,
        &paths.branch_marker(),
        "balerix/f/c/a",
        "main",
    )
    .unwrap();
    let w = &paths.workspace;
    let base = git(w, &["rev-parse", "origin/main"]).trim().to_string();

    // committed: a rename, a modification, a binary, a new file
    git(w, &["mv", "LICENSE", "COPYING"]);
    std::fs::write(w.join("README"), "hi\nmore\n").unwrap();
    std::fs::write(w.join("img.bin"), [0u8, 1, 2, 255, 0, 7]).unwrap();
    std::fs::create_dir_all(w.join("src")).unwrap();
    std::fs::write(w.join("src/lib.rs"), "fn a() {}\n").unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "agent work"]);
    let head = git(w, &["rev-parse", "HEAD"]).trim().to_string();
    // uncommitted: an edit and an untracked file
    std::fs::write(w.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    std::fs::write(w.join("notes.txt"), "todo\n").unwrap();

    let d = rt.diff(&id, "origin/main").unwrap();
    assert_eq!(
        (d.base_ref.as_str(), d.merge_base.clone(), d.head.clone()),
        ("origin/main", base, head.clone())
    );
    assert!(!d.truncated);
    let names: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        names,
        vec!["COPYING", "README", "img.bin", "notes.txt", "src/lib.rs"],
        "path order"
    );
    let by = |p: &str| d.files.iter().find(|f| f.path == p).unwrap();
    let copying = by("COPYING");
    assert_eq!(
        (
            copying.status,
            copying.old_path.as_deref(),
            copying.uncommitted
        ),
        (FileStatus::Renamed, Some("LICENSE"), false)
    );
    assert!(
        copying.patch.contains("rename from LICENSE"),
        "{}",
        copying.patch
    );
    let readme = by("README");
    assert_eq!(
        (readme.status, readme.uncommitted, readme.binary),
        (FileStatus::Modified, false, false)
    );
    assert!(readme.patch.contains("+more"), "{}", readme.patch);
    let img = by("img.bin");
    assert_eq!(
        (img.status, img.binary, img.patch.as_str()),
        (FileStatus::Added, true, "")
    );
    let notes = by("notes.txt");
    assert_eq!((notes.status, notes.uncommitted), (FileStatus::Added, true));
    assert!(notes.patch.contains("+todo"), "{}", notes.patch);
    let lib = by("src/lib.rs");
    assert_eq!((lib.status, lib.uncommitted), (FileStatus::Added, true));
    assert!(lib.patch.contains("+fn b() {}"), "{}", lib.patch);
    assert!(
        d.files
            .iter()
            .all(|f| f.patch.is_empty() || f.patch.starts_with("diff --git ")),
        "every patch carries its header"
    );
    // what `git diff` itself says, minus nothing
    assert_eq!(
        readme.patch,
        git(
            w,
            &["diff", "--no-color", "-U3", &d.merge_base, "--", "README"]
        )
    );

    // the version: stable while nothing changes, sensitive to every kind of change
    let v0 = rt.version(&id, "origin/main").unwrap();
    assert_eq!(v0.head, head);
    assert_eq!(v0.fingerprint.len(), 64);
    assert_eq!(rt.version(&id, "origin/main").unwrap(), v0, "stable");
    std::fs::write(w.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    let v1 = rt.version(&id, "origin/main").unwrap();
    assert_ne!(v1.fingerprint, v0.fingerprint, "an edit");
    std::fs::write(w.join("fresh.txt"), "new\n").unwrap();
    let v2 = rt.version(&id, "origin/main").unwrap();
    assert_ne!(v2.fingerprint, v1.fingerprint, "an untracked file");
    git(w, &["add", "notes.txt"]);
    let v3 = rt.version(&id, "origin/main").unwrap();
    assert_eq!(
        v3.fingerprint, v2.fingerprint,
        "a byte-identical git add is invisible"
    );
    git(w, &["commit", "-q", "-m", "notes"]);
    let v4 = rt.version(&id, "origin/main").unwrap();
    assert_ne!(v4.fingerprint, v3.fingerprint, "a commit");
    assert_ne!(v4.head, v0.head);
    std::fs::remove_file(w.join("fresh.txt")).unwrap();
    let v5 = rt.version(&id, "origin/main").unwrap();
    assert_ne!(v5.fingerprint, v4.fingerprint, "a deletion");
    // put the tree back as the later assertions expect it
    git(w, &["reset", "-q", "--soft", "HEAD~1"]);
    git(w, &["reset", "-q", "notes.txt"]);
    std::fs::write(w.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    assert_eq!(
        rt.diff(&id, "origin/main").unwrap().files.len(),
        d.files.len(),
        "restored"
    );

    // file and tree
    assert_eq!(
        rt.read_file(&id, "src/lib.rs").unwrap(),
        b"fn a() {}\nfn b() {}\n".to_vec()
    );
    assert_eq!(rt.read_file(&id, "nope"), Err(WorkspaceError::NoSuchPath));
    assert_eq!(rt.read_file(&id, "src"), Err(WorkspaceError::NotAFile));
    assert_eq!(
        rt.read_file(&id, ".git"),
        Err(WorkspaceError::InvalidPath(".git segment".into())),
        "the worktree's .git file is refused before any I/O"
    );
    assert_eq!(
        rt.read_file(&id, "../../repo/HEAD"),
        Err(WorkspaceError::InvalidPath("\"..\" segment".into()))
    );
    std::os::unix::fs::symlink("/etc/hostname", w.join("escape")).unwrap();
    assert_eq!(
        rt.read_file(&id, "escape"),
        Err(WorkspaceError::NotAFile),
        "symlinks are not followed"
    );
    std::os::unix::fs::symlink(&crew.repo, w.join("repo-link")).unwrap();
    assert_eq!(
        rt.list_dir(&id, "repo-link"),
        Err(WorkspaceError::NotADirectory)
    );
    std::fs::write(
        w.join("big"),
        vec![b'x'; (WORKSPACE_FILE_LIMIT + 1) as usize],
    )
    .unwrap();
    assert_eq!(
        rt.read_file(&id, "big"),
        Err(WorkspaceError::TooLarge {
            limit: WORKSPACE_FILE_LIMIT
        })
    );
    let tree = rt.list_dir(&id, "").unwrap();
    let names: Vec<(&str, EntryKind)> = tree
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect();
    assert!(!names.iter().any(|(n, _)| *n == ".git"), "{names:?}");
    assert!(names.contains(&("src", EntryKind::Dir)));
    assert!(names.contains(&("escape", EntryKind::Symlink)));
    assert!(names.contains(&("img.bin", EntryKind::File)));
    let src = rt.list_dir(&id, "src").unwrap();
    assert_eq!(src.path, "src");
    assert_eq!(src.entries[0].name, "lib.rs");
    assert_eq!(src.entries[0].size, Some(20));
    assert_eq!(
        rt.list_dir(&id, "README"),
        Err(WorkspaceError::NotADirectory)
    );
    assert_eq!(rt.list_dir(&id, "nope"), Err(WorkspaceError::NoSuchPath));

    // repo config an agent could write must not run a program here
    let marker = root.join("fsmonitor-ran");
    let hook = root.join("fsmonitor.sh");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch {}\necho\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        w,
        &["config", "core.fsmonitor", &hook.display().to_string()],
    );
    git(w, &["config", "diff.external", &hook.display().to_string()]);
    let again = rt.diff(&id, "origin/main").unwrap();
    // `escape` and `repo-link` (untracked symlinks from the reads above) and
    // `big` are all untracked now, so three files join, not one.
    assert_eq!(
        again.files.len(),
        d.files.len() + 3,
        "big, escape and repo-link joined"
    );
    assert!(
        !marker.exists(),
        "core.fsmonitor / diff.external from repo config ran"
    );
    assert!(
        again
            .files
            .iter()
            .find(|f| f.path == "README")
            .unwrap()
            .patch
            .contains("+more")
    );

    // A nested repository the agent can create carries its own config, which
    // the `FILTER_KEYS` probe never reads: with `diff.submodule = diff` in the
    // crew config git descends into `sub` for the per-file diff, and there the
    // `-c` overrides still apply (they travel in `GIT_CONFIG_PARAMETERS`) but
    // the argv `--no-ext-diff` does not, so the nested `diff.external` would
    // run as the daemon. `--submodule=short` prints the two hashes instead and
    // `--ignore-submodules=dirty` drops the dirty check's own child process.
    let sub = w.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q", "-b", "main"]);
    std::fs::write(sub.join("f"), "a\n").unwrap();
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-q", "-m", "nested"]);
    git(
        &sub,
        &["config", "diff.external", &hook.display().to_string()],
    );
    git(w, &["add", "sub"]);
    git(w, &["commit", "-q", "-m", "nest a repository"]);
    // dirty in its own worktree: what the default dirty check would look at
    std::fs::write(sub.join("f"), "a\nb\n").unwrap();
    git(w, &["config", "diff.submodule", "diff"]);
    // the plain `git add`/`commit` above ran the planted `core.fsmonitor`
    // hook (they carry none of the `-c` overrides), so clear the marker: the
    // assertion below is about the nested repository alone.
    let _ = std::fs::remove_file(&marker);
    let nested = rt.diff(&id, "origin/main").unwrap();
    let gitlink = nested.files.iter().find(|f| f.path == "sub").unwrap();
    assert_eq!(gitlink.status, FileStatus::Added);
    assert!(
        !marker.exists(),
        "the nested repository's diff.external ran"
    );

    // README must be wired to the "pwn" filter, or nothing would ever run
    // it and the marker checks below would pass even without the guard.
    std::fs::write(w.join(".gitattributes"), "README filter=pwn\n").unwrap();

    // a clean/smudge/process filter in repo config would run as the daemon
    git(
        w,
        &["config", "filter.pwn.clean", &hook.display().to_string()],
    );
    // Positive control: plain git here does run the clean filter, so the
    // `!marker.exists()` assertions below say something. `--no-ext-diff`, so
    // only the filter can be what touched the marker (`diff.external` is
    // still set in this config from the block above).
    git(w, &["diff", "--no-ext-diff", "-U3", "HEAD", "--", "README"]);
    assert!(marker.exists(), "the plain git diff runs the clean filter");
    std::fs::remove_file(&marker).unwrap();
    let e = rt.diff(&id, "origin/main").unwrap_err();
    assert_eq!(
        e,
        WorkspaceError::Filter {
            key: "filter.pwn.clean".into()
        }
    );
    assert!(!marker.exists(), "the filter ran");
    assert!(
        matches!(
            rt.version(&id, "origin/main"),
            Err(WorkspaceError::Filter { .. })
        ),
        "version is refused by the same check"
    );
    git(w, &["config", "--unset", "filter.pwn.clean"]);
    assert!(rt.diff(&id, "origin/main").is_ok(), "unset: diffs again");

    // a filter in a file the repository config includes: the probe reads it
    // (`--includes`) and names the filter key, not the include.
    let included = root.join("included.config");
    std::fs::write(
        &included,
        format!("[filter \"inc\"]\n\tclean = {}\n", hook.display()),
    )
    .unwrap();
    git(
        w,
        &["config", "include.path", &included.display().to_string()],
    );
    assert_eq!(
        rt.diff(&id, "origin/main").unwrap_err(),
        WorkspaceError::Filter {
            key: "filter.inc.clean".into()
        }
    );
    assert!(!marker.exists(), "the filter ran");
    git(w, &["config", "--unset", "include.path"]);
    assert!(rt.diff(&id, "origin/main").is_ok(), "unset: diffs again");

    // extensions.worktreeConfig would let an agent write a filter into
    // <gitdir>/worktrees/<id>/config.worktree, a file `--local` cannot
    // see; refused on the extension key itself instead.
    git(w, &["config", "extensions.worktreeConfig", "true"]);
    git(
        w,
        &[
            "config",
            "--worktree",
            "filter.pwn.clean",
            &hook.display().to_string(),
        ],
    );
    let e = rt.diff(&id, "origin/main").unwrap_err();
    assert_eq!(
        e,
        WorkspaceError::Filter {
            key: "extensions.worktreeconfig".into()
        }
    );
    assert!(!marker.exists(), "the filter ran");
    // unset the filter while the extension is still on (so `--worktree`
    // still resolves to config.worktree), then the extension itself.
    git(w, &["config", "--worktree", "--unset", "filter.pwn.clean"]);
    git(w, &["config", "--unset", "extensions.worktreeConfig"]);
    assert!(rt.diff(&id, "origin/main").is_ok(), "unset: diffs again");
    std::fs::remove_file(w.join(".gitattributes")).unwrap();

    // a missing base is a git error naming the subcommand
    let e = rt.diff(&id, "origin/nope").unwrap_err();
    assert!(
        matches!(&e, WorkspaceError::Tool { subcommand, .. } if subcommand == "merge-base"),
        "{e}"
    );

    // Last, because it breaks the worktree: with its `.git` file deleted —
    // the agent owns the worktree — git discovery would walk up and run
    // every command below in whatever repository contains the state root
    // (under `target/tmp`, this checkout). `GIT_CEILING_DIRECTORIES` stops
    // it at the agent's root, so the first call fails instead.
    std::fs::remove_file(w.join(".git")).unwrap();
    let e = rt.diff(&id, "origin/main").unwrap_err();
    assert!(
        matches!(&e, WorkspaceError::Tool { subcommand, .. } if subcommand == "config"),
        "{e}"
    );
}
