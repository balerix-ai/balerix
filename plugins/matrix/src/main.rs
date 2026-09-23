//! `balerix-plugin-matrix`: read the daemon's environment, build the
//! actor's plumbing, say hello, serve. Failures print `matrix: …` to
//! stderr and exit 1; that lands in the plugin's tmux window and
//! `plugins/matrix/logs/`. The actor itself does not start until
//! `configure` arrives with the homeserver and the credentials.

use balerix_plugin_matrix::MatrixPlugin;
use balerix_plugin_matrix::actor::{Command, Counters, Health, Queue};
use balerix_plugin_matrix::client::MatrixLauncher;
use balerix_plugin_sdk::{Env, Host, Metrics, serve};

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
        let metrics = Metrics::new(&env.name);
        let counters = Counters::new(&metrics)?;
        let health = Health::new();
        let queue = Queue::new(counters.events_dropped.clone());
        let launcher = MatrixLauncher {
            host: Host::new(env.clone())?,
            counters: counters.clone(),
            health: health.clone(),
        };
        let plugin = MatrixPlugin::new(metrics, health, queue, launcher);
        let queue = plugin.queue();
        let phase_watch = tokio::spawn(balerix_plugin_common::phases::run(
            host.clone(),
            move |changes| queue.push(Command::Phases(changes)),
        ));
        eprintln!("matrix: starting");
        let result = serve(&host, env!("CARGO_PKG_VERSION"), plugin).await;
        phase_watch.abort();
        result?;
        Ok(())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("matrix: {e:#}");
        std::process::exit(1);
    }
}
