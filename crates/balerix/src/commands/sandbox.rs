//! `sandbox-exec` and `sandbox-probe`.
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::ExitCode;

pub fn exec(argv: &[OsString]) -> ExitCode {
    let Some((program, args)) = argv.split_first() else {
        eprintln!("balerix sandbox-exec: no command");
        return ExitCode::from(126);
    };
    #[cfg(target_os = "linux")]
    if let Err(e) = balerix_runtime::seccomp::deny_unix_sockets() {
        eprintln!("balerix sandbox-exec: {e}");
        return ExitCode::from(126);
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("balerix sandbox-exec: Linux only");
        return ExitCode::from(126);
    }
    #[cfg(target_os = "linux")]
    {
        let e = std::process::Command::new(program).args(args).exec();
        eprintln!("balerix sandbox-exec: {}: {e}", program.to_string_lossy());
        ExitCode::from(if e.kind() == std::io::ErrorKind::NotFound {
            127
        } else {
            126
        })
    }
}

/// errno values for "the sandbox said no" (nono's mediation answers EPERM;
/// Landlock answers EACCES).
const EPERM: i32 = 1;
const EACCES: i32 = 13;

/// Runs inside the sandbox: prints one JSON line and exits 0 only when TCP
/// to `tcp` works, a Unix socket in `inside` works and `outside` is refused.
pub fn probe(tcp: u16, inside: &Path, outside: &Path) -> ExitCode {
    use std::os::unix::net::{UnixListener, UnixStream};
    let show = |r: std::io::Result<()>| match r {
        Ok(()) => "ok".to_string(),
        Err(e) => e.to_string(),
    };
    let tcp_r = show(std::net::TcpStream::connect(("127.0.0.1", tcp)).map(drop));
    let sock = inside.join("probe.sock");
    let inside_r = show((|| {
        let l = UnixListener::bind(&sock)?;
        let _c = UnixStream::connect(&sock)?;
        drop(l);
        std::fs::remove_file(&sock)
    })());
    let outside_r = match UnixStream::connect(outside) {
        Ok(_) => "connected".to_string(),
        Err(e) if matches!(e.raw_os_error(), Some(EPERM | EACCES)) => "refused".to_string(),
        Err(e) => e.to_string(),
    };
    let pass = tcp_r == "ok" && inside_r == "ok" && outside_r == "refused";
    println!(
        "{}",
        serde_json::json!({"tcp": tcp_r, "inside": inside_r, "outside": outside_r})
    );
    if pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
