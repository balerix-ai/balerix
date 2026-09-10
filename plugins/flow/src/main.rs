//! `balerix-plugin-flow`: read the daemon's environment, say hello, serve.
//! Failures print `flow: …` to stderr and exit 1; that lands in the
//! plugin's tmux window and `plugins/flow/logs/`. The SDK logs through
//! `tracing`, so a stderr subscriber puts its lines in the same place.

use balerix_plugin_flow::FlowPlugin;
use balerix_plugin_sdk::{Env, Host, serve};

fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let env = Env::from_process()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let host = Host::new(env.clone())?;
        let plugin = FlowPlugin::new(Host::new(env)?)?;
        eprintln!("flow: starting");
        serve(&host, env!("CARGO_PKG_VERSION"), plugin).await?;
        Ok(())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("flow: {e:#}");
        std::process::exit(1);
    }
}
