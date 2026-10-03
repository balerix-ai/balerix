#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Daemon's admin API as the operator calls it (Spec O §7.4): against
//! a stub for each answer's meaning, and against a real
//! `balerix serve --mode kubernetes` over TLS from the operator's own
//! authority.
mod support;

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use balerix_api::{CrewSpec, ErrorBody, FleetRequest, FleetSpec};
use balerix_operator::daemon_client::{ClientError, DaemonClient};
use balerix_operator::pki;

const TOKEN: &str = "admin-0123456789abcdef0123456789abcdef";
const AGENT_TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn request(name: &str, token: &str) -> FleetRequest {
    let settings =
        serde_json::from_value(serde_json::json!({ "runner": { "type": "pod" } })).unwrap();
    FleetRequest {
        spec: FleetSpec {
            name: name.into(),
            tools: BTreeMap::new(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/api".into(),
                    git_ref: "main".into(),
                    agents: BTreeMap::from([("a".to_string(), settings)]),
                    ..Default::default()
                },
            )]),
        },
        credentials: Default::default(),
        agent_tokens: Some(BTreeMap::from([(format!("{name}/c/a"), token.to_string())])),
    }
}

async fn stub(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn refusal(status: StatusCode, message: &str) -> (StatusCode, Json<ErrorBody>) {
    (
        status,
        Json(ErrorBody {
            error: message.to_string(),
        }),
    )
}

/// The stubs speak plain HTTP; `new` refuses that.
fn client(base: &str) -> DaemonClient {
    DaemonClient::insecure_for_tests(base, TOKEN).unwrap()
}

#[tokio::test]
async fn each_answer_has_its_meaning() {
    let base = stub(
        Router::new()
            .route(
                "/v1/fleets/rejected",
                put(|| async { refusal(StatusCode::BAD_REQUEST, "flow: states.x: unknown state") }),
            )
            .route(
                "/v1/fleets/owned",
                put(|| async {
                    refusal(
                        StatusCode::CONFLICT,
                        "fleet owned is managed by plugin github",
                    )
                }),
            )
            .route(
                "/v1/fleets/broken",
                put(|| async { refusal(StatusCode::INTERNAL_SERVER_ERROR, "disk full") }),
            )
            .route(
                "/v1/fleets/absent",
                get(|| async { refusal(StatusCode::NOT_FOUND, "not found") })
                    .delete(|| async { refusal(StatusCode::NOT_FOUND, "not found") }),
            )
            .route(
                "/readyz",
                get(|| async { refusal(StatusCode::SERVICE_UNAVAILABLE, "daemon pool: pending") }),
            ),
    )
    .await;
    let c = client(&base);
    assert_eq!(
        c.apply(&request("rejected", AGENT_TOKEN))
            .await
            .unwrap_err(),
        ClientError::Rejected("flow: states.x: unknown state".into())
    );
    assert_eq!(
        c.apply(&request("owned", AGENT_TOKEN)).await.unwrap_err(),
        ClientError::Conflict("fleet owned is managed by plugin github".into())
    );
    assert_eq!(
        c.apply(&request("broken", AGENT_TOKEN)).await.unwrap_err(),
        ClientError::Unexpected {
            status: 500,
            message: "disk full".into()
        }
    );
    assert_eq!(c.get("absent").await.unwrap(), None);
    c.down("absent").await.unwrap();
    assert_eq!(
        c.ready().await.unwrap_err(),
        ClientError::Unavailable("daemon pool: pending".into())
    );
}

#[tokio::test]
async fn the_admin_token_is_the_bearer_and_is_never_printed() {
    let base = stub(Router::new().route(
        "/readyz",
        get(|headers: HeaderMap| async move {
            let sent = headers.get("authorization").and_then(|v| v.to_str().ok());
            if sent == Some(&format!("Bearer {TOKEN}")) {
                (StatusCode::OK, "ready").into_response()
            } else {
                refusal(StatusCode::UNAUTHORIZED, "missing or invalid admin token").into_response()
            }
        }),
    ))
    .await;
    let c = client(&base);
    c.ready().await.unwrap();
    let shown = format!("{c:?}");
    assert!(
        shown.contains("<redacted>") && !shown.contains(TOKEN),
        "{shown}"
    );
}

#[tokio::test]
async fn a_daemon_that_does_not_answer_is_unavailable_and_never_names_the_token() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let c = client(&format!("http://{addr}"));
    let ready = c.ready().await.unwrap_err();
    assert!(matches!(ready, ClientError::Unavailable(_)));
    let apply = c.apply(&request("f", AGENT_TOKEN)).await.unwrap_err();
    assert!(matches!(apply, ClientError::Unavailable(_)));
    for text in [
        ready.to_string(),
        format!("{ready:?}"),
        apply.to_string(),
        format!("{apply:?}"),
    ] {
        assert!(
            !text.contains(TOKEN) && !text.contains(AGENT_TOKEN),
            "{text}"
        );
    }
}

#[tokio::test]
async fn a_body_that_cannot_be_read_says_why_there_is_no_message() {
    // a 500 that promises 100 bytes and closes after 3: the body read fails
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
        socket
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 100\r\n\r\nabc")
            .await
            .unwrap();
        // dropped: the connection closes short of its length
    });
    let c = client(&format!("http://{addr}"));
    match c.apply(&request("f", AGENT_TOKEN)).await.unwrap_err() {
        ClientError::Unexpected {
            status: 500,
            message,
        } => {
            assert!(
                message.starts_with("the response body could not be read: "),
                "{message}"
            );
            assert!(!message.contains(TOKEN), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_success_body_that_is_not_a_record_is_unexpected_not_unavailable() {
    // §5.2 keeps DaemonUnavailable for a Daemon that does not answer
    let base = stub(
        Router::new().route(
            "/v1/fleets/garbled",
            put(|| async { (StatusCode::OK, "not a fleet record") })
                .get(|| async { (StatusCode::OK, "not a fleet record") }),
        ),
    )
    .await;
    let c = client(&base);
    for got in [
        c.apply(&request("garbled", AGENT_TOKEN)).await.unwrap_err(),
        c.get("garbled").await.unwrap_err(),
    ] {
        match got {
            ClientError::Unexpected {
                status: 200,
                message,
            } => {
                assert!(
                    message.starts_with("error decoding response body: ")
                        && !message.contains(&base),
                    "{message}"
                );
                assert!(!message.contains(TOKEN), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_plain_http_base_is_refused_and_the_token_is_never_named() {
    let authority = pki::new_authority("team-a", "default", now()).unwrap();
    let refused = DaemonClient::new(
        "http://127.0.0.1:1",
        &authority.cert_pem,
        TOKEN,
        Duration::from_secs(5),
    )
    .unwrap_err();
    match &refused {
        ClientError::Setup(message) => {
            assert!(message.contains("http"), "{message}");
            assert!(!message.contains(TOKEN), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `balerix serve --mode kubernetes` on a free port, serving a
/// certificate this operator's `pki` issued.
fn serve(balerix: &std::path::Path, root: &std::path::Path) -> (Kill, String, pki::Issued) {
    let authority = pki::new_authority("team-a", "default", now()).unwrap();
    let serving = pki::issue_serving(
        &authority,
        "team-a",
        "default",
        &["127.0.0.1".to_string()],
        now(),
    )
    .unwrap();
    std::fs::write(root.join("tls.crt"), &serving.cert_pem).unwrap();
    std::fs::write(root.join("tls.key"), &serving.key_pem).unwrap();
    std::fs::write(root.join("admin-token"), TOKEN).unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let log = std::fs::File::create(root.join("daemon.log")).unwrap();
    let child = Command::new(balerix)
        .args([
            "serve",
            "--mode",
            "kubernetes",
            "--tmux-socket",
            "unused",
            "--bind",
        ])
        .arg(format!("127.0.0.1:{port}"))
        .arg("--tls-cert")
        .arg(root.join("tls.crt"))
        .arg("--tls-key")
        .arg(root.join("tls.key"))
        .arg("--admin-token-file")
        .arg(root.join("admin-token"))
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    (Kill(child), format!("https://127.0.0.1:{port}"), authority)
}

#[tokio::test]
async fn a_real_kubernetes_mode_daemon_takes_the_operators_apply() {
    let Some(balerix) = support::balerix() else {
        return;
    };
    let root = support::temp_root("client-real-daemon");
    let (_daemon, base, authority) = serve(&balerix, &root);
    let c = DaemonClient::new(&base, &authority.cert_pem, TOKEN, Duration::from_secs(10)).unwrap();
    let mut waited = 0;
    while let Err(e) = c.ready().await {
        waited += 1;
        assert!(
            waited < 150,
            "the daemon never became ready: {e:?}\n{}",
            std::fs::read_to_string(root.join("daemon.log"))
                .unwrap_or_else(|read| format!("(daemon.log unreadable: {read})"))
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert_eq!(c.get("payments").await.unwrap(), None);
    let record = c.apply(&request("payments", AGENT_TOKEN)).await.unwrap();
    assert_eq!(record.owner.as_deref(), Some("kubernetes"));
    // an upsert: the operator re-sends on every reconcile
    c.apply(&request("payments", AGENT_TOKEN)).await.unwrap();
    assert_eq!(
        c.get("payments").await.unwrap().unwrap().spec,
        request("payments", AGENT_TOKEN).spec
    );
    // §7.4: a token under 32 characters is a 400, and nothing lands
    let short = c.apply(&request("other", "short")).await.unwrap_err();
    assert_eq!(
        short,
        ClientError::Rejected("agent_tokens.other/c/a: a token is at least 32 characters".into())
    );
    assert_eq!(c.get("other").await.unwrap(), None);
    // §7.4's 409s: a body without tokens is the CLI's, on a fleet of the
    // operator's and on one that does not exist
    let mut as_cli = request("payments", AGENT_TOKEN);
    as_cli.agent_tokens = None;
    assert_eq!(
        c.apply(&as_cli).await.unwrap_err(),
        ClientError::Conflict(
            "fleet payments is managed by kubernetes; change it through its Fleet object".into()
        )
    );
    let mut absent = request("cli-made", AGENT_TOKEN);
    absent.agent_tokens = None;
    assert_eq!(
        c.apply(&absent).await.unwrap_err(),
        ClientError::Conflict("this daemon is in kubernetes mode; create a Fleet object".into())
    );
    c.down("payments").await.unwrap();

    // a client holding another authority does not trust this Daemon
    let stranger = pki::new_authority("team-a", "default", now()).unwrap();
    let untrusting =
        DaemonClient::new(&base, &stranger.cert_pem, TOKEN, Duration::from_secs(5)).unwrap();
    assert!(matches!(
        untrusting.ready().await.unwrap_err(),
        ClientError::Unavailable(_)
    ));
    // and a wrong token is not "unavailable"
    let wrong = DaemonClient::new(
        &base,
        &authority.cert_pem,
        "not-the-token",
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(
        wrong.get("payments").await.unwrap_err(),
        ClientError::Unexpected {
            status: 401,
            message: "missing or invalid admin token".into()
        }
    );
}
