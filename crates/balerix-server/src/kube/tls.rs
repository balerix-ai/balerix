//! TLS serving for Kubernetes mode (Spec O §7.3, §10.3): axum-server on
//! rustls with the ring provider, from a mounted certificate and key the
//! operator issued under its per-Daemon authority.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;

pub struct TlsServer {
    handle: Handle<SocketAddr>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

/// Binds `addr` and serves `router` over TLS until `shutdown`. The
/// listener comes up inside the server task; `local_addr` waits for it.
pub async fn serve_tls(
    addr: SocketAddr,
    cert: &Path,
    key: &Path,
    router: Router,
) -> std::io::Result<TlsServer> {
    // rustls wants one process-wide provider; a second install (a test
    // running two daemons) answers `Err` and is harmless
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = RustlsConfig::from_pem_file(cert, key).await?;
    let handle: Handle<SocketAddr> = Handle::new();
    let h = handle.clone();
    let task = tokio::spawn(async move {
        axum_server::bind_rustls(addr, config)
            .handle(h)
            .serve(router.into_make_service())
            .await
    });
    Ok(TlsServer { handle, task })
}

impl TlsServer {
    /// The bound address, once listening; `None` if the server task ended
    /// first (a bind failure), which `shutdown` then reports.
    pub async fn local_addr(&self) -> Option<SocketAddr> {
        self.handle.listening().await
    }

    /// Stops accepting, gives in-flight requests five seconds, and
    /// returns the server task's result.
    pub async fn shutdown(self) -> std::io::Result<()> {
        self.handle.graceful_shutdown(Some(Duration::from_secs(5)));
        self.task
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?
    }
}
