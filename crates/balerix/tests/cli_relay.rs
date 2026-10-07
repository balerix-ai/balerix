#![allow(clippy::unwrap_used, clippy::expect_used)]

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn hook_relay_never_fails_the_agent() {
    Command::new(env!("CARGO_BIN_EXE_balerix"))
        .arg("hook-relay")
        .env("BALERIX_API_URL", "http://127.0.0.1:1")
        .env("BALERIX_AGENT_ID", "f/c/a")
        .env("BALERIX_HOOK_SECRET", "s")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#)
        .assert()
        .success()
        .stdout("{}\n")
        .stderr(predicate::str::contains("balerix hook-relay:"));
    Command::new(env!("CARGO_BIN_EXE_balerix"))
        .arg("hook-relay")
        .env_remove("BALERIX_API_URL")
        .write_stdin("{}")
        .assert()
        .success()
        .stdout("{}\n")
        .stderr(predicate::str::contains("BALERIX_API_URL"));
}

/// #116: an over-limit payload still answers `{}` (fail-open), and
/// stderr names the limit rather than a 400 about truncated JSON.
#[test]
fn an_over_limit_event_fails_open_with_a_message_naming_the_limit() {
    let mut big = String::from(r#"{"hook_event_name":"PostToolUse","x":""#);
    big.push_str(&"y".repeat(1 << 20));
    big.push_str(r#""}"#);
    Command::new(env!("CARGO_BIN_EXE_balerix"))
        .arg("hook-relay")
        .env("BALERIX_API_URL", "http://127.0.0.1:1")
        .env("BALERIX_AGENT_ID", "f/c/a")
        .env("BALERIX_HOOK_SECRET", "s")
        .write_stdin(big)
        .assert()
        .success()
        .stdout("{}\n")
        .stderr(predicate::str::contains(
            "balerix hook-relay: the hook event is over the daemon's 1 MiB limit (1048576 bytes); not sent",
        ));
}
