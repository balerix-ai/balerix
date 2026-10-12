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

use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use balerix_api::link::ATTACH_MAX_MESSAGE;
use balerix_api::{Resize, ResizeFrame};
use balerix_core::PtyStream;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};

/// Messages held for each direction, the same depth as `attach::bridge`'s
/// own channel.
pub(crate) const CHUNKS: usize = 64;
/// How long the pump gives its close frame to leave.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// A terminal size the viewer asked for; only the latest matters.
type Size = Option<(u16, u16)>;

pub struct WsPty {
    reader: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    writer: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Holds the writing direction open for the stream's whole life: the
    /// viewer is done when the stream is dropped, not when its writer is.
    _open: mpsc::Sender<Vec<u8>>,
    resize: watch::Sender<Size>,
}

/// The pump's ends of a `WsPty`'s channels.
pub(crate) struct PumpEnds {
    to_pty: mpsc::Sender<Vec<u8>>,
    from_pty: mpsc::Receiver<Vec<u8>>,
    resize: watch::Receiver<Size>,
}

/// A `WsPty` and the ends `pump` drives it with.
pub(crate) fn pair() -> (WsPty, PumpEnds) {
    let (to_pty, from_ws) = mpsc::channel(CHUNKS);
    let (to_ws, from_pty) = mpsc::channel(CHUNKS);
    let (resize_tx, resize) = watch::channel(None);
    let pty = WsPty {
        reader: Mutex::new(Some(from_ws)),
        writer: Mutex::new(Some(to_ws.clone())),
        _open: to_ws,
        resize: resize_tx,
    };
    (
        pty,
        PumpEnds {
            to_pty,
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
        }))
    }
    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        let tx = self
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(taken)?;
        Ok(Box::new(ChanWriter { tx }))
    }
    /// Never waits: a size not yet sent is replaced by the newer one.
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.resize.send(Some((cols, rows))).map_err(|_| gone())
    }
}

struct ChanReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
}

impl Read for ChanReader {
    /// Blocks for the next frame; EOF once the pump is gone. A blocking
    /// read, like a PTY's: `attach::bridge` calls it on a blocking thread,
    /// never on a tokio worker.
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.blocking_recv() {
                Some(bytes) => self.buf = bytes,
                None => return Ok(0),
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
}

impl Write for ChanWriter {
    /// At most `ATTACH_MAX_MESSAGE` bytes a call (`write_all` loops), and
    /// blocks while `CHUNKS` messages wait for the sidecar. A blocking
    /// write, like a PTY's: `attach::bridge` calls it on a blocking thread.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len().min(ATTACH_MAX_MESSAGE);
        if n == 0 {
            return Ok(0);
        }
        self.tx
            .blocking_send(bytes[..n].to_vec())
            .map(|()| n)
            .map_err(|_| gone())
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

/// The pump's writing half: the viewer's bytes and the latest size, until
/// the viewer is done (then a close frame, given `CLOSE_WAIT`) or a send
/// fails.
async fn write_to_sidecar(
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
