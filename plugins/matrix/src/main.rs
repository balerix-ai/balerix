//! `balerix-plugin-matrix`: read the daemon's environment, build the
//! actor's plumbing, say hello, serve. Failures print `matrix: …` to
//! stderr and exit 1; that lands in the plugin's tmux window and
//! `plugins/matrix/logs/`. The actor itself does not start until
//! `configure` arrives with the homeserver and the credentials.

use std::sync::Arc;

use balerix_api::FleetRecord;
use balerix_plugin_matrix::MatrixPlugin;
use balerix_plugin_matrix::actor::{Command, Counters, Health, Queue};
use balerix_plugin_matrix::client::MatrixLauncher;
use balerix_plugin_matrix::plugin::phase_changes;
use balerix_plugin_sdk::{Env, Host, Metrics, serve};

/// Feeds `Command::Phases` from `fleets/watch` for as long as the task
/// lives. Started before `serve`, like the web plugin's own watch: the
/// route (here, the queue) needs the token and the `fleets` capability,
/// not readiness. All the judgement is in `phase_changes`
/// (`balerix_plugin_matrix::plugin`); this loop only keeps the previous
/// frame and forwards whatever changes come back.
async fn watch_phases(host: Host, queue: Arc<Queue>) {
    let mut watch = host.watch_fleets();
    let mut previous: Option<Vec<FleetRecord>> = None;
    loop {
        let current = watch.next().await;
        let changes = phase_changes(previous.as_deref(), &current);
        if !changes.is_empty() {
            queue.push(Command::Phases(changes));
        }
        previous = Some(current);
    }
}

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
        let phase_watch = tokio::spawn(watch_phases(host.clone(), plugin.queue()));
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
