#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

const PAYMENTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/payments.yaml");

/// A `balerix` command with an empty HOME so the real host is never read.
fn balerix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("GH_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME");
    cmd
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let p = dir.join(name);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
    p
}

#[test]
fn resolves_the_example_to_yaml() {
    let home = tempfile::tempdir().unwrap();
    balerix(home.path())
        .args(["config", "resolve", PAYMENTS, "--no-host-defaults"])
        .assert()
        .success()
        .stdout(predicate::str::contains("name: payments"))
        .stdout(predicate::str::contains("opus"))
        .stdout(predicate::str::contains("22.11.0"))
        .stdout(predicate::str::contains("type: tmux"));
}

#[test]
fn json_flag_prints_parseable_json() {
    let home = tempfile::tempdir().unwrap();
    let out = balerix(home.path())
        .args([
            "config",
            "resolve",
            PAYMENTS,
            "--no-host-defaults",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["name"], "payments");
    assert_eq!(
        v["crews"]["backend"]["agents"]["bob"]["claude"]["settings"]["model"],
        "opus"
    );
}

#[test]
fn name_flag_overrides_the_file() {
    let home = tempfile::tempdir().unwrap();
    balerix(home.path())
        .args([
            "config",
            "resolve",
            PAYMENTS,
            "--no-host-defaults",
            "--name",
            "renamed",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("name: renamed"));
}

#[test]
fn host_defaults_are_layered_beneath_the_file() {
    let home = tempfile::tempdir().unwrap();
    write(
        home.path(),
        ".claude/settings.json",
        r#"{"theme":"dark","model":"haiku"}"#,
    );
    balerix(home.path())
        .args(["config", "resolve", PAYMENTS])
        .assert()
        .success()
        .stdout(predicate::str::contains("theme: dark"))
        .stdout(predicate::str::contains("model: sonnet"))
        .stdout(predicate::str::contains("haiku").not());
}

#[test]
fn credentials_never_reach_output() {
    let home = tempfile::tempdir().unwrap();
    write(
        home.path(),
        ".claude/.credentials.json",
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-FAKE-cred"}}"#,
    );
    write(
        home.path(),
        ".claude.json",
        r#"{"oauthAccount":{"emailAddress":"nobody@example.invalid"}}"#,
    );
    write(
        home.path(),
        ".config/gh/hosts.yml",
        "github.com:\n    oauth_token: gho_FAKE_token\n",
    );
    balerix(home.path())
        .args(["config", "resolve", PAYMENTS])
        .assert()
        .success()
        .stdout(predicate::str::contains("sk-ant-FAKE-cred").not())
        .stdout(predicate::str::contains("gho_FAKE_token").not())
        .stdout(predicate::str::contains("nobody@example.invalid").not())
        .stderr(predicate::str::contains("sk-ant-FAKE-cred").not())
        .stderr(predicate::str::contains("gho_FAKE_token").not())
        .stderr(predicate::str::contains("nobody@example.invalid").not());
}

#[test]
fn invalid_config_exits_1_with_the_path_on_stderr() {
    let home = tempfile::tempdir().unwrap();
    let file = write(
        home.path(),
        "bad.yaml",
        "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  backend:\n    repo: acme/api\n    agents:\n      alice: { tools: { node: \"22\" } }\n",
    );
    balerix(home.path())
        .args([
            "config",
            "resolve",
            file.to_str().unwrap(),
            "--no-host-defaults",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::starts_with(
            "error: crews.backend.agents.alice.tools.node: expected an exact version",
        ));
}

#[test]
fn missing_file_exits_1() {
    let home = tempfile::tempdir().unwrap();
    balerix(home.path())
        .args([
            "config",
            "resolve",
            "/nope/fleet.yaml",
            "--no-host-defaults",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("failed to read /nope/fleet.yaml"));
}
