#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Fake tools on PATH: `serve` discovers them but calls none without a fleet.
fn fake_tools(dir: &Path) {
    for t in ["git", "gh", "mise", "nono", "tmux"] {
        fs::write(dir.join(t), "#!/bin/sh\nexit 0\n").unwrap();
    }
}

fn balerix(home: &Path, tools: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_balerix"));
    cmd.env("HOME", home)
        .env("PATH", tools)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("BALERIX_API_URL");
    cmd
}

fn wait_for_file(path: &Path) -> String {
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

fn get(url: &str, token: Option<&str>) -> (u16, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.get(url);
    if let Some(t) = token {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    let mut resp = req.call().unwrap();
    (
        resp.status().as_u16(),
        resp.body_mut().read_to_string().unwrap(),
    )
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn foreground_serve_writes_endpoint_and_answers_with_the_token() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let server_dir = home.path().join(".local/state/balerix/server");
    let child = balerix(home.path(), tools.path())
        .args([
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--tmux-socket",
            &format!("balerix-test-{}", std::process::id()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _kill = Kill(child);
    let url = wait_for_file(&server_dir.join("endpoint"));
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    let token = fs::read_to_string(server_dir.join("token"))
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(token.len(), 64);
    assert!(server_dir.join("vault.key").exists());
    assert!(server_dir.join("balerix.pid").exists());
    assert_eq!(
        get(&format!("{url}/healthz"), None),
        (200, "ok".to_string())
    );
    assert_eq!(get(&format!("{url}/v1/fleets"), None).0, 401);
    assert_eq!(
        get(&format!("{url}/v1/fleets"), Some(&token)),
        (200, "[]".to_string())
    );
    assert_eq!(get(&format!("{url}/metrics"), None).0, 200);
}

#[test]
fn detached_serve_prints_the_endpoint_logs_to_a_file_and_stops_on_term() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let server_dir = home.path().join(".local/state/balerix/server");
    let out = balerix(home.path(), tools.path())
        .args([
            "serve",
            "-d",
            "--bind",
            "127.0.0.1:0",
            "--tmux-socket",
            &format!("balerix-test-{}-d", std::process::id()),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("http://127.0.0.1:"), "{stdout}");
    let url = fs::read_to_string(server_dir.join("endpoint"))
        .unwrap()
        .trim()
        .to_string();
    let pid: u32 = fs::read_to_string(server_dir.join("balerix.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(get(&format!("{url}/healthz"), None).0, 200);
    let log = fs::read_to_string(server_dir.join("server.log")).unwrap();
    assert!(log.contains("listening"), "{log}");

    // a second daemon refuses to start while the first answers
    let again = balerix(home.path(), tools.path())
        .args(["serve", "--bind", "127.0.0.1:0"])
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already running"));

    assert!(
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    // The daemon unlinks the endpoint file and then the pid file as two
    // steps, so poll for both: a loaded runner lands between them.
    let endpoint = server_dir.join("endpoint");
    let pid_file = server_dir.join("balerix.pid");
    let start = Instant::now();
    while endpoint.exists() || pid_file.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "not removed on SIGTERM: endpoint={} pid={}",
            endpoint.exists(),
            pid_file.exists()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn serve_rejects_a_non_loopback_bind_and_writes_no_endpoint() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let server_dir = home.path().join(".local/state/balerix/server");
    let out = balerix(home.path(), tools.path())
        .args(["serve", "--bind", "0.0.0.0:0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not loopback"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!server_dir.join("endpoint").exists());
}

/// A throwaway authority and a leaf for 127.0.0.1, as PEM files in `dir`:
/// (ca.crt, tls.crt, tls.key).
fn tls_files(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
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
fn tls_get(addr: &str, ca: &Path, path: &str, token: Option<&str>) -> (u16, String) {
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
    tls.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{auth}\r\n")
            .as_bytes(),
    )
    .unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, b)| b.to_string());
    (status, body)
}

#[test]
fn kubernetes_mode_serves_tls_with_the_mounted_token_and_reads_no_plugins_file() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let (ca, cert, key) = tls_files(home.path());
    let token_file = home.path().join("admin-token");
    fs::write(&token_file, "0123456789abcdef0123456789abcdef\n").unwrap();
    // a plugins.yaml a tmux daemon would read; kubernetes mode must not
    let config = home.path().join(".config/balerix");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("plugins.yaml"),
        "plugins:\n  - name: nope\n    source: ./nope\n",
    )
    .unwrap();
    let server_dir = home.path().join(".local/state/balerix/server");
    let child = balerix(home.path(), tools.path())
        .args([
            "serve",
            "--mode",
            "kubernetes",
            "--bind",
            "127.0.0.1:0",
            "--tmux-socket",
            "unused",
        ])
        .arg("--tls-cert")
        .arg(&cert)
        .arg("--tls-key")
        .arg(&key)
        .arg("--admin-token-file")
        .arg(&token_file)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _kill = Kill(child);
    let url = wait_for_file(&server_dir.join("endpoint"));
    assert!(url.starts_with("https://127.0.0.1:"), "{url}");
    let addr = url.trim_start_matches("https://");
    assert_eq!(tls_get(addr, &ca, "/healthz", None), (200, "ok".into()));
    // the pool actor turns Ready asynchronously: poll /readyz, bounded
    let start = Instant::now();
    let ready = loop {
        let r = tls_get(addr, &ca, "/readyz", None);
        if r.0 == 200 || start.elapsed() > Duration::from_secs(10) {
            break r;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(ready, (200, "ready".into()));
    assert_eq!(tls_get(addr, &ca, "/v1/fleets", None).0, 401);
    assert_eq!(
        tls_get(
            addr,
            &ca,
            "/v1/fleets",
            Some("0123456789abcdef0123456789abcdef")
        ),
        (200, "[]".into())
    );
    assert!(
        !server_dir.join("token").exists(),
        "the admin token comes from the mounted file, none is generated"
    );
    // nothing under plugins/: the file was not read, no plugin launched
    assert!(!home.path().join(".local/share/balerix/plugins").exists());
}

#[test]
fn kubernetes_mode_refuses_missing_tls_and_detach_and_tmux_mode_refuses_the_tls_flags() {
    let home = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    fake_tools(tools.path());
    let out = balerix(home.path(), tools.path())
        .args(["serve", "--mode", "kubernetes", "--bind", "127.0.0.1:0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(
        "serve --mode kubernetes needs --tls-cert, --tls-key and --admin-token-file (Spec O §7.3)"
    ));
    let (_, cert, key) = tls_files(home.path());
    let token_file = home.path().join("admin-token");
    fs::write(&token_file, "0123456789abcdef0123456789abcdef\n").unwrap();
    let out = balerix(home.path(), tools.path())
        .args([
            "serve",
            "-d",
            "--mode",
            "kubernetes",
            "--bind",
            "127.0.0.1:0",
        ])
        .arg("--tls-cert")
        .arg(&cert)
        .arg("--tls-key")
        .arg(&key)
        .arg("--admin-token-file")
        .arg(&token_file)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains(
        "--detach is not available with --mode kubernetes; a pod runs the daemon in the foreground"
    ));
    let out = balerix(home.path(), tools.path())
        .args(["serve", "--bind", "127.0.0.1:0"])
        .arg("--tls-cert")
        .arg(&cert)
        .arg("--tls-key")
        .arg(&key)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains(
        "--tls-cert, --tls-key, --tls-ca and --admin-token-file are for --mode kubernetes"
    ));
    // --tls-ca alone is refused in tmux mode like the other kube flags
    let out = balerix(home.path(), tools.path())
        .args(["serve", "--bind", "127.0.0.1:0"])
        .arg("--tls-ca")
        .arg(&cert)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains(
        "--tls-cert, --tls-key, --tls-ca and --admin-token-file are for --mode kubernetes"
    ));
    fs::write(&token_file, "short\n").unwrap();
    let out = balerix(home.path(), tools.path())
        .args(["serve", "--mode", "kubernetes", "--bind", "127.0.0.1:0"])
        .arg("--tls-cert")
        .arg(&cert)
        .arg("--tls-key")
        .arg(&key)
        .arg("--admin-token-file")
        .arg(&token_file)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("the admin token is at least 32 characters")
    );
}
