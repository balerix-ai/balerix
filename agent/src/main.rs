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
            // Task 13 replaces this arm with `sidecar::main(args)`; until
            // then the one thing a sidecar does without a bundle is refuse.
            let termination_log = args.termination_log.clone();
            let e = anyhow::anyhow!(
                "cannot read the agent bundle at {}: {}",
                args.bundle.display(),
                std::fs::read(&args.bundle)
                    .err()
                    .map_or("the sidecar is not implemented yet".to_string(), |e| e
                        .to_string())
            );
            balerix_agent::cli::terminate(&termination_log, &e)
        }
    }
}
