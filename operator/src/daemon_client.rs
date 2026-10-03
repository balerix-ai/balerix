//! The Daemon's admin API, as the operator uses it (Spec O §5.2, §7.4):
//! `PUT /v1/fleets/{name}` with one token per agent, the forced `DELETE`,
//! the fleet's record, and `/readyz`. It trusts one authority, the
//! Daemon's own (§10.3), and nothing else; the Daemon holds no Kubernetes
//! credentials, so everything between the two goes through here (O-8).

use std::sync::Arc;
use std::time::Duration;

use balerix_api::{DownQuery, ErrorBody, FleetRecord, FleetRequest};
use reqwest::StatusCode;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// No answer, a TLS failure, or `/readyz` saying 503: the Fleet is
    /// `DaemonUnavailable` and the reconcile is retried (§5.2).
    #[error("the Daemon is unavailable: {0}")]
    Unavailable(String),
    /// A 400: `Accepted=False` with this message; nothing landed.
    #[error("{0}")]
    Rejected(String),
    /// A 409: the fleet is another owner's.
    #[error("{0}")]
    Conflict(String),
    #[error("the Daemon answered {status}: {message}")]
    Unexpected { status: u16, message: String },
    /// The authority or the URL this client was given is unusable.
    #[error("{0}")]
    Setup(String),
}

/// Holds the admin token: no derived `Debug`, and no error text built here
/// carries it.
pub struct DaemonClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonClient")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

fn unavailable(e: reqwest::Error) -> ClientError {
    // reqwest's own text names the URL, never a header
    ClientError::Unavailable(e.without_url().to_string())
}

/// A success answer's body as `T`. A body that does not decode is the
/// Daemon answering something unexpected, not a Daemon that does not answer
/// (§5.2): `Unexpected`, with the decode error's own text.
async fn decoded<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ClientError> {
    let status = response.status().as_u16();
    response.json().await.map_err(|e| {
        if !e.is_decode() {
            return unavailable(e);
        }
        let e = e.without_url();
        let message = match std::error::Error::source(&e) {
            Some(cause) => format!("{e}: {cause}"),
            None => e.to_string(),
        };
        ClientError::Unexpected { status, message }
    })
}

/// The answer's message: the `ErrorBody`'s `error`, or the body as plain
/// text when it is not one. A body that cannot be read is reported as that,
/// with the read error's own text.
async fn message(response: reqwest::Response) -> String {
    match response.text().await {
        Ok(text) => match serde_json::from_str::<ErrorBody>(&text) {
            Ok(body) => body.error,
            // not an ErrorBody: the text itself is the message
            Err(_) => text,
        },
        Err(e) => format!("the response body could not be read: {}", e.without_url()),
    }
}

impl DaemonClient {
    /// `base_url` is the Daemon's `status.endpoint`; `authority_pem` the
    /// one certificate trusted. A base that is not `https://` is refused:
    /// the admin token is never sent in clear.
    pub fn new(
        base_url: &str,
        authority_pem: &str,
        admin_token: &str,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let setup =
            |what: &str, e: &dyn std::fmt::Display| ClientError::Setup(format!("{what}: {e}"));
        let scheme = reqwest::Url::parse(base_url)
            .map_err(|e| setup("the Daemon's endpoint is not a URL", &e))?
            .scheme()
            .to_string();
        if scheme != "https" {
            return Err(ClientError::Setup(format!(
                "the Daemon's endpoint is {scheme}://, not https://: the admin token is never sent in clear"
            )));
        }
        // one process-wide provider; `Err` only means one is already installed
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(authority_pem.as_bytes()) {
            let cert = cert.map_err(|e| setup("the authority is not a PEM certificate", &e))?;
            roots
                .add(cert)
                .map_err(|e| setup("the authority is not a usable certificate", &e))?;
        }
        if roots.is_empty() {
            return Err(ClientError::Setup(
                "the authority holds no certificate".to_string(),
            ));
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| setup("TLS", &e))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .no_proxy()
            .timeout(timeout)
            .build()
            .map_err(|e| setup("the HTTP client", &e))?;
        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
            token: admin_token.to_string(),
        })
    }

    /// A client over plain HTTP with no TLS settings. It exists for the
    /// stub tests in `tests/client_it.rs` alone and must never be called by
    /// a controller: it sends the admin token in clear.
    #[doc(hidden)]
    pub fn insecure_for_tests(base_url: &str, admin_token: &str) -> Result<Self, ClientError> {
        // reqwest builds with no provider of its own; as in `new`
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ClientError::Setup(format!("the HTTP client: {e}")))?;
        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
            token: admin_token.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn refused(response: reqwest::Response) -> ClientError {
        let status = response.status();
        let message = message(response).await;
        match status {
            StatusCode::BAD_REQUEST => ClientError::Rejected(message),
            StatusCode::CONFLICT => ClientError::Conflict(message),
            other => ClientError::Unexpected {
                status: other.as_u16(),
                message,
            },
        }
    }

    /// `GET /readyz`: the Daemon's system pool and nothing else (§7.3).
    pub async fn ready(&self) -> Result<(), ClientError> {
        let response = self
            .http
            .get(self.url("/readyz"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(unavailable)?;
        match response.status() {
            StatusCode::OK => Ok(()),
            StatusCode::SERVICE_UNAVAILABLE => {
                Err(ClientError::Unavailable(message(response).await))
            }
            _ => Err(Self::refused(response).await),
        }
    }

    /// The operator's apply: an upsert, re-sent on every reconcile.
    pub async fn apply(&self, request: &FleetRequest) -> Result<FleetRecord, ClientError> {
        let response = self
            .http
            .put(self.url(&format!("/v1/fleets/{}", request.spec.name)))
            .bearer_auth(&self.token)
            .json(request)
            .send()
            .await
            .map_err(unavailable)?;
        if !response.status().is_success() {
            return Err(Self::refused(response).await);
        }
        decoded(response).await
    }

    /// The fleet's record, with its status; `None` when the Daemon has
    /// none by that name.
    pub async fn get(&self, fleet: &str) -> Result<Option<FleetRecord>, ClientError> {
        let response = self
            .http
            .get(self.url(&format!("/v1/fleets/{fleet}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(unavailable)?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => decoded(response).await.map(Some),
            _ => Err(Self::refused(response).await),
        }
    }

    /// The operator's down: `DELETE …?force=true` (§7.4). A fleet the
    /// Daemon does not have is already down.
    pub async fn down(&self, fleet: &str) -> Result<(), ClientError> {
        let query = DownQuery {
            force: true,
            ..Default::default()
        }
        .to_query_string();
        let response = self
            .http
            .delete(self.url(&format!("/v1/fleets/{fleet}?{query}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(unavailable)?;
        match response.status() {
            s if s == StatusCode::NOT_FOUND || s.is_success() => Ok(()),
            _ => Err(Self::refused(response).await),
        }
    }
}
