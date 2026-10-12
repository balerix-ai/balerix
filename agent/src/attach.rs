//! `attach` on the sidecar's side (Spec O §7.2): the runner's PTY on the
//! agent's window, carried to the Daemon over a second socket. The
//! protocol is the plugin attach protocol with the roles reversed: binary
//! frames are terminal bytes both ways; the Daemon's one text frame is a
//! resize; the Daemon closes when the viewer is done, and the stream's drop
//! then ends the grouped tmux session (`TmuxAttach`).

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use balerix_api::link::ATTACH_MAX_MESSAGE;
use balerix_api::{LinkResult, ResizeFrame, TextFrame};
use balerix_core::PtyStream;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

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

/// Mirrors `balerix-server`'s `attach::bridge` with the roles reversed.
///
/// Reading and writing never wait on each other: the PTY's output goes
/// out from a task of its own, so a send the Daemon is not reading yet
/// does not stop the viewer's input from reaching the PTY, and the other
/// way round.
pub async fn bridge(ws: Ws, stream: Box<dyn PtyStream>) {
    let (mut sink, mut source) = ws.split();
    let (reader, mut writer) = match (stream.reader(), stream.writer()) {
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
    let mut outbound = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                return;
            }
        }
        // the window closed
        let _ = tokio::time::timeout(CLOSE_WAIT, sink.close()).await;
    });
    let mut outbound_done = false;
    loop {
        tokio::select! {
            _ = &mut outbound => {
                outbound_done = true;
                break;
            }
            msg = source.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    match tokio::task::spawn_blocking(move || {
                        let result = writer.write_all(&bytes).and_then(|()| writer.flush());
                        (writer, result)
                    })
                    .await
                    {
                        Ok((w, Ok(()))) => writer = w,
                        _ => break,
                    }
                }
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
    // runtime, and awaited so the grouped session is really gone
    let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
}
