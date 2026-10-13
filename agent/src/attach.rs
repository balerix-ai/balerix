//! `attach` on the sidecar's side (Spec O §7.2): the runner's PTY on the
//! agent's window, carried to the Daemon over a second socket. The
//! protocol is the plugin attach protocol with the roles reversed: binary
//! frames are terminal bytes both ways; the Daemon's one text frame is a
//! resize; the Daemon closes when the viewer is done, and the stream's drop
//! then ends the grouped tmux session (`TmuxAttach`).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{ATTACH_MAX_MESSAGE, LinkResult, ResizeFrame, TextFrame};
use balerix_core::PtyStream;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Bytes, Message};

use crate::link::{LinkDeps, Ws, connect, runner_failed, runner_message};

const READ_CHUNK: usize = 8192;

/// Attaches through the runner, opens the session's socket (on this
/// agent's path, with its token and the link protocol: the Daemon pairs a
/// session only with the agent it asked), answers `Ok` once it is up, and
/// leaves the bridge running.
pub async fn open(deps: &Arc<LinkDeps>, session: &str) -> LinkResult {
    let (runner, id) = (deps.runner.clone(), deps.id.clone());
    let stream = match tokio::task::spawn_blocking(move || runner.attach(&id)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return runner_failed(runner_message(&e)),
        Err(e) => return runner_failed(format!("task failed: {e}")),
    };
    let path = format!("/v1/agents/{}/link/attach/{session}", deps.id);
    let ws = match connect(deps, &path, Some(ATTACH_MAX_MESSAGE)).await {
        Ok(ws) => ws,
        Err(e) => {
            let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
            return runner_failed(format!("attach socket: {e}"));
        }
    };
    tokio::spawn(bridge(ws, stream));
    LinkResult::Ok
}

/// How long the bridge gives its close frame to leave.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// How long one piece of the Daemon's input may wait on the terminal: a
/// terminal that takes none of it for this long (its program is not
/// reading, or its output is not being read, and the paste filled what the
/// PTY holds) ends the session, rather than holding a blocking thread and
/// this task until it reads again. `balerix-server`'s `WRITE_STALL`.
pub const WRITE_STALL: Duration = Duration::from_secs(30);
/// The most of the input handed to the terminal in one write: the stall
/// bound then measures progress, not the size of a paste.
const WRITE_PIECE: usize = ATTACH_MAX_MESSAGE;
/// The input read ahead of the terminal while a write waits. Past it the
/// socket is not read until the terminal catches up, and TCP pushes back
/// on the Daemon.
const HELD_MAX: usize = 16 * WRITE_PIECE;

/// Mirrors `balerix-server`'s `attach::bridge` with the roles reversed.
///
/// Reading and writing never wait on each other: the PTY's output goes
/// out from a task of its own, so a send the Daemon is not reading yet
/// does not stop the viewer's input from reaching the PTY, and the other
/// way round. A write to the PTY waits on a blocking thread while the
/// socket is still read, so the Daemon closing (or going away) ends the
/// session even while the terminal is not taking input; a write that
/// makes no progress for `WRITE_STALL` ends it too.
pub async fn bridge(ws: Ws, stream: Box<dyn PtyStream>) {
    bridge_with(ws, stream, WRITE_STALL).await;
}

/// `bridge` with the stall bound given, for tests.
pub async fn bridge_with(ws: Ws, stream: Box<dyn PtyStream>, stall: Duration) {
    let (mut sink, mut source) = ws.split();
    let (reader, writer) = match (stream.reader(), stream.writer()) {
        (Ok(r), Ok(w)) => (r, w),
        _ => {
            let _ = tokio::time::timeout(CLOSE_WAIT, sink.close()).await;
            let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
            return;
        }
    };
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    let pump = tokio::task::spawn_blocking(move || {
        let mut reader: Box<dyn Read + Send> = reader;
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    // a close of the bridge's own (a stall), sent by the task that owns
    // the sink once it is between sends
    let (closing, mut close_rx) = tokio::sync::oneshot::channel::<CloseFrame>();
    let mut outbound = tokio::spawn(async move {
        let mut may_close = true;
        loop {
            tokio::select! {
                frame = &mut close_rx, if may_close => match frame {
                    Ok(frame) => {
                        let close = sink.send(Message::Close(Some(frame)));
                        let _ = tokio::time::timeout(CLOSE_WAIT, close).await;
                        return;
                    }
                    // the bridge is ending without one
                    Err(_) => may_close = false,
                },
                bytes = rx.recv() => match bytes {
                    Some(bytes) => {
                        if sink.send(Message::Binary(bytes.into())).await.is_err() {
                            return;
                        }
                    }
                    None => break,
                },
            }
        }
        // the window closed
        let _ = tokio::time::timeout(CLOSE_WAIT, sink.close()).await;
    });
    let mut outbound_done = false;
    let mut input = Input::new(writer, stall);
    loop {
        tokio::select! {
            _ = &mut outbound => {
                outbound_done = true;
                break;
            }
            written = input.progress(), if input.busy() => match written {
                Ok(()) => {}
                Err(Stuck::Failed) => break,
                Err(Stuck::Stalled) => {
                    let reason = format!("the terminal took no input for {} s", stall.as_secs());
                    tracing::warn!("attach: {reason}; ending the session");
                    // 1011 with the reason, which the Daemon relays to the
                    // viewer; it leaves only if the outbound task is not
                    // stuck in a send the Daemon is not reading, so the
                    // wait is bounded
                    let _ = closing.send(CloseFrame {
                        code: CloseCode::Error,
                        reason: reason.into(),
                    });
                    outbound_done =
                        tokio::time::timeout(CLOSE_WAIT, &mut outbound).await.is_ok();
                    break;
                }
            },
            msg = source.next(), if input.has_room() => match msg {
                Some(Ok(Message::Binary(bytes))) => input.push(bytes),
                Some(Ok(Message::Text(text))) => match ResizeFrame::parse(text.as_str()) {
                    TextFrame::Resize(frame) => {
                        if let Err(e) = stream.resize(frame.resize.cols, frame.resize.rows) {
                            tracing::debug!("attach resize failed: {e}");
                        }
                    }
                    TextFrame::ZeroSized => {}
                    TextFrame::Malformed => break,
                },
                // a ping's pong is queued by tungstenite and leaves with
                // the next write
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    if !outbound_done {
        outbound.abort();
    }
    drop(pump);
    // the tmux stream's drop kills its client and waits for it: off the
    // runtime, and awaited so the grouped session is really gone. A write
    // still waiting on the terminal then fails (a PTY master's write, once
    // its client is gone). The writer goes after the stream, never
    // before: a PTY writer's drop sends the terminal an EOF, which a live
    // client would hand to the agent.
    let _ = tokio::task::spawn_blocking(move || {
        drop(stream);
        drop(input);
    })
    .await;
}

type Writer = Box<dyn Write + Send>;

/// Why the input stopped reaching the terminal.
enum Stuck {
    /// A write failed.
    Failed,
    /// One piece waited the stall bound and did not go in.
    Stalled,
}

/// The input on its way to the terminal: one piece of at most
/// `WRITE_PIECE` in a blocking write at a time, what is left of its
/// message, and up to `HELD_MAX` read ahead behind it. The same as
/// `balerix-server`'s `attach::Input`.
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
