//! The daemon's end of `GET
//! /v1/plugin-host/agents/{fleet}/{crew}/{agent}/attach` (plugins spec
//! §18.4): one WebSocket bridged to the runner's `PtyStream`. Binary
//! frames are terminal bytes both ways; the one text frame is a resize.
//! Dropping the stream at the end is what ends the terminal session.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::pin::Pin;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use balerix_api::{ATTACH_MAX_MESSAGE, ResizeFrame, TextFrame};
use balerix_core::PtyStream;
use tokio::sync::mpsc;

const READ_CHUNK: usize = 8192;
/// The window closed, or the peer is done.
pub const CLOSE_NORMAL: u16 = 1000;
/// A text frame that is not a resize.
pub const CLOSE_UNSUPPORTED: u16 = 1003;
/// The runner's side failed.
pub const CLOSE_ERROR: u16 = 1011;

/// Carries the attached stream from the handler to the bridge without ever
/// letting it drop on a tokio worker. Dropping a `TmuxAttach` kills its
/// client, waits for it and runs `tmux kill-session` — blocking work — and
/// the handler attaches *before* the upgrade, so the closure axum drops
/// when the upgrade never completes is holding the stream. The bridge
/// takes it out and offloads its own drops; anything else offloads here.
pub struct StreamGuard(Option<Box<dyn PtyStream>>);

impl StreamGuard {
    pub fn new(stream: Box<dyn PtyStream>) -> StreamGuard {
        StreamGuard(Some(stream))
    }

    /// The stream, out of the guard; the guard's own drop then does nothing.
    pub fn take(&mut self) -> Option<Box<dyn PtyStream>> {
        self.0.take()
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let Some(stream) = self.0.take() else { return };
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                tokio::task::spawn_blocking(move || drop(stream));
            }
            // No runtime at all (a plain thread): dropping here is the
            // only option left. A runtime that is shutting down still
            // answers `Ok` and accepts the blocking task; the stream is
            // then dropped as the blocking queue drains, the same
            // exposure as the `spawn_blocking(drop)` calls in `bridge`.
            Err(_) => drop(stream),
        }
    }
}

/// How long one piece of the viewer's input may wait on the terminal.
/// A terminal that takes none of it for this long (its program is not
/// reading, and a paste filled what the PTY holds) ends the session with
/// `CLOSE_ERROR`, rather than holding a blocking thread and this task
/// until it reads again.
pub const WRITE_STALL: Duration = Duration::from_secs(30);
/// The most of the viewer's input handed to the terminal in one write:
/// the stall bound then measures progress, not the size of a paste.
const WRITE_PIECE: usize = ATTACH_MAX_MESSAGE;
/// The viewer's input read ahead of the terminal while a write waits.
/// Past it the socket is not read until the terminal catches up, and
/// TCP pushes back on the viewer.
const HELD_MAX: usize = 16 * WRITE_PIECE;

pub async fn bridge(socket: WebSocket, guard: StreamGuard) {
    bridge_with(socket, guard, WRITE_STALL).await;
}

async fn bridge_with(mut socket: WebSocket, mut guard: StreamGuard, stall: Duration) {
    let Some(stream) = guard.take() else {
        close(&mut socket, CLOSE_ERROR, "attach: the stream is gone").await;
        return;
    };
    let (reader, writer) = match (stream.reader(), stream.writer()) {
        (Ok(r), Ok(w)) => (r, w),
        (Err(e), _) | (_, Err(e)) => {
            close(&mut socket, CLOSE_ERROR, &format!("attach: {e}")).await;
            let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
            return;
        }
    };
    let (tx, mut rx) = mpsc::channel::<Out>(64);
    // The reader blocks on the PTY; it lives on a blocking thread and ends
    // when the stream is dropped below (the read fails once the client
    // process is gone) or the socket is.
    let pump = tokio::task::spawn_blocking(move || pump_reader(reader, &tx));
    // The writer is a blocking handle on the PTY master too, so its writes
    // wait off the runtime, and the socket is still read while one does:
    // a viewer that leaves while the terminal is not taking its input
    // ends the session at once, not when the terminal reads again.
    let mut input = Input::new(writer, stall);
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(Out::Ended(ended)) => {
                    close(&mut socket, ended.code, &ended.reason).await;
                    break;
                }
                Some(Out::Bytes(bytes)) => {
                    if socket.send(Message::Binary(Bytes::from(bytes))).await.is_err() {
                        break;
                    }
                }
                None => {
                    close(&mut socket, CLOSE_NORMAL, "the window closed").await;
                    break;
                }
            },
            written = input.progress(), if input.busy() => match written {
                Ok(()) => {}
                Err(Stuck::Failed) => {
                    close(&mut socket, CLOSE_ERROR, "the terminal's writer failed").await;
                    break;
                }
                Err(Stuck::Stalled) => {
                    let reason = format!("the terminal took no input for {} s", stall.as_secs());
                    close(&mut socket, CLOSE_ERROR, &reason).await;
                    break;
                }
            },
            msg = socket.recv(), if input.has_room() => match msg {
                Some(Ok(Message::Binary(bytes))) => input.push(bytes),
                Some(Ok(Message::Text(text))) => match ResizeFrame::parse(text.as_str()) {
                    TextFrame::Resize(frame) => {
                        if let Err(e) = stream.resize(frame.resize.cols, frame.resize.rows) {
                            tracing::debug!("attach resize failed: {e}");
                        }
                    }
                    // a hidden container's fit, not a fault
                    TextFrame::ZeroSized => {}
                    TextFrame::Malformed => {
                        close(&mut socket, CLOSE_UNSUPPORTED, "expected a resize frame").await;
                        break;
                    }
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    // Ending the session is synchronous — the tmux stream's drop kills its
    // client and waits for it — so it happens off the async runtime, like
    // the reader. Awaited so the window is really gone before the handler
    // returns; the reader ends by itself once the stream is closed, and so
    // does a write still waiting on the terminal (a PTY master's write
    // fails once its client is gone; a Kubernetes stream's once it is
    // dropped). The writer goes after the stream, never before: a PTY
    // writer's drop sends the terminal an EOF, which a live client would
    // hand to the agent.
    drop(pump);
    let _ = tokio::task::spawn_blocking(move || {
        drop(stream);
        drop(input);
    })
    .await;
}

type Writer = Box<dyn Write + Send>;

/// Why the viewer's input stopped reaching the terminal.
enum Stuck {
    /// A write failed.
    Failed,
    /// One piece waited `stall` and did not go in.
    Stalled,
}

/// The viewer's input on its way to the terminal: one piece of at most
/// `WRITE_PIECE` in a blocking write at a time, what is left of its
/// message, and up to `HELD_MAX` read ahead behind it.
struct Input {
    /// The writer while no write is in flight.
    idle: Option<Writer>,
    writing: Option<tokio::task::JoinHandle<(Writer, std::io::Result<()>)>>,
    /// The rest of the message the piece in flight came from.
    rest: Bytes,
    held: VecDeque<Bytes>,
    held_bytes: usize,
    stall: Duration,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl Input {
    fn new(writer: Writer, stall: Duration) -> Input {
        Input {
            idle: Some(writer),
            writing: None,
            rest: Bytes::new(),
            held: VecDeque::new(),
            held_bytes: 0,
            stall,
            deadline: Box::pin(tokio::time::sleep(stall)),
        }
    }

    /// A write is in flight.
    fn busy(&self) -> bool {
        self.writing.is_some()
    }

    /// The socket may be read: less than `HELD_MAX` waits behind the
    /// write in flight.
    fn has_room(&self) -> bool {
        self.held_bytes < HELD_MAX
    }

    fn push(&mut self, bytes: Bytes) {
        if bytes.is_empty() {
            return;
        }
        match self.idle.take() {
            Some(writer) => {
                self.rest = bytes;
                self.start(writer);
            }
            None => {
                self.held_bytes += bytes.len();
                self.held.push_back(bytes);
            }
        }
    }

    fn start(&mut self, mut writer: Writer) {
        let piece = self.rest.split_to(self.rest.len().min(WRITE_PIECE));
        self.deadline
            .as_mut()
            .reset(tokio::time::Instant::now() + self.stall);
        self.writing = Some(tokio::task::spawn_blocking(move || {
            let result = writer.write_all(&piece).and_then(|()| writer.flush());
            (writer, result)
        }));
    }

    /// Waits for the piece in flight and starts the next one, if any.
    /// Cancel-safe: the write and the deadline are polled by reference.
    async fn progress(&mut self) -> Result<(), Stuck> {
        let Some(writing) = self.writing.as_mut() else {
            return std::future::pending().await;
        };
        let done = tokio::select! {
            done = writing => done,
            () = self.deadline.as_mut() => return Err(Stuck::Stalled),
        };
        self.writing = None;
        let Ok((writer, Ok(()))) = done else {
            return Err(Stuck::Failed);
        };
        if self.rest.is_empty()
            && let Some(next) = self.held.pop_front()
        {
            self.held_bytes -= next.len();
            self.rest = next;
        }
        if self.rest.is_empty() {
            self.idle = Some(writer);
        } else {
            self.start(writer);
        }
        Ok(())
    }
}

/// The terminal's side, as the reader's pump hands it to the bridge.
enum Out {
    Bytes(Vec<u8>),
    /// The stream ended with a close of its own to pass on.
    Ended(Ended),
}

/// A stream's reader fails with this (`io::Error::other`) when the
/// terminal's far end closed with a reason the viewer should see: a
/// Kubernetes sidecar's 1011 when its terminal took no input. The bridge
/// closes the viewer's socket with the same code and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ended {
    pub code: u16,
    pub reason: String,
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.code, self.reason)
    }
}

impl std::error::Error for Ended {}

fn pump_reader(mut reader: Box<dyn Read + Send>, tx: &mpsc::Sender<Out>) {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Err(e) if e.get_ref().is_some_and(|inner| inner.is::<Ended>()) => {
                if let Some(ended) = e.into_inner().and_then(|i| i.downcast::<Ended>().ok()) {
                    let _ = tx.blocking_send(Out::Ended(*ended));
                }
                return;
            }
            // A read error is the ordinary end of a terminal session, not a
            // fault: a Linux PTY master answers `EIO`, never EOF, once the
            // last client of the slave is gone. §18.4 reserves 1011 for the
            // runner failing, so both arms close 1000 — reporting 1011 for
            // every window that simply closed would make the error code
            // meaningless.
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.blocking_send(Out::Bytes(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
        }
    }
}

/// A close frame's reason fits in the control frame: 123 bytes at most,
/// cut at a character boundary (a runner error can be long).
fn close_reason(reason: &str) -> String {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: close_reason(reason).into(),
        })))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::thread::ThreadId;
    use std::time::Duration;

    /// A stream whose drop reports the thread it ran on.
    struct Marker(SyncSender<ThreadId>);

    impl PtyStream for Marker {
        fn reader(&self) -> std::io::Result<Box<dyn Read + Send>> {
            Err(std::io::Error::other("no reader"))
        }
        fn writer(&self) -> std::io::Result<Box<dyn Write + Send>> {
            Err(std::io::Error::other("no writer"))
        }
        fn resize(&self, _: u16, _: u16) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Marker {
        fn drop(&mut self) {
            let _ = self.0.send(std::thread::current().id());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_still_held_stream_is_dropped_off_the_runtime() {
        let (tx, rx) = sync_channel::<ThreadId>(1);
        let here = std::thread::current().id();
        drop(StreamGuard::new(Box::new(Marker(tx))));
        let dropped_on =
            tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5)))
                .await
                .unwrap()
                .expect("the stream was dropped");
        assert_ne!(dropped_on, here, "the drop left the tokio worker");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_taken_stream_is_the_caller_s_to_drop() {
        let (tx, rx) = sync_channel::<ThreadId>(1);
        let mut guard = StreamGuard::new(Box::new(Marker(tx)));
        let stream = guard.take().expect("the stream");
        assert!(guard.take().is_none(), "taken once");
        drop(guard);
        assert!(
            rx.try_recv().is_err(),
            "the guard dropped a stream it no longer holds"
        );
        drop(stream);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the caller's own drop");
    }

    #[test]
    fn a_close_reason_fits_the_control_frame() {
        assert_eq!(close_reason("the window closed"), "the window closed");
        let long = format!("attach: {}", "x".repeat(300));
        assert_eq!(close_reason(&long).len(), 123);
        let multibyte = "ü".repeat(70); // 140 bytes
        assert_eq!(close_reason(&multibyte).len(), 122, "a character boundary");
    }

    /// A terminal whose program never reads: every write blocks until the
    /// stream is dropped, then fails, as a PTY master's does once its
    /// client is gone. `entered` hears each write begin, `freed` each
    /// blocked write let go.
    struct Unread {
        /// Dropped with the stream: the blocked calls then return.
        _open: (SyncSender<()>, SyncSender<()>),
        writer: Mutex<Option<Receiver<()>>>,
        reader: Mutex<Option<Receiver<()>>>,
        entered: SyncSender<()>,
        freed: SyncSender<()>,
    }

    struct Blocked {
        until_dropped: Receiver<()>,
        entered: SyncSender<()>,
        freed: SyncSender<()>,
    }

    impl Write for Blocked {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.try_send(());
            let _ = self.until_dropped.recv();
            let _ = self.freed.try_send(());
            Err(std::io::Error::other("the client is gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Silent(Receiver<()>);

    impl Read for Silent {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    impl Unread {
        fn new() -> (Unread, Receiver<()>, Receiver<()>) {
            let (w_tx, w_rx) = sync_channel(0);
            let (r_tx, r_rx) = sync_channel(0);
            let (entered, entered_rx) = sync_channel(16);
            let (freed, freed_rx) = sync_channel(16);
            let pty = Unread {
                _open: (w_tx, r_tx),
                writer: Mutex::new(Some(w_rx)),
                reader: Mutex::new(Some(r_rx)),
                entered,
                freed,
            };
            (pty, entered_rx, freed_rx)
        }
    }

    impl PtyStream for Unread {
        fn reader(&self) -> std::io::Result<Box<dyn Read + Send>> {
            Ok(Box::new(Silent(
                self.reader.lock().unwrap().take().unwrap(),
            )))
        }
        fn writer(&self) -> std::io::Result<Box<dyn Write + Send>> {
            Ok(Box::new(Blocked {
                until_dropped: self.writer.lock().unwrap().take().unwrap(),
                entered: self.entered.clone(),
                freed: self.freed.clone(),
            }))
        }
        fn resize(&self, _: u16, _: u16) -> std::io::Result<()> {
            Ok(())
        }
    }

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// A listener bridging one socket to `pty` with `stall`; answers the
    /// client and a receiver that hears the bridge return.
    async fn bridged(
        pty: Box<dyn PtyStream>,
        stall: Duration,
    ) -> (Client, tokio::sync::oneshot::Receiver<()>) {
        use axum::extract::WebSocketUpgrade;
        let (done_tx, done) = tokio::sync::oneshot::channel();
        let slot = std::sync::Arc::new(Mutex::new(Some((pty, done_tx))));
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move |ws: WebSocketUpgrade| async move {
                let (pty, done_tx) = slot.lock().unwrap().take().unwrap();
                let guard = StreamGuard::new(pty);
                ws.on_upgrade(move |socket| async move {
                    bridge_with(socket, guard, stall).await;
                    let _ = done_tx.send(());
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        (client, done)
    }

    async fn heard(rx: Receiver<()>, what: &str) {
        tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .unwrap_or_else(|_| panic!("{what}"));
    }

    /// The viewer pastes into a terminal that is not reading, then leaves:
    /// the bridge sees it go while the write is still blocked, and ending
    /// the session lets the write go.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_viewer_that_leaves_during_a_blocked_write_ends_the_session() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message as Msg;
        let (pty, entered, freed) = Unread::new();
        let (mut client, done) = bridged(Box::new(pty), Duration::from_secs(3600)).await;
        client
            .send(Msg::Binary(vec![b'x'; 1000].into()))
            .await
            .unwrap();
        heard(entered, "the write began").await;
        // more input queues behind the blocked write; the close follows it
        client
            .send(Msg::Binary(vec![b'y'; 1000].into()))
            .await
            .unwrap();
        client.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("the bridge saw the viewer leave")
            .unwrap();
        heard(freed, "the blocked write was let go").await;
    }

    /// A write that makes no progress for the stall bound ends the session
    /// with 1011 and says why.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_that_stalls_ends_the_session_with_a_reason() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as Msg;
        let (pty, entered, freed) = Unread::new();
        let (mut client, done) = bridged(Box::new(pty), Duration::from_secs(1)).await;
        client
            .send(Msg::Binary(vec![b'x'; 1000].into()))
            .await
            .unwrap();
        heard(entered, "the write began").await;
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match client.next().await {
                    Some(Ok(Msg::Close(frame))) => return frame,
                    Some(Ok(_)) => {}
                    other => panic!("{other:?}"),
                }
            }
        })
        .await
        .expect("a close at the stall bound")
        .expect("a close frame");
        assert_eq!(u16::from(frame.code), CLOSE_ERROR);
        assert_eq!(frame.reason.as_str(), "the terminal took no input for 1 s");
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("the bridge returned")
            .unwrap();
        heard(freed, "the blocked write was let go").await;
    }

    /// A terminal whose far end closed with a reason of its own (a
    /// Kubernetes sidecar's stall): its reader fails with `Ended`.
    struct EndsWith(Ended);

    impl PtyStream for EndsWith {
        fn reader(&self) -> std::io::Result<Box<dyn Read + Send>> {
            struct Failing(Ended);
            impl Read for Failing {
                fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                    Err(std::io::Error::other(self.0.clone()))
                }
            }
            Ok(Box::new(Failing(self.0.clone())))
        }
        fn writer(&self) -> std::io::Result<Box<dyn Write + Send>> {
            Ok(Box::new(std::io::sink()))
        }
        fn resize(&self, _: u16, _: u16) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The terminal's own close reaches the viewer with its code and
    /// reason, not as 1000 "the window closed".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_close_the_terminal_ended_with_is_passed_on() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message as Msg;
        let ended = Ended {
            code: CLOSE_ERROR,
            reason: "the terminal took no input for 30 s".into(),
        };
        let (mut client, done) =
            bridged(Box::new(EndsWith(ended)), Duration::from_secs(3600)).await;
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match client.next().await {
                    Some(Ok(Msg::Close(frame))) => return frame,
                    Some(Ok(_)) => {}
                    other => panic!("{other:?}"),
                }
            }
        })
        .await
        .expect("a close")
        .expect("a close frame");
        assert_eq!(u16::from(frame.code), CLOSE_ERROR);
        assert_eq!(frame.reason.as_str(), "the terminal took no input for 30 s");
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("the bridge returned")
            .unwrap();
    }

    #[test]
    fn outside_a_runtime_the_drop_is_inline() {
        let (tx, rx) = sync_channel::<ThreadId>(1);
        let here = std::thread::current().id();
        drop(StreamGuard::new(Box::new(Marker(tx))));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(here),
            "no runtime to protect, so the drop runs here"
        );
    }
}
