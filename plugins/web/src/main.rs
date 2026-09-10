//! `balerix-plugin-web`: read the daemon's environment, start the watch,
//! say hello, serve. Failures print `web: …` to stderr and exit 1; that
//! lands in the plugin's tmux window and `plugins/web/logs/`. The SDK logs
//! through `tracing`, so a stderr subscriber puts its lines in the same
//! place.

use balerix_plugin_sdk::{Env, Host, serve};
use balerix_plugin_web::WebPlugin;

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
        let host = Host::new(env)?;
        let plugin = WebPlugin::new(host.clone())?;
        let watch = plugin.start_watch();
        eprintln!("web: starting");
        let result = serve(&host, env!("CARGO_PKG_VERSION"), plugin).await;
        watch.abort();
        result?;
        Ok(())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("web: {e:#}");
        std::process::exit(1);
    }
}
