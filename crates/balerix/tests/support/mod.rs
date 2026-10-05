//! Shared scaffolding for the tests that run the `balerix` binary: a
//! sandboxed `HOME`, fake tools, and an HTTPS client on raw rustls.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// `support` is compiled into every integration test that declares it; a
// helper only one of them uses is not dead code for the suite.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Fake tools on PATH: `serve` discovers them but calls none without a fleet.
pub fn fake_tools(dir: &Path) {
    for t in ["git", "gh", "mise", "nono", "tmux"] {
        fs::write(dir.join(t), "#!/bin/sh\nexit 0\n").unwrap();
    }
}

pub fn balerix(home: &Path, tools: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home)
        .env("PATH", tools)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("BALERIX_API_URL");
    cmd
}

pub fn wait_for_file(path: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(s) = fs::read_to_string(path)
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub struct Kill(pub Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A throwaway authority and a leaf for 127.0.0.1, as PEM files in `dir`:
/// (ca.crt, tls.crt, tls.key).
pub fn tls_files(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "balerix test authority");
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let ca_pem = dir.join("ca.crt");
    fs::write(&ca_pem, ca_cert.pem()).unwrap();
    let issuer = rcgen::Issuer::new(ca, ca_key);
    let leaf =
        rcgen::CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()])
            .unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = leaf.signed_by(&key, &issuer).unwrap();
    let cert_pem = dir.join("tls.crt");
    let key_pem = dir.join("tls.key");
    fs::write(&cert_pem, cert.pem()).unwrap();
    fs::write(&key_pem, key.serialize_pem()).unwrap();
    (ca_pem, cert_pem, key_pem)
}

/// One HTTPS GET over rustls on a plain TcpStream: the core workspace has
/// no TLS client (P3-1 holds for ureq and reqwest), and the test needs
/// none beyond rustls itself.
pub fn tls_get(addr: &str, ca: &Path, path: &str, token: Option<&str>) -> (u16, String) {
    tls_request(addr, ca, "GET", path, token, None)
}

/// `tls_get` for any method with an optional JSON body; the answer is
/// parsed as JSON (`null` for an empty body, a string when it is not JSON).
pub fn tls_call(
    addr: &str,
    ca: &Path,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (u16, Value) {
    let body = body.map(|b| serde_json::to_vec(b).unwrap());
    let (status, text) = tls_request(addr, ca, method, path, token, body.as_deref());
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, value)
}

fn tls_request(
    addr: &str,
    ca: &Path,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> (u16, String) {
    use rustls_pki_types::pem::PemObject;
    use std::io::{Read, Write};
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pki_types::CertificateDer::pem_file_iter(ca).unwrap() {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = rustls_pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(config), name).unwrap();
    let mut tcp = std::net::TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let content = body.map_or(String::new(), |b| {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        )
    });
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{auth}{content}\r\n"
    )
    .into_bytes();
    request.extend_from_slice(body.unwrap_or_default());
    tls.write_all(&request).unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .map_or((String::new(), String::new()), |(h, b)| {
            (h.to_ascii_lowercase(), b.to_string())
        });
    let body = if head.contains("transfer-encoding: chunked") {
        dechunk(&body)
    } else {
        body
    };
    (status, body)
}

/// A chunked body joined: `<hex size>\r\n<data>\r\n` until a size of 0.
fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16).unwrap();
        if n == 0 {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}
