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
