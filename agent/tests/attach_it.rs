#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The sidecar's attach bridge (Spec O §7.2) when the Daemon stops reading
//! while the viewer pastes. The terminal is an echo with a PTY's shape:
//! its program reads input only while its output has room, so once the
//! Daemon stops reading, the paste backs up into the sidecar. The sidecar
//! must hold a bounded part of it, and then recover when the Daemon reads
//! again, or end the session cleanly when it does not.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use balerix_agent::attach::{bridge, bridge_with};
use balerix_agent::link::Ws;
use balerix_api::ATTACH_MAX_MESSAGE;
use balerix_core::PtyStream;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// What one direction of the fake terminal holds, as a PTY's buffers do.
const PIPE: usize = 16 << 10;
/// The kernel's buffers for both ends of the attach socket, kept small
/// so the paste cannot disappear into them.
const SOCKET_BUFFER: u32 = 16 << 10;
const CHUNK: usize = ATTACH_MAX_MESSAGE;
const PASTE: usize = 4 << 20;
/// What the sidecar may take of the paste with nothing read back: its
/// read-ahead (1 MiB), the PTY output it queues (64 reads of at most
/// 8 KiB), the pieces in flight and the small buffers, with room.
const BOUND: usize = 3 << 20;

/// One direction of the terminal: a bounded byte queue whose writes wait
/// for room and whose reads wait for bytes, until it is closed.
#[derive(Default)]
struct Pipe {
    state: Mutex<(VecDeque<u8>, bool)>,
    changed: Condvar,
}

impl Pipe {
    fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        while state.0.len() >= PIPE && !state.1 {
            state = self.changed.wait(state).unwrap();
        }
        if state.1 {
            // a PTY master's write once its client is gone
            return Err(io::Error::from_raw_os_error(5));
        }
        let n = bytes.len().min(PIPE - state.0.len());
        state.0.extend(&bytes[..n]);
        self.changed.notify_all();
        Ok(n)
    }

    fn read(&self, out: &mut [u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        while state.0.is_empty() && !state.1 {
            state = self.changed.wait(state).unwrap();
        }
        if state.1 {
            return Ok(0);
        }
        let n = out.len().min(state.0.len());
        for (o, b) in out.iter_mut().zip(state.0.drain(..n)) {
            *o = b;
        }
        self.changed.notify_all();
        Ok(n)
    }

    fn close(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

struct PipeEnd(Arc<Pipe>);

impl Read for PipeEnd {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.0.read(out)
    }
}

impl Write for PipeEnd {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A terminal whose program echoes its input, writing each piece out
/// before it reads the next. Its drop closes both directions, as ending a
/// tmux attach does; `dropped` says it happened.
struct Echo {
    input: Arc<Pipe>,
    output: Arc<Pipe>,
    dropped: Arc<AtomicBool>,
}

impl Echo {
    fn start() -> (Echo, Arc<AtomicBool>) {
        Echo::with_program(true)
    }

    /// A terminal whose program never reads its input (busy, or
    /// stopped): a paste fills the input pipe and stays there.
    fn not_reading() -> (Echo, Arc<AtomicBool>) {
        Echo::with_program(false)
    }

    fn with_program(running: bool) -> (Echo, Arc<AtomicBool>) {
        let (input, output) = (Arc::new(Pipe::default()), Arc::new(Pipe::default()));
        std::thread::spawn({
            let (input, output) = (input.clone(), output.clone());
            move || {
                if !running {
                    return;
                }
                let mut buf = vec![0u8; 4096];
                while let Ok(n @ 1..) = input.read(&mut buf) {
                    let mut out = &buf[..n];
                    while !out.is_empty() {
                        match output.write(out) {
                            Ok(w) => out = &out[w..],
                            Err(_) => return,
                        }
                    }
                }
            }
        });
        let dropped = Arc::new(AtomicBool::new(false));
        let echo = Echo {
            input,
            output,
            dropped: dropped.clone(),
        };
        (echo, dropped)
    }
}

impl PtyStream for Echo {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(PipeEnd(self.output.clone())))
    }
    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(PipeEnd(self.input.clone())))
    }
    fn resize(&self, _: u16, _: u16) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.input.close();
        self.output.close();
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// The Daemon's end and the sidecar's end of one attach socket, both with
/// `ATTACH_MAX_MESSAGE` and small kernel buffers.
async fn socket_pair() -> (WebSocket, Ws) {
    let listening = tokio::net::TcpSocket::new_v4().unwrap();
    listening.set_recv_buffer_size(SOCKET_BUFFER).unwrap();
    listening.set_send_buffer_size(SOCKET_BUFFER).unwrap();
    listening.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = listening.listen(4).unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<WebSocket>();
    let app = Router::new().route(
        "/attach",
        get(move |ws: WebSocketUpgrade| async move {
            // the Daemon's limits (`kube/link.rs`)
            ws.max_message_size(ATTACH_MAX_MESSAGE)
                .max_frame_size(ATTACH_MAX_MESSAGE)
                .on_upgrade(move |socket| async move {
                    let _ = tx.send(socket);
                })
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await });

    let connecting = tokio::net::TcpSocket::new_v4().unwrap();
    connecting.set_recv_buffer_size(SOCKET_BUFFER).unwrap();
    connecting.set_send_buffer_size(SOCKET_BUFFER).unwrap();
    let tcp = connecting.connect(addr).await.unwrap();
    // the sidecar's limits (`link::connect`)
    let config = WebSocketConfig::default()
        .max_message_size(Some(ATTACH_MAX_MESSAGE))
        .max_frame_size(Some(ATTACH_MAX_MESSAGE));
    let (sidecar, _) = tokio_tungstenite::client_async_with_config(
        format!("ws://{addr}/attach"),
        MaybeTlsStream::Plain(tcp),
        Some(config),
    )
    .await
    .unwrap();
    let daemon = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    (daemon, sidecar)
}

/// Chunk `i` is filled with `i % 251`, so order and loss both show.
fn chunk(i: usize) -> Vec<u8> {
    vec![u8::try_from(i % 251).unwrap(); CHUNK]
}

/// The viewer's paste, from the Daemon, counting what the sidecar took;
/// answers the Daemon's sink once it is all sent, or the send's error.
fn paste(
    mut sink: SplitSink<WebSocket, AxMsg>,
    sent: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<Result<SplitSink<WebSocket, AxMsg>, axum::Error>> {
    tokio::spawn(async move {
        for i in 0..PASTE / CHUNK {
            sink.send(AxMsg::Binary(chunk(i).into())).await?;
            sent.store((i + 1) * CHUNK, Ordering::SeqCst);
        }
        Ok(sink)
    })
}

/// Waits until `sent` stops moving for 2 s, asserting it stays under
/// `BOUND`; answers where it stopped.
async fn until_stalled(sent: &AtomicUsize) -> usize {
    let mut last = usize::MAX;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let now = sent.load(Ordering::SeqCst);
        assert!(
            now < BOUND,
            "the sidecar took {} KiB of the paste with nothing read back",
            now >> 10
        );
        if now == last {
            return now;
        }
        last = now;
    }
}

/// The Daemon stops reading while the viewer pastes 4 MiB: the echo backs
/// up, the sidecar takes a bounded part of the paste and stops reading.
/// Once the Daemon reads again, the whole paste comes back, in order, and
/// the Daemon's close then ends the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paste_the_daemon_does_not_read_back_is_bounded_and_recovers() {
    let (daemon, sidecar) = socket_pair().await;
    let (echo, dropped) = Echo::start();
    let session = tokio::spawn(bridge(sidecar, Box::new(echo)));
    let (sink, mut stream) = daemon.split();
    let sent = Arc::new(AtomicUsize::new(0));
    let pasting = paste(sink, sent.clone());

    let stalled = until_stalled(&sent).await;
    assert!(
        stalled < PASTE,
        "the paste backed up: {} KiB",
        stalled >> 10
    );
    assert!(!session.is_finished(), "the session is still up");

    // the Daemon reads again: the paste comes back whole and in order
    let mut got = Vec::with_capacity(PASTE);
    tokio::time::timeout(Duration::from_secs(30), async {
        while got.len() < PASTE {
            match stream.next().await.unwrap().unwrap() {
                AxMsg::Binary(b) => got.extend_from_slice(&b),
                AxMsg::Ping(_) | AxMsg::Pong(_) => {}
                other => panic!("{other:?}"),
            }
        }
    })
    .await
    .expect("the echo of the whole paste");
    for (i, c) in got.chunks(CHUNK).enumerate() {
        assert!(c == chunk(i).as_slice(), "chunk {i} came back changed");
    }
    let mut sink = tokio::time::timeout(Duration::from_secs(10), pasting)
        .await
        .expect("the paste was taken whole")
        .unwrap()
        .unwrap();

    // the viewer is done: the Daemon closes and the session ends
    sink.send(AxMsg::Close(None)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("the Daemon's close ended the session")
        .unwrap();
    assert!(dropped.load(Ordering::SeqCst), "the terminal was let go");
}

/// The Daemon never reads again: the sidecar's write makes no progress
/// for the stall bound, and the sidecar ends the session, lets the
/// terminal go and closes the socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paste_that_cannot_drain_ends_the_session_at_the_stall_bound() {
    let (daemon, sidecar) = socket_pair().await;
    let (echo, dropped) = Echo::start();
    let stall = Duration::from_secs(2);
    let session = tokio::spawn(bridge_with(sidecar, Box::new(echo), stall));
    let (sink, _unread) = daemon.split();
    let sent = Arc::new(AtomicUsize::new(0));
    let pasting = paste(sink, sent.clone());

    tokio::time::timeout(Duration::from_secs(20), session)
        .await
        .expect("the session ended at the stall bound")
        .unwrap();
    assert!(dropped.load(Ordering::SeqCst), "the terminal was let go");
    let ended = tokio::time::timeout(Duration::from_secs(10), pasting)
        .await
        .expect("the Daemon's paste ended with the socket")
        .unwrap();
    assert!(ended.is_err(), "the socket closed under the paste");
    assert!(sent.load(Ordering::SeqCst) < BOUND);
}

/// The Daemon goes away while the paste is backed up: the sidecar ends
/// the session at once, not at the stall bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_leaving_during_a_backed_up_paste_ends_the_session_at_once() {
    let (daemon, sidecar) = socket_pair().await;
    let (echo, dropped) = Echo::start();
    let session = tokio::spawn(bridge(sidecar, Box::new(echo)));
    let (sink, stream) = daemon.split();
    let sent = Arc::new(AtomicUsize::new(0));
    let pasting = paste(sink, sent.clone());
    until_stalled(&sent).await;

    pasting.abort();
    let _ = pasting.await;
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("the Daemon leaving ended the session")
        .unwrap();
    assert!(dropped.load(Ordering::SeqCst), "the terminal was let go");
}

/// The terminal's program never reads its input, and the Daemon reads
/// everything: the sidecar ends the session at the stall bound with 1011
/// and the reason, which is what the Daemon passes on to the viewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_that_takes_no_input_is_closed_1011_with_the_reason() {
    let (daemon, sidecar) = socket_pair().await;
    let (echo, dropped) = Echo::not_reading();
    let session = tokio::spawn(bridge_with(sidecar, Box::new(echo), Duration::from_secs(2)));
    let (mut sink, mut stream) = daemon.split();
    sink.send(AxMsg::Binary(vec![b'p'; CHUNK].into()))
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.next().await {
                Some(Ok(AxMsg::Close(frame))) => return frame,
                Some(Ok(_)) => {}
                other => panic!("{other:?}"),
            }
        }
    })
    .await
    .expect("a close at the stall bound")
    .expect("a close frame");
    assert_eq!(frame.code, 1011);
    assert_eq!(frame.reason.as_str(), "the terminal took no input for 2 s");
    tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("the session ended")
        .unwrap();
    assert!(dropped.load(Ordering::SeqCst), "the terminal was let go");
}
