//! TLS serving for Kubernetes mode (Spec O §7.3, §10.3): axum-server on
//! rustls with the ring provider, from a mounted certificate and key the
//! operator issued under its per-Daemon authority.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use rustls_pki_types::pem::PemObject;

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

fn build(roots: rustls::RootCertStore) -> Result<Arc<rustls::ClientConfig>, rustls::Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The Daemon's trust for a plugin (Spec O §23.1): the authority file's
/// certificates and nothing else, no webpki or native roots. Its body is the
/// SDK's `client_config`; this crate does not depend on the SDK.
pub fn client_config(ca: &Path) -> std::io::Result<Arc<rustls::ClientConfig>> {
    let fail = |e: &dyn std::fmt::Display| std::io::Error::other(format!("{}: {e}", ca.display()));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pki_types::CertificateDer::pem_file_iter(ca).map_err(|e| fail(&e))? {
        roots
            .add(cert.map_err(|e| fail(&e))?)
            .map_err(|e| fail(&e))?;
    }
    if roots.is_empty() {
        return Err(fail(&"no certificate in the authority file"));
    }
    build(roots).map_err(|e| fail(&e))
}

/// A config that trusts nothing: with no authority an `https://` plugin
/// fails its handshake rather than falling back to the system's roots, which
/// reqwest's rustls feature would otherwise use (§23.1).
pub fn no_roots() -> std::io::Result<Arc<rustls::ClientConfig>> {
    build(rustls::RootCertStore::empty()).map_err(std::io::Error::other)
}
