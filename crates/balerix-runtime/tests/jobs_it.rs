#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §8.3, §8.4, §20.3: what the sync and harvest Jobs do, on a
//! shared slice laid out as the pod mounts it.
mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use balerix_core::{AgentId, CrewRef, RepoRef};
use balerix_runtime::jobs::{PoolLevel, SyncError, harvest, sync_crew, sync_pool};
use balerix_runtime::layout::{PodMounts, SharedSlice};
use balerix_runtime::{StateLayout, Workspace};

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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository with one commit on `main`, and the work tree it came
/// from (to push more).
fn upstream(root: &Path) -> (RepoRef, PathBuf) {
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
    git(&work, &["remote", "add", "up", &bare.display().to_string()]);
    (
        RepoRef::parse(&format!("file://{}", bare.display())).unwrap(),
        work,
    )
}

/// The slice as a Job finds it: the four mount points exist and are empty.
fn slice(root: &Path) -> SharedSlice {
    let s = SharedSlice::new(root.join("shared"));
    for d in ["repo", "crew", "fleet", "daemon"] {
        std::fs::create_dir_all(s.root.join(d)).unwrap();
    }
    s
}

fn crew() -> CrewRef {
    "f/c".parse().unwrap()
}

fn none() -> BTreeMap<String, String> {
    BTreeMap::new()
}

/// What a path holds: length, mtime and, for a regular file, its bytes.
type Entry = (u64, std::time::SystemTime, Option<Vec<u8>>);

fn listing(dir: &Path) -> BTreeMap<String, Entry> {
    fn walk(dir: &Path, out: &mut BTreeMap<String, Entry>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let m = std::fs::symlink_metadata(e.path()).unwrap();
            let bytes = m.is_file().then(|| std::fs::read(e.path()).unwrap());
            out.insert(
                e.path().display().to_string(),
                (m.len(), m.modified().unwrap(), bytes),
            );
            if m.is_dir() {
                walk(&e.path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

fn chmod_tree(dir: &Path, writable: bool) {
    let mode = |m: u32| if writable { m | 0o200 } else { m & !0o222 };
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        let meta = std::fs::symlink_metadata(&p).unwrap();
        if meta.is_dir() {
            if writable {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode(meta.mode())))
                    .unwrap();
            }
            chmod_tree(&p, writable);
        }
        if !meta.file_type().is_symlink() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode(meta.mode())))
                .unwrap();
        }
    }
}

#[test]
fn crew_sync_makes_the_cache_fetches_the_ref_and_reports_its_commit() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-crew-sync");
    let (repo, work) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    let first = sync_crew(
        &tools,
        &slice,
        &scratch,
        &crew(),
        &repo,
        "main",
        None,
        &none(),
    )
    .unwrap();
    assert_eq!(first, git(&work, &["rev-parse", "HEAD"]));
    let paths = slice.crew();
    assert!(paths.repo.join(".git").is_dir());
    assert_eq!(git(&paths.repo, &["config", "gc.auto"]), "0");
    assert!(paths.no_hooks().is_dir(), "the pod's git calls name it");
    // review focus 3: an empty table still leaves the pool and its marker
    assert!(paths.mise_pool().is_dir());
    assert!(paths.installed_marker().is_file());
    assert!(paths.logs.join("git.log").is_file());

    std::fs::write(work.join("more"), "x\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "more"]);
    git(&work, &["push", "-q", "up", "main"]);
    let second = sync_crew(
        &tools,
        &slice,
        &scratch,
        &crew(),
        &repo,
        "main",
        None,
        &none(),
    )
    .unwrap();
    assert_eq!(second, git(&work, &["rev-parse", "HEAD"]));
    assert_ne!(second, first, "an existing cache is fetched");
}

/// The gh token goes under `scratch/gh` for the credential helper, and
/// nowhere on the volume.
#[test]
fn a_gh_token_is_written_under_scratch_and_never_onto_the_volume() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-crew-sync-gh");
    let (repo, _work) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    let token = "gho_0123456789abcdef0123456789abcdef";
    sync_crew(
        &tools,
        &slice,
        &scratch,
        &crew(),
        &repo,
        "main",
        Some(token),
        &none(),
    )
    .unwrap();
    let hosts = std::fs::read_to_string(scratch.join("gh/hosts.yml")).unwrap();
    assert!(hosts.contains(token), "{hosts}");
    let git_log = std::fs::read_to_string(slice.crew().logs.join("git.log")).unwrap();
    assert!(git_log.contains("auth git-credential"), "{git_log}");
    for (path, (_, _, bytes)) in listing(&slice.root) {
        let held = bytes.is_some_and(|b| b.windows(token.len()).any(|w| w == token.as_bytes()));
        assert!(!held, "{path} holds the token");
    }
}

/// Review focus 2.
#[test]
fn a_ref_the_remote_lacks_is_a_cache_error_that_names_it() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-crew-sync-noref");
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let e = sync_crew(
        &tools,
        &slice,
        &root.join("scratch"),
        &crew(),
        &repo,
        "nope",
        None,
        &none(),
    )
    .unwrap_err();
    assert!(matches!(e, SyncError::Cache(_)), "{e:?}");
    assert_eq!(e.to_string(), "cache: f/c: the remote has no branch nope");
}

#[test]
fn a_remote_that_is_not_there_is_a_cache_error_and_a_later_run_recovers() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-crew-sync-noremote");
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    let gone = RepoRef::parse(&format!("file://{}/no-such.git", root.display())).unwrap();
    let e = sync_crew(
        &tools,
        &slice,
        &scratch,
        &crew(),
        &gone,
        "main",
        None,
        &none(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.starts_with("cache: f/c: "), "{e}");
    assert!(!slice.crew().repo.join(".git").exists());
    sync_crew(
        &tools,
        &slice,
        &scratch,
        &crew(),
        &repo,
        "main",
        None,
        &none(),
    )
    .unwrap();
}

/// A branch the remote deleted is not served from the cache's stale
/// remote-tracking ref.
#[test]
fn a_branch_deleted_on_the_remote_is_no_longer_synced() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-crew-sync-pruned");
    let (repo, work) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    sync_crew(
        &tools,
        &slice,
        &scratch,
        &crew(),
        &repo,
        "main",
        None,
        &none(),
    )
    .unwrap();
    git(&work, &["push", "-q", "up", "main:b"]);
    sync_crew(&tools, &slice, &scratch, &crew(), &repo, "b", None, &none()).unwrap();
    git(&work, &["push", "-q", "up", ":b"]);
    let e = sync_crew(&tools, &slice, &scratch, &crew(), &repo, "b", None, &none())
        .unwrap_err()
        .to_string();
    assert_eq!(e, "cache: f/c: the remote has no branch b");
}

/// Review focus 3, the upper pools.
#[test]
fn pool_sync_with_an_empty_table_still_leaves_the_pool_and_its_marker() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise", false));
        return;
    };
    let root = support::temp_root("jobs-pool-sync");
    let slice = slice(&root);
    let scratch = root.join("scratch");
    sync_pool(&tools, &slice, &scratch, &PoolLevel::Daemon, &none()).unwrap();
    assert!(slice.daemon_pool().is_dir());
    assert!(slice.daemon_root().join("mise.installed").is_file());
    let fleet = PoolLevel::Fleet("f".parse().unwrap());
    sync_pool(&tools, &slice, &scratch, &fleet, &none()).unwrap();
    assert!(slice.fleet().mise_pool().is_dir());
    assert!(slice.fleet().installed_marker().is_file());
    // a marker that outlived its pool must not short-circuit the install
    std::fs::remove_dir_all(slice.fleet().mise_pool()).unwrap();
    std::fs::remove_file(&slice.fleet().mise_toml).unwrap();
    sync_pool(&tools, &slice, &scratch, &fleet, &none()).unwrap();
    assert!(slice.fleet().mise_pool().is_dir());
    // the stale marker was dropped, so the install ran again and rewrote
    // the level file; a short-circuit would have left it missing
    assert!(slice.fleet().mise_toml.is_file());
}

/// A clone as the sidecar makes it, on a pod layout over `slice`, with
/// one unpushed commit. Returns the claim and the commit.
fn pod_clone(
    tools: &balerix_runtime::ToolPaths,
    root: &Path,
    slice: &SharedSlice,
    repo: &RepoRef,
    id: &AgentId,
) -> (PathBuf, String) {
    let claim = root.join("agent");
    let layout = StateLayout::pod(
        PodMounts {
            agent: claim.clone(),
            shared: slice.root.clone(),
            run: root.join("run"),
        },
        id,
    );
    let paths = layout.agent(id);
    Workspace {
        tools,
        gh_config_dir: None,
        cache_is_read_only: true,
    }
    .ensure_clone(
        &id.to_string(),
        &layout.crew(&id.crew_ref()),
        &paths,
        repo,
        "balerix/f/c/a",
        "main",
    )
    .unwrap();
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    (claim, sha)
}

/// Review focus 4.
#[test]
fn a_harvest_reads_a_claim_it_cannot_write_and_leaves_it_as_it_was() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("jobs-harvest");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    sync_crew(
        &tools,
        &slice,
        &scratch,
        &crew(),
        &repo,
        "main",
        None,
        &none(),
    )
    .unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    let (claim, sha) = pod_clone(&tools, &root, &slice, &repo, &id);
    chmod_tree(&claim, false);
    std::fs::set_permissions(&claim, std::fs::Permissions::from_mode(0o555)).unwrap();
    let before = listing(&claim);

    let got = harvest(&tools, &slice, &scratch, &id, &claim);
    let after = listing(&claim);
    std::fs::set_permissions(&claim, std::fs::Permissions::from_mode(0o755)).unwrap();
    chmod_tree(&claim, true); // let TempRoot remove it
    assert_eq!(got.unwrap().as_deref(), Some("balerix/f/c/a"));
    assert_eq!(
        git(
            &slice.crew().repo,
            &["rev-parse", "refs/heads/balerix/f/c/a"]
        ),
        sha
    );
    assert_eq!(after, before, "the harvest wrote under the claim");
    assert!(scratch.join("nono-git-profile.json").is_file());
    assert!(
        claim.join("workspace/work.txt").is_file(),
        "nothing is removed"
    );
}

#[test]
fn a_claim_with_no_clone_has_nothing_to_harvest() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("jobs-harvest-empty");
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    sync_crew(
        &tools,
        &slice,
        &scratch,
        &crew(),
        &repo,
        "main",
        None,
        &none(),
    )
    .unwrap();
    let claim = root.join("agent");
    std::fs::create_dir_all(&claim).unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    assert_eq!(
        harvest(&tools, &slice, &scratch, &id, &claim).unwrap(),
        None
    );
}

/// Spec N's case, on the Job's path: a nono that cannot run fails the
/// harvest; it is never read as "nothing to harvest".
#[test]
fn a_harvest_without_a_working_nono_fails() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git+mise+nono", false));
        return;
    };
    let root = support::temp_root("jobs-harvest-no-nono");
    let (repo, _) = upstream(&root);
    let slice = slice(&root);
    let scratch = root.join("scratch");
    sync_crew(
        &tools,
        &slice,
        &scratch,
        &crew(),
        &repo,
        "main",
        None,
        &none(),
    )
    .unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    let (claim, _) = pod_clone(&tools, &root, &slice, &repo, &id);
    let no_nono = balerix_runtime::ToolPaths {
        nono: root.join("no-such-nono"),
        ..tools.clone()
    };
    let e = harvest(&no_nono, &slice, &scratch, &id, &claim)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("f/c/a: "), "{e}");
    assert!(e.contains("no-such-nono"), "{e}");
}

#[test]
fn remove_crew_empties_the_slice_and_keeps_the_mount_points() {
    let root = support::temp_root("remove-crew");
    let slice = SharedSlice::new(root.join("shared"));
    let crew = slice.crew();
    // what a sync left behind: a cache, a pool, logs, and the fleet and
    // daemon pools the crew only reads
    std::fs::create_dir_all(crew.repo.join(".git/objects")).unwrap();
    std::fs::write(crew.repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir_all(crew.root.join("mise/installs")).unwrap();
    std::fs::create_dir_all(crew.root.join("no-hooks")).unwrap();
    std::fs::create_dir_all(&crew.logs).unwrap();
    std::fs::create_dir_all(slice.fleet().root.join("mise")).unwrap();
    std::fs::create_dir_all(slice.daemon_pool()).unwrap();

    balerix_runtime::jobs::remove_crew(&slice).unwrap();

    // the mount points stay (a Job cannot remove a mount), empty
    assert!(crew.repo.is_dir() && std::fs::read_dir(&crew.repo).unwrap().next().is_none());
    assert!(crew.root.is_dir() && std::fs::read_dir(&crew.root).unwrap().next().is_none());
    // what the crew only reads is untouched
    assert!(slice.fleet().root.join("mise").is_dir());
    assert!(slice.daemon_pool().is_dir());
    // a second run over an empty slice is fine
    balerix_runtime::jobs::remove_crew(&slice).unwrap();
}
