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
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
        cache_is_read_only: false,
        git_read: &[],
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
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
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
        "the clone's .git is refused before any I/O"
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
    assert!(!marker.exists(), "the filter ran under version");
    git(w, &["config", "--unset", "filter.pwn.clean"]);
    assert!(rt.diff(&id, "origin/main").is_ok(), "unset: diffs again");

    // a filter in a file the repository config includes. Outside what the
    // git profile reads, the include itself cannot be read and the probe
    // fails (#108); in the clone, the probe reads it (`--includes`) and
    // names the filter key, not the include.
    let filter = format!("[filter \"inc\"]\n\tclean = {}\n", hook.display());
    let outside = root.join("included.config");
    std::fs::write(&outside, &filter).unwrap();
    git(
        w,
        &["config", "include.path", &outside.display().to_string()],
    );
    let e = rt.diff(&id, "origin/main").unwrap_err();
    assert!(
        matches!(&e, WorkspaceError::Tool { subcommand, .. } if subcommand == "config"),
        "{e}"
    );
    assert!(!marker.exists(), "the filter ran");
    let e = rt.version(&id, "origin/main").unwrap_err();
    assert!(
        matches!(&e, WorkspaceError::Tool { subcommand, .. } if subcommand == "config"),
        "version fails at the probe as diff does: {e:?}"
    );
    assert!(!marker.exists(), "the filter ran under version");
    git(w, &["config", "--unset", "include.path"]);
    let included = w.join(".git/included.config");
    std::fs::write(&included, &filter).unwrap();
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
    // .git/config.worktree, a file `--local` cannot see; refused on the
    // extension key itself instead.
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

    // a promisor remote (#67): a diff over a missing object would fetch
    // it from `remote.<x>.url` as the daemon and run `remote.<x>.uploadpack`
    // on a git older than 2.45.1, whatever `GIT_NO_LAZY_FETCH` says.
    // Refused by key, the extension first and any promisor remote alone.
    for (k, v) in [
        ("core.repositoryformatversion", "1"),
        ("extensions.partialClone", "evil"),
        ("remote.evil.promisor", "true"),
        ("remote.evil.url", "/nonexistent"),
    ] {
        git(w, &["config", k, v]);
    }
    assert_eq!(
        rt.diff(&id, "origin/main").unwrap_err(),
        WorkspaceError::Filter {
            key: "extensions.partialclone".into()
        }
    );
    git(w, &["config", "--unset", "extensions.partialClone"]);
    assert_eq!(
        rt.diff(&id, "origin/main").unwrap_err(),
        WorkspaceError::Filter {
            key: "remote.evil.promisor".into()
        }
    );
    assert!(
        matches!(
            rt.version(&id, "origin/main"),
            Err(WorkspaceError::Filter { .. })
        ),
        "version is refused by the same check"
    );
    assert!(!marker.exists(), "the filter ran under version");
    git(w, &["config", "--unset", "remote.evil.promisor"]);
    assert!(rt.diff(&id, "origin/main").is_ok(), "unset: diffs again");

    // a missing base is a git error naming the subcommand
    let e = rt.diff(&id, "origin/nope").unwrap_err();
    assert!(
        matches!(&e, WorkspaceError::Tool { subcommand, .. } if subcommand == "merge-base"),
        "{e}"
    );

    // Last, because it breaks the worktree: with its `.git` directory
    // deleted — the agent owns the clone — git discovery would walk up and
    // run every command below in whatever repository contains the state
    // root (under `target/tmp`, this checkout). `GIT_CEILING_DIRECTORIES`
    // stops it at the agent's root, so the first call fails instead.
    std::fs::remove_dir_all(w.join(".git")).unwrap();
    let e = rt.diff(&id, "origin/main").unwrap_err();
    assert!(
        matches!(&e, WorkspaceError::Tool { subcommand, .. } if subcommand == "config"),
        "{e}"
    );
}

/// A clone ready for the reader: `bare_repo`'s, with one committed and one
/// uncommitted change and an untracked file.
fn clone_with_changes(
    root: &Path,
    tools: &balerix_runtime::ToolPaths,
) -> (AgentId, balerix_runtime::StateLayout) {
    let layout = support::layout(root);
    let repo = bare_repo(root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools,
        gh_config_dir: None,
        cache_is_read_only: false,
        git_read: &[],
    };
    ws.ensure_repo("f/c/a", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    let w = &paths.workspace;
    std::fs::write(w.join("README"), "hi\ncommitted\n").unwrap();
    git(w, &["commit", "-q", "-am", "agent work"]);
    std::fs::write(w.join("LICENSE"), "mit\nedited\n").unwrap();
    std::fs::write(w.join("notes.txt"), "todo\n").unwrap();
    (id, layout)
}

/// #108: every git call the workspace reader makes runs under the git
/// profile, as the clone step's do
/// (`workspace_it::daemon_git_in_a_clone_runs_under_the_git_profile`).
#[test]
fn the_reader_runs_git_under_the_git_profile() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("inspect-sandboxed");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (id, layout) = clone_with_changes(&root, &tools);
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let log_path = crew.logs.join("git.log");
    std::fs::write(&log_path, "").unwrap();
    // a typechange, which git prints as two sections under one header
    let w = &paths.workspace;
    std::fs::remove_file(w.join("README")).unwrap();
    std::os::unix::fs::symlink("LICENSE", w.join("README")).unwrap();

    let files_under = |dir: &Path| -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                out.push(e.path().display().to_string());
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                }
            }
        }
        out.sort();
        out
    };
    let nono_before = files_under(&paths.nono_home);
    let root_before = files_under(&paths.root);

    let rt = Runtime::new(layout.clone(), tools.clone());
    let d = rt.diff(&id, "origin/main").unwrap();
    let names: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["LICENSE", "README", "notes.txt"]);
    assert_eq!(d.files[1].status, FileStatus::Typechange);
    // the combined diff, split, is what one diff per file printed
    for f in &d.files[..2] {
        assert_eq!(
            f.patch,
            git(
                w,
                &[
                    "diff",
                    "--no-color",
                    "-U3",
                    "--find-renames",
                    &d.merge_base,
                    "--",
                    &f.path
                ]
            ),
            "{}",
            f.path
        );
    }
    assert_eq!(d.files[1].patch.matches("diff --git ").count(), 2);
    assert!(
        d.files[2].patch.contains("+todo"),
        "an untracked file diffs against /dev/null: {}",
        d.files[2].patch
    );
    rt.version(&id, "origin/main").unwrap();

    // nono's per-run state goes to a per-call home that is removed again:
    // nothing new under the agent's own nono home (its live session's),
    // and no temp home left beside it. The profile and the logs are the
    // only new files.
    assert_eq!(files_under(&paths.nono_home), nono_before);
    let new_files: Vec<String> = files_under(&paths.root)
        .into_iter()
        .filter(|f| !root_before.contains(f))
        .collect();
    let expected = [
        paths.git_profile.clone(),
        paths.logs.clone(),
        paths.logs.join("nono-git.log"),
        paths.logs.join("nono.validate.log"),
        paths.nono_home.clone(),
    ];
    for f in &new_files {
        assert!(
            expected.iter().any(|e| e.display().to_string() == *f),
            "{f} is new (a temp home left behind?): {new_files:#?}"
        );
    }

    let log = std::fs::read_to_string(&log_path).unwrap();
    // no audit trail per call either (#108)
    let profile_arg = format!("run --no-audit --profile {}", paths.git_profile.display());
    let calls: Vec<&str> = log.lines().filter(|l| l.starts_with("$ ")).collect();
    assert_eq!(
        calls.len(),
        7,
        "diff: the filter probe, status, merge-base, name-status, one combined \
         patch, one --no-index; version: one script (#108, #174): {log}"
    );
    for line in &calls {
        assert!(
            line.contains(&profile_arg),
            "a reader git call ran outside the git profile: {line}"
        );
    }
    assert!(
        !log.contains("+todo") && !log.contains("committed"),
        "the log keeps argv, not the diff: {log}"
    );
}

/// #174: `version` is one sandboxed call — the filter probe, `merge-base`,
/// the name-only diff and `status` in one `/bin/sh` under the git profile —
/// logged as one argv-only `git.log` entry, and its fingerprint is the one
/// the separate git calls give.
#[test]
fn version_is_one_sandboxed_call() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("inspect-version-once");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (id, layout) = clone_with_changes(&root, &tools);
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let w = &paths.workspace;
    let log_path = crew.logs.join("git.log");
    let rt = Runtime::new(layout.clone(), tools.clone());
    // the profile is written (and validated) by the first call
    rt.version(&id, "origin/main").unwrap();
    std::fs::write(&log_path, "").unwrap();

    let v = rt.version(&id, "origin/main").unwrap();
    let calls = |log: &str| -> Vec<String> {
        log.lines()
            .filter(|l| l.starts_with("$ "))
            .map(str::to_string)
            .collect()
    };
    let log = std::fs::read_to_string(&log_path).unwrap();
    let one = calls(&log);
    assert_eq!(one.len(), 1, "one sandboxed call per version: {log}");
    let profile_arg = format!("run --no-audit --profile {}", paths.git_profile.display());
    assert!(one[0].contains(&profile_arg), "{}", one[0]);
    assert!(one[0].contains("/bin/sh -c"), "{}", one[0]);
    assert!(!log.contains("README"), "argv only, no stdout: {log}");

    // the fingerprint the four separate calls would give
    let head = git(w, &["rev-parse", "HEAD"]).trim().to_string();
    let merge_base = git(w, &["merge-base", "origin/main", "HEAD"])
        .trim()
        .to_string();
    let mut seen: std::collections::BTreeSet<String> =
        git(w, &["diff", "--name-only", "-z", &merge_base])
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
    let status = balerix_runtime::inspect::parse_status(&git(
        w,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
        ],
    ));
    seen.extend(status.changed);
    seen.extend(status.untracked);
    assert_eq!(
        seen.iter().map(String::as_str).collect::<Vec<_>>(),
        ["LICENSE", "README", "notes.txt"]
    );
    let entries: Vec<balerix_runtime::inspect::PathStat> = seen
        .into_iter()
        .map(|path| {
            use std::os::unix::fs::MetadataExt;
            let m = std::fs::symlink_metadata(w.join(&path)).unwrap();
            balerix_runtime::inspect::PathStat {
                path,
                stat: Some((m.len(), m.mtime(), m.mtime_nsec())),
            }
        })
        .collect();
    assert_eq!(v.head, head);
    assert_eq!(
        v.fingerprint,
        balerix_runtime::inspect::fingerprint_of(&head, &merge_base, &entries)
    );

    // a refusal and a git failure are one call too, and keep their shape;
    // README is wired to the filter, so a status or diff would run it
    let marker = root.join("filter-ran");
    let hook = root.join("filter.sh");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(w.join(".gitattributes"), "README filter=pwn\n").unwrap();
    git(
        w,
        &["config", "filter.pwn.clean", &hook.display().to_string()],
    );
    std::fs::write(&log_path, "").unwrap();
    assert_eq!(
        rt.version(&id, "origin/main"),
        Err(WorkspaceError::Filter {
            key: "filter.pwn.clean".into()
        })
    );
    assert!(!marker.exists(), "the filter ran under version");
    assert_eq!(calls(&std::fs::read_to_string(&log_path).unwrap()).len(), 1);
    git(w, &["config", "--unset", "filter.pwn.clean"]);
    std::fs::remove_file(w.join(".gitattributes")).unwrap();
    let e = rt.version(&id, "origin/nope").unwrap_err();
    assert!(
        matches!(&e, WorkspaceError::Tool { subcommand, stderr, args, .. }
            if subcommand == "merge-base" && stderr.contains("origin/nope")
                && args.ends_with(&["merge-base".into(), "origin/nope".into(), "HEAD".into()])),
        "{e:?}"
    );
}

/// #108: a clone pointed at another repository's objects (an `alternates`
/// line the agent wrote) shows the operator nothing of that repository
/// through the reader: under the git profile its objects cannot be read.
/// Positive control: plain git in the same clone reads them.
#[test]
fn the_reader_cannot_read_another_repositorys_objects() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("inspect-foreign");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (id, layout) = clone_with_changes(&root, &tools);
    let paths = layout.agent(&id);
    let w = &paths.workspace;

    // another repository with the same history, so the merge-base exists
    // and only the secret commit's objects are foreign
    let foreign = root.join("foreign");
    git(
        &root,
        &[
            "clone",
            "-q",
            &root.join("upstream.git").display().to_string(),
            &foreign.display().to_string(),
        ],
    );
    std::fs::write(foreign.join("secret.txt"), "TOP SECRET\n").unwrap();
    git(&foreign, &["add", "."]);
    git(&foreign, &["commit", "-q", "-m", "secret"]);
    let secret = git(&foreign, &["rev-parse", "HEAD"]).trim().to_string();

    let alternates = w.join(".git/objects/info/alternates");
    let mut lines = std::fs::read_to_string(&alternates).unwrap_or_default();
    lines.push_str(&format!("{}\n", foreign.join(".git/objects").display()));
    std::fs::write(&alternates, lines).unwrap();
    git(w, &["update-ref", "refs/heads/stolen", &secret]);
    assert!(
        git(w, &["diff", "--no-color", "origin/main", "stolen"]).contains("+TOP SECRET"),
        "plain git reads the foreign objects through the alternates line"
    );

    let rt = Runtime::new(layout.clone(), tools.clone());
    git(w, &["reset", "-q", "--hard", "stolen"]);
    // deleted in the worktree, so a diff would print the blob
    std::fs::remove_file(w.join("secret.txt")).unwrap();
    let e = rt.diff(&id, "origin/main").unwrap_err();
    assert!(!format!("{e:?}").contains("TOP SECRET"), "{e:?}");
    // refused by the sandbox at the object read, not by some other error
    let object_read_denied = |e: &WorkspaceError| match e {
        WorkspaceError::Tool { stderr, .. } => {
            stderr.contains(&format!(
                "unable to open loose object {secret}: Permission denied"
            )) && stderr.contains(&format!(
                "{}: Permission denied",
                foreign.join(".git/objects/pack").display()
            ))
        }
        _ => false,
    };
    assert!(object_read_denied(&e), "{e:?}");
    let e = rt.version(&id, "origin/main").unwrap_err();
    assert!(object_read_denied(&e), "{e:?}");
}

/// Past `WORKSPACE_FILE_COUNT_LIMIT` the kept files still get their
/// patches from the one combined diff, restricted to them by pathspec.
#[test]
fn a_truncated_diff_still_patches_every_file_it_keeps() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("inspect-truncated");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (id, layout) = clone_with_changes(&root, &tools);
    let paths = layout.agent(&id);
    let w = &paths.workspace;
    for i in 0..balerix_api::WORKSPACE_FILE_COUNT_LIMIT + 5 {
        std::fs::write(w.join(format!("f{i:04}")), format!("{i}\n")).unwrap();
    }
    git(w, &["add", "."]);
    git(w, &["commit", "-q", "-m", "many"]);
    let log_path = layout.crew(&id.crew_ref()).logs.join("git.log");
    std::fs::write(&log_path, "").unwrap();
    let rt = Runtime::new(layout.clone(), tools.clone());
    let d = rt.diff(&id, "origin/main").unwrap();
    assert!(d.truncated);
    assert_eq!(d.files.len(), balerix_api::WORKSPACE_FILE_COUNT_LIMIT);
    for f in &d.files {
        assert!(
            f.patch
                .starts_with(&format!("diff --git a/{0} b/{0}\n", f.path)),
            "{}: {}",
            f.path,
            f.patch
        );
    }
    assert!(
        !d.files.iter().any(|f| f.path == "notes.txt"),
        "the untracked file sorts past the cap"
    );
    let log = std::fs::read_to_string(&log_path).unwrap();
    let calls: Vec<&str> = log.lines().filter(|l| l.starts_with("$ ")).collect();
    assert_eq!(calls.len(), 5, "no per-file diff: {log}");
    assert!(
        calls[4].contains(" --literal-pathspecs diff "),
        "{}",
        calls[4]
    );
}

/// What the agent's clone may hold that would mislead the split of the one
/// combined diff (#108 review): prefixes its config changes, two renames
/// that print the same `diff --git` line, and patches far over the
/// per-file cap.
#[test]
fn the_combined_diff_holds_up_against_what_the_clone_can_do() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("inspect-combined");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (id, layout) = clone_with_changes(&root, &tools);
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let w = &paths.workspace;
    let rt = Runtime::new(layout.clone(), tools.clone());
    let log_path = crew.logs.join("git.log");
    let diff_calls = |log: &str| {
        log.lines()
            .filter(|l| l.starts_with("$ ") && l.contains(" diff "))
            .count()
    };

    // mnemonic and custom prefixes in the clone's config: still one
    // combined diff (and one --no-index for the untracked file)
    git(w, &["config", "diff.mnemonicPrefix", "true"]);
    git(w, &["config", "diff.srcPrefix", "x/"]);
    std::fs::write(&log_path, "").unwrap();
    let d = rt.diff(&id, "origin/main").unwrap();
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert_eq!(
        diff_calls(&log),
        3,
        "name-status, combined, --no-index: {log}"
    );
    assert!(
        d.files[0]
            .patch
            .starts_with("diff --git a/LICENSE b/LICENSE\n"),
        "{}",
        d.files[0].patch
    );
    git(w, &["config", "--unset", "diff.mnemonicPrefix"]);
    git(w, &["config", "--unset", "diff.srcPrefix"]);

    // `p` → `q b/r` and `p b/q` → `r` both print `diff --git a/p b/q b/r`
    std::fs::write(w.join("p"), "first file\n".repeat(20)).unwrap();
    std::fs::create_dir_all(w.join("p b")).unwrap();
    std::fs::write(w.join("p b/q"), "second file\n".repeat(20)).unwrap();
    git(w, &["add", "-A"]);
    git(w, &["commit", "-q", "-m", "two files"]);
    git(
        &root,
        &[
            "-C",
            &w.display().to_string(),
            "push",
            "-q",
            "origin",
            "HEAD:main",
        ],
    );
    git(w, &["fetch", "-q", "origin"]);
    std::fs::create_dir_all(w.join("q b")).unwrap();
    git(w, &["mv", "p", "q b/r"]);
    git(w, &["mv", "p b/q", "r"]);
    git(w, &["commit", "-q", "-m", "swap"]);
    let d = rt.diff(&id, "origin/main").unwrap();
    let by = |p: &str| d.files.iter().find(|f| f.path == p).unwrap().clone();
    let one = by("q b/r");
    let two = by("r");
    assert_eq!(one.old_path.as_deref(), Some("p"));
    assert_eq!(two.old_path.as_deref(), Some("p b/q"));
    assert!(
        one.patch.contains("rename to q b/r") && !one.patch.contains("rename to r\n"),
        "{}",
        one.patch
    );
    assert!(
        two.patch.contains("rename to r\n") && !two.patch.contains("rename to q b/r"),
        "{}",
        two.patch
    );

    // three files, each ~2 MiB of change: each patch is capped and shaped
    // exactly as git's own diff of that file alone would be
    let big = "+".repeat(99) + "\n";
    for name in ["big1", "big2", "big3"] {
        std::fs::write(w.join(name), big.repeat(20_000)).unwrap();
    }
    git(w, &["add", "big1", "big2", "big3"]);
    git(w, &["commit", "-q", "-m", "big"]);
    std::fs::write(&log_path, "").unwrap();
    let d = rt.diff(&id, "origin/main").unwrap();
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        diff_calls(&log) <= 5,
        "one combined diff; per-file only for notes.txt and the ambiguous renames: {log}"
    );
    assert!(!log.contains(" -- big"), "{log}");
    for name in ["big1", "big2", "big3"] {
        let f = d.files.iter().find(|f| f.path == name).unwrap();
        assert!(f.truncated && !f.binary, "{name}");
        assert!(f.patch.len() <= balerix_api::WORKSPACE_PATCH_LIMIT);
        let own = git(
            w,
            &[
                "diff",
                "--no-color",
                "-U3",
                "--find-renames",
                &d.merge_base,
                "--",
                name,
            ],
        );
        let line_end = own.as_bytes()[..balerix_api::WORKSPACE_PATCH_LIMIT]
            .iter()
            .rposition(|b| *b == b'\n')
            .unwrap();
        assert_eq!(f.patch, own[..=line_end], "{name}");
    }
}
