use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "balerix-operator",
    version,
    about = "The balerix operator: Daemon, Fleet, Crew, Agent and Plugin objects into pods, claims, Secrets and Jobs (Spec O §5)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the five CustomResourceDefinitions, or write one file each.
    Crds {
        /// A directory for `<plural>.balerix.ai.yaml`; stdout when absent.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn crds(out: Option<PathBuf>) -> Result<()> {
    let files = balerix_operator::api::crd_files()?;
    match out {
        None => {
            let docs: Vec<&str> = files.iter().map(|(_, yaml)| yaml.trim_end()).collect();
            println!("{}", docs.join("\n---\n"));
        }
        Some(dir) => {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
            for (name, yaml) in &files {
                let path = dir.join(name);
                std::fs::write(&path, yaml)
                    .with_context(|| format!("cannot write {}", path.display()))?;
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Crds { out } => crds(out),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("balerix-operator: {e:#}");
            ExitCode::FAILURE
        }
    }
}
