//! The sidecar's end of the link (Spec O §7.2): one outbound WebSocket to
//! the Daemon, reconnecting with back-off. Requests are dispatched to the
//! runner, the workspace reader or the sidecar loop (`stop`, `restart`) and
//! answered by id; a `status` frame goes out on connect and on every
//! change the sidecar loop publishes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use balerix_api::{
    FailureKind, LINK_PROTOCOL, LINK_PROTOCOL_HEADER, LinkFailure, LinkOp, LinkReply, LinkRequest,
    LinkResult, LinkStatus, SidecarFrame,
};
use balerix_core::{AgentId, AgentRunner, RunnerError, WorkspaceError, WorkspaceReader};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};

pub const RECONNECT_MIN: Duration = Duration::from_secs(1);
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// The Daemon pings every link this often (`PING_INTERVAL` in the
/// server's `kube/link.rs`).
pub const DAEMON_PING_INTERVAL: Duration = Duration::from_secs(30);
/// Three missed pings: a link that has carried nothing for this long is
/// dead (a Daemon node gone, a partition: no FIN ever arrives), and the
/// sidecar reconnects.
pub const LINK_IDLE: Duration = Duration::from_secs(3 * DAEMON_PING_INTERVAL.as_secs());
/// TCP, TLS and the upgrade, together.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What the Daemon may change about the agent's desired state (plugins
/// spec §16.4 over the link); the sidecar loop moves its stopped set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Stop,
    Restart,
}

pub struct LinkDeps {
    pub id: AgentId,
    pub token: String,
    /// `https://host:port` (or `http://` under `--allow-plain-http`).
    pub daemon_url: String,
    pub tls: Arc<rustls::ClientConfig>,
    pub runner: Arc<dyn AgentRunner>,
    pub workspace: Arc<dyn WorkspaceReader>,
    pub control: mpsc::UnboundedSender<Control>,
    pub status: watch::Receiver<LinkStatus>,
}

impl std::fmt::Debug for LinkDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkDeps")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .field("daemon_url", &self.daemon_url)
            .finish_non_exhaustive()
    }
}

/// `https://` → `wss://`, `http://` → `ws://`, plus `path`.
pub fn ws_url(daemon_url: &str, path: &str) -> String {
    let base = daemon_url.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{base}{path}")
}

/// A socket to the Daemon at `path`, with the token and the protocol,
/// within `CONNECT_TIMEOUT`.
pub async fn connect(deps: &LinkDeps, path: &str) -> Result<Ws> {
    let mut req = ws_url(&deps.daemon_url, path).into_client_request()?;
    req.headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {}", deps.token).parse()?);
    req.headers_mut()
        .insert(LINK_PROTOCOL_HEADER, LINK_PROTOCOL.to_string().parse()?);
    let connecting =
        connect_async_tls_with_config(req, None, false, Some(Connector::Rustls(deps.tls.clone())));
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| anyhow::anyhow!("no answer within {} s", CONNECT_TIMEOUT.as_secs()))??;
    Ok(ws)
}

/// For the life of the sidecar: connect, serve the session, reconnect.
pub async fn run(deps: Arc<LinkDeps>) {
    run_with_idle(deps, LINK_IDLE).await
}

/// `run` with another idle limit; the tests' silent Daemon uses it.
#[doc(hidden)]
pub async fn run_with_idle(deps: Arc<LinkDeps>, idle: Duration) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match connect(&deps, &format!("/v1/agents/{}/link", deps.id)).await {
            Ok(ws) => {
                tracing::info!("link up");
                backoff = RECONNECT_MIN;
                session(deps.clone(), ws, idle).await;
                tracing::warn!("link closed");
            }
            Err(e) => tracing::warn!("link: cannot connect: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

async fn send(ws: &mut Ws, frame: &SidecarFrame) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(frame)?.into()))
        .await?;
    Ok(())
}

async fn session(deps: Arc<LinkDeps>, mut ws: Ws, idle: Duration) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SidecarFrame>();
    let mut status = deps.status.clone();
    // the current status first, before any request is read: the Daemon
    // reconciles the stopped set on each connection's first status, and a
    // Daemon that restarted sees the agent at once
    let current = status.borrow_and_update().clone();
    if send(&mut ws, &SidecarFrame::Status(current)).await.is_err() {
        return;
    }
    // reset by every frame the Daemon sends, its pings among them; what
    // the sidecar sends proves nothing about the other end
    let mut deadline = tokio::time::Instant::now() + idle;
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => {
                tracing::warn!("link: nothing from the Daemon in {} s", idle.as_secs());
                return;
            }
            changed = status.changed() => {
                if changed.is_err() {
                    return;
                }
                let frame = SidecarFrame::Status(status.borrow_and_update().clone());
                if send(&mut ws, &frame).await.is_err() {
                    return;
                }
            }
            Some(frame) = out_rx.recv() => {
                if send(&mut ws, &frame).await.is_err() {
                    return;
                }
            }
            msg = ws.next() => {
                if matches!(msg, Some(Ok(_))) {
                    deadline = tokio::time::Instant::now() + idle;
                }
                match msg {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<LinkRequest>(text.as_str()) {
                        Ok(req) => {
                            // each request on its own task: a paced send_keys
                            // must not hold up a status frame or another request
                            let deps = deps.clone();
                            let out = out_tx.clone();
                            tokio::spawn(async move {
                                let result = dispatch(&deps, req.op).await;
                                let _ = out.send(SidecarFrame::Reply(LinkReply { id: req.id, result }));
                            });
                        }
                        Err(e) => tracing::warn!("link: not a request: {e}"),
                    },
                    Some(Ok(Message::Ping(p))) => {
                        if ws.send(Message::Pong(p)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// A runner error's text without the id its display leads with: the
/// Daemon's `RunnerError::Link { id, message }` puts the id back.
pub(crate) fn runner_message(e: &RunnerError) -> String {
    let text = e.to_string();
    let id = match e {
        RunnerError::Tool { id, .. }
        | RunnerError::Parse { id, .. }
        | RunnerError::StillRunning { id, .. }
        | RunnerError::Proc { id, .. }
        | RunnerError::Link { id, .. } => id,
    };
    match text.strip_prefix(&format!("{id}: ")) {
        Some(rest) => rest.to_string(),
        None => text,
    }
}

pub(crate) fn runner_failed(message: String) -> LinkResult {
    LinkResult::Failed {
        failure: LinkFailure {
            reason: FailureKind::Runner,
            message,
        },
    }
}

/// `WorkspaceError` by variant, with its text. For the variants the Daemon
/// rebuilds from `message` (`Missing`, `InvalidPath`, `Filter`, `Tool`),
/// the message is the field it rebuilds, so nothing reads twice; for the
/// others, the whole display text.
pub fn workspace_failure(e: WorkspaceError) -> LinkFailure {
    use WorkspaceError as W;
    let reason = match &e {
        W::Missing(_) => FailureKind::Missing,
        W::NoSuchPath => FailureKind::NoSuchPath,
        W::InvalidPath(_) => FailureKind::InvalidPath,
        W::NotAFile => FailureKind::NotAFile,
        W::NotADirectory => FailureKind::NotADirectory,
        W::TooLarge { limit } => FailureKind::TooLarge { limit: *limit },
        W::Tool { .. } => FailureKind::Tool,
        W::Filter { .. } => FailureKind::Filter,
        W::Io { .. } => FailureKind::Io,
    };
    let message = match e {
        W::Missing(field) | W::InvalidPath(field) | W::Filter { key: field } => field,
        W::Tool { stderr, .. } => stderr,
        other => other.to_string(),
    };
    LinkFailure { reason, message }
}

/// A port call off the runtime. `Err(String)` is a task failure.
async fn blocking<T, E>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<Result<T, E>, String>
where
    T: Send + 'static,
    E: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("task failed: {e}"))
}

fn workspace_result<T>(
    r: Result<Result<T, WorkspaceError>, String>,
    ok: impl FnOnce(T) -> LinkResult,
) -> LinkResult {
    match r {
        Ok(Ok(v)) => ok(v),
        Ok(Err(e)) => LinkResult::Failed {
            failure: workspace_failure(e),
        },
        Err(m) => runner_failed(m),
    }
}

pub async fn dispatch(deps: &Arc<LinkDeps>, op: LinkOp) -> LinkResult {
    let id = deps.id.clone();
    match op {
        LinkOp::SendText { text, submit } => {
            let r = deps.runner.clone();
            match blocking(move || r.send_text(&id, &text, submit)).await {
                Ok(Ok(())) => LinkResult::Ok,
                Ok(Err(e)) => runner_failed(runner_message(&e)),
                Err(m) => runner_failed(m),
            }
        }
        LinkOp::SendKeys { steps, delay_ms } => {
            let r = deps.runner.clone();
            let delay = Duration::from_millis(delay_ms);
            match blocking(move || r.send_keys(&id, &steps, delay)).await {
                Ok(Ok(())) => LinkResult::Ok,
                Ok(Err(e)) => runner_failed(runner_message(&e)),
                Err(m) => runner_failed(m),
            }
        }
        LinkOp::Stop => match deps.control.send(Control::Stop) {
            Ok(()) => LinkResult::Ok,
            Err(_) => runner_failed("the sidecar loop is gone".into()),
        },
        LinkOp::Restart => match deps.control.send(Control::Restart) {
            Ok(()) => LinkResult::Ok,
            Err(_) => runner_failed("the sidecar loop is gone".into()),
        },
        LinkOp::Attach { session } => crate::attach::open(deps, &session).await,
        LinkOp::WorkspaceDiff { base_ref } => {
            let w = deps.workspace.clone();
            workspace_result(blocking(move || w.diff(&id, &base_ref)).await, |diff| {
                LinkResult::Diff { diff }
            })
        }
        LinkOp::WorkspaceFile { path } => {
            let w = deps.workspace.clone();
            workspace_result(blocking(move || w.read_file(&id, &path)).await, |bytes| {
                LinkResult::File { bytes }
            })
        }
        LinkOp::WorkspaceTree { path } => {
            let w = deps.workspace.clone();
            workspace_result(blocking(move || w.list_dir(&id, &path)).await, |tree| {
                LinkResult::Tree { tree }
            })
        }
        LinkOp::WorkspaceVersion { base_ref } => {
            let w = deps.workspace.clone();
            workspace_result(
                blocking(move || w.version(&id, &base_ref)).await,
                |version| LinkResult::Version { version },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ws_url_swaps_the_scheme_and_appends_the_path() {
        assert_eq!(ws_url("https://d:7643/", "/v1/x"), "wss://d:7643/v1/x");
        assert_eq!(
            ws_url("http://127.0.0.1:1", "/v1/x"),
            "ws://127.0.0.1:1/v1/x"
        );
    }

    /// The frame for each variant the Daemon rebuilds from `message`
    /// (`kube/link.rs` `workspace_err`), and that rebuild displaying as the
    /// sidecar's error did, once.
    #[test]
    fn workspace_failures_round_trip_through_the_daemons_rebuild() {
        let cases = [
            (
                WorkspaceError::Missing("f/c/a".into()),
                json!({ "reason": "missing", "message": "f/c/a" }),
            ),
            (
                WorkspaceError::InvalidPath("absolute path".into()),
                json!({ "reason": "invalid_path", "message": "absolute path" }),
            ),
            (
                WorkspaceError::Filter {
                    key: "filter.lfs.smudge".into(),
                },
                json!({ "reason": "filter", "message": "filter.lfs.smudge" }),
            ),
        ];
        for (e, frame) in cases {
            let failure = workspace_failure(e.clone());
            assert_eq!(serde_json::to_value(&failure).unwrap(), frame);
            let rebuilt = match failure.reason {
                FailureKind::Missing => WorkspaceError::Missing(failure.message),
                FailureKind::InvalidPath => WorkspaceError::InvalidPath(failure.message),
                FailureKind::Filter => WorkspaceError::Filter {
                    key: failure.message,
                },
                other => panic!("{other:?}"),
            };
            assert_eq!(rebuilt.to_string(), e.to_string());
        }
        // `Tool` carries its stderr alone: the Daemon rebuilds the id and a
        // subcommand of its own around it
        let failure = workspace_failure(WorkspaceError::Tool {
            id: "f/c/a".into(),
            subcommand: "diff".into(),
            args: vec![],
            stderr: "fatal: bad revision\nmore".into(),
        });
        assert_eq!(
            serde_json::to_value(&failure).unwrap(),
            json!({ "reason": "tool", "message": "fatal: bad revision\nmore" })
        );
        let rebuilt = WorkspaceError::Tool {
            id: "f/c/a".into(),
            subcommand: "link".into(),
            args: vec![],
            stderr: failure.message,
        };
        assert_eq!(rebuilt.to_string(), "f/c/a: git link: fatal: bad revision");
        // the others carry their display text
        let failure = workspace_failure(WorkspaceError::TooLarge { limit: 1 << 20 });
        assert_eq!(failure.reason, FailureKind::TooLarge { limit: 1 << 20 });
        assert_eq!(failure.message, "file larger than 1 MiB");
    }

    /// A runner failure goes without the id its display leads with: the
    /// Daemon's `RunnerError::Link { id, message }` puts the id back once.
    #[test]
    fn runner_failures_round_trip_through_the_daemons_rebuild() {
        let id: AgentId = "f/c/a".parse().unwrap();
        let cases = [
            (
                RunnerError::Tool {
                    id: id.to_string(),
                    subcommand: "send-keys".into(),
                    args: vec![],
                    stderr: "no such window".into(),
                },
                "tmux send-keys: no such window",
            ),
            (
                RunnerError::StillRunning {
                    id: id.to_string(),
                    pid: 42,
                },
                "agent processes still running after stop (pid 42)",
            ),
            (
                RunnerError::Proc {
                    id: id.to_string(),
                    message: "/proc/42/stat: Permission denied".into(),
                },
                "cannot tell whether the agent's processes are gone: /proc/42/stat: Permission denied",
            ),
        ];
        for (e, message) in cases {
            let LinkResult::Failed { failure } = runner_failed(runner_message(&e)) else {
                panic!("not a failure");
            };
            assert_eq!(
                serde_json::to_value(&failure).unwrap(),
                json!({ "reason": "runner", "message": message })
            );
            let rebuilt = RunnerError::Link {
                id: id.to_string(),
                message: failure.message,
            };
            assert_eq!(rebuilt.to_string(), e.to_string());
        }
    }
}
