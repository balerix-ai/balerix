//! The plugin → daemon half (plugins spec §4.1): one method per route,
//! bearer from `Env`, loopback only.

use std::fmt;
use std::time::Duration;

use axum::http::HeaderValue;
use balerix_api::{
    ErrorBody, FleetRecord, HelloRequest, HelloResponse, KvKeys, PLUGIN_PROTOCOL, PluginAction,
    ResizeFrame, WorkspaceDiff, WorkspaceTree, WorkspaceVersion,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::{Env, SdkError};

const TIMEOUT: Duration = Duration::from_secs(10);
/// `workspace_diff` alone: a first diff of a large repository can outlast
/// the client's 10 s.
const DIFF_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Host {
    env: Env,
    http: reqwest::Client,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").field("env", &self.env).finish()
    }
}

/// Percent-encodes everything the predicate does not call safe
/// (`reqwest`'s `query` feature is not enabled — plugins spec §16.1: no
/// extra dependency weight for it).
fn encode(s: &str, safe: impl Fn(u8) -> bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if safe(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'~' | b'-')
}

/// Everything outside `[A-Za-z0-9._~/-]`, for building query strings.
fn urlencode(s: &str) -> String {
    encode(s, |b| unreserved(b) || matches!(b, b'.' | b'/'))
}

/// A kv key as one path segment: `.` and `/` are escaped too. The URL
/// parser resolves `.` and `..` segments (their `%2e` spellings
/// included) while it builds the request, so an unescaped `../x` would
/// leave for a different route instead of collecting the daemon's
/// `kv: invalid key` (plugins spec §4.1).
fn path_encode(s: &str) -> String {
    encode(s, unreserved)
}

impl Host {
    pub fn new(env: Env) -> Result<Self, SdkError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        Ok(Self { env, http })
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/plugin-host/{path}", self.env.api_url)
    }

    /// Sends with the bearer; `Ok((status, bytes))` for any status.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>), SdkError> {
        let resp = req
            .bearer_auth(&self.env.token)
            .send()
            .await
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        Ok((status, bytes.to_vec()))
    }

    fn status_error(status: u16, bytes: &[u8]) -> SdkError {
        let text = String::from_utf8_lossy(bytes).trim().to_string();
        let message = serde_json::from_slice::<ErrorBody>(bytes)
            .map(|e| e.error)
            .unwrap_or(text);
        SdkError::Status { status, message }
    }

    /// 2xx → parsed body; anything else → `Status`.
    async fn json<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, SdkError> {
        let (status, bytes) = self.send(req).await?;
        if !(200..300).contains(&status) {
            return Err(Self::status_error(status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|e| SdkError::Transport(format!("bad reply: {e}")))
    }

    /// `POST /v1/plugin-host/hello` (plugins spec §4.1): announces the
    /// plugin's version and listen address; the daemon marks it `Ready`
    /// and answers with the daemon-level config.
    pub async fn hello(&self, version: &str, listen: &str) -> Result<HelloResponse, SdkError> {
        let req = HelloRequest {
            name: self.env.name.clone(),
            version: version.to_string(),
            protocol: PLUGIN_PROTOCOL,
            listen: listen.to_string(),
        };
        self.json(self.http.post(self.url("hello")).json(&req))
            .await
    }

    pub async fn fleets(&self) -> Result<Vec<FleetRecord>, SdkError> {
        self.json(self.http.get(self.url("fleets"))).await
    }

    pub async fn fleet(&self, name: &str) -> Result<Option<FleetRecord>, SdkError> {
        let (status, bytes) = self
            .send(self.http.get(self.url(&format!("fleets/{name}"))))
            .await?;
        match status {
            200..=299 => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| SdkError::Transport(format!("bad reply: {e}"))),
            404 => Ok(None),
            _ => Err(Self::status_error(status, &bytes)),
        }
    }

    pub async fn action(&self, agent: &str, action: &PluginAction) -> Result<(), SdkError> {
        self.json::<serde_json::Value>(
            self.http
                .post(self.url(&format!("agents/{agent}/actions")))
                .json(action),
        )
        .await
        .map(|_| ())
    }

    pub async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, SdkError> {
        let (status, bytes) = self
            .send(self.http.get(self.url(&format!("kv/{}", path_encode(key)))))
            .await?;
        match status {
            200..=299 => Ok(Some(bytes)),
            404 => Ok(None),
            _ => Err(Self::status_error(status, &bytes)),
        }
    }

    pub async fn kv_put(&self, key: &str, bytes: &[u8], secret: bool) -> Result<(), SdkError> {
        let req = self
            .http
            .put(self.url(&format!("kv/{}?secret={secret}", path_encode(key))))
            .header("content-type", "application/octet-stream")
            .body(bytes.to_vec());
        self.json::<serde_json::Value>(req).await.map(|_| ())
    }

    pub async fn kv_delete(&self, key: &str) -> Result<(), SdkError> {
        self.json::<serde_json::Value>(
            self.http
                .delete(self.url(&format!("kv/{}", path_encode(key)))),
        )
        .await
        .map(|_| ())
    }

    pub async fn kv_list(&self, prefix: &str) -> Result<Vec<String>, SdkError> {
        let keys: KvKeys = self
            .json(
                self.http
                    .get(self.url(&format!("kv?prefix={}", urlencode(prefix)))),
            )
            .await?;
        Ok(keys.keys)
    }

    /// `GET agents/{id}/workspace/diff` (Spec C §2.2): the agent's
    /// worktree against the crew's base. Needs `workspace` and an active
    /// pair; 404 `no workspace for agent …` before the worktree exists.
    pub async fn workspace_diff(&self, agent: &str) -> Result<WorkspaceDiff, SdkError> {
        self.json(
            self.http
                .get(self.url(&format!("agents/{agent}/workspace/diff")))
                .timeout(DIFF_TIMEOUT),
        )
        .await
    }

    /// `GET agents/{id}/workspace/file?path=`: the bytes, `None` when the
    /// worktree exists and the path does not (`no such path`); every
    /// other refusal — no worktree, not a file, over 1 MiB, a bad path —
    /// is the daemon's status and message.
    pub async fn workspace_file(
        &self,
        agent: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, SdkError> {
        let (status, bytes) = self
            .send(self.http.get(self.url(&format!(
                "agents/{agent}/workspace/file?path={}",
                urlencode(path)
            ))))
            .await?;
        match status {
            200..=299 => Ok(Some(bytes)),
            404 => match Self::status_error(status, &bytes) {
                SdkError::Status { message, .. } if message == "no such path" => Ok(None),
                e => Err(e),
            },
            _ => Err(Self::status_error(status, &bytes)),
        }
    }

    /// `GET agents/{id}/workspace/tree?path=`: one directory listing; the
    /// empty path is the root.
    pub async fn workspace_tree(&self, agent: &str, path: &str) -> Result<WorkspaceTree, SdkError> {
        self.json(self.http.get(self.url(&format!(
            "agents/{agent}/workspace/tree?path={}",
            urlencode(path)
        ))))
        .await
    }

    /// `GET agents/{id}/workspace/version` (Spec D §2.2): a cheap
    /// fingerprint of the worktree; equal values mean `workspace_diff`
    /// would answer the same. 404 `no workspace for agent …` before the
    /// worktree exists.
    pub async fn workspace_version(&self, agent: &str) -> Result<WorkspaceVersion, SdkError> {
        self.json(
            self.http
                .get(self.url(&format!("agents/{agent}/workspace/version"))),
        )
        .await
    }

    /// `ws://` twin of `url`: the streams of §18.4.
    fn ws_url(&self, path: &str) -> String {
        self.url(path).replacen("http://", "ws://", 1)
    }

    /// Opens one of the daemon's WebSocket routes with the bearer; a
    /// refused handshake is the daemon's status and error, as for HTTP.
    async fn connect(&self, path: &str) -> Result<Socket, SdkError> {
        let mut req = self
            .ws_url(path)
            .into_client_request()
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", self.env.token))
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        req.headers_mut().insert("authorization", bearer);
        match connect_async(req).await {
            Ok((socket, _)) => Ok(socket),
            Err(tungstenite::Error::Http(resp)) => {
                let status = resp.status().as_u16();
                let body = resp.body().clone().unwrap_or_default();
                Err(Self::status_error(status, &body))
            }
            Err(e) => Err(SdkError::Transport(e.to_string())),
        }
    }

    /// `GET agents/{id}/attach` (WS): a terminal on the agent's window
    /// (plugin-protocol §3 "Streams"). Needs the `attach` capability and
    /// an active pair.
    pub async fn attach(&self, agent: &str) -> Result<Attach, SdkError> {
        let (tx, rx) = self
            .connect(&format!("agents/{agent}/attach"))
            .await?
            .split();
        Ok(Attach {
            rx: AttachRead { rx, closed: None },
            tx: AttachWrite { tx },
        })
    }

    /// `GET fleets/watch` (WS): the complete fleets list on every change.
    /// Connects lazily and reconnects forever; drop it to stop.
    pub fn watch_fleets(&self) -> FleetWatch {
        FleetWatch {
            host: self.clone(),
            socket: None,
            backoff: BACKOFF_MIN,
            retry_at: None,
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

fn transport(e: tungstenite::Error) -> SdkError {
    SdkError::Transport(e.to_string())
}

/// Why the daemon ended an attach (plugins spec §18.4): 1000 when the
/// window closed, 1003 for a text frame that was not a resize, 1011 when
/// the runner's side failed; 1006 with the transport's error when the
/// socket broke without a close frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReason {
    pub code: u16,
    pub reason: String,
}

/// The reading half of an attach: terminal output.
pub struct AttachRead {
    rx: SplitStream<Socket>,
    closed: Option<CloseReason>,
}

impl AttachRead {
    /// The next chunk of output; `None` once the daemon closed the stream
    /// (`close_reason` then says why).
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        if self.closed.is_some() {
            return None;
        }
        loop {
            match self.rx.next().await {
                Some(Ok(Message::Binary(bytes))) => return Some(bytes.to_vec()),
                Some(Ok(Message::Close(frame))) => {
                    self.closed = Some(match frame {
                        Some(f) => CloseReason {
                            code: u16::from(f.code),
                            reason: f.reason.to_string(),
                        },
                        None => CloseReason {
                            code: 1005,
                            reason: String::new(),
                        },
                    });
                    return None;
                }
                Some(Err(e)) => {
                    self.closed = Some(CloseReason {
                        code: 1006,
                        reason: e.to_string(),
                    });
                    return None;
                }
                None => {
                    self.closed = Some(CloseReason {
                        code: 1006,
                        reason: "the socket ended without a close frame".into(),
                    });
                    return None;
                }
                Some(Ok(_)) => {}
            }
        }
    }

    /// Why `read` returned `None`; `None` while the stream is open.
    pub fn close_reason(&self) -> Option<&CloseReason> {
        self.closed.as_ref()
    }
}

/// The writing half of an attach: keystrokes and resizes.
pub struct AttachWrite {
    tx: SplitSink<Socket, Message>,
}

impl AttachWrite {
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), SdkError> {
        self.tx
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .map_err(transport)
    }

    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), SdkError> {
        let text = serde_json::to_string(&ResizeFrame::new(cols, rows))
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        self.tx
            .send(Message::Text(text.into()))
            .await
            .map_err(transport)
    }

    pub async fn close(mut self) {
        let _ = self.tx.send(Message::Close(None)).await;
        let _ = self.tx.close().await;
    }
}

/// A terminal on an agent's window; `split` for a bridge that reads and
/// writes concurrently.
pub struct Attach {
    rx: AttachRead,
    tx: AttachWrite,
}

impl fmt::Debug for Attach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Attach")
    }
}

impl Attach {
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        self.rx.read().await
    }
    /// Why `read` returned `None`; `None` while the stream is open.
    pub fn close_reason(&self) -> Option<&CloseReason> {
        self.rx.close_reason()
    }
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), SdkError> {
        self.tx.write(bytes).await
    }
    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), SdkError> {
        self.tx.resize(cols, rows).await
    }
    pub async fn close(self) {
        self.tx.close().await;
    }
    pub fn split(self) -> (AttachRead, AttachWrite) {
        (self.rx, self.tx)
    }
}

/// What one read of the watch socket produced.
enum Frame {
    /// A complete fleets list.
    List(Vec<FleetRecord>),
    /// A frame that is not a list (a ping, a pong): keep reading the
    /// same socket.
    Skipped,
    /// A text frame that is not a list: the daemon speaks a dialect this
    /// SDK does not; the socket is dropped and reconnected behind the
    /// backoff rather than read again at once.
    Bad(String),
    /// The socket is finished; a new one has to be opened.
    Closed,
}

/// `fleets/watch` as a stream of complete lists (§18.4). Every frame is
/// the whole list, so a reconnect simply yields it again.
pub struct FleetWatch {
    host: Host,
    socket: Option<Socket>,
    backoff: Duration,
    /// The deadline a `wait` in progress owes, kept across a cancelled
    /// `next` so a caller that gives up on the sleep does not reconnect
    /// unthrottled.
    retry_at: Option<tokio::time::Instant>,
}

impl FleetWatch {
    /// The next complete list. Never ends: a refused handshake and a lost
    /// socket are both retried behind the same 1–10 s backoff — a daemon
    /// that accepts the handshake and then ends the stream at once must
    /// not turn into a connect/close storm — and only a list that
    /// actually arrives resets it (pings do not). Drop the watch to stop.
    pub async fn next(&mut self) -> Vec<FleetRecord> {
        loop {
            if self.socket.is_none() {
                // a wait a cancelled `next` left unserved comes first
                if self.retry_at.is_some() {
                    self.wait().await;
                }
                match self.host.connect("fleets/watch").await {
                    Ok(s) => self.socket = Some(s),
                    Err(e) => {
                        tracing::warn!(
                            plugin = %self.host.env.name,
                            "fleets/watch: {e}; retrying in {:?}",
                            self.backoff
                        );
                        self.wait().await;
                        continue;
                    }
                }
            }
            match self.frame().await {
                Frame::List(list) => {
                    self.backoff = BACKOFF_MIN;
                    return list;
                }
                Frame::Skipped => {}
                Frame::Bad(e) => {
                    tracing::warn!(
                        plugin = %self.host.env.name,
                        "fleets/watch: bad frame ({e}); reconnecting in {:?}",
                        self.backoff
                    );
                    self.socket = None;
                    self.wait().await;
                }
                Frame::Closed => {
                    self.socket = None;
                    self.wait().await;
                }
            }
        }
    }

    /// One frame from the open socket. `Closed` for a socket that is
    /// gone — including the `None` the loop above has just ruled out, so
    /// an impossible state waits like any other loss instead of spinning.
    async fn frame(&mut self) -> Frame {
        let Some(socket) = self.socket.as_mut() else {
            return Frame::Closed;
        };
        match socket.next().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                Ok(list) => Frame::List(list),
                Err(e) => Frame::Bad(e.to_string()),
            },
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => Frame::Closed,
            Some(Ok(_)) => Frame::Skipped,
        }
    }

    /// Waits out the current backoff. The deadline is fixed, and the
    /// backoff doubled (up to `BACKOFF_MAX`), the moment the wait starts:
    /// a `next` cancelled mid-sleep resumes the same deadline instead of
    /// reconnecting at once.
    async fn wait(&mut self) {
        let until = match self.retry_at {
            Some(t) => t,
            None => {
                let t = tokio::time::Instant::now() + self.backoff;
                self.retry_at = Some(t);
                self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
                t
            }
        };
        tokio::time::sleep_until(until).await;
        self.retry_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeHost;
    use balerix_api::{FleetRecord, FleetSpec, PluginAction};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn record(name: &str) -> FleetRecord {
        FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
            ..Default::default()
        })
    }

    #[test]
    fn urlencode_escapes_everything_outside_the_safe_set() {
        assert_eq!(urlencode("a b/c"), "a%20b/c");
    }

    #[test]
    fn a_kv_key_travels_as_one_segment_so_dots_reach_the_daemon() {
        assert_eq!(path_encode("state/f/c/a"), "state%2Ff%2Fc%2Fa");
        let url: reqwest::Url = format!("http://h/v1/plugin-host/kv/{}", path_encode("../x"))
            .parse()
            .unwrap();
        assert_eq!(url.path(), "/v1/plugin-host/kv/%2E%2E%2Fx");
    }

    #[tokio::test]
    #[allow(clippy::bool_assert_comparison)]
    async fn every_route_round_trips_through_the_fake_host() {
        let fake =
            FakeHost::start("tok", json!({ "greeting": "hi" }), vec![record("payments")]).await;
        let host = Host::new(fake.env("flow", std::path::Path::new("/s"))).unwrap();
        let hello = host.hello("0.1.0", "127.0.0.1:4321").await.unwrap();
        assert_eq!(hello.config["greeting"], "hi");
        let seen = fake.hellos();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            (
                seen[0].name.as_str(),
                seen[0].protocol,
                seen[0].listen.as_str()
            ),
            ("flow", 1, "127.0.0.1:4321")
        );

        let fleets = host.fleets().await.unwrap();
        assert_eq!(fleets.len(), 1);
        assert_eq!(fleets[0].name(), "payments");
        assert_eq!(
            host.fleet("payments").await.unwrap().unwrap().name(),
            "payments"
        );
        assert_eq!(host.fleet("nope").await.unwrap(), None);

        host.action("payments/backend/bob", &PluginAction::Restart)
            .await
            .unwrap();
        assert_eq!(
            fake.actions(),
            vec![("payments/backend/bob".to_string(), PluginAction::Restart)]
        );

        assert_eq!(host.kv_get("state/x").await.unwrap(), None);
        host.kv_put("state/x", b"working", false).await.unwrap();
        host.kv_put("secret/t", b"\x00\x01", true).await.unwrap();
        assert_eq!(
            host.kv_get("state/x").await.unwrap().as_deref(),
            Some(&b"working"[..])
        );
        assert_eq!(
            host.kv_get("secret/t").await.unwrap().as_deref(),
            Some(&b"\x00\x01"[..])
        );
        assert_eq!(fake.kv()["secret/t"].1, true, "the secret flag was sent");
        assert_eq!(
            host.kv_list("state/").await.unwrap(),
            vec!["state/x".to_string()]
        );
        assert_eq!(host.kv_list("").await.unwrap().len(), 2);
        host.kv_delete("state/x").await.unwrap();
        assert_eq!(host.kv_get("state/x").await.unwrap(), None);
    }

    #[tokio::test]
    async fn attach_streams_bytes_and_resizes_through_the_fake_host() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut a = host.attach("payments/backend/bob").await.unwrap();
        a.write(b"ls\n").await.unwrap();
        assert_eq!(
            a.read().await.as_deref(),
            Some(&b"ls\n"[..]),
            "the fake echoes"
        );
        a.resize(120, 40).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while fake.resizes().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            fake.resizes(),
            vec![(
                "payments/backend/bob".to_string(),
                json!({ "resize": { "cols": 120, "rows": 40 } })
            )]
        );
        let (mut rd, mut wr) = a.split();
        wr.write(b"x").await.unwrap();
        assert_eq!(rd.read().await.as_deref(), Some(&b"x"[..]));
        wr.close().await;
        assert_eq!(rd.read().await, None, "closed");
        assert_eq!(fake.attaches(), vec!["payments/backend/bob".to_string()]);
        let mut env = fake.env("web", std::path::Path::new("/s"));
        env.token = "wrong".into();
        let e = Host::new(env).unwrap().attach("f/c/a").await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "daemon: HTTP 401: unknown plugin or bad token"
        );
    }

    #[tokio::test]
    async fn watch_fleets_yields_the_current_list_then_every_change_and_reconnects() {
        let fake = FakeHost::start("tok", json!({}), vec![record("payments")]).await;
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut watch = host.watch_fleets();
        let first = watch.next().await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name(), "payments");
        fake.set_fleets(vec![record("payments"), record("billing")]);
        let second = watch.next().await;
        assert_eq!(second.len(), 2);
        fake.set_fleets(vec![]);
        assert!(watch.next().await.is_empty());
        // a dropped socket: the next call reconnects and yields the list again
        fake.drop_watchers();
        fake.set_fleets(vec![record("again")]);
        let after = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
            .await
            .expect("reconnected");
        assert_eq!(after[0].name(), "again");
    }

    /// A daemon that accepts the handshake and then ends the stream at
    /// once — `watch.rs` returns without a frame when a list will not
    /// serialize — must not be reconnected in a tight loop: the backoff
    /// is the throttle for a lost socket, not only for a refused one.
    #[tokio::test]
    async fn a_socket_lost_after_the_handshake_waits_out_the_backoff() {
        let fake =
            std::sync::Arc::new(FakeHost::start("tok", json!({}), vec![record("payments")]).await);
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut watch = host.watch_fleets();
        assert_eq!(
            watch.next().await.len(),
            1,
            "the first frame arrives at once"
        );
        // Close every watch socket as fast as they open: without the
        // backoff this is an unthrottled connect/close storm, and each
        // reconnect would hand `next` the list again within milliseconds.
        let closing = tokio::spawn({
            let fake = std::sync::Arc::clone(&fake);
            async move {
                loop {
                    fake.drop_watchers();
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
        });
        let quick = tokio::time::timeout(BACKOFF_MIN / 2, watch.next()).await;
        assert!(
            quick.is_err(),
            "a lost socket was reconnected inside the {BACKOFF_MIN:?} floor: {quick:?}"
        );
        closing.abort();
        let after = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
            .await
            .expect("reconnected once the daemon stopped closing");
        assert_eq!(after.len(), 1);
    }

    /// A frame the SDK cannot parse is a daemon speaking another dialect:
    /// the socket is dropped and reconnected behind the backoff, not read
    /// again in a loop that logs on every turn.
    #[tokio::test]
    async fn a_bad_watch_frame_reconnects_behind_the_backoff() {
        let fake = FakeHost::start("tok", json!({}), vec![record("payments")]).await;
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut watch = host.watch_fleets();
        assert_eq!(watch.next().await.len(), 1);
        assert_eq!(fake.watch_connections(), 1);
        fake.inject_watch_text("not a fleets list");
        let quick = tokio::time::timeout(BACKOFF_MIN / 2, watch.next()).await;
        assert!(quick.is_err(), "reconnected inside the backoff: {quick:?}");
        let after = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
            .await
            .expect("reconnected after the backoff");
        assert_eq!(after.len(), 1);
        assert_eq!(fake.watch_connections(), 2, "one reconnect, not a storm");
    }

    /// A `select!`-driven caller can cancel `next()` while it sleeps out
    /// the backoff; the next call must still owe what is left of it.
    #[tokio::test]
    async fn a_cancelled_wait_still_owes_its_backoff() {
        let fake = FakeHost::start("tok", json!({}), vec![record("payments")]).await;
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut watch = host.watch_fleets();
        assert_eq!(watch.next().await.len(), 1);
        fake.drop_watchers();
        let start = tokio::time::Instant::now();
        while start.elapsed() < BACKOFF_MIN / 2 {
            let cancelled =
                tokio::time::timeout(std::time::Duration::from_millis(20), watch.next()).await;
            assert!(cancelled.is_err(), "reconnected early: {cancelled:?}");
        }
        assert_eq!(
            fake.watch_connections(),
            1,
            "every cancelled call reconnected without waiting"
        );
        let after = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
            .await
            .expect("reconnected once the backoff was served");
        assert_eq!(after.len(), 1);
        assert_eq!(fake.watch_connections(), 2);
    }

    #[tokio::test]
    async fn an_attach_reader_reports_why_the_daemon_closed() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut a = host.attach("f/c/a").await.unwrap();
        a.write(b"hi").await.unwrap();
        assert_eq!(a.read().await.as_deref(), Some(&b"hi"[..]));
        assert!(a.close_reason().is_none(), "still open");
        fake.close_attaches(1011, "the terminal's writer failed");
        assert_eq!(a.read().await, None);
        let why = a.close_reason().expect("the daemon's close frame");
        assert_eq!(
            (why.code, why.reason.as_str()),
            (1011, "the terminal's writer failed")
        );
        assert_eq!(a.read().await, None, "stays closed");
    }

    #[tokio::test]
    async fn statuses_and_transport_failures_are_reported() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let mut env = fake.env("flow", std::path::Path::new("/s"));
        env.token = "wrong".into();
        let e = Host::new(env)
            .unwrap()
            .hello("0.1.0", "127.0.0.1:1")
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "daemon: HTTP 401: unknown plugin or bad token"
        );
        let mut env = fake.env("flow", std::path::Path::new("/s"));
        env.api_url = "http://127.0.0.1:1".into();
        let e = Host::new(env)
            .unwrap()
            .hello("0.1.0", "127.0.0.1:1")
            .await
            .unwrap_err();
        assert!(matches!(e, SdkError::Transport(_)), "{e}");
        let mut env = fake.env("flow", std::path::Path::new("/s"));
        env.token = "s3cret-value".into();
        let dbg = format!("{:?}", Host::new(env).unwrap());
        assert!(
            !dbg.contains("s3cret-value") && dbg.contains("<redacted>"),
            "{dbg}"
        );
    }
}
