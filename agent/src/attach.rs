//! `attach` on the sidecar's side (Spec O §7.2): the runner's PTY on the
//! agent's window, carried to the Daemon over a second socket. The
//! protocol is the plugin attach protocol with the roles reversed: binary
//! frames are terminal bytes both ways; the Daemon's one text frame is a
//! resize; the Daemon closes when the viewer is done, and the stream's drop
//! then ends the grouped tmux session (`TmuxAttach`).

use std::io::{Read, Write};
use std::sync::Arc;

use balerix_api::{FailureKind, LinkFailure, LinkResult, ResizeFrame, TextFrame};
use balerix_core::PtyStream;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::link::{LinkDeps, Ws, connect};

const READ_CHUNK: usize = 8192;

fn failed(message: String) -> LinkResult {
    LinkResult::Failed {
        failure: LinkFailure {
            reason: FailureKind::Runner,
            message,
        },
    }
}

/// Attaches through the runner, opens the session's socket (on this
/// agent's path, with its token and the link protocol: the Daemon pairs a
/// session only with the agent it asked), answers `Ok` once it is up, and
/// leaves the bridge running.
pub async fn open(deps: &Arc<LinkDeps>, session: &str) -> LinkResult {
    let (runner, id) = (deps.runner.clone(), deps.id.clone());
    let stream = match tokio::task::spawn_blocking(move || runner.attach(&id)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return failed(e.to_string()),
        Err(e) => return failed(format!("task failed: {e}")),
    };
    let path = format!("/v1/agents/{}/link/attach/{session}", deps.id);
    let ws = match connect(deps, &path).await {
        Ok(ws) => ws,
        Err(e) => {
            let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
            return failed(format!("attach socket: {e}"));
        }
    };
    tokio::spawn(bridge(ws, stream));
    LinkResult::Ok
}

/// Mirrors `balerix-server`'s `attach::bridge` with the roles reversed.
pub async fn bridge(mut ws: Ws, stream: Box<dyn PtyStream>) {
    let (reader, mut writer) = match (stream.reader(), stream.writer()) {
        (Ok(r), Ok(w)) => (r, w),
        _ => {
            let _ = ws.close(None).await;
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
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(bytes) => {
                    if ws.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    // the window closed
                    let _ = ws.close(None).await;
                    break;
                }
            },
            msg = ws.next() => match msg {
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
                Some(Ok(Message::Ping(p))) => {
                    let _ = ws.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    drop(pump);
    // the tmux stream's drop kills its client and waits for it: off the
    // runtime, and awaited so the grouped session is really gone
    let _ = tokio::task::spawn_blocking(move || drop(stream)).await;
}
