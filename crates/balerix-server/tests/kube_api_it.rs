#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §7.3: the operator's apply, the owner rule, readiness, and the
//! stopped set over the link.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use balerix_api::{
    AgentPhase, AgentSettings, AgentStatus, CrewSpec, FleetPhase, FleetSpec, Keep,
    LINK_PROTOCOL_HEADER, LinkOp, LinkReply, LinkRequest, LinkResult, LinkStatus, PluginAction,
    SidecarFrame,
};
use balerix_core::{AgentId, PassThrough};
use balerix_server::testing::Harness;
use balerix_server::{Daemon, router, serve};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn spec() -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        tools: BTreeMap::new(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/api".into(),
                git_ref: "main".into(),
                agents: BTreeMap::from([("a".to_string(), AgentSettings::default())]),
                ..CrewSpec::default()
            },
        )]),
    }
}

struct World {
    daemon: Arc<Daemon>,
    base: String,
    port: u16,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

impl World {
    /// A blocking request off the runtime, as `api_it` makes them.
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let token = token.map(str::to_string);
        let method = method.to_string();
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(5)))
                .http_status_as_error(false)
                .build()
                .into();
            let mut req = match method.as_str() {
                "GET" => agent.get(&url).force_send_body(),
                "POST" => agent.post(&url),
                "PUT" => agent.put(&url),
                "DELETE" => agent.delete(&url).force_send_body(),
                _ => unreachable!(),
            };
            if let Some(t) = token {
                req = req.header("Authorization", &format!("Bearer {t}"));
            }
            let mut resp = match body {
                Some(b) => req.send_json(&b).unwrap(),
                None => req.send_empty().unwrap(),
            };
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().unwrap();
            (
                status,
                serde_json::from_str(&text).unwrap_or(Value::String(text)),
            )
        })
        .await
        .unwrap()
    }
}

async fn world(h: &Harness) -> World {
    let dir = tempfile::tempdir().unwrap();
    let daemon = h.daemon(Arc::new(PassThrough), dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, router(daemon.clone()), async {
        let _ = rx.await;
    }));
    World {
        daemon,
        base: format!("http://127.0.0.1:{port}"),
        port,
        _dir: dir,
        stop: Some(stop),
    }
}

fn request(tokens: Option<Value>) -> Value {
    let mut v = json!({ "spec": serde_json::to_value(spec()).unwrap() });
    if let Some(t) = tokens {
        v["agent_tokens"] = t;
    }
    v
}

async fn wait_for(mut f: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !f().await {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tmux_daemon_refuses_agent_tokens() {
    let h = Harness::new(Duration::from_secs(3600));
    let w = world(&h).await;
    let (status, body) = w
        .call(
            "PUT",
            "/v1/fleets/f",
            Some("admin-tok"),
            Some(request(Some(json!({ "f/c/a": TOKEN })))),
        )
        .await;
    assert_eq!(
        (status, body["error"].as_str().unwrap()),
        (
            400,
            "agent_tokens is accepted only by a daemon in kubernetes mode"
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operators_put_records_a_kubernetes_fleet_the_cli_cannot_touch() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let id: AgentId = "f/c/a".parse().unwrap();

    let (status, body) = w
        .call(
            "PUT",
            "/v1/fleets/f",
            Some("admin-tok"),
            Some(request(Some(json!({ "f/c/a": TOKEN })))),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["owner"], "kubernetes");
    assert_eq!(body["status"]["agents"]["f/c/a"]["phase"], "pending");
    assert_eq!(w.daemon.hook_secret(&id).await.as_deref(), Some(TOKEN));

    // the CLI's shapes answer 409
    let managed = "fleet f is managed by kubernetes; change it through its Fleet object";
    let (status, body) = w
        .call(
            "PUT",
            "/v1/fleets/f",
            Some("admin-tok"),
            Some(request(None)),
        )
        .await;
    assert_eq!((status, body["error"].as_str().unwrap()), (409, managed));
    let (status, body) = w
        .call("POST", "/v1/fleets", Some("admin-tok"), Some(request(None)))
        .await;
    assert_eq!((status, body["error"].as_str().unwrap()), (409, managed));
    let (status, body) = w
        .call(
            "DELETE",
            "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false&force=false",
            Some("admin-tok"),
            None,
        )
        .await;
    assert_eq!((status, body["error"].as_str().unwrap()), (409, managed));

    // a second operator PUT is an upsert that keeps the token
    let (status, _) = w
        .call(
            "PUT",
            "/v1/fleets/f",
            Some("admin-tok"),
            Some(request(Some(json!({ "f/c/a": TOKEN })))),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(w.daemon.hook_secret(&id).await.as_deref(), Some(TOKEN));

    // the token rules
    let (status, body) = w
        .call(
            "PUT",
            "/v1/fleets/f",
            Some("admin-tok"),
            Some(request(Some(json!({})))),
        )
        .await;
    assert_eq!(
        (status, body["error"].as_str().unwrap()),
        (400, "agent_tokens: no token for f/c/a")
    );
    let (status, body) = w
        .call(
            "PUT",
            "/v1/fleets/f",
            Some("admin-tok"),
            Some(request(Some(json!({ "f/c/a": "short" })))),
        )
        .await;
    assert_eq!(
        (status, body["error"].as_str().unwrap()),
        (400, "agent_tokens.f/c/a: a token is at least 32 characters")
    );
    let (status, body) = w
        .call(
            "PUT",
            "/v1/fleets/f",
            Some("admin-tok"),
            Some(request(Some(json!({ "f/c/a": TOKEN, "f/c/zed": TOKEN })))),
        )
        .await;
    assert_eq!(
        (status, body["error"].as_str().unwrap()),
        (400, "agent_tokens: f/c/zed is not an agent of the fleet")
    );
    let (status, body) = w
        .call(
            "POST",
            "/v1/fleets",
            Some("admin-tok"),
            Some(request(Some(json!({ "f/c/a": TOKEN })))),
        )
        .await;
    assert_eq!(
        (status, body["error"].as_str().unwrap()),
        (400, "agent_tokens: use PUT /v1/fleets/{name}")
    );

    // the operator's down is a forced one
    let (status, body) = w
        .call(
            "DELETE",
            "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=false&force=true",
            Some("admin-tok"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    wait_for(async || w.daemon.get(&"f".parse().unwrap()).await.unwrap().is_down()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readyz_follows_the_pool_channel() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    wait_for(async || w.call("GET", "/readyz", None, None).await.0 == 200).await;
    let (status, body) = w.call("GET", "/readyz", None, None).await;
    assert_eq!((status, body), (200, Value::String("ready".into())));
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn link(port: u16, token: &str) -> Ws {
    link_as(port, "f/c/a", token).await
}

async fn link_as(port: u16, agent: &str, token: &str) -> Ws {
    let mut req = format!("ws://127.0.0.1:{port}/v1/agents/{agent}/link")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    req.headers_mut()
        .insert(LINK_PROTOCOL_HEADER, "1".parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

fn status_frame(phase: AgentPhase) -> Message {
    Message::Text(
        serde_json::to_string(&SidecarFrame::Status(LinkStatus {
            status: AgentStatus {
                phase,
                ..AgentStatus::default()
            },
            pid: Some(56),
            hook_failures: 0,
        }))
        .unwrap()
        .into(),
    )
}

async fn next_request(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> LinkRequest {
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) => {}
            other => panic!("{other:?}"),
        }
    }
}

async fn reply_ok(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
) {
    ws.send(Message::Text(
        serde_json::to_string(&SidecarFrame::Reply(LinkReply {
            id,
            result: LinkResult::Ok,
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
}

/// Review Focus 5 and §16.4 over the link: `stop`/`restart` travel as
/// frames, and a status frame that disagrees with the stopped set is
/// corrected, which is how a sidecar that was away learns of a stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stopped_set_travels_over_the_link_and_is_reconciled_on_reconnect() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    w.daemon
        .apply_kube(
            &name,
            spec(),
            BTreeMap::from([("f/c/a".to_string(), TOKEN.to_string())]),
        )
        .await
        .unwrap();
    let mut ws = link(w.port, TOKEN).await;
    ws.send(status_frame(AgentPhase::Ready)).await.unwrap();
    wait_for(async || {
        w.daemon.get(&name).await.unwrap().status.agents["f/c/a"].phase == AgentPhase::Ready
    })
    .await;

    // a stop action: the frame goes out, the set is recorded
    let (d, i) = (w.daemon.clone(), id.clone());
    let action = tokio::spawn(async move { d.execute_action(&i, &PluginAction::Stop, None).await });
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Stop);
    reply_ok(&mut ws, req.id).await;
    action.await.unwrap().unwrap();
    assert!(w.daemon.get(&name).await.unwrap().stopped.contains("f/c/a"));
    // the sidecar reports Stopped: nothing more is sent
    ws.send(status_frame(AgentPhase::Stopped)).await.unwrap();
    wait_for(async || {
        w.daemon.get(&name).await.unwrap().status.agents["f/c/a"].phase == AgentPhase::Stopped
    })
    .await;

    // the link drops; a restart action lands with nobody listening
    ws.close(None).await.unwrap();
    wait_for(async || !w.daemon.kube().unwrap().linked(&id)).await;
    w.daemon
        .execute_action(&id, &PluginAction::Restart, None)
        .await
        .unwrap();
    assert!(!w.daemon.get(&name).await.unwrap().stopped.contains("f/c/a"));

    // the sidecar comes back still stopped: it is told to restart
    let mut ws = link(w.port, TOKEN).await;
    ws.send(status_frame(AgentPhase::Stopped)).await.unwrap();
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Restart);
    reply_ok(&mut ws, req.id).await;
    ws.send(status_frame(AgentPhase::Ready)).await.unwrap();

    // and the other way: a stop recorded while away
    ws.close(None).await.unwrap();
    wait_for(async || !w.daemon.kube().unwrap().linked(&id)).await;
    w.daemon
        .execute_action(&id, &PluginAction::Stop, None)
        .await
        .unwrap();
    let mut ws = link(w.port, TOKEN).await;
    ws.send(status_frame(AgentPhase::Ready)).await.unwrap();
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Stop);
    reply_ok(&mut ws, req.id).await;

    // down: a stop to every linked agent, then the fleet is Down and
    // nothing is sent for the Stopped report that follows
    let (d, n) = (w.daemon.clone(), name.clone());
    let down = tokio::spawn(async move {
        d.down_as(
            &n,
            Keep::default(),
            false,
            &balerix_server::Caller::Admin { force: true },
        )
        .await
    });
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Stop);
    reply_ok(&mut ws, req.id).await;
    let record = down.await.unwrap().unwrap();
    assert_eq!(record.status.phase, FleetPhase::Down);
    assert!(record.is_down());
    ws.send(status_frame(AgentPhase::Stopped)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // no request followed: the next frame the fake sees is a ping or nothing
    let record = w.daemon.get(&name).await.unwrap();
    assert!(record.is_down());
}

/// The reconnect rule extended to a downed fleet: a forced delete that
/// landed while the link was down still stops the agent once its sidecar
/// links again, and the report does not bring the fleet back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sidecar_that_links_after_a_down_is_told_to_stop() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    w.daemon
        .apply_kube(
            &name,
            spec(),
            BTreeMap::from([("f/c/a".to_string(), TOKEN.to_string())]),
        )
        .await
        .unwrap();
    let record = w
        .daemon
        .down_as(
            &name,
            Keep::default(),
            false,
            &balerix_server::Caller::Admin { force: true },
        )
        .await
        .unwrap();
    assert!(record.is_down());

    let mut ws = link(w.port, TOKEN).await;
    ws.send(status_frame(AgentPhase::Ready)).await.unwrap();
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Stop);
    reply_ok(&mut ws, req.id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let record = w.daemon.get(&name).await.unwrap();
    assert!(record.is_down());
    assert!(record.status.agents.is_empty());
}

/// No text frame (a request) reaches the fake within `wait`; pings are
/// the hub's and do not count.
async fn assert_no_request(ws: &mut Ws, wait: Duration) {
    let quiet = tokio::time::timeout(wait, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => return other,
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "a frame arrived: {quiet:?}");
}

/// The stopped set is reconciled on a link's first status frame only: on
/// a live link, the `Stopped` report of a restart's own stop can land
/// after the resume, and correcting it would send a second `restart`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_status_on_a_live_link_is_mirrored_not_corrected() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    let id: AgentId = "f/c/a".parse().unwrap();
    w.daemon
        .apply_kube(
            &name,
            spec(),
            BTreeMap::from([("f/c/a".to_string(), TOKEN.to_string())]),
        )
        .await
        .unwrap();
    let mut ws = link(w.port, TOKEN).await;
    ws.send(status_frame(AgentPhase::Ready)).await.unwrap();
    wait_for(async || {
        w.daemon.get(&name).await.unwrap().status.agents["f/c/a"].phase == AgentPhase::Ready
    })
    .await;

    let (d, i) = (w.daemon.clone(), id.clone());
    let action =
        tokio::spawn(async move { d.execute_action(&i, &PluginAction::Restart, None).await });
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Stop);
    reply_ok(&mut ws, req.id).await;
    let req = next_request(&mut ws).await;
    assert_eq!(req.op, LinkOp::Restart);
    reply_ok(&mut ws, req.id).await;
    action.await.unwrap().unwrap();

    // the stop's own report, late: mirrored, and nothing is sent
    ws.send(status_frame(AgentPhase::Stopped)).await.unwrap();
    wait_for(async || {
        w.daemon.get(&name).await.unwrap().status.agents["f/c/a"].phase == AgentPhase::Stopped
    })
    .await;
    assert_no_request(&mut ws, Duration::from_millis(500)).await;
}

/// `sync_plugins` downs a fleet whose plugin is not declared; the
/// operator is not a plugin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_sync_leaves_a_kubernetes_fleet_up() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    w.daemon
        .apply_kube(
            &name,
            spec(),
            BTreeMap::from([("f/c/a".to_string(), TOKEN.to_string())]),
        )
        .await
        .unwrap();
    let report = w.daemon.sync_plugins().await.unwrap();
    assert!(report.downed.is_empty(), "{report:?}");
    let record = w.daemon.get(&name).await.unwrap();
    assert_eq!(record.desired, balerix_api::Desired::Up);
}

/// An agent the operator's `PUT` drops leaves the status, and a status
/// frame its still-open link sends afterwards is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_agent_leaves_the_status_and_its_frames_are_ignored() {
    const TOKEN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world(&h).await;
    let name = "f".parse().unwrap();
    let mut two = spec();
    two.crews
        .get_mut("c")
        .unwrap()
        .agents
        .insert("b".to_string(), AgentSettings::default());
    w.daemon
        .apply_kube(
            &name,
            two,
            BTreeMap::from([
                ("f/c/a".to_string(), TOKEN.to_string()),
                ("f/c/b".to_string(), TOKEN_B.to_string()),
            ]),
        )
        .await
        .unwrap();
    let mut ws_b = link_as(w.port, "f/c/b", TOKEN_B).await;
    ws_b.send(status_frame(AgentPhase::Ready)).await.unwrap();
    wait_for(async || {
        w.daemon.get(&name).await.unwrap().status.agents["f/c/b"].phase == AgentPhase::Ready
    })
    .await;

    let record = w
        .daemon
        .apply_kube(
            &name,
            spec(),
            BTreeMap::from([("f/c/a".to_string(), TOKEN.to_string())]),
        )
        .await
        .unwrap();
    assert!(
        !record.status.agents.contains_key("f/c/b"),
        "{:?}",
        record.status.agents
    );

    ws_b.send(status_frame(AgentPhase::Ready)).await.unwrap();
    // a frame on the remaining agent's link is a barrier: the actor
    // handles its inbox in order
    let mut ws_a = link(w.port, TOKEN).await;
    ws_a.send(status_frame(AgentPhase::Ready)).await.unwrap();
    wait_for(async || {
        w.daemon.get(&name).await.unwrap().status.agents["f/c/a"].phase == AgentPhase::Ready
    })
    .await;
    let record = w.daemon.get(&name).await.unwrap();
    assert!(
        !record.status.agents.contains_key("f/c/b"),
        "{:?}",
        record.status.agents
    );
    assert_eq!(record.status.phase, FleetPhase::Ready);
}
