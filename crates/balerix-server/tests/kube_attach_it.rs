#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.2: `attach` over the link. The Daemon asks for a session, the
//! sidecar opens a second socket for it, and the plugin-facing `PtyStream`
//! is that socket.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use balerix_api::link::ATTACH_MAX_MESSAGE;
use balerix_api::{
    AgentSettings, CrewSpec, FleetSpec, LINK_PROTOCOL_HEADER, LinkOp, LinkReply, LinkRequest,
    LinkResult, SidecarFrame,
};
use balerix_core::{AgentId, AgentRunner, PassThrough, PtyStream, RunnerError};
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

/// A linked sidecar for `f/c/a` and an attach through it: the plugin's
/// stream, the attach socket (the sidecar's end) and the link.
async fn attached(w: &World) -> (Box<dyn PtyStream>, Ws, Ws) {
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
    let pty_ws = ws(w.port, &format!("/v1/agents/f/c/a/link/attach/{session}"))
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
    (stream, pty_ws, link)
}

/// Waits until `sent` stops moving for 2 s, asserting it stays under
/// `bound`; answers where it stopped.
async fn until_stalled(sent: &AtomicUsize, bound: usize) -> usize {
    let mut last = usize::MAX;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let now = sent.load(Ordering::SeqCst);
        assert!(
            now < bound,
            "the Daemon took {} MiB with the viewer not reading",
            now >> 20
        );
        if now == last {
            return now;
        }
        last = now;
    }
}

const CHUNK: usize = ATTACH_MAX_MESSAGE;
/// Far more than the Daemon may hold: 64 messages of `CHUNK` (4 MiB) plus
/// both ends' socket buffers stay well under `BOUND`.
const OFFERED: usize = 256 << 20;
const BOUND: usize = 32 << 20;

/// Chunk `i` is filled with `i % 251`, so order and loss both show.
fn chunk(i: usize) -> Vec<u8> {
    vec![u8::try_from(i % 251).unwrap(); CHUNK]
}

fn assert_chunks(got: &[u8]) {
    for (i, c) in got.chunks(CHUNK).enumerate() {
        assert_eq!(c, chunk(i).as_slice(), "chunk {i}");
    }
}

/// A viewer that does not read holds the sidecar's terminal bytes back
/// over TCP: the Daemon keeps a bounded number of messages for it and
/// stops reading the sidecar's socket. Once the viewer reads, every byte
/// arrives, in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_viewer_applies_backpressure_to_the_sidecar() {
    let w = world().await;
    let (stream, mut pty_ws, _link) = attached(&w).await;
    let mut reader = stream.reader().unwrap(); // taken, not read yet

    // the sidecar sends until told to stop, then closes; it answers how
    // many bytes it sent
    let sent = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let sidecar = tokio::spawn({
        let (sent, stop) = (sent.clone(), stop.clone());
        async move {
            let mut i = 0;
            while !stop.load(Ordering::SeqCst) && i * CHUNK < OFFERED {
                pty_ws.send(Message::Binary(chunk(i).into())).await.unwrap();
                i += 1;
                sent.store(i * CHUNK, Ordering::SeqCst);
            }
            pty_ws.close(None).await.unwrap();
            i * CHUNK
        }
    });
    until_stalled(&sent, BOUND).await;
    stop.store(true, Ordering::SeqCst);

    // the viewer reads now: everything sent arrives, in order, then EOF
    let read = tokio::task::spawn_blocking(move || {
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        got
    });
    let sent = tokio::time::timeout(Duration::from_secs(30), sidecar)
        .await
        .expect("the sidecar's sends complete once the viewer reads")
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .expect("the viewer read to the end")
        .unwrap();
    assert_eq!(got.len(), sent, "every byte sent was read");
    assert_chunks(&got);
    tokio::task::spawn_blocking(move || drop(stream))
        .await
        .unwrap();
}

/// The attach socket takes messages of at most `ATTACH_MAX_MESSAGE`: a
/// larger one ends the session, and nothing of it reaches the viewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_attach_socket_refuses_a_message_over_its_limit() {
    let w = world().await;
    let (stream, mut pty_ws, _link) = attached(&w).await;
    let mut reader = stream.reader().unwrap();
    let big = vec![b'x'; ATTACH_MAX_MESSAGE * 16];
    let mut sent = 0usize;
    while sent < OFFERED {
        match tokio::time::timeout(
            Duration::from_secs(5),
            pty_ws.send(Message::Binary(big.clone().into())),
        )
        .await
        {
            Ok(Ok(())) => sent += big.len(),
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert!(
        sent < BOUND,
        "the Daemon took {} MiB in messages over its limit",
        sent >> 20
    );
    let got = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            let mut got = Vec::new();
            let _ = reader.read_to_end(&mut got);
            got
        }),
    )
    .await
    .expect("the session ended")
    .unwrap();
    assert!(got.is_empty(), "{} bytes reached the viewer", got.len());
    tokio::task::spawn_blocking(move || drop(stream))
        .await
        .unwrap();
}

/// The viewer pastes 4 MiB while it is not reading, and the sidecar is in
/// a send the Daemon is not reading yet. This sidecar does one thing at a
/// time (the real one sends without stopping reads), so only the Daemon
/// reading while it sends keeps the session moving. Once the viewer
/// reads, both directions finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paste_while_the_viewer_is_not_reading_does_not_hang_the_session() {
    const OUTPUT: usize = 64 << 20;
    const PASTE: usize = 4 << 20;
    let w = world().await;
    let (stream, mut pty_ws, _link) = attached(&w).await;
    let mut reader = stream.reader().unwrap();
    let mut writer = stream.writer().unwrap();

    // a sidecar that does one thing at a time: all its output, then the
    // viewer's input
    let sent = Arc::new(AtomicUsize::new(0));
    let sidecar = tokio::spawn({
        let sent = sent.clone();
        async move {
            for i in 0..OUTPUT / CHUNK {
                pty_ws.send(Message::Binary(chunk(i).into())).await.unwrap();
                sent.store((i + 1) * CHUNK, Ordering::SeqCst);
            }
            let mut pasted = Vec::new();
            while pasted.len() < PASTE {
                match pty_ws.next().await.unwrap().unwrap() {
                    Message::Binary(b) => pasted.extend_from_slice(&b),
                    Message::Ping(_) | Message::Pong(_) => {}
                    other => panic!("{other:?}"),
                }
            }
            pasted
        }
    });
    until_stalled(&sent, BOUND).await;

    // the paste, with nothing read yet
    let paste = tokio::task::spawn_blocking(move || {
        let bytes: Vec<u8> = (0..PASTE).map(|i| (i % 253) as u8).collect();
        writer.write_all(&bytes).unwrap();
        bytes
    });
    tokio::time::sleep(Duration::from_secs(1)).await;

    // the viewer reads: the output arrives, then the sidecar reads the paste
    let read = tokio::task::spawn_blocking(move || {
        let mut got = vec![0u8; OUTPUT];
        reader.read_exact(&mut got).unwrap();
        got
    });
    let got = tokio::time::timeout(Duration::from_secs(60), read)
        .await
        .expect("the viewer read the sidecar's output")
        .unwrap();
    assert_chunks(&got);
    let pasted = tokio::time::timeout(Duration::from_secs(60), sidecar)
        .await
        .expect("the sidecar read the paste")
        .unwrap();
    let bytes = tokio::time::timeout(Duration::from_secs(10), paste)
        .await
        .expect("the paste was taken")
        .unwrap();
    assert_eq!(pasted, bytes);
    tokio::task::spawn_blocking(move || drop(stream))
        .await
        .unwrap();
}
