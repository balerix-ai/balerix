//! The Daemon's end of the sidecar link (Spec O §7.2). One WebSocket per
//! agent, opened by the sidecar at `GET /v1/agents/{f}/{c}/{a}/link` with
//! the agent's token. Requests leave as `LinkRequest` text frames, each
//! answered by a `LinkReply` carrying the same id; the sidecar's `status`
//! frames go to the fleet actor. `LinkHub` implements the two sync ports
//! over the link, so `execute_action` and the plugin routes work unchanged;
//! a call for an agent whose link is down fails naming it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::PathRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use balerix_api::{
    FailureKind, LINK_PROTOCOL, LINK_PROTOCOL_HEADER, LinkFailure, LinkOp, LinkRequest, LinkResult,
    LinkStatus, SidecarFrame, WorkspaceDiff, WorkspaceTree, WorkspaceVersion,
};
use balerix_core::{
    AgentId, AgentRunner, CrewRef, FleetName, LaunchPlan, ObservedState, ProcessState, PtyStream,
    RunnerError, WorkspaceError, WorkspaceReader,
};
use tokio::sync::mpsc;

use crate::api::{ApiError, AppState};
use crate::auth::bearer;
use crate::daemon::Daemon;

/// How long a blocking call waits for the sidecar's reply.
pub const LINK_CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Reaps a sidecar that vanished without a close frame (as `watch.rs`).
const PING_INTERVAL: Duration = Duration::from_secs(30);

type Pending = Arc<Mutex<HashMap<u64, SyncSender<LinkResult>>>>;

struct Conn {
    tx: mpsc::UnboundedSender<LinkRequest>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    /// Which connection this is: a reconnect replaces an older one, and
    /// the older one's teardown must not unregister the newer.
    epoch: u64,
    /// The last `status` frame, for `observe`.
    last: Option<LinkStatus>,
}

#[derive(Default)]
pub struct LinkHub {
    conns: Mutex<HashMap<AgentId, Conn>>,
    epochs: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LinkError {
    #[error("link down")]
    Down,
    #[error("the sidecar did not answer within {} s", LINK_CALL_TIMEOUT.as_secs())]
    Timeout,
    #[error("{}", .0.message)]
    Failed(LinkFailure),
    #[error("unexpected reply to {0}")]
    Unexpected(&'static str),
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl LinkHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn linked(&self, agent: &AgentId) -> bool {
        lock(&self.conns).contains_key(agent)
    }

    fn register(
        &self,
        agent: &AgentId,
        tx: mpsc::UnboundedSender<LinkRequest>,
        pending: Pending,
    ) -> u64 {
        let epoch = self.epochs.fetch_add(1, Ordering::SeqCst);
        let mut conns = lock(&self.conns);
        let old = conns.insert(
            agent.clone(),
            Conn {
                tx,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
                epoch,
                last: None,
            },
        );
        // An older connection's calls in flight fail with `Down`: dropping
        // their senders disconnects the receivers they wait on. Under the
        // `conns` lock, so no call can still be adding to that map.
        if let Some(old) = old {
            lock(&old.pending).clear();
        }
        epoch
    }

    fn unregister(&self, agent: &AgentId, epoch: u64) {
        let mut conns = lock(&self.conns);
        if conns.get(agent).is_some_and(|c| c.epoch == epoch) {
            conns.remove(agent);
        }
    }

    fn note_status(&self, agent: &AgentId, epoch: u64, status: &LinkStatus) {
        if let Some(c) = lock(&self.conns).get_mut(agent)
            && c.epoch == epoch
        {
            c.last = Some(status.clone());
        }
    }

    /// One request and its reply. Blocking: every port method is, and the
    /// daemon calls them in `spawn_blocking`; never call this on a tokio
    /// worker.
    pub fn call(&self, agent: &AgentId, op: LinkOp) -> Result<LinkResult, LinkError> {
        let (tx, rx) = sync_channel(1);
        let (sender, pending, id) = {
            let conns = lock(&self.conns);
            let c = conns.get(agent).ok_or(LinkError::Down)?;
            let id = c.next_id.fetch_add(1, Ordering::SeqCst);
            lock(&c.pending).insert(id, tx);
            (c.tx.clone(), c.pending.clone(), id)
        };
        if sender.send(LinkRequest { id, op }).is_err() {
            lock(&pending).remove(&id);
            return Err(LinkError::Down);
        }
        match rx.recv_timeout(LINK_CALL_TIMEOUT) {
            Ok(LinkResult::Failed { failure }) => Err(LinkError::Failed(failure)),
            Ok(r) => Ok(r),
            Err(RecvTimeoutError::Timeout) => {
                lock(&pending).remove(&id);
                Err(LinkError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(LinkError::Down),
        }
    }

    /// The route's upgrade body: registers the link, pumps frames both
    /// ways, pings, and on the socket's end tells the daemon.
    pub async fn serve(
        self: Arc<Self>,
        daemon: Arc<Daemon>,
        agent: AgentId,
        mut socket: WebSocket,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel::<LinkRequest>();
        let pending: Pending = Arc::default();
        let epoch = self.register(&agent, tx, pending.clone());
        tracing::info!(agent = %agent, "sidecar linked");
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await; // the first tick is immediate
        loop {
            tokio::select! {
                req = rx.recv() => match req {
                    Some(req) => {
                        let Ok(json) = serde_json::to_string(&req) else { break };
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<SidecarFrame>(text.as_str()) {
                        Ok(SidecarFrame::Status(status)) => {
                            self.note_status(&agent, epoch, &status);
                            daemon.link_status(&agent, status).await;
                        }
                        Ok(SidecarFrame::Reply(reply)) => {
                            if let Some(tx) = lock(&pending).remove(&reply.id) {
                                // the caller may have timed out and gone
                                let _ = tx.try_send(reply.result);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(agent = %agent, "link: not a sidecar frame: {e}");
                            let _ = socket.send(Message::Close(Some(CloseFrame {
                                code: crate::attach::CLOSE_UNSUPPORTED,
                                reason: "expected a sidecar frame".into(),
                            }))).await;
                            break;
                        }
                    },
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
            }
        }
        self.unregister(&agent, epoch);
        // Calls still in flight fail with `Down` now, not at the timeout.
        // After `unregister`: no call can find this connection any more.
        lock(&pending).clear();
        // a replaced connection is not a link loss
        if !self.linked(&agent) {
            tracing::info!(agent = %agent, "sidecar link closed");
            daemon.link_down(&agent).await;
        }
    }
}

fn runner_err(agent: &AgentId, e: LinkError) -> RunnerError {
    RunnerError::Link {
        id: agent.to_string(),
        message: e.to_string(),
    }
}

fn workspace_err(agent: &AgentId, e: LinkError) -> WorkspaceError {
    match e {
        LinkError::Failed(LinkFailure { reason, message }) => match reason {
            FailureKind::Missing => WorkspaceError::Missing(message),
            FailureKind::NoSuchPath => WorkspaceError::NoSuchPath,
            FailureKind::InvalidPath => WorkspaceError::InvalidPath(message),
            FailureKind::NotAFile => WorkspaceError::NotAFile,
            FailureKind::NotADirectory => WorkspaceError::NotADirectory,
            FailureKind::TooLarge { limit } => WorkspaceError::TooLarge { limit },
            FailureKind::Tool => WorkspaceError::Tool {
                id: agent.to_string(),
                subcommand: "link".into(),
                args: vec![],
                stderr: message,
            },
            FailureKind::Filter => WorkspaceError::Filter { key: message },
            FailureKind::Io | FailureKind::Runner => WorkspaceError::Io {
                path: PathBuf::from("link"),
                message,
            },
        },
        other => WorkspaceError::Io {
            path: PathBuf::from("link"),
            message: format!("{agent}: {other}"),
        },
    }
}

const NOT_THE_DAEMONS: &str =
    "the daemon neither starts nor stops agents in kubernetes mode; the sidecar does (Spec O §7.2)";

impl AgentRunner for LinkHub {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        Err(RunnerError::Link {
            id: crew.to_string(),
            message: NOT_THE_DAEMONS.into(),
        })
    }
    fn ensure_agent(&self, agent: &AgentId, _plan: &LaunchPlan) -> Result<(), RunnerError> {
        Err(runner_err(
            agent,
            LinkError::Failed(LinkFailure {
                reason: FailureKind::Runner,
                message: NOT_THE_DAEMONS.into(),
            }),
        ))
    }
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        self.ensure_agent(
            agent,
            &LaunchPlan {
                cwd: PathBuf::new(),
                env: Default::default(),
                argv: vec![],
                script: PathBuf::new(),
            },
        )
    }
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.ensure_crew(crew)
    }
    /// Every linked agent of the fleet, from its last `status` frame: a pid
    /// is `Running`, no pid is `Exited`; an agent without a link is absent.
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError> {
        let mut out = ObservedState::default();
        for (id, c) in lock(&self.conns).iter() {
            if id.fleet != *fleet {
                continue;
            }
            let state = match c.last.as_ref().and_then(|s| s.pid) {
                Some(pid) => ProcessState::Running { pid },
                None => ProcessState::Exited { code: None },
            };
            out.set(id, state);
        }
        Ok(out)
    }
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        self.call(
            agent,
            LinkOp::SendText {
                text: text.to_string(),
                submit,
            },
        )
        .map(|_| ())
        .map_err(|e| runner_err(agent, e))
    }
    fn send_keys(
        &self,
        agent: &AgentId,
        steps: &[balerix_api::KeyStep],
        delay: Duration,
    ) -> Result<(), RunnerError> {
        self.call(
            agent,
            LinkOp::SendKeys {
                steps: steps.to_vec(),
                delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            },
        )
        .map(|_| ())
        .map_err(|e| runner_err(agent, e))
    }
    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError> {
        // Task 8 replaces this body with the second socket.
        Err(runner_err(agent, LinkError::Unexpected("attach")))
    }
}

impl WorkspaceReader for LinkHub {
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        match self.call(
            agent,
            LinkOp::WorkspaceDiff {
                base_ref: base_ref.into(),
            },
        ) {
            Ok(LinkResult::Diff { diff }) => Ok(diff),
            Ok(_) => Err(workspace_err(
                agent,
                LinkError::Unexpected("workspace.diff"),
            )),
            Err(e) => Err(workspace_err(agent, e)),
        }
    }
    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError> {
        match self.call(agent, LinkOp::WorkspaceFile { path: path.into() }) {
            Ok(LinkResult::File { bytes }) => Ok(bytes),
            Ok(_) => Err(workspace_err(
                agent,
                LinkError::Unexpected("workspace.file"),
            )),
            Err(e) => Err(workspace_err(agent, e)),
        }
    }
    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError> {
        match self.call(agent, LinkOp::WorkspaceTree { path: path.into() }) {
            Ok(LinkResult::Tree { tree }) => Ok(tree),
            Ok(_) => Err(workspace_err(
                agent,
                LinkError::Unexpected("workspace.tree"),
            )),
            Err(e) => Err(workspace_err(agent, e)),
        }
    }
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        match self.call(
            agent,
            LinkOp::WorkspaceVersion {
                base_ref: base_ref.into(),
            },
        ) {
            Ok(LinkResult::Version { version }) => Ok(version),
            Ok(_) => Err(workspace_err(
                agent,
                LinkError::Unexpected("workspace.version"),
            )),
            Err(e) => Err(workspace_err(agent, e)),
        }
    }
}

/// `GET /v1/agents/{fleet}/{crew}/{agent}/link`: the sidecar's one socket.
/// Authenticated like the hook route (401 for a bad token or an unknown
/// agent alike), then the protocol header, then the upgrade.
pub(crate) async fn link(
    State(state): State<AppState>,
    path: Result<Path<(String, String, String)>, PathRejection>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(hub) = state.daemon.kube().cloned() else {
        return ApiError::new(StatusCode::NOT_FOUND, "not a daemon in kubernetes mode")
            .into_response();
    };
    let Path((fleet, crew, agent)) = match path {
        Ok(p) => p,
        Err(e) => return ApiError::new(e.status(), e.body_text()).into_response(),
    };
    let unauthorized =
        || ApiError::new(StatusCode::UNAUTHORIZED, "unknown agent or bad secret").into_response();
    let Ok(id) = format!("{fleet}/{crew}/{agent}").parse::<AgentId>() else {
        return unauthorized();
    };
    let Some(token) = bearer(&headers) else {
        return unauthorized();
    };
    if !state.daemon.verify_secret(&id, token).await {
        return unauthorized();
    }
    let got = headers
        .get(LINK_PROTOCOL_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u32>().ok());
    if got != Some(LINK_PROTOCOL) {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "{LINK_PROTOCOL_HEADER}: this daemon speaks link protocol {LINK_PROTOCOL}, got {}",
                got.map_or("nothing".to_string(), |v| v.to_string())
            ),
        )
        .into_response();
    }
    let daemon = state.daemon.clone();
    ws.on_upgrade(move |socket| hub.serve(daemon, id, socket))
}
