//! The daemon's end of `GET
//! /v1/plugin-host/agents/{fleet}/{crew}/{agent}/attach` (plugins spec
//! §18.4): one WebSocket bridged to the runner's `PtyStream`. Binary
//! frames are terminal bytes both ways; the one text frame is a resize.
//! Dropping the stream at the end is what ends the terminal session.

use std::io::{Read, Write};

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use balerix_api::{ResizeFrame, TextFrame};
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

pub async fn bridge(mut socket: WebSocket, mut guard: StreamGuard) {
    let Some(stream) = guard.take() else {
        close(&mut socket, CLOSE_ERROR, "attach: the stream is gone").await;
        return;
    };
    let (reader, mut writer) = match (stream.reader(), stream.writer()) {
        (Ok(r), Ok(w)) => (r, w),
        (Err(e), _) | (_, Err(e)) => {
            close(&mut socket, CLOSE_ERROR, &format!("attach: {e}")).await;
            let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
            return;
        }
    };
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    // The reader blocks on the PTY; it lives on a blocking thread and ends
    // when the stream is dropped below (the read fails once the client
    // process is gone) or the socket is.
    let pump = tokio::task::spawn_blocking(move || pump_reader(reader, &tx));
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(bytes) => {
                    if socket.send(Message::Binary(Bytes::from(bytes))).await.is_err() {
                        break;
                    }
                }
                None => {
                    close(&mut socket, CLOSE_NORMAL, "the window closed").await;
                    break;
                }
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    // The writer is a blocking handle on the PTY master: a
                    // full slave buffer (a stopped client and a paste) would
                    // pin a tokio worker, so the write waits off the runtime
                    // and the writer comes back with its result.
                    match tokio::task::spawn_blocking(move || {
                        let result = writer.write_all(&bytes).and_then(|()| writer.flush());
                        (writer, result)
                    })
                    .await
                    {
                        Ok((w, Ok(()))) => writer = w,
                        Ok((_, Err(_))) | Err(_) => {
                            close(&mut socket, CLOSE_ERROR, "the terminal's writer failed").await;
                            break;
                        }
                    }
                }
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
    // returns; the reader ends by itself once the stream is closed.
    drop(pump);
    let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
}

fn pump_reader(mut reader: Box<dyn Read + Send>, tx: &mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            // A read error is the ordinary end of a terminal session, not a
            // fault: a Linux PTY master answers `EIO`, never EOF, once the
            // last client of the slave is gone. §18.4 reserves 1011 for the
            // runner failing, so both arms close 1000 — reporting 1011 for
            // every window that simply closed would make the error code
            // meaningless.
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
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
    use std::sync::mpsc::{SyncSender, sync_channel};
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
