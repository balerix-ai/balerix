#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Fake tools on PATH: `serve` discovers them but calls none without a fleet.
fn fake_tools(dir: &Path) {
    for t in ["git", "gh", "mise", "nono", "tmux"] {
        fs::write(dir.join(t), "#!/bin/sh\nexit 0\n").unwrap();
    }
}

fn balerix(home: &Path, tools: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home)
        .env("PATH", tools)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("BALERIX_API_URL");
    cmd
}

fn wait_for_file(path: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(s) = fs::read_to_string(path)
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn get(url: &str, token: Option<&str>) -> (u16, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.get(url);
    if let Some(t) = token {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    let mut resp = req.call().unwrap();
    (
        resp.status().as_u16(),
        resp.body_mut().read_to_string().unwrap(),
    )
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn foreground_serve_writes_endpoint_and_answers_with_the_token() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let server_dir = home.path().join(".local/state/balerix/server");
    let child = balerix(home.path(), tools.path())
        .args([
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--tmux-socket",
            &format!("balerix-test-{}", std::process::id()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _kill = Kill(child);
    let url = wait_for_file(&server_dir.join("endpoint"));
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    let token = fs::read_to_string(server_dir.join("token"))
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(token.len(), 64);
    assert!(server_dir.join("vault.key").exists());
    assert!(server_dir.join("balerix.pid").exists());
    assert_eq!(
        get(&format!("{url}/healthz"), None),
        (200, "ok".to_string())
    );
    assert_eq!(get(&format!("{url}/v1/fleets"), None).0, 401);
    assert_eq!(
        get(&format!("{url}/v1/fleets"), Some(&token)),
        (200, "[]".to_string())
    );
    assert_eq!(get(&format!("{url}/metrics"), None).0, 200);
}

#[test]
fn detached_serve_prints_the_endpoint_logs_to_a_file_and_stops_on_term() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let server_dir = home.path().join(".local/state/balerix/server");
    let out = balerix(home.path(), tools.path())
        .args([
            "serve",
            "-d",
            "--bind",
            "127.0.0.1:0",
            "--tmux-socket",
            &format!("balerix-test-{}-d", std::process::id()),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("http://127.0.0.1:"), "{stdout}");
    let url = fs::read_to_string(server_dir.join("endpoint"))
        .unwrap()
        .trim()
        .to_string();
    let pid: u32 = fs::read_to_string(server_dir.join("balerix.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(get(&format!("{url}/healthz"), None).0, 200);
    let log = fs::read_to_string(server_dir.join("server.log")).unwrap();
    assert!(log.contains("listening"), "{log}");

    // a second daemon refuses to start while the first answers
    let again = balerix(home.path(), tools.path())
        .args(["serve", "--bind", "127.0.0.1:0"])
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already running"));

    assert!(
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let start = Instant::now();
    while server_dir.join("endpoint").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "endpoint file not removed on SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!server_dir.join("balerix.pid").exists());
}

#[test]
fn serve_rejects_a_non_loopback_bind_and_writes_no_endpoint() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let server_dir = home.path().join(".local/state/balerix/server");
    let out = balerix(home.path(), tools.path())
        .args(["serve", "--bind", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not loopback"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!server_dir.join("endpoint").exists());
}
