//! Plugins spec §18.4 through a real listener: `fleets/watch` frames on
//! every change, and `attach` bridged to the fake runner's echo PTY.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use balerix_api::{ActivationState, AgentSettings, CrewSpec, FleetRequest, FleetSpec, GitSettings};
use balerix_core::fakes::PtyFault;
use balerix_core::plugin_id;
use balerix_plugin_sdk::{Env, Host, Plugin, bind, run};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use support::{World, world};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A plugin with every method left at its default.
struct Silent;
impl Plugin for Silent {}

async fn token(w: &World, plugin: &str) -> String {
    w.daemon
        .hook_secret(&plugin_id(&plugin.parse().unwrap()))
        .await
        .unwrap()
}

fn ws_url(w: &World, path: &str) -> String {
    format!("{}{path}", w.api.base.replacen("http://", "ws://", 1))
}

async fn ws(url: &str, token: &str) -> Result<Socket, tungstenite::Error> {
    let mut req = url.into_client_request()?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    connect_async(req).await.map(|(s, _)| s)
}

fn status_of(e: &tungstenite::Error) -> Option<u16> {
    match e {
        tungstenite::Error::Http(r) => Some(r.status().as_u16()),
        _ => None,
    }
}

/// The refused handshake's response body, as text.
fn body_of(e: &tungstenite::Error) -> String {
    match e {
        tungstenite::Error::Http(r) => match r.body() {
            Some(b) => String::from_utf8_lossy(b).into_owned(),
            None => String::new(),
        },
        _ => String::new(),
    }
}

/// The next text frame, parsed, within five seconds.
async fn next_text(s: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match s.next().await.expect("socket open").unwrap() {
                Message::Text(t) => return serde_json::from_str(&t).unwrap(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    })
    .await
    .expect("a text frame")
}

/// Frames until one satisfies `pred` (the actor publishes several per apply).
async fn wait_frame(s: &mut Socket, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let v = next_text(s).await;
            if pred(&v) {
                return v;
            }
        }
    })
    .await
    .expect("the expected frame")
}

fn spec(agents: &[(&str, &[(&str, Value)])]) -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: agents
                    .iter()
                    .map(|(n, plugins)| {
                        let s = AgentSettings {
                            plugins: plugins
                                .iter()
                                .map(|(p, c)| (p.to_string(), c.clone()))
                                .collect(),
                            ..Default::default()
                        };
                        (n.to_string(), s)
                    })
                    .collect(),
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

/// Refuses `activate` for a config carrying `reject`, and stays refusing
/// afterwards — so an apply's rollback cannot re-activate the pair it had
/// already accepted either, and that pair's row is written `rejected`.
#[derive(Default)]
struct Rejecting {
    poisoned: Mutex<bool>,
}

impl Plugin for Rejecting {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let refuse = config.get("reject").is_some() || *self.poisoned.lock().unwrap();
        if refuse {
            *self.poisoned.lock().unwrap() = true;
            return Err(format!("rejected for {agent}"));
        }
        Ok(())
    }
}

/// Starts an SDK plugin under `name` and says hello with its token.
async fn start_plugin<P: Plugin + 'static>(w: &World, name: &str, plugin: Arc<P>) -> Host {
    let env = Env {
        api_url: w.api.base.clone(),
        name: name.into(),
        token: token(w, name).await,
        scratch: w.dir.path().join("s"),
    };
    let (listener, listen) = bind().await.unwrap();
    let tok = env.token.clone();
    tokio::spawn(async move { run(listener, plugin, &tok).await });
    let host = Host::new(env).unwrap();
    host.hello("0.1.0", &listen).await.unwrap();
    host
}

/// Starts a silent SDK plugin under `name` and says hello with its token.
async fn start_silent(w: &World, name: &str) -> Host {
    start_plugin(w, name, Arc::new(Silent)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fleets_watch_sends_the_full_list_on_every_change() {
    let w = world().await;
    let e = ws(
        &ws_url(&w, "/v1/plugin-host/fleets/watch"),
        &token(&w, "flow").await,
    )
    .await
    .unwrap_err();
    assert_eq!(status_of(&e), Some(403), "flow lacks `fleets`: {e}");
    let e = ws(&ws_url(&w, "/v1/plugin-host/fleets/watch"), "nope")
        .await
        .unwrap_err();
    assert_eq!(status_of(&e), Some(401));

    let web = token(&w, "web").await;
    let mut s = ws(&ws_url(&w, "/v1/plugin-host/fleets/watch"), &web)
        .await
        .unwrap();
    assert_eq!(
        next_text(&mut s).await,
        json!([]),
        "the first frame is the current list"
    );

    // an apply: the record appears, then its agent
    let req = json!(FleetRequest {
        spec: spec(&[("a", &[("flow", json!({ "v": 1 }))])]),
        credentials: Default::default()
    });
    let (st, _) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(st, 200);
    let frame = wait_frame(&mut s, |v| v[0]["status"]["agents"]["f/c/a"].is_object()).await;
    assert_eq!(frame[0]["spec"]["name"], "f");
    assert_eq!(
        frame[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"], "pending",
        "the activation overlay is in the frame"
    );

    // a registry-only change: the plugin says hello and the pair activates,
    // with no actor snapshot behind it
    let _flow = start_silent(&w, "flow").await;
    wait_frame(&mut s, |v| {
        v[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"] == "active"
    })
    .await;

    // purge: the list empties
    let (st, _) = w.api.admin(
        "DELETE",
        "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=true",
        None,
    );
    assert_eq!(st, 200);
    wait_frame(&mut s, |v| v.as_array().is_some_and(Vec::is_empty)).await;
    s.close(None).await.unwrap();
}

/// A rejected apply rolls its activations back and writes the rows itself;
/// no actor snapshot is behind that, so it has to tick the change counter
/// like any other registry write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_activation_is_a_watch_frame() {
    let w = world().await;
    let _flow = start_plugin(&w, "flow", Arc::new(Rejecting::default())).await;
    let req = json!(FleetRequest {
        spec: spec(&[
            ("a", &[("flow", json!({ "v": 1 }))]),
            ("b", &[("flow", json!({ "v": 1 }))]),
        ]),
        credentials: Default::default()
    });
    let (st, v) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(st, 200, "{v}");

    let mut s = ws(
        &ws_url(&w, "/v1/plugin-host/fleets/watch"),
        &token(&w, "web").await,
    )
    .await
    .unwrap();
    let active = |v: &Value, agent: &str| {
        v[0]["status"]["agents"][agent]["plugins"]["flow"]["state"] == "active"
    };
    wait_frame(&mut s, |v| active(v, "f/c/a") && active(v, "f/c/b")).await;

    // `a` takes its new config, `b` refuses, and the rollback of `a` is
    // refused too: its row becomes `rejected` and the apply fails.
    let bad = json!(FleetRequest {
        spec: spec(&[
            ("a", &[("flow", json!({ "v": 2 }))]),
            ("b", &[("flow", json!({ "reject": true }))]),
        ]),
        credentials: Default::default()
    });
    let (st, v) = w.api.admin("PUT", "/v1/fleets/f", Some(&bad));
    assert_eq!(st, 400, "{v}");
    let frame = wait_frame(&mut s, |v| {
        v[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"] == "rejected"
    })
    .await;
    assert_eq!(
        frame[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"]["message"],
        "rejected for f/c/a"
    );
    s.close(None).await.unwrap();
}

/// `plugin sync` drops the activation rows of every removed plugin through
/// `replace_plugins`, and the plugin fleet's actor snapshot never reaches
/// `changes` — so the sync ticks the counter itself, or a watch keeps
/// serving rows for a plugin that is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_plugin_is_a_watch_frame() {
    let w = world().await;
    let _flow = start_silent(&w, "flow").await;
    let req = json!(FleetRequest {
        spec: spec(&[("a", &[("flow", json!({ "v": 1 }))])]),
        credentials: Default::default()
    });
    let (st, v) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(st, 200, "{v}");

    let mut s = ws(
        &ws_url(&w, "/v1/plugin-host/fleets/watch"),
        &token(&w, "web").await,
    )
    .await
    .unwrap();
    wait_frame(&mut s, |v| {
        v[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"] == "active"
    })
    .await;

    // `web` stays declared (the watch rides its token); `flow` goes.
    std::fs::write(
        w.dir.path().join("plugins.yaml"),
        "plugins:\n  - name: web\n    source: ./web-pkg\n",
    )
    .unwrap();
    let (st, report) = w.api.admin("POST", "/v1/plugins/sync", None);
    assert_eq!(st, 200, "{report}");
    assert_eq!(report["stopped"], json!(["flow"]));

    let frame = wait_frame(&mut s, |v| {
        v[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"].is_null()
    })
    .await;
    assert!(
        frame[0]["status"]["agents"]["f/c/a"].is_object(),
        "the agent is still there, only the activation went: {frame}"
    );
    s.close(None).await.unwrap();
}

/// A world with `web` active for `f/c/a` (and `f/c/b` without it): the
/// web token and the running plugin, which must outlive the test.
async fn world_with_web_active() -> (World, String, Host) {
    let w = world().await;
    let web = token(&w, "web").await;
    let web_plugin = start_silent(&w, "web").await;
    let req = json!(FleetRequest {
        spec: spec(&[("a", &[("web", json!({}))]), ("b", &[])]),
        credentials: Default::default()
    });
    let (st, _) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(st, 200);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let r = w.daemon.get(&"f".parse().unwrap()).await.unwrap();
            if r.status.observed_generation == 1
                && r.status.agents["f/c/a"]
                    .plugins
                    .get("web")
                    .is_some_and(|p| p.state == ActivationState::Active)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    (w, web, web_plugin)
}

/// The close frame a socket ends with.
async fn close_frame(s: &mut Socket) -> (u16, String) {
    loop {
        match s.next().await.expect("a frame").unwrap() {
            Message::Close(Some(f)) => return (u16::from(f.code), f.reason.to_string()),
            Message::Close(None) => return (1005, String::new()),
            _ => {}
        }
    }
}

/// §18.4: 1011 is the runner's side failing — a stream whose reader or
/// writer cannot be taken after the upgrade, or a PTY write that fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_side_failure_closes_1011_with_its_reason() {
    let (w, web, _web_plugin) = world_with_web_active().await;
    let url = ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach");

    w.h.runner.fault_next_attach(PtyFault::Reader);
    let mut s = ws(&url, &web).await.unwrap();
    let (code, reason) = close_frame(&mut s).await;
    assert_eq!(code, 1011);
    assert_eq!(reason, "attach: no reader: the pty is gone");

    w.h.runner.fault_next_attach(PtyFault::Writer);
    let mut s = ws(&url, &web).await.unwrap();
    let (code, reason) = close_frame(&mut s).await;
    assert_eq!(
        (code, reason.as_str()),
        (1011, "attach: no writer: the pty is gone")
    );

    w.h.runner.fault_next_attach(PtyFault::Write);
    let mut s = ws(&url, &web).await.unwrap();
    s.send(Message::Binary(b"x".to_vec().into())).await.unwrap();
    let (code, reason) = close_frame(&mut s).await;
    assert_eq!(
        (code, reason.as_str()),
        (1011, "the terminal's writer failed"),
        "not the 1000 reason"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_bridges_the_runner_pty_and_gates_on_activation() {
    let (w, web, _web_plugin) = world_with_web_active().await;

    // gates: capability, activation, token
    let e = ws(
        &ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach"),
        &token(&w, "flow").await,
    )
    .await
    .unwrap_err();
    assert_eq!(status_of(&e), Some(403), "{e}");
    let e = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/b/attach"), &web)
        .await
        .unwrap_err();
    assert_eq!(status_of(&e), Some(404), "b is not active for web: {e}");
    let e = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach"), "nope")
        .await
        .unwrap_err();
    assert_eq!(status_of(&e), Some(401));

    // the runner fails before the upgrade: an ordinary 500, its text in
    // the body, and no socket to close
    w.h.runner.fail_next("attach", "f/c/a", "no such window");
    let e = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach"), &web)
        .await
        .unwrap_err();
    assert_eq!(status_of(&e), Some(500), "{e}");
    assert!(
        body_of(&e).contains("no such window"),
        "the runner's error: {}",
        body_of(&e)
    );

    // the bridge: echo, resize, an unsupported text frame closes 1003
    let mut s = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach"), &web)
        .await
        .unwrap();
    s.send(Message::Binary(b"hello".to_vec().into()))
        .await
        .unwrap();
    let echo = s.next().await.unwrap().unwrap();
    assert_eq!(echo.into_data().as_ref(), b"hello");
    s.send(Message::Text(r#"{"resize":{"cols":120,"rows":40}}"#.into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !w
            .h
            .runner
            .resizes()
            .contains(&("f/c/a".to_string(), 120, 40))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the resize reached the runner");
    // a zero-sized resize is cosmetic (a hidden container), not a fault:
    // ignored, the session goes on
    s.send(Message::Text(r#"{"resize":{"cols":0,"rows":40}}"#.into()))
        .await
        .unwrap();
    s.send(Message::Binary(b"still".to_vec().into()))
        .await
        .unwrap();
    let echo = s.next().await.unwrap().unwrap();
    assert_eq!(
        echo.into_data().as_ref(),
        b"still",
        "the socket survived a zero-sized resize"
    );
    assert!(
        !w.h.runner
            .resizes()
            .iter()
            .any(|(_, c, r)| *c == 0 || *r == 0),
        "a zero dimension never reaches the runner: {:?}",
        w.h.runner.resizes()
    );
    s.send(Message::Text("junk".into())).await.unwrap();
    let close = s.next().await.unwrap().unwrap();
    match close {
        Message::Close(Some(f)) => assert_eq!(u16::from(f.code), 1003, "{f:?}"),
        other => panic!("expected a close, got {other:?}"),
    }

    // a second attach, then the window dies: 1000
    let mut s2 = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach"), &web)
        .await
        .unwrap();
    s2.send(Message::Binary(b"x".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(s2.next().await.unwrap().unwrap().into_data().as_ref(), b"x");
    assert!(w.h.runner.close_attach(&"f/c/a".parse().unwrap()));
    let close = s2.next().await.unwrap().unwrap();
    match close {
        Message::Close(Some(f)) => assert_eq!(u16::from(f.code), 1000, "{f:?}"),
        other => panic!("expected a close, got {other:?}"),
    }
    assert_eq!(
        w.h.runner
            .calls()
            .iter()
            .filter(|c| *c == "attach f/c/a")
            .count(),
        3,
        "two bridges and the one the runner refused"
    );
}
