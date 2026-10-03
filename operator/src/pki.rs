//! One certificate authority per Daemon, and the serving certificates it
//! signs (Spec O §10.3). The controller keeps each pair in a Secret with
//! its expiry as an annotation, so nothing here or there parses X.509.
//! Times are unix seconds, given by the caller.

use rand::Rng;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use time::OffsetDateTime;

pub const AUTHORITY_DAYS: i64 = 3650;
pub const SERVING_DAYS: i64 = 90;
pub const RENEW_BEFORE_DAYS: i64 = 30;
/// A certificate is valid from this long before it is made: a pod whose
/// clock runs a little behind the operator's must still accept it.
const SKEW_SECS: i64 = 300;
const DAY_SECS: i64 = 86_400;

/// A certificate and its private key, PEM, and when it expires.
#[derive(Clone, PartialEq, Eq)]
pub struct Issued {
    pub cert_pem: String,
    pub key_pem: String,
    pub not_after: i64,
}

impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issued")
            .field("key_pem", &"<redacted>")
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PkiError {
    #[error("certificate: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("certificate: the time {0} is out of range")]
    Time(i64),
    #[error("certificate: a serving certificate was asked for with no names")]
    NoNames,
}

fn at(unix: i64) -> Result<OffsetDateTime, PkiError> {
    OffsetDateTime::from_unix_timestamp(unix).map_err(|_| PkiError::Time(unix))
}

/// The authority's parameters are a function of the Daemon alone, so an
/// authority read back from its Secret signs with the same name and key
/// identifier as the one that was made.
fn authority_params(namespace: &str, daemon: &str) -> Result<CertificateParams, PkiError> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.distinguished_name.push(
        DnType::CommonName,
        format!("balerix daemon {namespace}/{daemon}"),
    );
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    Ok(params)
}

pub fn new_authority(namespace: &str, daemon: &str, now: i64) -> Result<Issued, PkiError> {
    let mut params = authority_params(namespace, daemon)?;
    let not_after = now + AUTHORITY_DAYS * DAY_SECS;
    params.not_before = at(now - SKEW_SECS)?;
    params.not_after = at(not_after)?;
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(Issued {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        not_after,
    })
}

/// A serving certificate for `names` (DNS names, or IP addresses as
/// text), signed by the Daemon's authority.
pub fn issue_serving(
    authority: &Issued,
    namespace: &str,
    daemon: &str,
    names: &[String],
    now: i64,
) -> Result<Issued, PkiError> {
    let common_name = names.first().ok_or(PkiError::NoNames)?;
    let issuer = Issuer::new(
        authority_params(namespace, daemon)?,
        KeyPair::from_pem(&authority.key_pem)?,
    );
    let mut params = CertificateParams::new(names.to_vec())?;
    params
        .distinguished_name
        .push(DnType::CommonName, common_name.clone());
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let not_after = now + SERVING_DAYS * DAY_SECS;
    params.not_before = at(now - SKEW_SECS)?;
    params.not_after = at(not_after)?;
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, &issuer)?;
    Ok(Issued {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        not_after,
    })
}

pub fn needs_renewal(not_after: i64, now: i64) -> bool {
    now >= not_after - RENEW_BEFORE_DAYS * DAY_SECS
}

/// The names a sidecar, a plugin or the operator may call the Daemon by.
pub fn daemon_names(namespace: &str, daemon: &str) -> Vec<String> {
    let service = crate::desired::names::daemon(daemon);
    vec![
        service.clone(),
        format!("{service}.{namespace}"),
        format!("{service}.{namespace}.svc"),
        format!("{service}.{namespace}.svc.cluster.local"),
    ]
}

/// 32 random bytes as hex: an agent's token (the Daemon wants at least
/// 32 characters, §7.4) or a Daemon's admin token.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::sync::Arc;

    use rustls::client::danger::ServerCertVerifier;
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

    use super::*;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn verify(authority: &Issued, leaf: &Issued, name: &str, at: i64) -> Result<(), String> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(authority.cert_pem.as_bytes()).unwrap())
            .unwrap();
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        verifier
            .verify_server_cert(
                &CertificateDer::from_pem_slice(leaf.cert_pem.as_bytes()).unwrap(),
                &[],
                &ServerName::try_from(name.to_string()).unwrap(),
                &[],
                UnixTime::since_unix_epoch(std::time::Duration::from_secs(at as u64)),
            )
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }

    #[test]
    fn a_serving_certificate_verifies_for_the_daemons_names_until_it_expires() {
        let ca = new_authority("team-a", "default", NOW).unwrap();
        let names = daemon_names("team-a", "default");
        assert_eq!(
            names,
            [
                "balerix-default",
                "balerix-default.team-a",
                "balerix-default.team-a.svc",
                "balerix-default.team-a.svc.cluster.local"
            ]
        );
        let leaf = issue_serving(&ca, "team-a", "default", &names, NOW).unwrap();
        assert_eq!(ca.not_after, NOW + 3650 * DAY);
        assert_eq!(leaf.not_after, NOW + 90 * DAY);
        for name in &names {
            verify(&ca, &leaf, name, NOW + DAY).unwrap();
        }
        // a clock five minutes behind the operator's still accepts it
        verify(&ca, &leaf, &names[2], NOW - 299).unwrap();
        let wrong = verify(&ca, &leaf, "other.team-a.svc", NOW + DAY).unwrap_err();
        assert!(wrong.contains("NotValidForName"), "{wrong}");
        let late = verify(&ca, &leaf, &names[2], NOW + 91 * DAY).unwrap_err();
        assert!(late.contains("Expired"), "{late}");
    }

    /// The controller keeps only the authority's PEM pair in a Secret: a
    /// certificate issued from that pair read back, on a later reconcile,
    /// must chain to the same authority.
    #[test]
    fn an_authority_read_back_from_its_secret_issues_the_same_chain() {
        let ca = new_authority("team-a", "default", NOW).unwrap();
        let read_back = Issued {
            cert_pem: ca.cert_pem.clone(),
            key_pem: ca.key_pem.clone(),
            not_after: ca.not_after,
        };
        let names = vec!["127.0.0.1".to_string()];
        let leaf = issue_serving(&read_back, "team-a", "default", &names, NOW + 60 * DAY).unwrap();
        verify(&ca, &leaf, "127.0.0.1", NOW + 61 * DAY).unwrap();
        let other = new_authority("team-a", "default", NOW).unwrap();
        assert!(verify(&other, &leaf, "127.0.0.1", NOW + 61 * DAY).is_err());
    }

    #[test]
    fn a_serving_certificate_needs_a_name() {
        let ca = new_authority("team-a", "default", NOW).unwrap();
        let e = issue_serving(&ca, "team-a", "default", &[], NOW).unwrap_err();
        assert!(e.to_string().contains("no names"), "{e}");
    }

    #[test]
    fn renewal_starts_thirty_days_before_expiry() {
        let not_after = NOW + 90 * DAY;
        assert!(!needs_renewal(not_after, NOW));
        assert!(!needs_renewal(not_after, NOW + 60 * DAY - 1));
        assert!(needs_renewal(not_after, NOW + 60 * DAY));
        assert!(needs_renewal(not_after, NOW + 200 * DAY));
    }

    #[test]
    fn a_key_and_a_token_are_never_printed() {
        let ca = new_authority("team-a", "default", NOW).unwrap();
        let shown = format!("{ca:?}");
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(!shown.contains("PRIVATE KEY"), "{shown}");
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
