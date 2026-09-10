#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

const PAYMENTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/payments.yaml");

/// Fake tools on PATH so discovery succeeds without the real ones.
fn fake_tools(dir: &Path) {
    for t in ["git", "gh", "mise", "nono", "tmux"] {
        fs::write(dir.join(t), "#!/bin/sh\nexit 0\n").unwrap();
    }
}

fn balerix(home: &Path, tools: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home)
        .env("PATH", tools)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("GH_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME");
    cmd
}

#[test]
fn renders_the_four_files_with_redacted_credentials_by_default() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    // host credentials exist → must be redacted
    fs::create_dir_all(home.path().join(".claude")).unwrap();
    fs::write(
        home.path().join(".claude").join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"sk-SECRET"}}"#,
    )
    .unwrap();
    fs::create_dir_all(home.path().join(".config").join("gh")).unwrap();
    fs::write(
        home.path().join(".config").join("gh").join("hosts.yml"),
        "github.com:\n    oauth_token: gho_SECRET\n",
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    let assert = balerix(home.path(), tools.path())
        .args([
            "dev",
            "materialize",
            PAYMENTS,
            "backend/bob",
            "--out",
            &out.path().display().to_string(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent dir:"))
        .stdout(predicate::str::contains("launch.sh"))
        .stdout(predicate::str::contains("<redacted> placeholders"));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let agent_dir = stdout
        .lines()
        .next()
        .unwrap()
        .trim_start_matches("agent dir: ")
        .to_string();
    let settings =
        fs::read_to_string(Path::new(&agent_dir).join("home/.claude/settings.json")).unwrap();
    assert!(settings.contains("\"model\": \"opus\""));
    assert!(settings.contains("/v1/agents/payments/backend/bob/events"));
    assert!(settings.contains("hook-relay"));
    assert!(Path::new(&agent_dir).join("home/.gitconfig").exists());
    let creds =
        fs::read_to_string(Path::new(&agent_dir).join("home/.claude/.credentials.json")).unwrap();
    assert!(!creds.contains("SECRET") && creds.contains("<redacted>"));
    let hosts =
        fs::read_to_string(Path::new(&agent_dir).join("home/.config/gh/hosts.yml")).unwrap();
    assert!(!hosts.contains("SECRET"));
    assert!(
        fs::read_to_string(Path::new(&agent_dir).join("mise.toml"))
            .unwrap()
            .contains("python = \"3.12.8\"")
    );
    assert!(
        fs::read_to_string(Path::new(&agent_dir).join("nono-profile.json"))
            .unwrap()
            .contains("\"open_port\"")
    );
    let launch = fs::read_to_string(Path::new(&agent_dir).join("launch.sh")).unwrap();
    assert!(launch.starts_with("#!/bin/sh"));
    assert!(launch.contains("'--verbose'"));
}

#[test]
fn with_credentials_writes_real_values() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    fs::create_dir_all(home.path().join(".claude")).unwrap();
    fs::write(
        home.path().join(".claude").join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"sk-REAL"}}"#,
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    balerix(home.path(), tools.path())
        .args([
            "dev",
            "materialize",
            PAYMENTS,
            "backend/alice",
            "--out",
            &out.path().display().to_string(),
            "--with-credentials",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("credentials: written (real)"));
    let creds =
        fs::read_to_string(out.path().join(
            "state/fleets/payments/crews/backend/agents/alice/home/.claude/.credentials.json",
        ))
        .unwrap();
    assert!(creds.contains("sk-REAL"));
}

#[test]
fn unknown_agent_and_missing_tools_fail_clearly() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    balerix(home.path(), tools.path())
        .args([
            "dev",
            "materialize",
            PAYMENTS,
            "backend/nobody",
            "--no-host-defaults",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no agent \"payments/backend/nobody\"",
        ));
    let empty = tempfile::tempdir().unwrap();
    let tmpdir = tempfile::tempdir().unwrap();
    balerix(home.path(), empty.path())
        .env("TMPDIR", tmpdir.path())
        .args([
            "dev",
            "materialize",
            PAYMENTS,
            "backend/bob",
            "--no-host-defaults",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required tool not found on PATH"));
    // A failure before rendering must leave no `balerix-materialize-*` temp
    // dir behind.
    assert_eq!(fs::read_dir(tmpdir.path()).unwrap().count(), 0);
}

#[test]
fn dev_is_hidden_from_help() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    balerix(home.path(), tools.path())
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("dev").not());
}
