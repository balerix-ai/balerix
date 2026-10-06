#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.2: the Daemon's end of the sidecar link, with a fake sidecar
//! on a tungstenite client.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{
    AgentPhase, AgentSettings, AgentStatus, CredentialBundle, CrewSpec, FailureKind, FleetSpec,
    LINK_PROTOCOL_HEADER, LinkFailure, LinkOp, LinkReply, LinkRequest, LinkResult, LinkStatus,
    SidecarFrame,
};
use balerix_core::{
    AgentId, AgentRunner, PassThrough, ProcessState, RunnerError, WorkspaceError, WorkspaceReader,
};
use balerix_server::testing::Harness;
use balerix_server::{Daemon, router, serve};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// The operator's apply: a Kubernetes-mode daemon takes no other.
fn tokens() -> BTreeMap<String, String> {
    BTreeMap::from([("f/c/a".to_string(), TOKEN.to_string())])
}

fn spec() -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/api".into(),
                git_ref: "main".into(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
                ..CrewSpec::default()
            },
        )]),
    }
}

struct World {
    daemon: Arc<Daemon>,
    port: u16,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

async fn world(h: &Harness) -> World {
    let dir = tempfile::tempdir().unwrap();
    let daemon = h.daemon(Arc::new(PassThrough), dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = rx.await;
    }));
    World {
        daemon,
        port,
        _dir: dir,
        stop: Some(stop),
    }
}

async fn connect(
    port: u16,
    token: &str,
    protocol: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let mut req = format!("ws://127.0.0.1:{port}/v1/agents/f/c/a/link")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    if !protocol.is_empty() {
        req.headers_mut()
            .insert(LINK_PROTOCOL_HEADER, protocol.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

async fn wait_for(mut f: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !f().await {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn text(frame: &SidecarFrame) -> Message {
    Message::Text(serde_json::to_string(frame).unwrap().into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sidecar_links_sends_status_and_answers_calls() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    w.daemon
        .apply_kube(&name, spec(), tokens(), None)
        .await
        .unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    let token = w.daemon.hook_secret(&id).await.unwrap();
    let hub = w.daemon.kube().unwrap().clone();
    assert!(!hub.linked(&id));

    let mut ws = connect(w.port, &token, "1").await.unwrap();
    wait_for(async || hub.linked(&id)).await;

    // a status frame is mirrored into the record
    ws.send(text(&SidecarFrame::Status(LinkStatus {
        status: AgentStatus {
            phase: AgentPhase::Ready,
            restarts: 2,
            ..AgentStatus::default()
        },
        pid: Some(56),
        hook_failures: 0,
    })))
    .await
    .unwrap();
    wait_for(async || {
        let r = w.daemon.get(&name).await.unwrap();
        r.status
            .agents
            .get("f/c/a")
            .is_some_and(|a| a.phase == AgentPhase::Ready && a.restarts == 2)
    })
    .await;
    assert_eq!(
        hub.observe(&name).unwrap().get(&id),
        Some(&ProcessState::Running { pid: 56 }),
        "observe answers from the last status frame"
    );
    assert_eq!(
        w.daemon.get(&name).await.unwrap().status.phase,
        balerix_api::FleetPhase::Ready,
        "the fleet phase follows the mirrored agents"
    );

    // a call goes out as a request and its reply comes back to the caller
    let (hub2, id2) = (hub.clone(), id.clone());
    let call = tokio::task::spawn_blocking(move || hub2.send_text(&id2, "hello", true));
    let frame = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let req: LinkRequest = serde_json::from_str(frame.as_str()).unwrap();
    assert_eq!(
        req.op,
        LinkOp::SendText {
            text: "hello".into(),
            submit: true
        }
    );
    ws.send(text(&SidecarFrame::Reply(LinkReply {
        id: req.id,
        result: LinkResult::Ok,
    })))
    .await
    .unwrap();
    call.await.unwrap().unwrap();

    // a failure is rebuilt as the port's error
    let (hub2, id2) = (hub.clone(), id.clone());
    let call = tokio::task::spawn_blocking(move || hub2.read_file(&id2, "nope"));
    let frame = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let req: LinkRequest = serde_json::from_str(frame.as_str()).unwrap();
    assert_eq!(
        req.op,
        LinkOp::WorkspaceFile {
            path: "nope".into()
        }
    );
    ws.send(text(&SidecarFrame::Reply(LinkReply {
        id: req.id,
        result: LinkResult::Failed {
            failure: LinkFailure {
                reason: FailureKind::NoSuchPath,
                message: "no such path".into(),
            },
        },
    })))
    .await
    .unwrap();
    assert_eq!(call.await.unwrap(), Err(WorkspaceError::NoSuchPath));

    // the link closes: calls fail naming it, the record says so
    ws.close(None).await.unwrap();
    wait_for(async || !hub.linked(&id)).await;
    let (hub2, id2) = (hub.clone(), id.clone());
    let err = tokio::task::spawn_blocking(move || hub2.send_text(&id2, "x", false))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        err,
        RunnerError::Link {
            id: "f/c/a".into(),
            message: "link down".into()
        }
    );
    assert_eq!(err.to_string(), "f/c/a: link down");
    wait_for(async || {
        w.daemon.get(&name).await.unwrap().status.agents["f/c/a"].message == "link down"
    })
    .await;
    assert_eq!(
        hub.observe(&name).unwrap().get(&id),
        None,
        "absent without a link"
    );

    // a reconnect replaces the old connection
    let _ws = connect(w.port, &token, "1").await.unwrap();
    wait_for(async || hub.linked(&id)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_link_route_refuses_a_bad_token_a_wrong_protocol_and_a_tmux_daemon() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    w.daemon
        .apply_kube(&name, spec(), tokens(), None)
        .await
        .unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    let token = w.daemon.hook_secret(&id).await.unwrap();
    let status = |e: tokio_tungstenite::tungstenite::Error| match e {
        tokio_tungstenite::tungstenite::Error::Http(r) => r.status().as_u16(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        status(connect(w.port, "wrong", "1").await.unwrap_err()),
        401
    );
    assert_eq!(status(connect(w.port, &token, "2").await.unwrap_err()), 400);
    assert_eq!(status(connect(w.port, &token, "").await.unwrap_err()), 400);

    let tmux = Harness::new(Duration::from_secs(3600));
    let w2 = world(&tmux).await;
    w2.daemon
        .apply(&name, spec(), CredentialBundle::default(), false)
        .await
        .unwrap();
    let token = w2.daemon.hook_secret(&id).await.unwrap();
    assert_eq!(
        status(connect(w2.port, &token, "1").await.unwrap_err()),
        404
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_in_flight_fails_link_down_when_its_link_closes_or_is_replaced() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    w.daemon
        .apply_kube(&name, spec(), tokens(), None)
        .await
        .unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    let token = w.daemon.hook_secret(&id).await.unwrap();
    let hub = w.daemon.kube().unwrap().clone();
    let link_down = RunnerError::Link {
        id: "f/c/a".into(),
        message: "link down".into(),
    };

    // the socket closes with a request unanswered
    let mut ws = connect(w.port, &token, "1").await.unwrap();
    wait_for(async || hub.linked(&id)).await;
    let (hub2, id2) = (hub.clone(), id.clone());
    let started = Instant::now();
    let call = tokio::task::spawn_blocking(move || hub2.send_text(&id2, "hello", true));
    ws.next().await.unwrap().unwrap().into_text().unwrap();
    ws.close(None).await.unwrap();
    assert_eq!(call.await.unwrap().unwrap_err(), link_down);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "not the timeout path"
    );

    // a reconnect replaces the connection with a request unanswered
    let mut old = connect(w.port, &token, "1").await.unwrap();
    wait_for(async || hub.linked(&id)).await;
    let (hub2, id2) = (hub.clone(), id.clone());
    let started = Instant::now();
    let call = tokio::task::spawn_blocking(move || hub2.send_text(&id2, "hello", true));
    old.next().await.unwrap().unwrap().into_text().unwrap();
    let _new = connect(w.port, &token, "1").await.unwrap();
    assert_eq!(call.await.unwrap().unwrap_err(), link_down);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "not the timeout path"
    );
}
