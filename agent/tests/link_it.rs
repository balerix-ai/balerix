#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.2 from the sidecar's side, against a fake Daemon on plain
//! `ws://`: connect, status, requests and replies, attach, reconnect.

mod support;

use std::future::IntoFuture;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::routing::get;

use balerix_agent::link::{Control, LINK_IDLE, LinkDeps, run, run_with_idle};
use balerix_agent::tls::client_config;
use balerix_api::{
    AgentPhase, AgentStatus, FailureKind, LinkOp, LinkRequest, LinkResult, LinkStatus,
    SidecarFrame, WorkspaceDiff, WorkspaceTree, WorkspaceVersion,
};
use balerix_core::fakes::FakeRunner;
use balerix_core::{AgentId, WorkspaceError, WorkspaceReader};
use support::fake_daemon::{Conn, FakeDaemon};
use tokio::sync::{mpsc, watch};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// A reader of two paths: `f` is five bytes, anything else is absent;
/// the other reads are never made here.
struct TwoFiles;

impl WorkspaceReader for TwoFiles {
    fn diff(&self, _: &AgentId, _: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        Err(WorkspaceError::NoSuchPath)
    }
    fn read_file(&self, _: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError> {
        if path == "f" {
            Ok(b"hello".to_vec())
        } else {
            Err(WorkspaceError::NoSuchPath)
        }
    }
    fn list_dir(&self, _: &AgentId, _: &str) -> Result<WorkspaceTree, WorkspaceError> {
        Err(WorkspaceError::NoSuchPath)
    }
    fn version(&self, _: &AgentId, _: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        Err(WorkspaceError::NoSuchPath)
    }
}

struct Side {
    runner: Arc<FakeRunner>,
    control: mpsc::UnboundedReceiver<Control>,
    status: watch::Sender<LinkStatus>,
}

fn start_sidecar_link(daemon_url: &str) -> Side {
    let (deps, side) = sidecar(daemon_url);
    tokio::spawn(run(deps));
    side
}

fn sidecar(daemon_url: &str) -> (Arc<LinkDeps>, Side) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    std::fs::write(
        dir.path().join("ca.crt"),
        ca.self_signed(&key).unwrap().pem(),
    )
    .unwrap();
    let tls = client_config(&dir.path().join("ca.crt")).unwrap();
    let runner = Arc::new(FakeRunner::default());
    let (control_tx, control) = mpsc::unbounded_channel();
    let (status, status_rx) = watch::channel(LinkStatus {
        status: AgentStatus::default(),
        pid: None,
        hook_failures: 0,
    });
    let deps = Arc::new(LinkDeps {
        id: "f/c/a".parse().unwrap(),
        token: TOKEN.into(),
        daemon_url: daemon_url.into(),
        tls,
        runner: runner.clone(),
        workspace: Arc::new(TwoFiles),
        control: control_tx,
        status: status_rx,
    });
    (
        deps,
        Side {
            runner,
            control,
            status,
        },
    )
}

async fn reply(conn: &mut Conn) -> (u64, LinkResult) {
    loop {
        match conn.from_sidecar.recv().await.unwrap() {
            SidecarFrame::Reply(r) => return (r.id, r.result),
            SidecarFrame::Status(_) => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_link_connects_answers_requests_and_reconnects() {
    let mut daemon = FakeDaemon::start(None).await;
    let url = format!("http://{}", daemon.addr);
    let mut side = start_sidecar_link(&url);

    let mut conn = tokio::time::timeout(Duration::from_secs(5), daemon.links.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(conn.headers["balerix-link-protocol"], "1");
    // the current status comes first
    match conn.from_sidecar.recv().await.unwrap() {
        SidecarFrame::Status(s) => assert_eq!(s.status.phase, AgentPhase::Pending),
        other => panic!("{other:?}"),
    }

    conn.to_sidecar
        .send(LinkRequest {
            id: 1,
            op: LinkOp::SendText {
                text: "hi".into(),
                submit: true,
            },
        })
        .unwrap();
    assert_eq!(reply(&mut conn).await, (1, LinkResult::Ok));
    assert!(
        side.runner
            .calls()
            .iter()
            .any(|c| c == "send_text f/c/a \"hi\" submit=true")
    );

    conn.to_sidecar
        .send(LinkRequest {
            id: 2,
            op: LinkOp::Stop,
        })
        .unwrap();
    assert_eq!(reply(&mut conn).await, (2, LinkResult::Ok));
    assert_eq!(side.control.recv().await.unwrap(), Control::Stop);
    conn.to_sidecar
        .send(LinkRequest {
            id: 3,
            op: LinkOp::Restart,
        })
        .unwrap();
    assert_eq!(reply(&mut conn).await, (3, LinkResult::Ok));
    assert_eq!(side.control.recv().await.unwrap(), Control::Restart);

    conn.to_sidecar
        .send(LinkRequest {
            id: 4,
            op: LinkOp::WorkspaceFile {
                path: "missing".into(),
            },
        })
        .unwrap();
    let (id, result) = reply(&mut conn).await;
    assert_eq!(id, 4);
    assert!(
        matches!(result, LinkResult::Failed { failure } if failure.reason == FailureKind::NoSuchPath)
    );
    conn.to_sidecar
        .send(LinkRequest {
            id: 5,
            op: LinkOp::WorkspaceFile { path: "f".into() },
        })
        .unwrap();
    assert_eq!(
        reply(&mut conn).await,
        (
            5,
            LinkResult::File {
                bytes: b"hello".to_vec()
            }
        )
    );

    // attach: the second socket comes up with the token and the protocol
    // before the reply
    conn.to_sidecar
        .send(LinkRequest {
            id: 6,
            op: LinkOp::Attach {
                session: "s1".into(),
            },
        })
        .unwrap();
    let mut att = tokio::time::timeout(Duration::from_secs(5), daemon.attaches.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(att.session, "s1");
    assert_eq!(att.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(att.headers["balerix-link-protocol"], "1");
    assert_eq!(reply(&mut conn).await, (6, LinkResult::Ok));
    // FakeRunner's attach is an echo: bytes written come back on the reader
    att.to_sidecar
        .send(AxMsg::Binary(b"abc".to_vec().into()))
        .unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(5), att.from_sidecar.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(echoed, AxMsg::Binary(b"abc".to_vec().into()));
    att.to_sidecar
        .send(AxMsg::Text(r#"{"resize":{"cols":100,"rows":30}}"#.into()))
        .unwrap();
    let start = Instant::now();
    while !side
        .runner
        .resizes()
        .iter()
        .any(|(a, c, r)| a == "f/c/a" && *c == 100 && *r == 30)
    {
        assert!(start.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // the window dies: the sidecar closes the attach socket
    assert!(side.runner.close_attach(&"f/c/a".parse().unwrap()));
    let start = Instant::now();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), att.from_sidecar.recv())
            .await
            .unwrap()
        {
            Some(AxMsg::Close(_)) | None => break,
            Some(_) => {}
        }
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    // a status change is a frame
    side.status
        .send(LinkStatus {
            status: AgentStatus {
                phase: AgentPhase::Ready,
                ..AgentStatus::default()
            },
            pid: Some(7),
            hook_failures: 1,
        })
        .unwrap();
    loop {
        match conn.from_sidecar.recv().await.unwrap() {
            SidecarFrame::Status(s) if s.status.phase == AgentPhase::Ready => {
                assert_eq!((s.pid, s.hook_failures), (Some(7), 1));
                break;
            }
            _ => {}
        }
    }

    // the Daemon drops the link: the sidecar is back within the back-off,
    // and its first frame is the status the Daemon reconciles on
    drop(conn);
    let mut again = tokio::time::timeout(Duration::from_secs(5), daemon.links.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.headers["authorization"], format!("Bearer {TOKEN}"));
    match again.from_sidecar.recv().await.unwrap() {
        SidecarFrame::Status(s) => assert_eq!(s.status.phase, AgentPhase::Ready),
        other => panic!("{other:?}"),
    }
}

/// A Daemon whose node died: the socket stays open and nothing arrives,
/// not even its pings. The sidecar gives up after the idle limit and
/// reconnects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_gone_silent_is_left_after_the_idle_limit() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sockets, mut sockets_rx) = mpsc::unbounded_channel::<WebSocket>();
    let silent = Router::new().route(
        "/v1/agents/{f}/{c}/{a}/link",
        get(move |ws: WebSocketUpgrade| async move {
            // held, never read nor written
            ws.on_upgrade(move |socket| async move {
                let _ = sockets.send(socket);
            })
        }),
    );
    tokio::spawn(axum::serve(listener, silent).into_future());
    let (deps, _side) = sidecar(&url);
    tokio::spawn(run_with_idle(deps, Duration::from_secs(1)));

    let _first = tokio::time::timeout(Duration::from_secs(5), sockets_rx.recv())
        .await
        .unwrap()
        .unwrap();
    // idle limit (1 s) plus the first back-off (1 s), with room
    let _second = tokio::time::timeout(Duration::from_secs(6), sockets_rx.recv())
        .await
        .expect("the sidecar stayed on a silent link")
        .unwrap();
}

#[tokio::test]
async fn link_deps_never_print_the_token() {
    let (deps, _side) = sidecar("http://127.0.0.1:1");
    let shown = format!("{deps:?}");
    assert!(shown.contains("<redacted>"), "{shown}");
    assert!(!shown.contains(TOKEN), "{shown}");
}

#[test]
fn the_idle_limit_is_three_missed_daemon_pings() {
    assert_eq!(LINK_IDLE, Duration::from_secs(3 * 30));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_that_is_not_up_yet_is_reached_when_it_comes_up() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let _side = start_sidecar_link(&format!("http://{addr}"));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut daemon = FakeDaemon::start(Some(addr)).await;
    let conn = tokio::time::timeout(Duration::from_secs(10), daemon.links.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.headers["balerix-link-protocol"], "1");
}
