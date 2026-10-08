#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `balerix sandbox-probe` and the daemon's mediation probe that runs it.
#![cfg(target_os = "linux")]

const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

struct Fixture {
    _dir: tempfile::TempDir,
    tcp: std::net::TcpListener,
    inside: std::path::PathBuf,
    outside: std::path::PathBuf,
    _ul: std::os::unix::net::UnixListener,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("inside");
    std::fs::create_dir(&inside).unwrap();
    let outside = dir.path().join("outside.sock");
    let ul = std::os::unix::net::UnixListener::bind(&outside).unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    Fixture {
        _dir: dir,
        tcp,
        inside,
        outside,
        _ul: ul,
    }
}

fn probe(f: &Fixture, prefix: &[&str]) -> (bool, serde_json::Value) {
    let port = f.tcp.local_addr().unwrap().port().to_string();
    let mut cmd = std::process::Command::new(prefix.first().copied().unwrap_or(BALERIX));
    cmd.args(prefix.iter().skip(1));
    if !prefix.is_empty() {
        cmd.arg(BALERIX);
    }
    let out = cmd
        .args(["sandbox-probe", "--tcp", &port, "--inside"])
        .arg(&f.inside)
        .arg("--outside")
        .arg(&f.outside)
        .output()
        .unwrap();
    let line = String::from_utf8_lossy(&out.stdout);
    (
        out.status.success(),
        serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("{e}: {line:?} {out:?}")),
    )
}

#[test]
fn unsandboxed_the_outside_socket_connects_so_the_probe_fails() {
    let f = fixture();
    let (ok, v) = probe(&f, &[]);
    assert_eq!(
        v,
        serde_json::json!({"tcp":"ok","inside":"ok","outside":"connected"})
    );
    assert!(!ok);
}

#[test]
fn under_sandbox_exec_the_inside_socket_fails_so_the_probe_fails() {
    let f = fixture();
    let (ok, v) = probe(&f, &[BALERIX, "sandbox-exec", "--"]);
    assert_eq!(v["tcp"], "ok");
    let inside = v["inside"].as_str().unwrap();
    assert!(
        inside.contains("not supported") || inside.contains("97"),
        "{v}"
    );
    assert!(!ok);
}

/// The probe's real outcome depends on the host: with `ptrace_scope` 2
/// nono's pathname mediation is unusable.
#[test]
fn the_mediation_probe_reports_this_hosts_answer() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Ok(tools) = balerix_runtime::ToolPaths::discover_in(&path, std::path::Path::new(BALERIX))
    else {
        assert!(
            std::env::var_os("BALERIX_REQUIRE_TOOLS").is_none_or(|v| v != "1"),
            "nono and friends are required (BALERIX_REQUIRE_TOOLS=1)"
        );
        eprintln!("skip: tools not available");
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let got = balerix_runtime::socket_policy::probe_mediation(&tools, scratch.path());
    let ptrace = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").unwrap_or_default();
    if std::env::var_os("BALERIX_REQUIRE_MEDIATION").is_some_and(|v| v == "1") {
        assert_eq!(
            got,
            Ok(()),
            "BALERIX_REQUIRE_MEDIATION=1 but the probe failed (ptrace_scope {ptrace})"
        );
    } else if ptrace.trim() == "2" {
        let e = got.expect_err("ptrace_scope 2 should make pathname mediation unusable");
        // The failure must come from inside the sandbox, not from a rejected profile.
        assert!(
            e.0.contains("\"tcp\":") && !e.0.contains("Profile parse error"),
            "{e}"
        );
    }
    assert!(
        std::fs::read_dir(scratch.path()).unwrap().next().is_none(),
        "scratch left behind"
    );
}
