use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use balerix_operator::controllers::RunConfig;
use balerix_operator::desired::common::Images;
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
    /// Print the five CustomResourceDefinitions, or write them as the
    /// operator chart's templates (`mise run crds`).
    Crds {
        /// The chart's `templates/crds/`: one guarded template per kind.
        #[arg(long)]
        chart_dir: Option<PathBuf>,
    },
    /// Run the controllers against the cluster the kubeconfig names
    /// (`KUBECONFIG`, or in-cluster).
    Run(RunArgs),
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Namespaces to watch, comma-separated; every namespace when absent (§5.6).
    #[arg(long, value_delimiter = ',')]
    watch_namespaces: Option<Vec<String>>,
    /// The operator's own namespace; `POD_NAMESPACE` when absent.
    #[arg(long, env = "POD_NAMESPACE")]
    namespace: Option<String>,
    /// The daemon image; this operator's version of `ghcr.io/balerix-ai/balerix` when absent.
    #[arg(long)]
    daemon_image: Option<String>,
    /// The agent image; this operator's version of `ghcr.io/balerix-ai/balerix-agent` when absent.
    #[arg(long)]
    agent_image: Option<String>,
}

async fn run(args: RunArgs) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let mut images = Images::for_version(version);
    if let Some(i) = args.daemon_image {
        images.daemon = i;
    }
    if let Some(i) = args.agent_image {
        images.agent = i;
    }
    let namespace = args
        .namespace
        .context("--namespace or POD_NAMESPACE is required: the Daemon's NetworkPolicy admits the operator's namespace")?;
    let mut cfg = RunConfig::new(version, images, &namespace);
    cfg.watch_namespaces = args.watch_namespaces;
    let config = kube::Config::infer()
        .await
        .context("cannot connect to the cluster (KUBECONFIG, or in-cluster)")?;
    let watches = balerix_operator::watch_client::watch_client(config.clone())
        .context("cannot build the watch client")?;
    let client = balerix_operator::request_client::request_client(config)
        .context("cannot build the client")?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("cannot listen for SIGTERM")?;
    tokio::select! {
        () = balerix_operator::controllers::run(client, watches, cfg) => {}
        _ = sigterm.recv() => tracing::info!("SIGTERM: stopping"),
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted: stopping"),
    }
    Ok(())
}

fn crds(chart_dir: Option<PathBuf>) -> Result<()> {
    match chart_dir {
        None => {
            let files = balerix_operator::api::crd_files()?;
            let docs: Vec<&str> = files.iter().map(|(_, yaml)| yaml.trim_end()).collect();
            println!("{}", docs.join("\n---\n"));
        }
        Some(dir) => {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
            for (name, text) in balerix_operator::api::chart_crd_files()? {
                let path = dir.join(&name);
                std::fs::write(&path, text)
                    .with_context(|| format!("cannot write {}", path.display()))?;
            }
        }
    }
    Ok(())
}

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
    let result = match cli.command {
        Command::Crds { chart_dir } => crds(chart_dir),
        Command::Run(args) => match tokio::runtime::Runtime::new() {
            Ok(rt) => rt.block_on(run(args)),
            Err(e) => Err(e.into()),
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("balerix-operator: {e:#}");
            ExitCode::FAILURE
        }
    }
}
