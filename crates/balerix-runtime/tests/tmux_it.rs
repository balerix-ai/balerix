#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use balerix_core::{AgentId, AgentRunner, LaunchPlan, ProcessState};
use balerix_runtime::tmux::ATTACH_SESSION_PREFIX;
use balerix_runtime::{ANCHOR_WINDOW, TmuxRunner};

/// Kills the test's tmux server on every exit path, a panic included, so
/// a failing run does not leave a server behind.
struct KillServer {
    tmux: std::path::PathBuf,
    socket: String,
}

impl Drop for KillServer {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.tmux)
            .args(["-L", &self.socket, "kill-server"])
            .status();
    }
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Two crews on one socket race at daemon start: the first `new-session`
/// brings the server up, and for a moment it has no session at all. tmux
/// then answers `has-session -t =<crew>` with "no current target" rather
/// than "can't find session", and that must read as absent, not as a
/// failed step — a failed step waits out the whole resync cadence.
#[test]
fn ensure_crew_on_a_server_with_no_sessions_creates_the_session() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let socket = format!("balerix-test-empty-{}", std::process::id());
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let tmux = |args: &[&str]| {
        std::process::Command::new(&tools.tmux)
            .args(["-L", &socket])
            .args(args)
            .output()
            .unwrap()
    };
    // A server that stays up with zero sessions (the transient state
    // caught mid-race, held still).
    let out = tmux(&[
        "-f",
        "/dev/null",
        "start-server",
        ";",
        "set",
        "-g",
        "exit-empty",
        "off",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let probe = tmux(&["has-session", "-t", "=f/c"]);
    assert!(
        String::from_utf8_lossy(&probe.stderr).contains("no current target"),
        "the precondition this test exists for: {}",
        String::from_utf8_lossy(&probe.stderr)
    );

    let crew = "f/c/a".parse::<AgentId>().unwrap().crew_ref();
    let created = r.ensure_crew(&crew);
    let kill = tmux(&["kill-server"]);
    created.unwrap();
    assert!(kill.status.success());
}

#[test]
fn session_window_observe_exit_respawn_and_teardown() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux");
    let socket = format!("balerix-test-{}", std::process::id());
    let _server = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let fleet = id.fleet.clone();

    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let log = agent_dir.join("logs").join("tmux.log");
    let script = agent_dir.join("launch.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho hello-from-agent\nexec sleep 300\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script: script.clone(),
    };

    assert!(
        r.observe(&fleet).unwrap().crews.is_empty(),
        "no server yet is not an error"
    );
    r.ensure_crew(&crew).unwrap();
    r.ensure_crew(&crew).unwrap();
    assert!(
        r.observe(&fleet).unwrap().crews[&crew.crew].is_empty(),
        "anchor window is not an agent"
    );

    r.ensure_agent(&id, &plan).unwrap();
    let pid = match r.observe(&fleet).unwrap().get(&id) {
        Some(ProcessState::Running { pid }) => *pid,
        other => panic!("expected running, got {other:?}"),
    };
    wait_for(|| std::fs::read_to_string(&log).is_ok_and(|s| s.contains("hello-from-agent")));

    // kill the script's process group leader → pane dead, remain-on-exit keeps the window
    assert!(
        std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_for(|| {
        matches!(
            r.observe(&fleet).unwrap().get(&id),
            Some(ProcessState::Exited { .. })
        )
    });

    // Truncate first so this assertion can only pass if the respawn path
    // itself (not the leftover content from the first start) re-attaches
    // the pipe before the respawned script writes.
    std::fs::write(&log, "").unwrap();
    r.ensure_agent(&id, &plan).unwrap(); // respawn
    wait_for(
        || matches!(r.observe(&fleet).unwrap().get(&id), Some(ProcessState::Running { pid: p }) if *p != pid),
    );
    wait_for(|| std::fs::read_to_string(&log).is_ok_and(|s| s.contains("hello-from-agent")));

    r.send_text(&id, "ignored", true).unwrap();
    r.stop_agent(&id).unwrap();
    r.stop_agent(&id).unwrap(); // absent → ok
    assert_eq!(r.observe(&fleet).unwrap().get(&id), None);
    assert!(
        r.observe(&fleet).unwrap().crews.contains_key(&crew.crew),
        "session survives via the anchor"
    );
    r.stop_crew(&crew).unwrap();
    r.stop_crew(&crew).unwrap();
    assert!(r.observe(&fleet).unwrap().crews.is_empty());
}

/// Plugins spec §18.4, §11.1: a real tmux attach through the PTY sees the
/// pane, typed bytes reach it, the resize reaches the client, the grouped
/// session dies with the stream, and `stop_crew` takes the whole group.
#[test]
fn attach_streams_the_pane_and_the_grouped_session_dies_with_the_stream() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux-attach");
    let socket = format!("balerix-test-attach-{}", std::process::id());
    let _server = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let fleet = id.fleet.clone();
    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let script = agent_dir.join("launch.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho hello-from-agent\nexec cat > typed.log\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script: script.clone(),
    };
    let tmux = |args: &[&str]| -> String {
        let out = std::process::Command::new(&tools.tmux)
            .args(["-L", &socket])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let sessions = || tmux(&["list-sessions", "-F", "#{session_name}"]);

    assert!(r.attach(&id).is_err(), "no window yet");
    r.ensure_crew(&crew).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| {
        std::fs::read_to_string(agent_dir.join("logs/tmux.log"))
            .is_ok_and(|s| s.contains("hello-from-agent"))
    });

    // Park the crew session on the anchor first, so the attach's
    // `select-window -t =a` below has somewhere to leak from: without this
    // the crew session is already on `a` and the assertion that it stays
    // put would hold whatever the grouped session did.
    tmux(&["select-window", "-t", &format!("=f/c:={ANCHOR_WINDOW}")]);
    assert_eq!(
        tmux(&["display-message", "-p", "-t", "f/c", "#{window_name}"]).trim(),
        ANCHOR_WINDOW
    );

    let stream = r.attach(&id).unwrap();
    let mut reader = stream.reader().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut seen = Vec::new();
    wait_for(|| {
        while let Ok(chunk) = rx.try_recv() {
            seen.extend(chunk);
        }
        String::from_utf8_lossy(&seen).contains("hello-from-agent")
    });
    let mut writer = stream.writer().unwrap();
    assert!(stream.writer().is_err(), "the writer is taken once");
    // Enter is CR, as a real terminal (and xterm.js) sends it: tmux drops a
    // bare LF from a client instead of passing it to the pane.
    writer.write_all(b"typed-through-attach\r").unwrap();
    writer.flush().unwrap();
    wait_for(|| {
        std::fs::read_to_string(agent_dir.join("typed.log"))
            .is_ok_and(|s| s.contains("typed-through-attach"))
    });
    stream.resize(100, 30).unwrap();
    wait_for(|| {
        tmux(&["list-clients", "-F", "#{client_width}x#{client_height}"]).contains("100x30")
    });
    let attach_session = sessions()
        .lines()
        .find(|l| l.starts_with(ATTACH_SESSION_PREFIX))
        .unwrap()
        .to_string();
    assert_eq!(
        tmux(&[
            "display-message",
            "-p",
            "-t",
            &attach_session,
            "#{window_name}"
        ])
        .trim(),
        "a",
        "the grouped session shows the agent's window"
    );
    assert_eq!(
        tmux(&["display-message", "-p", "-t", "f/c", "#{window_name}"]).trim(),
        ANCHOR_WINDOW,
        "the viewer's select-window never moved the operator's own session"
    );

    drop(stream);
    wait_for(|| !sessions().contains(ATTACH_SESSION_PREFIX));
    assert!(
        sessions().contains("f/c"),
        "the crew session survives the stream"
    );
    assert!(matches!(
        r.observe(&fleet).unwrap().get(&id),
        Some(ProcessState::Running { .. })
    ));

    // A client that cannot create its session is an error, not a live
    // stream that emits tmux's error line and EOF: the same runner behind
    // a tmux whose `new-session` fails (the crew session, its windows and
    // `list-windows` are the real tmux's).
    let wrapper = root.join("tmux-broken-new-session.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in new-session) echo 'no such session' >&2; exit 1;; esac; done\nexec {} \"$@\"\n",
            tools.tmux.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let broken = TmuxRunner::new(wrapper, socket.clone());
    let err = match broken.attach(&id) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a client whose session never appeared was handed out as a stream"),
    };
    assert!(
        err.contains("before its session appeared"),
        "the failed client is reported, not handed out: {err}"
    );
    assert!(
        !sessions().contains(ATTACH_SESSION_PREFIX),
        "nothing left of the failed attach"
    );

    // a second attach, then stop_crew must take the group with it
    let again = r.attach(&id).unwrap();
    wait_for(|| sessions().contains(ATTACH_SESSION_PREFIX));
    r.stop_crew(&crew).unwrap();
    wait_for(|| sessions().trim().is_empty());
    drop(again);
}

/// Spec C §3.3: a multi-line `send_text` arrives whole. `send-keys -l`
/// would deliver a literal newline as Ctrl-J to the application; a
/// buffer paste delivers the text as a terminal paste does, `\r` between
/// the lines, which a cooked tty turns back into `\n` for `cat`.
#[test]
fn send_text_with_newlines_arrives_as_one_paste() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux-paste");
    let socket = format!("balerix-test-paste-{}", std::process::id());
    let _server = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let stdin_log = agent_dir.join("stdin.log");
    let script = agent_dir.join("launch.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec cat >> {}\n", stdin_log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script: script.clone(),
    };
    r.ensure_crew(&crew).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| {
        matches!(
            r.observe(&id.fleet).unwrap().get(&id),
            Some(ProcessState::Running { .. })
        )
    });
    // give `cat` a moment to be the pane's foreground process
    std::thread::sleep(Duration::from_millis(300));

    r.send_text(&id, "single", true).unwrap();
    r.send_text(&id, "line one\nline two\n\nline four", true)
        .unwrap();
    wait_for(|| std::fs::read_to_string(&stdin_log).is_ok_and(|s| s.contains("line four")));
    let got = std::fs::read_to_string(&stdin_log).unwrap();
    assert_eq!(got, "single\nline one\nline two\n\nline four\n", "{got:?}");
    // the paste buffer was deleted afterwards
    let buffers = std::process::Command::new(&tools.tmux)
        .args(["-L", &socket, "list-buffers"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&buffers.stdout).contains("balerix-send-"),
        "{}",
        String::from_utf8_lossy(&buffers.stdout)
    );
    r.stop_crew(&crew).unwrap();
}

/// A `paste-buffer` failure (the target window gone, an agent dying
/// concurrently with a `send_text`) must not leak the buffer `load-buffer`
/// created: `send_text` deletes it itself before returning the error.
#[test]
fn send_text_deletes_the_buffer_when_paste_buffer_fails() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let socket = format!("balerix-test-paste-fail-{}", std::process::id());
    let _server = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    // The crew session (and its anchor window) exists, but the agent's own
    // window was never created, so `paste-buffer -t` targets a window that
    // is not there.
    r.ensure_crew(&crew).unwrap();

    let result = r.send_text(&id, "line one\nline two", true);
    assert!(
        result.is_err(),
        "paste-buffer at a missing window must fail"
    );

    let buffers = std::process::Command::new(&tools.tmux)
        .args(["-L", &socket, "list-buffers"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&buffers.stdout).contains("balerix-send-"),
        "the buffer load-buffer created must not survive a failed paste: {}",
        String::from_utf8_lossy(&buffers.stdout)
    );
    r.stop_crew(&crew).unwrap();
}

/// Spec C §4.4 allows a 64 KiB review, and the header, the quoted lines
/// and the per-comment framing put the rendered message well past that.
/// tmux's client refuses a command whose packed argv exceeds 16 KiB
/// ("command too long"), so the text cannot travel as an argument of
/// `set-buffer`; it goes in on the client's stdin through `load-buffer -`
/// instead. ~70 KiB must arrive whole, and leave no buffer behind.
#[test]
fn send_text_of_seventy_kilobytes_arrives_whole() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux-paste-big");
    let socket = format!("balerix-test-paste-big-{}", std::process::id());
    let _server = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let stdin_log = agent_dir.join("stdin.log");
    let script = agent_dir.join("launch.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec cat >> {}\n", stdin_log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script: script.clone(),
    };
    r.ensure_crew(&crew).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| {
        matches!(
            r.observe(&id.fleet).unwrap().get(&id),
            Some(ProcessState::Running { .. })
        )
    });
    std::thread::sleep(Duration::from_millis(300));

    // 1000 × 72 bytes = 70.3 KiB, every line well under the tty's
    // canonical line limit and the last one without a newline.
    let text: String = (0..1000)
        .map(|i| format!("line {i:05} {}", "x".repeat(60)))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.len() > 70 << 10, "{}", text.len());
    r.send_text(&id, &text, true).unwrap();

    let expected = format!("{text}\n");
    wait_for(|| std::fs::read_to_string(&stdin_log).is_ok_and(|s| s.contains("line 00999")));
    let got = std::fs::read_to_string(&stdin_log).unwrap();
    assert_eq!(
        got.len(),
        expected.len(),
        "the paste must arrive whole: {} of {} bytes",
        got.len(),
        expected.len()
    );
    assert_eq!(got, expected);

    let buffers = std::process::Command::new(&tools.tmux)
        .args(["-L", &socket, "list-buffers"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&buffers.stdout).contains("balerix-send-"),
        "{}",
        String::from_utf8_lossy(&buffers.stdout)
    );
    r.stop_crew(&crew).unwrap();
}

/// A single line past `send-keys`' argv ceiling (~16 KiB) is pasted like a
/// multi-line text; a short single line still goes through `send-keys -l`.
#[test]
fn a_long_single_line_send_text_arrives_whole() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux-longline");
    let socket = format!("balerix-test-longline-{}", std::process::id());
    let _server = KillServer {
        tmux: tools.tmux.clone(),
        socket: socket.clone(),
    };
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let stdin_log = agent_dir.join("stdin.log");
    let script = agent_dir.join("launch.sh");
    // `-icanon` matters here and nowhere else in this file: Linux's tty
    // line discipline buffers canonical input until a newline and silently
    // truncates a line past ~4 KiB with none, which a real full-screen
    // agent avoids by putting its pty in raw mode; `cat` never does, so
    // the stand-in script must do it instead, or this single unterminated
    // line — the case this test exists to cover — would never arrive.
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nstty -icanon -echo\nexec cat >> {}\n",
            stdin_log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script: script.clone(),
    };
    r.ensure_crew(&crew).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| {
        matches!(
            r.observe(&id.fleet).unwrap().get(&id),
            Some(ProcessState::Running { .. })
        )
    });
    std::thread::sleep(Duration::from_millis(300));

    let long: String = (0..20_000)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    assert!(long.len() > 16 << 10 && !long.contains('\n'));
    r.send_text(&id, "short", true).unwrap();
    r.send_text(&id, &long, true).unwrap();
    wait_for(|| std::fs::read_to_string(&stdin_log).is_ok_and(|s| s.len() >= long.len() + 7));
    let got = std::fs::read_to_string(&stdin_log).unwrap();
    assert_eq!(got, format!("short\n{long}\n"), "len {}", got.len());
    // no buffer left behind
    let buffers = std::process::Command::new(&tools.tmux)
        .args(["-L", &socket, "list-buffers"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&buffers.stdout).contains("balerix-send-"));
    r.stop_crew(&crew).unwrap();
}
