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
use balerix_server::kube::{self, LinkHub, NoFiles, NoPool, serve_tls};
use balerix_server::{
    Daemon, FileFleetStore, Metrics, PluginClient, PluginEventHandler, PluginHostConfig, PluginKv,
    PluginRegistry, PluginSetup, Ports, ServerPaths, Vault, load_or_create_token, read_endpoint,
    remove_if_exists, router, serve, write_endpoint, write_pid,
};
use serde::Deserialize;

use crate::cli::{ServeArgs, ServeMode};
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
    match args.mode {
        ServeMode::Tmux => {
            if args.tls_cert.is_some()
                || args.tls_key.is_some()
                || args.tls_ca.is_some()
                || args.admin_token_file.is_some()
            {
                bail!(
                    "--tls-cert, --tls-key, --tls-ca and --admin-token-file are for --mode kubernetes"
                );
            }
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
        ServeMode::Kubernetes => {
            let (Some(cert), Some(key), Some(token_file)) =
                (&args.tls_cert, &args.tls_key, &args.admin_token_file)
            else {
                bail!(
                    "serve --mode kubernetes needs --tls-cert, --tls-key and --admin-token-file (Spec O §7.3)"
                );
            };
            if args.detach {
                bail!(
                    "--detach is not available with --mode kubernetes; a pod runs the daemon in the foreground"
                );
            }
            let addr: SocketAddr = bind
                .parse()
                .with_context(|| format!("bind address {bind:?} is not a valid host:port"))?;
            run_kubernetes(
                &layout,
                &paths,
                addr,
                &config.log,
                &KubeFiles {
                    cert,
                    key,
                    token_file,
                    ca: args.tls_ca.as_deref(),
                },
            )
        }
    }
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

/// The files `--mode kubernetes` reads; `ca` is optional (Spec O §23.1).
struct KubeFiles<'a> {
    cert: &'a Path,
    key: &'a Path,
    token_file: &'a Path,
    ca: Option<&'a Path>,
}

/// Spec O §7.3: TLS on the pod address, the operator's admin token, the
/// link hub as the runner and the workspace reader, no files to
/// materialise, a pool that is a Job's, and no `plugins.yaml`: the
/// plugins are the operator's list, sent with `PUT /v1/plugins` (§23.2),
/// so there is no startup plugin sync. One process per pod, so there is
/// no `already_running` check and no detach.
fn run_kubernetes(
    layout: &StateLayout,
    paths: &ServerPaths,
    addr: SocketAddr,
    log: &str,
    files: &KubeFiles<'_>,
) -> Result<String> {
    let KubeFiles {
        cert,
        key,
        token_file,
        ca,
    } = *files;
    init_tracing(paths, log, false)?;
    let tls = ca
        .map(kube::tls::client_config)
        .transpose()
        .with_context(|| "--tls-ca")?;
    let token = std::fs::read_to_string(token_file)
        .with_context(|| format!("cannot read {}", token_file.display()))?
        .trim()
        .to_string();
    if token.len() < 32 {
        bail!(
            "{}: the admin token is at least 32 characters",
            token_file.display()
        );
    }
    let vault = Vault::load_or_create(&paths.vault_key())?;
    let store = FileFleetStore::new(layout.fleets_dir(), vault.clone());
    let existing = store.load_all()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let hub = LinkHub::new();
        let ports = Ports {
            materializer: Arc::new(NoFiles),
            runner: hub.clone(),
            clock: Arc::new(SystemClock),
            store: Arc::new(store),
            workspace: hub.clone(),
            resolver: Arc::new(HostResolver),
            credentials: Arc::new(HostResolver),
            policy: ReconcilePolicy::default(),
            hook_url: format!("https://{addr}"),
            resync: RESYNC,
            kube: Some(hub),
        };
        let fleets = existing.len();
        let metrics = Metrics::new()?;
        let registry = PluginRegistry::new();
        let client = PluginClient::new(tls).map_err(|e| anyhow!("plugins: {e}"))?;
        let kv = Arc::new(PluginKv::new(layout.plugins_state_dir(), vault.clone()));
        let handler = PluginEventHandler::new(registry.clone(), client.clone(), metrics.clone());
        let daemon = Daemon::start(
            ports,
            handler,
            metrics,
            token,
            existing,
            PluginSetup::Declared {
                state_dir: layout.plugins_state_dir(),
                managed_dir: layout.managed_dir(),
            },
            registry,
            client,
            kv,
            Arc::new(NoPool),
        );
        let server = serve_tls(addr, cert, key, router(daemon)).await?;
        let Some(local) = server.local_addr().await else {
            server.shutdown().await?;
            bail!("cannot bind {addr}");
        };
        let url = format!("https://{local}");
        write_pid(&paths.pid(), std::process::id())?;
        write_endpoint(&paths.endpoint(), &url)?;
        tracing::info!(%url, fleets, "balerix daemon listening (kubernetes mode)");
        eprintln!("listening on {url}");
        shutdown_signal().await;
        tracing::info!("shutting down");
        server.shutdown().await?;
        remove_if_exists(&paths.endpoint())?;
        remove_if_exists(&paths.pid())?;
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(String::new())
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
            kube: None,
        };
        let fleets = existing.len();
        // One `Metrics` for the daemon and the chain: the handler's counters
        // are the ones `/metrics` encodes (plugins spec §12).
        let metrics = Metrics::new()?;
        let registry = PluginRegistry::new();
        let client = PluginClient::new(None).map_err(|e| anyhow!("plugins: {e}"))?;
        let kv = Arc::new(PluginKv::new(layout.plugins_state_dir(), vault.clone()));
        let handler = PluginEventHandler::new(registry.clone(), client.clone(), metrics.clone());
        let daemon = Daemon::start(
            ports,
            handler,
            metrics,
            token,
            existing,
            PluginSetup::Packages(PluginHostConfig {
                plugins_file: layout.config_root.join("plugins.yaml"),
                install_root: layout.plugins_data_dir(),
            }),
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
