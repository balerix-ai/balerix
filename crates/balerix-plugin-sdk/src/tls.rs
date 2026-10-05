//! Trust for a plugin in a pod (Spec O §23.1): one authority, the mounted
//! file, and nothing else; rustls on the ring provider, shared by the host
//! client (reqwest) and the streams (tokio-tungstenite).

use std::path::Path;
use std::sync::Arc;

use rustls_pki_types::pem::PemObject;

use crate::SdkError;

fn tls_error(path: &Path, e: impl std::fmt::Display) -> SdkError {
    SdkError::Transport(format!("{}: {e}", path.display()))
}

/// reqwest built with `rustls-no-provider` panics without a process-wide
/// provider, CA or not; a second install is a harmless `Err`.
pub(crate) fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn client_config(ca: &Path) -> Result<Arc<rustls::ClientConfig>, SdkError> {
    install_provider();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pki_types::CertificateDer::pem_file_iter(ca).map_err(|e| tls_error(ca, e))? {
        roots
            .add(cert.map_err(|e| tls_error(ca, e))?)
            .map_err(|e| tls_error(ca, e))?;
    }
    if roots.is_empty() {
        return Err(tls_error(ca, "no certificate in the authority file"));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| tls_error(ca, e))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}
