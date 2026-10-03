use std::process::ExitCode;

use balerix_agent::cli::{Cli, Command};
use clap::Parser;

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
    match cli.command {
        Command::Run(args) => match balerix_agent::run::run(&args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("balerix-agent run: {e:#}");
                ExitCode::FAILURE
            }
        },
        Command::Sidecar(args) => {
            let termination_log = args.termination_log.clone();
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    return balerix_agent::cli::terminate(&termination_log, "sidecar", &e.into());
                }
            };
            let r = rt.block_on(balerix_agent::sidecar::main(args));
            // a SIGTERM can land while a blocking step (a clone, an
            // install, a pass) is still running: dropping the runtime
            // would wait for it
            rt.shutdown_timeout(std::time::Duration::from_secs(1));
            match r {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => balerix_agent::cli::terminate(&termination_log, "sidecar", &e),
            }
        }
        Command::CrewSync(args) => balerix_agent::cli::finish(
            &args.dirs.termination_log,
            "crew-sync",
            balerix_agent::jobs::crew_sync(&args),
        ),
        Command::PoolSync(args) => balerix_agent::cli::finish(
            &args.dirs.termination_log,
            "pool-sync",
            balerix_agent::jobs::pool_sync(&args),
        ),
        Command::Harvest(args) => balerix_agent::cli::finish(
            &args.dirs.termination_log,
            "harvest",
            balerix_agent::jobs::harvest(&args),
        ),
    }
}
