#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Jobs' commands end with one line in the termination log, which is
//! all the operator reads of them (Spec O §20.3).
mod support;

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_balerix-agent");

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
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn upstream(root: &Path) -> (String, String) {
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
    (
        format!("file://{}", bare.display()),
        git(&work, &["rev-parse", "HEAD"]),
    )
}

struct Dirs {
    shared: std::path::PathBuf,
    scratch: std::path::PathBuf,
    log: std::path::PathBuf,
}

fn dirs(root: &Path) -> Dirs {
    let shared = root.join("shared");
    for d in ["repo", "crew", "fleet", "daemon"] {
        std::fs::create_dir_all(shared.join(d)).unwrap();
    }
    Dirs {
        shared,
        scratch: root.join("scratch"),
        log: root.join("termination-log"),
    }
}

fn run(d: &Dirs, args: &[&str]) -> (Option<i32>, String) {
    let _ = std::fs::remove_file(&d.log);
    let out = Command::new(BIN)
        .args(args)
        .arg("--shared-dir")
        .arg(&d.shared)
        .arg("--scratch-dir")
        .arg(&d.scratch)
        .arg("--termination-log")
        .arg(&d.log)
        .output()
        .unwrap();
    let logged = std::fs::read_to_string(&d.log).unwrap_or_default();
    assert_eq!(logged.lines().count(), 1, "one line: {logged:?} / {out:?}");
    (out.status.code(), logged.trim_end().to_string())
}

#[test]
fn crew_sync_reports_the_commit_and_a_missing_ref_as_a_cache_failure() {
    if support::tools().is_none() {
        return;
    }
    let root = support::temp_root("jobs-cli-crew-sync");
    let (url, sha) = upstream(&root);
    let d = dirs(&root);
    let (code, line) = run(
        &d,
        &[
            "crew-sync",
            "--crew",
            "f/c",
            "--repo",
            &url,
            "--ref",
            "main",
        ],
    );
    assert_eq!((code, line.as_str()), (Some(0), sha.as_str()));
    assert!(d.shared.join("crew/no-hooks").is_dir());
    let (code, line) = run(
        &d,
        &[
            "crew-sync",
            "--crew",
            "f/c",
            "--repo",
            &url,
            "--ref",
            "nope",
        ],
    );
    assert_eq!(
        (code, line.as_str()),
        (Some(1), "cache: f/c: the remote has no branch nope")
    );
}

#[test]
fn pool_sync_says_synced_and_wants_a_fleet_for_the_fleet_level() {
    if support::tools().is_none() {
        return;
    }
    let root = support::temp_root("jobs-cli-pool-sync");
    let d = dirs(&root);
    // an explicit empty table: the embedded one would download claude
    let (code, line) = run(&d, &["pool-sync", "--level", "daemon", "--no-tools"]);
    assert_eq!((code, line.as_str()), (Some(0), "synced"));
    assert!(d.shared.join("daemon/mise").is_dir());
    let (code, line) = run(&d, &["pool-sync", "--level", "fleet", "--fleet", "f"]);
    assert_eq!((code, line.as_str()), (Some(0), "synced"));
    assert!(d.shared.join("fleet/mise.installed").is_file());
    let (code, line) = run(&d, &["pool-sync", "--level", "fleet"]);
    assert_eq!(code, Some(1));
    assert_eq!(line, "pool-sync --level fleet needs --fleet <name>");
}

#[test]
fn a_tool_that_is_not_name_equals_version_is_refused_by_the_parser() {
    let out = Command::new(BIN)
        .args([
            "crew-sync",
            "--crew",
            "f/c",
            "--repo",
            "o/r",
            "--ref",
            "main",
            "--tool",
            "node",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("expected <name>=<version>"), "{stderr}");
}

#[test]
fn harvest_with_no_clone_says_so() {
    if support::tools().is_none() {
        return;
    }
    let root = support::temp_root("jobs-cli-harvest");
    let (url, _) = upstream(&root);
    let d = dirs(&root);
    let (code, _) = run(
        &d,
        &[
            "crew-sync",
            "--crew",
            "f/c",
            "--repo",
            &url,
            "--ref",
            "main",
        ],
    );
    assert_eq!(code, Some(0));
    let claim = root.join("agent");
    std::fs::create_dir_all(&claim).unwrap();
    let (code, line) = run(
        &d,
        &[
            "harvest",
            "--agent",
            "f/c/a",
            "--agent-dir",
            &claim.display().to_string(),
        ],
    );
    assert_eq!((code, line.as_str()), (Some(0), "nothing to harvest"));
    let (code, line) = run(
        &d,
        &[
            "harvest",
            "--agent",
            "not-an-id",
            "--agent-dir",
            &claim.display().to_string(),
        ],
    );
    assert_eq!(code, Some(1));
    assert!(line.starts_with("--agent not-an-id: "), "{line}");
}

#[test]
fn crew_remove_ends_with_removed_and_empties_the_slice() {
    let root = support::temp_root("crew-remove-cli");
    let shared = root.join("shared");
    std::fs::create_dir_all(shared.join("repo/.git")).unwrap();
    std::fs::create_dir_all(shared.join("crew/mise")).unwrap();
    std::fs::create_dir_all(shared.join("fleet/mise")).unwrap();
    let log = root.join("termination-log");
    let out = Command::new(BIN)
        .args(["crew-remove", "--crew", "f/c"])
        .arg("--shared-dir")
        .arg(&shared)
        .arg("--scratch-dir")
        .arg(root.join("scratch"))
        .arg("--termination-log")
        .arg(&log)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "removed\n");
    assert!(
        std::fs::read_dir(shared.join("repo"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        std::fs::read_dir(shared.join("crew"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(shared.join("fleet/mise").is_dir());

    // a bad crew name is the command's refusal, as the termination message,
    // before anything is removed: a full slice stays full
    std::fs::create_dir_all(shared.join("repo/.git")).unwrap();
    std::fs::create_dir_all(shared.join("crew/mise")).unwrap();
    let out = Command::new(BIN)
        .args(["crew-remove", "--crew", "not-a-crew"])
        .arg("--shared-dir")
        .arg(&shared)
        .arg("--termination-log")
        .arg(&log)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .starts_with("--crew not-a-crew:")
    );
    assert!(shared.join("repo/.git").is_dir() && shared.join("crew/mise").is_dir());
}
