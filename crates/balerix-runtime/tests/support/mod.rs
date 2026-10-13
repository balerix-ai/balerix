//! Shared helpers for the integration tests that drive real tools.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use balerix_runtime::{StateLayout, ToolPaths};

/// Tools from the test process's PATH (mise puts the pinned ones there); the
/// relay binary slot is filled with this test executable — it only has to
/// exist for the profile to validate.
pub fn tools() -> Option<ToolPaths> {
    let exe = std::env::current_exe().ok()?;
    ToolPaths::discover_in(&std::env::var_os("PATH").unwrap_or_default(), &exe).ok()
}

/// Returns `true` if the test may run. Otherwise prints a skip reason, or
/// panics when `BALERIX_REQUIRE_TOOLS=1` (CI never skips).
pub fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var_os("BALERIX_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
        panic!("{name} is required (BALERIX_REQUIRE_TOOLS=1) but not available");
    }
    eprintln!("skip: {name} not available");
    false
}

/// A fresh directory under `target/tmp`. Deliberately not `/tmp`: nono's
/// built-in groups grant `/tmp`, so a sandbox-escape assertion there would
/// pass vacuously.
pub fn temp_root(test: &str) -> balerix_runtime::testing::TempRoot {
    balerix_runtime::testing::TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), test)
}

pub fn layout(root: &Path) -> StateLayout {
    StateLayout::xdg(root.join("state"), root.join("data"), root.join("config"))
}

/// A pod layout on temp mounts: `<root>/agent`, `<root>/shared`, `<root>/run`.
pub fn pod_layout(root: &Path, id: &balerix_core::AgentId) -> StateLayout {
    StateLayout::pod(
        balerix_runtime::layout::PodMounts {
            agent: root.join("agent"),
            shared: root.join("shared"),
            run: root.join("run"),
        },
        id,
    )
}

/// True when Landlock is usable: `nono run` of `true` succeeds. `--allow-cwd`
/// is required because nono 0.75.0 refuses CWD access non-interactively
/// without it; without the flag the probe fails even though Landlock itself
/// is fine. `$HOME` must be a sibling of `root`, not nested inside it: nono's
/// built-in default profile denies a set of sensitive dotfiles under `$HOME`
/// (`.aws`, `.ssh`, `.1password`, …), and when `--allow-cwd` broadly allows a
/// directory that contains `$HOME`, those per-file denials sit underneath an
/// allowed parent — a shape Landlock cannot express, so nono refuses to start
/// ("Sandbox initialization failed: Landlock deny-overlap is not
/// enforceable") rather than silently under-enforcing.
pub fn landlock_works(tools: &ToolPaths, root: &Path) -> bool {
    let home = root.with_file_name(format!(
        "{}-nono-probe-home",
        root.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let result = std::process::Command::new(&tools.nono)
        .args(["-s", "run", "--allow-cwd", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(&home);
    result
}

/// A one-line tool, packed outside every balerix directory: the bytes of
/// a tar holding `bin/balerix-probe`.
pub fn probe_tarball(root: &std::path::Path) -> Vec<u8> {
    let src = root.join("probe-src");
    std::fs::create_dir_all(src.join("bin")).unwrap();
    let script = src.join("bin").join("balerix-probe");
    std::fs::write(&script, "#!/bin/sh\necho balerix-probe\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let tarball = root.join("balerix-probe-1.0.0.tar");
    let packed = std::process::Command::new("tar")
        .arg("-C")
        .arg(&src)
        .arg("-cf")
        .arg(&tarball)
        .arg("bin")
        .status()
        .unwrap();
    assert!(packed.success(), "tar");
    std::fs::read(&tarball).unwrap()
}

/// Serves `body` to every request on a loopback port, in a thread that
/// lives as long as the test process; the tarball's URL and a count of the
/// `GET`s answered. A `HEAD` gets the headers alone.
pub fn serve_tarball(body: Vec<u8>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/balerix-probe-1.0.0.tar",
        listener.local_addr().unwrap()
    );
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            // the rest of the head; the body of a GET or HEAD is empty
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-tar\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            if request_line.starts_with("GET ") {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = stream.write_all(&body);
            }
        }
    });
    (url, hits)
}
