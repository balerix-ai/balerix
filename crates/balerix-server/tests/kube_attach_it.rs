#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.2: `attach` over the link. The Daemon asks for a session, the
//! sidecar opens a second socket for it, and the plugin-facing `PtyStream`
//! is that socket.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{
    AgentSettings, CrewSpec, FleetSpec, LINK_PROTOCOL_HEADER, LinkOp, LinkReply, LinkRequest,
    LinkResult, SidecarFrame,
};
use balerix_core::{AgentId, AgentRunner, PassThrough, RunnerError};
use balerix_server::testing::Harness;
use balerix_server::{Daemon, router, serve};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
/// Agent `f/c/b`'s token: a second agent of the same crew.
const TOKEN_B: &str = "fedcba9876543210fedcba9876543210";
type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn spec() -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/api".into(),
                git_ref: "main".into(),
                agents: BTreeMap::from([
                    ("a".to_string(), AgentSettings::default()),
                    ("b".to_string(), AgentSettings::default()),
                ]),
                ..CrewSpec::default()
            },
        )]),
    }
}

async fn ws(port: u16, path: &str) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    ws_with(port, path, TOKEN, "1").await
}

/// An empty `protocol` sends no protocol header.
async fn ws_with(
    port: u16,
    path: &str,
    token: &str,
    protocol: &str,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://127.0.0.1:{port}{path}")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    if !protocol.is_empty() {
        req.headers_mut()
            .insert(LINK_PROTOCOL_HEADER, protocol.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(w, _)| w)
}

async fn next_request(link: &mut Ws) -> LinkRequest {
    loop {
        match link.next().await.unwrap().unwrap() {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) => {}
            other => panic!("{other:?}"),
        }
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

async fn serve_daemon(h: &Harness) -> World {
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

async fn world() -> World {
    let w = serve_daemon(&Harness::kube(Duration::from_secs(3600))).await;
    w.daemon
        .apply_kube(
            &"f".parse().unwrap(),
            spec(),
            BTreeMap::from([
                ("f/c/a".to_string(), TOKEN.to_string()),
                ("f/c/b".to_string(), TOKEN_B.to_string()),
            ]),
            None,
        )
        .await
        .unwrap();
    w
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_rides_a_second_socket_the_sidecar_opens() {
    let w = world().await;
    let id: AgentId = "f/c/a".parse().unwrap();
    let hub = w.daemon.kube().unwrap().clone();
    let mut link = ws(w.port, "/v1/agents/f/c/a/link").await.unwrap();
    let start = Instant::now();
    while !hub.linked(&id) {
        assert!(start.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (hub2, id2) = (hub.clone(), id.clone());
    let attach = tokio::task::spawn_blocking(move || hub2.attach(&id2));
    let req = next_request(&mut link).await;
    let LinkOp::Attach { session } = &req.op else {
        panic!("{req:?}");
    };
    // the sidecar: open the second socket, then answer
    let mut pty_ws = ws(w.port, &format!("/v1/agents/f/c/a/link/attach/{session}"))
        .await
        .unwrap();
    link.send(Message::Text(
        serde_json::to_string(&SidecarFrame::Reply(LinkReply {
            id: req.id,
            result: LinkResult::Ok,
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let stream = attach.await.unwrap().unwrap();

    // bytes the plugin writes reach the sidecar's socket as binary
    let mut writer = stream.writer().unwrap();
    tokio::task::spawn_blocking(move || {
        writer.write_all(b"ls\r").unwrap();
        writer.flush().unwrap();
    })
    .await
    .unwrap();
    assert_eq!(
        pty_ws.next().await.unwrap().unwrap(),
        Message::Binary(b"ls\r".to_vec().into())
    );

    // bytes from the sidecar reach the plugin's reader
    pty_ws
        .send(Message::Binary(b"total 0\r\n".to_vec().into()))
        .await
        .unwrap();
    let mut reader = stream.reader().unwrap();
    let got = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).unwrap();
        buf[..n].to_vec()
    })
    .await
    .unwrap();
    assert_eq!(got, b"total 0\r\n");

    // a resize is the one text frame
    stream.resize(100, 30).unwrap();
    assert_eq!(
        pty_ws.next().await.unwrap().unwrap(),
        Message::Text(r#"{"resize":{"cols":100,"rows":30}}"#.into())
    );
    assert!(stream.writer().is_err(), "the writer is taken once");

    // dropping the stream closes the second socket
    tokio::task::spawn_blocking(move || drop(stream))
        .await
        .unwrap();
    let start = Instant::now();
    loop {
        let left = Duration::from_secs(5).saturating_sub(start.elapsed());
        match tokio::time::timeout(left, pty_ws.next())
            .await
            .expect("the second socket closes within 5 s")
        {
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(_)) => {}
            Some(Err(_)) => break,
        }
    }

    // a sidecar that never opens the socket: the attach fails within the bound
    let (hub2, id2) = (hub.clone(), id.clone());
    let attach = tokio::task::spawn_blocking(move || hub2.attach(&id2));
    let req = next_request(&mut link).await;
    let LinkOp::Attach { session } = &req.op else {
        panic!("{req:?}");
    };
    // another agent's sidecar, with its own valid token, cannot claim A's
    // session: it is closed as an unknown one, and A's waiter stays put
    let mut thief = ws_with(
        w.port,
        &format!("/v1/agents/f/c/b/link/attach/{session}"),
        TOKEN_B,
        "1",
    )
    .await
    .unwrap();
    assert_unknown_session(&mut thief).await;
    link.send(Message::Text(
        serde_json::to_string(&SidecarFrame::Reply(LinkReply {
            id: req.id,
            result: LinkResult::Ok,
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    let Err(err) = attach.await.unwrap() else {
        panic!("the attach succeeded without a socket");
    };
    assert_eq!(
        err,
        RunnerError::Link {
            id: "f/c/a".into(),
            message: "the sidecar did not open the attach socket".into()
        }
    );

    // an attach socket nobody asked for is closed 1008
    let mut stray = ws(w.port, "/v1/agents/f/c/a/link/attach/nope")
        .await
        .unwrap();
    assert_unknown_session(&mut stray).await;
}

async fn assert_unknown_session(socket: &mut Ws) {
    match socket.next().await.unwrap().unwrap() {
        Message::Close(Some(frame)) => {
            assert_eq!(u16::from(frame.code), 1008);
            assert_eq!(frame.reason.as_str(), "unknown attach session");
        }
        other => panic!("{other:?}"),
    }
}

/// The attach route refuses exactly as the link route does: 401 for a bad
/// token or an unknown agent, 400 for a missing or wrong protocol header,
/// 404 on a daemon not in kubernetes mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_attach_route_refuses_like_the_link_route() {
    let refusal = |e: tokio_tungstenite::tungstenite::Error| match e {
        tokio_tungstenite::tungstenite::Error::Http(r) => (
            r.status().as_u16(),
            serde_json::from_slice::<serde_json::Value>(r.body().as_deref().unwrap()).unwrap(),
        ),
        other => panic!("{other:?}"),
    };
    let unauthorized = (
        401,
        serde_json::json!({"error": "unknown agent or bad secret"}),
    );

    let w = world().await;
    assert_eq!(
        refusal(
            ws_with(w.port, "/v1/agents/f/c/a/link/attach/s", "wrong", "1")
                .await
                .unwrap_err()
        ),
        unauthorized
    );
    assert_eq!(
        refusal(
            ws(w.port, "/v1/agents/f/c/zz/link/attach/s")
                .await
                .unwrap_err()
        ),
        unauthorized
    );

    let protocol = |got: &str| {
        (
            400,
            serde_json::json!({
                "error": format!("{LINK_PROTOCOL_HEADER}: this daemon speaks link protocol 1, got {got}")
            }),
        )
    };
    assert_eq!(
        refusal(
            ws_with(w.port, "/v1/agents/f/c/a/link/attach/s", TOKEN, "")
                .await
                .unwrap_err()
        ),
        protocol("nothing")
    );
    assert_eq!(
        refusal(
            ws_with(w.port, "/v1/agents/f/c/a/link/attach/s", TOKEN, "2")
                .await
                .unwrap_err()
        ),
        protocol("2")
    );

    let tmux = serve_daemon(&Harness::new(Duration::from_secs(3600))).await;
    assert_eq!(
        refusal(
            ws(tmux.port, "/v1/agents/f/c/a/link/attach/s")
                .await
                .unwrap_err()
        ),
        (
            404,
            serde_json::json!({"error": "not a daemon in kubernetes mode"})
        )
    );
}
