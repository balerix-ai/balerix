//! `balerix agent-supervise -- <argv…>` (Spec N amendment §13):
//! `launch.sh` starts every agent under it. This file owns the signals
//! and the exit code; the loop is `balerix_runtime::supervise`.

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::mpsc;

use balerix_runtime::supervise::{STOP_GRACE, supervise};
use tokio::signal::unix::{SignalKind, signal};

pub fn agent_supervise_command(argv: &[OsString]) -> ExitCode {
    match run(argv) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("balerix agent-supervise: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(argv: &[OsString]) -> std::io::Result<i32> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // Installed before the child exists: a hangup that arrived first
    // would otherwise kill the wrapper and leave the child unsupervised.
    let (mut hup, mut term, mut int) = {
        let _guard = rt.enter();
        (
            signal(SignalKind::hangup())?,
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        )
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        rt.block_on(async move {
            loop {
                tokio::select! {
                    _ = hup.recv() => {}
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
                if tx.send(()).is_err() {
                    break;
                }
            }
        });
    });
    supervise(argv, &rx, STOP_GRACE)
}
