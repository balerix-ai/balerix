#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_balerix-agent");

#[test]
fn version_prints_the_core_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("balerix-agent {}", env!("CARGO_PKG_VERSION"))
    );
}

/// Spec O §13's smoke test: a sidecar with no configuration exits 1 with
/// a message, and the message is the termination log's one line.
#[test]
fn a_sidecar_without_a_bundle_exits_one_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("termination-log");
    let out = Command::new(BIN)
        .args([
            "sidecar",
            "--bundle",
            "/nonexistent/agent.json",
            "--termination-log",
        ])
        .arg(&log)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot read the agent bundle at /nonexistent/agent.json"),
        "{stderr}"
    );
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.starts_with("cannot read the agent bundle at /nonexistent/agent.json"));
    assert_eq!(logged.lines().count(), 1);
}
