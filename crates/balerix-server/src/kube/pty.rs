//! A `PtyStream` whose terminal is a WebSocket the sidecar opened (Spec O
//! §7.2 `attach`). The plugin-facing `attach::bridge` reads and writes it
//! exactly as it does a tmux PTY; this file turns those blocking reads and
//! writes into frames on the sidecar's socket.

use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender};

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use balerix_api::{Resize, ResizeFrame};
use balerix_core::PtyStream;
use tokio::sync::mpsc;

/// What the plugin's side sends toward the sidecar.
pub(crate) enum Outbound {
    Bytes(Vec<u8>),
    Resize(u16, u16),
}

pub struct WsPty {
    reader: Mutex<Option<Receiver<Vec<u8>>>>,
    writer: Mutex<Option<mpsc::UnboundedSender<Outbound>>>,
    control: mpsc::UnboundedSender<Outbound>,
}

impl WsPty {
    pub(crate) fn new(from_ws: Receiver<Vec<u8>>, to_ws: mpsc::UnboundedSender<Outbound>) -> Self {
        Self {
            reader: Mutex::new(Some(from_ws)),
            writer: Mutex::new(Some(to_ws.clone())),
            control: to_ws,
        }
    }
}

fn taken() -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, "already taken")
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
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.control
            .send(Outbound::Resize(cols, rows))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the attach socket is gone"))
    }
}

struct ChanReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
}

impl Read for ChanReader {
    /// Blocks for the next frame; EOF once the pump is gone.
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(bytes) => self.buf = bytes,
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Ok(n)
    }
}

struct ChanWriter {
    tx: mpsc::UnboundedSender<Outbound>,
}

impl Write for ChanWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.tx
            .send(Outbound::Bytes(bytes.to_vec()))
            .map(|()| bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the attach socket is gone"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Drives the sidecar's attach socket until the plugin's side drops the
/// `WsPty` (every `Outbound` sender gone) or the socket ends.
pub(crate) async fn pump(
    mut socket: WebSocket,
    to_pty: Sender<Vec<u8>>,
    mut from_pty: mpsc::UnboundedReceiver<Outbound>,
) {
    loop {
        tokio::select! {
            out = from_pty.recv() => match out {
                Some(Outbound::Bytes(b)) => {
                    if socket.send(Message::Binary(Bytes::from(b))).await.is_err() {
                        break;
                    }
                }
                Some(Outbound::Resize(cols, rows)) => {
                    let frame = ResizeFrame { resize: Resize { cols, rows } };
                    let Ok(json) = serde_json::to_string(&frame) else { break };
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame {
                            code: crate::attach::CLOSE_NORMAL,
                            reason: "the viewer is done".into(),
                        })))
                        .await;
                    break;
                }
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if to_pty.send(b.to_vec()).is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    // dropping `to_pty` ends the reader with EOF; the bridge then closes
    // the plugin's socket 1000
}
