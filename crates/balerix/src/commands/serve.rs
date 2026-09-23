//! `balerix serve` (Phase 3 spec §5): wire the runtime adapters into the
//! daemon, bind, publish the endpoint, run until SIGINT/SIGTERM.

use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use balerix_core::{FleetStore, ReconcilePolicy};
use balerix_runtime::{Runtime, StateLayout, TmuxRunner};
use balerix_server::{
    Daemon, FileFleetStore, Metrics, PluginClient, PluginEventHandler, PluginHostConfig, PluginKv,
    PluginRegistry, Ports, ServerPaths, Vault, load_or_create_token, read_endpoint,
    remove_if_exists, router, serve, write_endpoint, write_pid,
};
use serde::Deserialize;

use crate::cli::ServeArgs;
use crate::wiring::{HostResolver, SystemClock, layout_from_env, server_paths, tool_paths};

const RESYNC: Duration = Duration::from_secs(30);
const DETACH_WAIT: Duration = Duration::from_secs(10);

/// `$XDG_CONFIG_HOME/balerix/config.toml`, all optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub bind: String,
    pub log: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    server: ServerTable,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerTable {
    bind: Option<String>,
    log: Option<String>,
}

impl ServerConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let file: ConfigFile = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("{}: invalid config: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        Ok(Self {
            bind: file
                .server
                .bind
                .unwrap_or_else(|| "127.0.0.1:7643".to_string()),
            log: file.server.log.unwrap_or_else(|| "info".to_string()),
        })
    }
}

/// Reject any `bind` address that is not loopback (Phase 3 spec P3-1: the
/// daemon speaks plain HTTP, so it may only ever listen on 127.0.0.1 / ::1).
/// Checked once here so both the foreground and `-d` (detach, which
/// re-execs itself with the same `--bind`) paths hit it before anything
/// else runs.
fn require_loopback(bind: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("bind address {bind:?} is not a valid host:port"))?;
    if !addr.ip().is_loopback() {
        bail!(
            "bind address {addr} is not loopback; the daemon speaks plain HTTP and only listens on 127.0.0.1 (Phase 3 spec P3-1)"
        );
    }
    Ok(addr)
}

pub fn serve_command(args: &ServeArgs) -> Result<String> {
    let layout = layout_from_env()?;
    let paths = server_paths(&layout);
    let config = ServerConfig::load(&layout.config_root.join("config.toml"))?;
    let bind = args.bind.clone().unwrap_or(config.bind);
    require_loopback(&bind)?;
    if args.detach {
        return detach(&paths, &bind, &args.tmux_socket);
    }
    run(
        &layout,
        &paths,
        &bind,
        &config.log,
        &args.tmux_socket,
        args.detached_child,
    )
}

fn already_running(paths: &ServerPaths) -> Result<Option<String>> {
    let Some(url) = read_endpoint(&paths.endpoint())? else {
        return Ok(None);
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .into();
    Ok(agent
        .get(format!("{url}/healthz"))
        .call()
        .is_ok()
        .then_some(url))
}

fn init_tracing(paths: &ServerPaths, level: &str, to_file: bool) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .with_context(|| format!("config.toml: invalid log level {level:?}"))?;
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    if to_file {
        std::fs::create_dir_all(&paths.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.log())?;
        builder.with_writer(std::sync::Mutex::new(file)).init();
    } else {
        builder.with_writer(std::io::stderr).init();
    }
    Ok(())
}

fn run(
    layout: &StateLayout,
    paths: &ServerPaths,
    bind: &str,
    log: &str,
    tmux_socket: &str,
    detached_child: bool,
) -> Result<String> {
    if let Some(url) = already_running(paths)? {
        bail!("a balerix daemon is already running at {url}");
    }
    init_tracing(paths, log, detached_child)?;
    let tools = tool_paths()?;
    let token = load_or_create_token(&paths.token())?;
    let vault = Vault::load_or_create(&paths.vault_key())?;
    let store = FileFleetStore::new(layout.fleets_dir(), vault.clone());
    let existing = store.load_all()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("cannot bind {bind}"))?;
        let url = format!("http://{}", listener.local_addr()?);
        let runtime = Arc::new(Runtime::new(layout.clone(), tools.clone()));
        let ports = Ports {
            materializer: runtime.clone(),
            runner: Arc::new(TmuxRunner::new(tools.tmux.clone(), tmux_socket)),
            clock: Arc::new(SystemClock),
            store: Arc::new(store),
            workspace: runtime.clone(),
            resolver: Arc::new(HostResolver),
            credentials: Arc::new(HostResolver),
            policy: ReconcilePolicy::default(),
            hook_url: url.clone(),
            resync: RESYNC,
        };
        let fleets = existing.len();
        // One `Metrics` for the daemon and the chain: the handler's counters
        // are the ones `/metrics` encodes (plugins spec §12).
        let metrics = Metrics::new()?;
        let registry = PluginRegistry::new();
        let client = PluginClient::new().map_err(|e| anyhow!("plugins: {e}"))?;
        let kv = Arc::new(PluginKv::new(layout.plugins_state_dir(), vault.clone()));
        let handler = PluginEventHandler::new(registry.clone(), client.clone(), metrics.clone());
        let daemon = Daemon::start(
            ports,
            handler,
            metrics,
            token,
            existing,
            PluginHostConfig {
                plugins_file: layout.config_root.join("plugins.yaml"),
                install_root: layout.plugins_data_dir(),
            },
            registry,
            client,
            kv,
            runtime,
        );
        let plugins = daemon
            .sync_plugins()
            .await
            .map_err(|e| anyhow!("plugins: {e}"))?;
        tracing::info!(
            installed = plugins.installed.len(),
            unchanged = plugins.unchanged.len(),
            "plugins synced"
        );
        // The endpoint file is the readiness signal: `serve -d` returns and
        // clients connect the moment it appears, so everything they may read
        // next — the pid file above all — must already be in place.
        write_pid(&paths.pid(), std::process::id())?;
        write_endpoint(&paths.endpoint(), &url)?;
        tracing::info!(%url, fleets, tmux_socket, "balerix daemon listening");
        if !detached_child {
            eprintln!("listening on {url}");
        }
        serve(listener, router(daemon), shutdown_signal()).await?;
        tracing::info!("shutting down; agents keep running in tmux");
        // Endpoint first, so clients and a new `serve` stop resolving this
        // daemon; pid last, so the file names the process for as long as it
        // exists. `cli_serve` polls for both to be gone.
        remove_if_exists(&paths.endpoint())?;
        remove_if_exists(&paths.pid())?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(String::new())
}

/// SIGINT or SIGTERM ends the daemon; SIGHUP is ignored so a closed
/// terminal does not take a detached daemon with it.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot listen for SIGTERM: {e}");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    let _hup = signal(SignalKind::hangup()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn detach(paths: &ServerPaths, bind: &str, tmux_socket: &str) -> Result<String> {
    if let Some(url) = already_running(paths)? {
        bail!("a balerix daemon is already running at {url}");
    }
    remove_if_exists(&paths.endpoint())?;
    std::fs::create_dir_all(&paths.dir)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log())?;
    let exe = std::env::current_exe().context("cannot determine balerix's own path")?;
    let mut child = Command::new(exe)
        .args([
            "serve",
            "--bind",
            bind,
            "--tmux-socket",
            tmux_socket,
            "--detached-child",
        ])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("cannot start the daemon")?;
    let start = Instant::now();
    loop {
        if let Some(url) = read_endpoint(&paths.endpoint())? {
            return Ok(format!(
                "balerix daemon started (pid {}) at {url}\nlog: {}\n",
                child.id(),
                paths.log().display()
            ));
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "daemon exited early ({status}); see {}",
                paths.log().display()
            );
        }
        if start.elapsed() > DETACH_WAIT {
            bail!(
                "daemon did not publish an endpoint within {}s; see {}",
                DETACH_WAIT.as_secs(),
                paths.log().display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_when_missing_and_parses_the_server_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = ServerConfig::load(&path).unwrap();
        assert_eq!(
            (c.bind.as_str(), c.log.as_str()),
            ("127.0.0.1:7643", "info")
        );
        std::fs::write(&path, "[server]\nbind = \"127.0.0.1:9000\"\n").unwrap();
        let c = ServerConfig::load(&path).unwrap();
        assert_eq!(
            (c.bind.as_str(), c.log.as_str()),
            ("127.0.0.1:9000", "info")
        );
        std::fs::write(&path, "[server]\nport = 1\n").unwrap();
        let e = ServerConfig::load(&path).unwrap_err().to_string();
        assert!(e.contains("config.toml") && e.contains("port"), "{e}");
    }

    #[test]
    fn require_loopback_accepts_loopback_and_rejects_everything_else() {
        require_loopback("127.0.0.1:0").unwrap();
        require_loopback("[::1]:0").unwrap();
        let e = require_loopback("0.0.0.0:1").unwrap_err().to_string();
        assert!(e.contains("not loopback"), "{e}");
    }
}
