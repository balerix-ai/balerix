//! A `PtyStream` whose terminal is a WebSocket the sidecar opened (Spec O
//! §7.2 `attach`). The plugin-facing `attach::bridge` reads and writes it
//! exactly as it does a tmux PTY; this file turns those blocking reads and
//! writes into frames on the sidecar's socket.
//!
//! Both directions are bounded. The socket's messages are at most
//! `ATTACH_MAX_MESSAGE` (set on the upgrade, and on the sidecar's client),
//! and each direction holds at most `CHUNKS` of them: the Daemon keeps no
//! more than about 4 MiB a way for one session, and a side that does not
//! keep up pushes back over TCP instead.
//!
//! Nothing waits on the sidecar without bound once the viewer is done: a
//! write still waiting for room gives up when the `WsPty` is dropped, and
//! what is queued for the sidecar then has `DRAIN_WAIT` to leave.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use balerix_api::{ATTACH_MAX_MESSAGE, Resize, ResizeFrame};
use balerix_core::PtyStream;

use crate::attach::{CLOSE_ERROR, Ended};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};

/// Messages held for each direction, the same depth as `attach::bridge`'s
/// own channel.
pub(crate) const CHUNKS: usize = 64;
/// How long the pump gives its close frame to leave.
const CLOSE_WAIT: Duration = Duration::from_secs(5);
/// How long what is queued for the sidecar may take to leave once the
/// viewer is done: the bound a terminal write has while it is attached.
const DRAIN_WAIT: Duration = crate::attach::WRITE_STALL;

/// A terminal size the viewer asked for; only the latest matters.
type Size = Option<(u16, u16)>;
/// A close the sidecar ended the session with, for the viewer.
type Relay = Arc<Mutex<Option<Ended>>>;

pub struct WsPty {
    reader: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    ended: Relay,
    writer: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Holds the writing direction open for the stream's whole life: the
    /// viewer is done when the stream is dropped, not when its writer is.
    _open: mpsc::Sender<Vec<u8>>,
    resize: watch::Sender<Size>,
}

/// The pump's ends of a `WsPty`'s channels.
pub(crate) struct PumpEnds {
    to_pty: mpsc::Sender<Vec<u8>>,
    /// The sidecar's 1011, set before `to_pty` is dropped.
    ended: Relay,
    from_pty: mpsc::Receiver<Vec<u8>>,
    resize: watch::Receiver<Size>,
}

/// A `WsPty` and the ends `pump` drives it with.
pub(crate) fn pair() -> (WsPty, PumpEnds) {
    let (to_pty, from_ws) = mpsc::channel(CHUNKS);
    let (to_ws, from_pty) = mpsc::channel(CHUNKS);
    let (resize_tx, resize) = watch::channel(None);
    let ended = Relay::default();
    let pty = WsPty {
        reader: Mutex::new(Some(from_ws)),
        ended: ended.clone(),
        writer: Mutex::new(Some(to_ws.clone())),
        _open: to_ws,
        resize: resize_tx,
    };
    (
        pty,
        PumpEnds {
            to_pty,
            ended,
            from_pty,
            resize,
        },
    )
}

fn taken() -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, "already taken")
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the attach socket is gone")
}

impl PtyStream for WsPty {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        let rx = self
            .reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(taken)?;
        Ok(Box::new(ChanReader {
            rx,
            buf: Vec::new(),
            ended: self.ended.clone(),
        }))
    }
    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        let tx = self
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(taken)?;
        Ok(Box::new(ChanWriter {
            tx,
            open: self.resize.subscribe(),
        }))
    }
    /// Never waits: a size not yet sent is replaced by the newer one.
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.resize.send(Some((cols, rows))).map_err(|_| gone())
    }
}

struct ChanReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    ended: Relay,
}

impl Read for ChanReader {
    /// Blocks for the next frame; EOF once the pump is gone, or the
    /// sidecar's 1011 as an `Ended` error (the bridge passes it on). A
    /// blocking read, like a PTY's: `attach::bridge` calls it on a
    /// blocking thread, never on a tokio worker.
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.blocking_recv() {
                Some(bytes) => self.buf = bytes,
                None => {
                    let ended = self.ended.lock().unwrap_or_else(|e| e.into_inner()).take();
                    return match ended {
                        Some(ended) => Err(io::Error::other(ended)),
                        None => Ok(0),
                    };
                }
            }
        }
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Ok(n)
    }
}

struct ChanWriter {
    tx: mpsc::Sender<Vec<u8>>,
    /// Ends (its sender is the `WsPty`'s) once the stream is dropped.
    open: watch::Receiver<Size>,
}

/// Resolves once the `WsPty` that `open` came from is dropped.
async fn dropped(mut open: watch::Receiver<Size>) {
    while open.changed().await.is_ok() {}
}

impl Write for ChanWriter {
    /// At most `ATTACH_MAX_MESSAGE` bytes a call (`write_all` loops), and
    /// blocks while `CHUNKS` messages wait for the sidecar, or until the
    /// `WsPty` is dropped (then it fails, like a PTY's write once its
    /// client is gone). A blocking write, like a PTY's: `attach::bridge`
    /// calls it on a blocking thread.
    ///
    /// Precondition: call it from a blocking thread (`spawn_blocking`) or a
    /// thread outside any runtime, never from a runtime worker. A worker
    /// also answers `Handle::try_current`, so that check does not catch
    /// it: `block_on` there panics, as `blocking_send` would.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len().min(ATTACH_MAX_MESSAGE);
        if n == 0 {
            return Ok(0);
        }
        let piece = bytes[..n].to_vec();
        let sent = match tokio::runtime::Handle::try_current() {
            // a `spawn_blocking` thread (see the precondition): the wait
            // needs no driver, only the channel's and the watch's wakeups
            Ok(rt) => {
                let open = self.open.clone();
                rt.block_on(async {
                    tokio::select! {
                        sent = self.tx.send(piece) => sent.is_ok(),
                        () = dropped(open) => false,
                    }
                })
            }
            Err(_) => self.tx.blocking_send(piece).is_ok(),
        };
        if sent { Ok(n) } else { Err(gone()) }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Drives the sidecar's attach socket until the plugin's side drops the
/// `WsPty` (the writer then closes the socket) or the socket ends.
///
/// Reading and writing never wait on each other: the writer is a task of
/// its own, so a send to a sidecar that is not reading cannot stop the
/// sidecar's bytes from reaching the viewer, and the other way round.
///
/// `to_pty` is bounded. A message that finds it full is held, and the
/// socket is not read again until the reader has made room for it: the
/// sidecar's sends then wait on TCP.
pub(crate) async fn pump(socket: WebSocket, ends: PumpEnds) {
    let PumpEnds {
        to_pty,
        ended,
        from_pty,
        resize,
    } = ends;
    let (sink, mut stream) = socket.split();
    let mut writer = tokio::spawn(write_to_sidecar(sink, from_pty, resize));
    let mut writer_done = false;
    let mut held: Option<Vec<u8>> = None;
    loop {
        tokio::select! {
            _ = &mut writer => {
                writer_done = true;
                break;
            }
            permit = to_pty.reserve(), if held.is_some() => match (permit, held.take()) {
                (Ok(permit), Some(bytes)) => permit.send(bytes),
                // the reader is gone
                _ => break,
            },
            msg = stream.next(), if held.is_none() => match msg {
                Some(Ok(Message::Binary(b))) => match to_pty.try_send(b.to_vec()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(bytes)) => held = Some(bytes),
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                },
                // the sidecar's own stall: the viewer hears it as sent
                Some(Ok(Message::Close(Some(frame)))) if frame.code == CLOSE_ERROR => {
                    *ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ended {
                        code: frame.code,
                        reason: frame.reason.to_string(),
                    });
                    break;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    if !writer_done {
        writer.abort();
    }
    // dropping `to_pty` ends the reader with EOF once it has read what is
    // queued; the bridge then closes the plugin's socket 1000
}

/// The pump's writing half: `forward`, given `DRAIN_WAIT` once the viewer
/// is done, so a sidecar that stops reading cannot hold it after that.
async fn write_to_sidecar(
    sink: SplitSink<WebSocket, Message>,
    from_pty: mpsc::Receiver<Vec<u8>>,
    resize: watch::Receiver<Size>,
) {
    let open = resize.clone();
    tokio::select! {
        () = forward(sink, from_pty, resize) => {}
        () = async {
            dropped(open).await;
            tokio::time::sleep(DRAIN_WAIT).await;
        } => {}
    }
}

/// The viewer's bytes and the latest size, until the viewer is done (then
/// a close frame, given `CLOSE_WAIT`) or a send fails.
async fn forward(
    mut sink: SplitSink<WebSocket, Message>,
    mut from_pty: mpsc::Receiver<Vec<u8>>,
    mut resize: watch::Receiver<Size>,
) {
    let mut resizing = true;
    loop {
        tokio::select! {
            out = from_pty.recv() => match out {
                Some(b) => {
                    if sink.send(Message::Binary(Bytes::from(b))).await.is_err() {
                        return;
                    }
                }
                None => {
                    let close = sink.send(Message::Close(Some(CloseFrame {
                        code: crate::attach::CLOSE_NORMAL,
                        reason: "the viewer is done".into(),
                    })));
                    let _ = tokio::time::timeout(CLOSE_WAIT, close).await;
                    return;
                }
            },
            changed = resize.changed(), if resizing => {
                if changed.is_err() {
                    // the `WsPty` is gone; its writer may still be draining
                    resizing = false;
                    continue;
                }
                let size = *resize.borrow_and_update();
                let Some((cols, rows)) = size else { continue };
                let frame = ResizeFrame { resize: Resize { cols, rows } };
                let Ok(json) = serde_json::to_string(&frame) else { return };
                if sink.send(Message::Text(json.into())).await.is_err() {
                    return;
                }
            }
        }
    }
}
