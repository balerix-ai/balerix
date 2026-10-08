//! Which Unix sockets a sandboxed process may use (spec 2026-10-08):
//! `Mediate` — nono's pathname mediation, own directories only;
//! `Deny` — no `socket(AF_UNIX)` at all, through `balerix sandbox-exec`;
//! `Open` — nono's default, any socket the uid can reach.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// `[sandbox] unix_sockets` in the daemon's `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnixSockets {
    #[default]
    Auto,
    Mediate,
    Deny,
    Open,
}

/// The policy a sandbox is rendered with. `default()` is `Open` only so
/// `Runtime::new` keeps its behaviour for tests and `dev`; `serve` and the
/// sidecar always set it explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SocketPolicy {
    Mediate,
    Deny,
    #[default]
    Open,
}

/// Why a start-up check of the socket policy failed: the mediation
/// probe, or `sandbox-exec` under `Deny`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProbeFailure(pub String);

/// The resolved policy and the one-line reason `serve` logs.
pub fn resolve(
    setting: UnixSockets,
    probe: impl FnOnce() -> Result<(), ProbeFailure>,
) -> Result<(SocketPolicy, String), ProbeFailure> {
    match setting {
        UnixSockets::Open => Ok((SocketPolicy::Open, "unix_sockets = \"open\": sandboxed processes can reach Unix sockets outside their grants, including the daemon's tmux server".into())),
        UnixSockets::Deny => Ok((SocketPolicy::Deny, "unix_sockets = \"deny\": sandboxed processes cannot create Unix sockets".into())),
        UnixSockets::Mediate => {
            probe()?;
            Ok((SocketPolicy::Mediate, "unix_sockets = \"mediate\": sandboxed processes use Unix sockets in their own directories only".into()))
        }
        UnixSockets::Auto => Ok(match probe() {
            Ok(()) => (SocketPolicy::Mediate, "unix_sockets = \"auto\": nono's pathname mediation works here; mediate: own directories only".into()),
            Err(e) => (SocketPolicy::Deny, format!("unix_sockets = \"auto\": nono's pathname mediation does not work here ({e}); deny: sandboxed processes cannot create Unix sockets (set \"open\" to allow them, with the risk)")),
        }),
    }
}

/// The rendered profile under `policy`. Mediate: pathname mediation at
/// the top level and in `platform_overrides.linux.linux`, and
/// `unix_socket_subtree_bind` gains `own` (kept after any operator
/// entries). Deny: `wrapper` (the balerix binary) is added to
/// `filesystem.read` so `sandbox-exec` can run. Open: unchanged.
pub fn apply_to_profile(
    mut profile: Value,
    policy: SocketPolicy,
    own: &[PathBuf],
    wrapper: &Path,
) -> Value {
    match policy {
        SocketPolicy::Open => {}
        SocketPolicy::Deny => {
            push_unique(&mut profile, &["filesystem", "read"], wrapper);
        }
        SocketPolicy::Mediate => {
            set_pathname(&mut profile, &["linux"]);
            set_pathname(&mut profile, &["platform_overrides", "linux", "linux"]);
            for dir in own {
                push_unique(
                    &mut profile,
                    &["filesystem", "unix_socket_subtree_bind"],
                    dir,
                );
            }
        }
    }
    profile
}

/// The command after nono's `--`: under Deny, run through `<balerix>
/// sandbox-exec --`, which refuses Unix socket creation and then execs
/// `argv`. Mediate and Open: `argv` unchanged.
pub fn wrap_command(policy: SocketPolicy, balerix: &Path, argv: Vec<String>) -> Vec<String> {
    match policy {
        SocketPolicy::Deny => [
            balerix.display().to_string(),
            "sandbox-exec".into(),
            "--".into(),
        ]
        .into_iter()
        .chain(argv)
        .collect(),
        SocketPolicy::Mediate | SocketPolicy::Open => argv,
    }
}

/// The object at `path`, created (with its parents) as needed; anything
/// else found on the way is replaced.
fn object_at<'v>(root: &'v mut Value, path: &[&str]) -> &'v mut serde_json::Map<String, Value> {
    let mut at = root;
    for key in path {
        at = child(at, key, json!({}));
    }
    if !at.is_object() {
        *at = json!({});
    }
    match at {
        Value::Object(map) => map,
        _ => unreachable!("made an object above"),
    }
}

/// `at[key]`, inserting `default` when `at` (made an object if it is not)
/// has none.
fn child<'v>(at: &'v mut Value, key: &str, default: Value) -> &'v mut Value {
    if !at.is_object() {
        *at = json!({});
    }
    match at {
        Value::Object(map) => map.entry(key).or_insert(default),
        _ => unreachable!("made an object above"),
    }
}

fn set_pathname(profile: &mut Value, path: &[&str]) {
    object_at(profile, path).insert("af_unix_mediation".into(), json!("pathname"));
}

/// Appends `entry` to the array at `path` (`[..dirs, key]`) unless a plain
/// string entry equal to it is already there.
fn push_unique(profile: &mut Value, path: &[&str], entry: &Path) {
    let Some((key, dirs)) = path.split_last() else {
        return;
    };
    let mut at = profile;
    for dir in dirs {
        at = child(at, dir, json!({}));
    }
    let slot = child(at, key, json!([]));
    if !slot.is_array() {
        *slot = json!([]);
    }
    let text = entry.to_string_lossy();
    if let Value::Array(list) = slot
        && !list.iter().any(|v| v.as_str() == Some(&*text))
    {
        list.push(json!(text));
    }
}

/// The probe's deadline (spec 2026-10-08).
const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Runs `balerix sandbox-probe` under a minimal nono profile with
/// pathname mediation; Ok when all three checks hold (TCP to the granted
/// port, a Unix socket in the granted directory, no Unix socket outside
/// it). `scratch` is a directory the caller owns (created if absent,
/// emptied afterwards).
pub fn probe_mediation(
    tools: &crate::tools::ToolPaths,
    scratch: &std::path::Path,
) -> Result<(), ProbeFailure> {
    probe_mediation_within(tools, scratch, PROBE_DEADLINE)
}

#[cfg(not(target_os = "linux"))]
fn probe_mediation_within(
    _tools: &crate::tools::ToolPaths,
    _scratch: &std::path::Path,
    _deadline: std::time::Duration,
) -> Result<(), ProbeFailure> {
    Err(ProbeFailure("pathname mediation is Linux only".into()))
}

#[cfg(target_os = "linux")]
fn probe_mediation_within(
    tools: &crate::tools::ToolPaths,
    scratch: &std::path::Path,
    deadline: std::time::Duration,
) -> Result<(), ProbeFailure> {
    let result = run_probe(tools, scratch, deadline);
    // Listener, socket, profile: nothing stays behind, whatever happened.
    if let Ok(entries) = std::fs::read_dir(scratch) {
        for e in entries.flatten() {
            let p = e.path();
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
    result.map_err(|cause| {
        let ptrace = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
            .map(|v| format!("; kernel.yama.ptrace_scope = {}", v.trim()))
            .unwrap_or_default();
        ProbeFailure(format!("{cause}{ptrace}"))
    })
}

#[cfg(target_os = "linux")]
fn run_probe(
    tools: &crate::tools::ToolPaths,
    scratch: &std::path::Path,
    deadline: std::time::Duration,
) -> Result<(), String> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let io = |what: &str| {
        let what = what.to_string();
        move |e: std::io::Error| format!("{what}: {e}")
    };
    let inside = scratch.join("inside");
    let outside_dir = scratch.join("outside");
    std::fs::create_dir_all(&inside).map_err(io("create scratch"))?;
    std::fs::create_dir_all(&outside_dir).map_err(io("create scratch"))?;
    let outside = outside_dir.join("probe.sock");
    let unix = std::os::unix::net::UnixListener::bind(&outside).map_err(io("bind probe socket"))?;
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").map_err(io("bind probe port"))?;
    let port = tcp.local_addr().map_err(io("probe port"))?.port();
    unix.set_nonblocking(true).map_err(io("probe socket"))?;
    tcp.set_nonblocking(true).map_err(io("probe port"))?;

    // Accept until the probe ends, so the connects complete.
    let stop = Arc::new(AtomicBool::new(false));
    let accepter = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                if let Ok((c, _)) = unix.accept() {
                    held.push(c.try_clone().ok());
                }
                if let Ok((c, _)) = tcp.accept() {
                    drop(c);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        })
    };

    let outcome = (|| {
        let balerix =
            std::fs::canonicalize(&tools.balerix).unwrap_or_else(|_| tools.balerix.clone());
        let mut read: Vec<String> = crate::sandbox::SYSTEM_READ
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        read.push(balerix.display().to_string());
        let inside_s = inside.display().to_string();
        let profile = serde_json::json!({
            "meta": { "name": "balerix-socket-probe" },
            "filesystem": {
                "read": read,
                "allow": [inside_s],
                "unix_socket_subtree_bind": [inside_s],
            },
            "workdir": { "access": "none" },
            "network": { "open_port": [port] },
            // nono reserves PATH; the probe execs the binary by absolute path.
            "environment": { "deny_vars": ["*"] },
            "linux": { "af_unix_mediation": "pathname" },
        });
        let profile_path = scratch.join("profile.json");
        std::fs::write(&profile_path, profile.to_string()).map_err(io("write profile"))?;
        let out = crate::tools::Cmd::new(&tools.nono)
            .env_clear()
            .env("HOME", scratch.display().to_string())
            .env("NONO_NO_UPDATE_CHECK", "1")
            .args(["-s", "run", "--no-audit", "--profile"])
            .args([profile_path.display().to_string()])
            .args(["--".to_string(), balerix.display().to_string()])
            .args([
                "sandbox-probe".to_string(),
                "--tcp".to_string(),
                port.to_string(),
                "--inside".to_string(),
                inside_s.clone(),
                "--outside".to_string(),
                outside.display().to_string(),
            ])
            .timeout(deadline)
            .run_with_exit_codes(&(0..=255).collect::<Vec<_>>())
            .map_err(|f| {
                if f.stderr.starts_with("timed out") {
                    format!("no answer within {} ms", deadline.as_millis())
                } else {
                    format!("could not run nono: {}", f.stderr)
                }
            })?;
        let want = serde_json::json!({"tcp": "ok", "inside": "ok", "outside": "refused"});
        let answer = out.stdout.lines().rev().find(|l| !l.trim().is_empty());
        if out.code == 0 {
            return match answer.map(serde_json::from_str::<serde_json::Value>) {
                Some(Ok(v)) if v == want => Ok(()),
                other => Err(format!(
                    "probe exited 0 without the expected answer: {}",
                    match other {
                        Some(Ok(v)) => v.to_string(),
                        Some(Err(_)) => answer.unwrap_or_default().to_string(),
                        None => "no output".to_string(),
                    }
                )),
            };
        }
        let said = [&out.stdout, &out.stderr]
            .into_iter()
            .map(|s| balerix_core::first_line(s))
            .find(|l| !l.is_empty())
            .unwrap_or("no output");
        Err(format!("probe exited {}: {said}", out.code))
    })();
    stop.store(true, Ordering::Relaxed);
    let _ = accepter.join();
    outcome
}

/// How long the `sandbox-exec` check may take at start-up.
pub const SANDBOX_EXEC_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Under `Deny`, before any sandbox is rendered: checks that `balerix`
/// runs `sandbox-exec -- <balerix> --version` (itself as the command, so
/// no PATH lookup), so a binary without it (or one that cannot install
/// its filter) stops start-up with the cause instead of every agent
/// failing in its pane.
pub fn check_sandbox_exec(balerix: &Path) -> Result<(), ProbeFailure> {
    check_sandbox_exec_within(balerix, SANDBOX_EXEC_BUDGET)
}

fn check_sandbox_exec_within(
    balerix: &Path,
    budget: std::time::Duration,
) -> Result<(), ProbeFailure> {
    let what = format!("`{0} sandbox-exec -- {0} --version`", balerix.display());
    let out = crate::tools::Cmd::new(balerix)
        .args(["sandbox-exec".to_string(), "--".to_string()])
        .args([balerix.display().to_string(), "--version".to_string()])
        .timeout(budget)
        .run_with_exit_codes(&(0..=255).collect::<Vec<_>>())
        .map_err(|f| {
            ProbeFailure(if f.stderr.starts_with("timed out") {
                format!("{what}: no answer within {} ms", budget.as_millis())
            } else {
                format!("{what}: {}", balerix_core::first_line(&f.stderr))
            })
        })?;
    if out.code == 0 {
        return Ok(());
    }
    let said = match balerix_core::first_line(&out.stderr) {
        "" => "no output",
        line => line,
    };
    // 2 is clap's exit for an unknown subcommand: an older binary
    let hint = if out.code == 2 {
        "; the `balerix` binary must match this release (in a pod, the agent image's base `balerix`)"
    } else {
        ""
    };
    Err(ProbeFailure(format!(
        "{what} exited {}: {said}{hint}",
        out.code
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass() -> Result<(), ProbeFailure> {
        Ok(())
    }
    fn fail() -> Result<(), ProbeFailure> {
        Err(ProbeFailure(
            "tcp connect refused under pathname mediation (ptrace_scope 2?)".into(),
        ))
    }

    /// A stand-in `balerix` running `body`, written under a temporary name
    /// and renamed into place once closed: exec'ing a file another thread
    /// still holds open for writing fails with ETXTBSY.
    fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let tmp = dir.join(format!(".{name}.tmp"));
        std::fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = dir.join(name);
        std::fs::rename(&tmp, &p).unwrap();
        p
    }

    #[test]
    fn sandbox_exec_check_accepts_a_binary_that_runs_it() {
        let dir = tempfile::tempdir().unwrap();
        let b = stub(dir.path(), "balerix", "exit 0");
        check_sandbox_exec(&b).unwrap();
    }

    #[test]
    fn sandbox_exec_check_names_an_older_binary() {
        let dir = tempfile::tempdir().unwrap();
        let b = stub(
            dir.path(),
            "balerix",
            "echo \"error: unrecognized subcommand 'sandbox-exec'\" >&2; exit 2",
        );
        let e = check_sandbox_exec(&b).unwrap_err().to_string();
        assert!(e.contains(&b.display().to_string()), "{e}");
        assert!(
            e.contains("exited 2: error: unrecognized subcommand"),
            "{e}"
        );
        assert!(e.contains("must match this release"), "{e}");
    }

    #[test]
    fn sandbox_exec_check_states_another_failures_cause() {
        let dir = tempfile::tempdir().unwrap();
        let b = stub(
            dir.path(),
            "balerix",
            "echo 'balerix sandbox-exec: cannot install the filter: Invalid argument' >&2; exit 126",
        );
        let e = check_sandbox_exec(&b).unwrap_err().to_string();
        assert!(
            e.contains("exited 126: balerix sandbox-exec: cannot install the filter"),
            "{e}"
        );
        assert!(!e.contains("must match this release"), "{e}");
    }

    #[test]
    fn sandbox_exec_check_states_why_the_binary_cannot_run() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let e = check_sandbox_exec(&missing).unwrap_err().to_string();
        assert!(e.contains(&missing.display().to_string()), "{e}");
        assert!(e.contains("No such file or directory"), "{e}");
    }

    #[test]
    fn sandbox_exec_check_gives_up_after_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        let b = stub(dir.path(), "balerix", "exec sleep 30");
        let start = std::time::Instant::now();
        let e = check_sandbox_exec_within(&b, std::time::Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(e.contains("no answer within 300 ms"), "{e}");
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn mediate_pins_pathname_mediation_and_grants_own_dirs() {
        let base = json!({"filesystem":{"read":["/usr"],"unix_socket_subtree_bind":["/run/op.sock.d"]},
                          "platform_overrides":{"linux":{"security":{"signal_mode":"isolated"}}}});
        let p = apply_to_profile(
            base,
            SocketPolicy::Mediate,
            &[PathBuf::from("/w"), PathBuf::from("/h")],
            Path::new("/opt/balerix"),
        );
        assert_eq!(p["linux"]["af_unix_mediation"], "pathname");
        assert_eq!(
            p["platform_overrides"]["linux"]["linux"]["af_unix_mediation"],
            "pathname"
        );
        assert_eq!(
            p["platform_overrides"]["linux"]["security"]["signal_mode"], "isolated",
            "kept"
        );
        assert_eq!(
            p["filesystem"]["unix_socket_subtree_bind"],
            json!(["/run/op.sock.d", "/w", "/h"])
        );
        assert_eq!(
            p["filesystem"]["read"],
            json!(["/usr"]),
            "no wrapper under Mediate"
        );
    }

    #[test]
    fn deny_adds_the_wrapper_to_read_once_and_nothing_else() {
        let base = json!({"filesystem":{"read":["/usr","/opt/balerix"]}});
        let p = apply_to_profile(
            base,
            SocketPolicy::Deny,
            &[PathBuf::from("/w")],
            Path::new("/opt/balerix"),
        );
        assert_eq!(p["filesystem"]["read"], json!(["/usr", "/opt/balerix"]));
        assert!(p.get("linux").is_none());
        let p = apply_to_profile(
            json!({"filesystem":{"read":["/usr"]}}),
            SocketPolicy::Deny,
            &[],
            Path::new("/opt/balerix"),
        );
        assert_eq!(p["filesystem"]["read"], json!(["/usr", "/opt/balerix"]));
    }

    #[test]
    fn mediate_pins_pathname_over_non_object_linux_blocks() {
        for base in [
            json!({"linux":"off"}),
            json!({"platform_overrides":{"linux":{"linux":"off"}}}),
            json!({"linux":"off","platform_overrides":"off"}),
        ] {
            let p = apply_to_profile(base.clone(), SocketPolicy::Mediate, &[], Path::new("/b"));
            assert_eq!(p["linux"]["af_unix_mediation"], "pathname", "{base}");
            assert_eq!(
                p["platform_overrides"]["linux"]["linux"]["af_unix_mediation"], "pathname",
                "{base}"
            );
        }
    }

    #[test]
    fn deny_wraps_the_command_in_sandbox_exec_and_the_others_do_not() {
        let argv = || ["mise", "exec", "--", "claude"].map(String::from).to_vec();
        assert_eq!(
            wrap_command(SocketPolicy::Deny, Path::new("/b"), argv()),
            ["/b", "sandbox-exec", "--", "mise", "exec", "--", "claude"].map(String::from)
        );
        for policy in [SocketPolicy::Mediate, SocketPolicy::Open] {
            assert_eq!(wrap_command(policy, Path::new("/b"), argv()), argv());
        }
    }

    #[test]
    fn open_changes_nothing() {
        let base = json!({"filesystem":{"read":["/usr"]}});
        assert_eq!(
            apply_to_profile(
                base.clone(),
                SocketPolicy::Open,
                &[PathBuf::from("/w")],
                Path::new("/b")
            ),
            base
        );
    }

    #[test]
    fn auto_mediates_when_the_probe_passes() {
        let (p, why) = resolve(UnixSockets::Auto, pass).unwrap();
        assert_eq!(p, SocketPolicy::Mediate);
        assert!(why.contains("mediate"), "{why}");
    }

    #[test]
    fn auto_denies_when_the_probe_fails_and_says_why() {
        let (p, why) = resolve(UnixSockets::Auto, fail).unwrap();
        assert_eq!(p, SocketPolicy::Deny);
        assert!(why.contains("ptrace_scope"), "{why}");
    }

    #[test]
    fn mediate_refuses_to_start_when_the_probe_fails() {
        let e = resolve(UnixSockets::Mediate, fail).unwrap_err();
        assert!(e.0.contains("ptrace_scope"), "{e}");
    }

    #[test]
    fn deny_and_open_never_run_the_probe() {
        for (s, want) in [
            (UnixSockets::Deny, SocketPolicy::Deny),
            (UnixSockets::Open, SocketPolicy::Open),
        ] {
            let (p, _) = resolve(s, || panic!("probe ran for {s:?}")).unwrap();
            assert_eq!(p, want);
        }
    }

    #[test]
    fn the_setting_parses_lowercase_and_refuses_others() {
        #[derive(serde::Deserialize)]
        struct T {
            v: UnixSockets,
        }
        let p = |s: &str| toml::from_str::<T>(&format!("v = \"{s}\"")).map(|t| t.v);
        assert_eq!(p("auto").unwrap(), UnixSockets::Auto);
        assert_eq!(p("mediate").unwrap(), UnixSockets::Mediate);
        assert_eq!(p("deny").unwrap(), UnixSockets::Deny);
        assert_eq!(p("open").unwrap(), UnixSockets::Open);
        assert!(p("Deny").is_err() && p("off").is_err());
    }
    #[cfg(target_os = "linux")]
    mod probe {
        use super::super::*;
        use crate::tools::ToolPaths;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::time::Duration;

        fn tools(nono: PathBuf) -> ToolPaths {
            let p = PathBuf::from("/bin/true");
            ToolPaths {
                git: p.clone(),
                gh: p.clone(),
                mise: p.clone(),
                nono,
                tmux: p.clone(),
                balerix: p,
            }
        }

        fn stub(dir: &Path, body: &str) -> PathBuf {
            let p = dir.join("nono");
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        }

        fn run(nono: PathBuf, scratch: &Path, d: Duration) -> ProbeFailure {
            let e = probe_mediation_within(&tools(nono), scratch, d).unwrap_err();
            assert!(
                std::fs::read_dir(scratch).unwrap().next().is_none(),
                "scratch left behind"
            );
            e
        }

        #[test]
        fn a_missing_nono_is_a_failure_with_a_cause() {
            let d = tempfile::tempdir().unwrap();
            let e = run(
                d.path().join("no-such-nono"),
                &d.path().join("s"),
                Duration::from_secs(5),
            );
            assert!(e.0.contains("could not run nono"), "{e}");
        }

        #[test]
        fn garbage_output_and_a_failing_exit_is_a_failure_with_a_cause() {
            let d = tempfile::tempdir().unwrap();
            let nono = stub(d.path(), "echo '<<junk>>'; echo oops >&2; exit 1");
            let e = run(nono, &d.path().join("s"), Duration::from_secs(5));
            assert!(e.0.contains("exited 1") && e.0.contains("<<junk>>"), "{e}");
        }

        #[test]
        fn a_hung_nono_times_out_and_the_failure_says_so() {
            let d = tempfile::tempdir().unwrap();
            let nono = stub(d.path(), "exec sleep 30");
            let t = std::time::Instant::now();
            let e = run(nono, &d.path().join("s"), Duration::from_millis(300));
            assert!(e.0.contains("no answer within"), "{e}");
            assert!(t.elapsed() < Duration::from_secs(10));
        }

        #[test]
        fn failures_carry_the_ptrace_scope_when_the_host_has_one() {
            let d = tempfile::tempdir().unwrap();
            let e = run(
                d.path().join("none"),
                &d.path().join("s"),
                Duration::from_secs(5),
            );
            if let Ok(v) = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope") {
                assert!(e.0.contains(&format!("ptrace_scope = {}", v.trim())), "{e}");
            }
        }

        #[test]
        fn a_zero_exit_is_a_pass() {
            let d = tempfile::tempdir().unwrap();
            let nono = stub(
                d.path(),
                r#"echo '{"tcp":"ok","inside":"ok","outside":"refused"}'; exit 0"#,
            );
            assert_eq!(
                probe_mediation_within(&tools(nono), &d.path().join("s"), Duration::from_secs(5)),
                Ok(())
            );
        }

        #[test]
        fn a_zero_exit_with_garbage_output_is_a_failure() {
            let d = tempfile::tempdir().unwrap();
            let nono = stub(d.path(), "echo '<<junk>>'; exit 0");
            let e = run(nono, &d.path().join("s"), Duration::from_secs(5));
            assert!(e.0.contains("<<junk>>"), "{e}");
        }
    }
}
