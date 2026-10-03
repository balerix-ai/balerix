//! Trust for the Daemon (Spec O §10.3): one authority, the mounted file,
//! and nothing else; rustls on the ring provider, shared by the hook
//! forwarder (reqwest) and the link (tokio-tungstenite).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use rustls_pki_types::pem::PemObject;

pub fn client_config(ca: &Path) -> Result<Arc<rustls::ClientConfig>> {
    // one process-wide provider; a second install is a harmless `Err`
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    let mut count = 0;
    let certs = rustls_pki_types::CertificateDer::pem_file_iter(ca)
        .with_context(|| format!("cannot read the authority at {}", ca.display()))?;
    for cert in certs {
        let cert = cert.with_context(|| format!("{}: not a PEM certificate", ca.display()))?;
        roots
            .add(cert)
            .with_context(|| format!("{}: not a usable certificate", ca.display()))?;
        count += 1;
    }
    ensure!(
        count > 0,
        "{}: no certificate in the authority file",
        ca.display()
    );
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// An HTTP client trusting `tls` alone, no proxy, one overall timeout.
/// `http://` URLs (tests, `--allow-plain-http`) never touch it.
pub fn http_client(tls: &Arc<rustls::ClientConfig>, timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls((**tls).clone())
        .no_proxy()
        .timeout(timeout)
        .build()?)
}
