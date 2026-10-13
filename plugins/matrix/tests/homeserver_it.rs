//! `MatrixLauncher` against a mock homeserver: an axum app answering the
//! handful of client-server endpoints start-up and the inbound pump use.
//! No homeserver runs in CI (Spec G §13), so this is where the adapter's
//! own sequencing is tested over real HTTP and a real `matrix-sdk` client;
//! `scripts/verify-matrix.sh` covers a real homeserver by hand.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// As in `main.rs`: the actor over the real `MatrixClient` is spawned here,
// and its future is deeper than the default limit.
#![recursion_limit = "256"]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use balerix_api::PluginAction;
use balerix_plugin_matrix::MatrixLauncher;
use balerix_plugin_matrix::actor::{Command, Counters, Health, Queue};
use balerix_plugin_matrix::client::restore_and_prove;
use balerix_plugin_matrix::config::{self, Secret};
use balerix_plugin_matrix::plugin::Launcher;
use balerix_plugin_matrix::session::{self, Session};
use balerix_plugin_sdk::testing::FakeHost;
use balerix_plugin_sdk::{Host, Metrics};
use matrix_sdk::SessionChange;
use serde_json::{Value, json};

const USER: &str = "@balerix:mock";

/// What the mock homeserver accepts and what it has seen.
#[derive(Default)]
struct Hs {
    /// The one access token `whoami` and `sync` accept.
    access: String,
    /// The refresh token `/refresh` accepts once, and the pair it answers
    /// with. Spent on use, the way Synapse rotates refresh tokens.
    refresh: Option<(String, String, String)>,
    refreshes: u32,
    /// `rooms` bodies for the next syncs, one per sync, in order.
    syncs: VecDeque<Value>,
    batch: u64,
}

#[derive(Clone)]
struct Mock {
    hs: Arc<Mutex<Hs>>,
    url: String,
}

impl Mock {
    async fn start(access: &str) -> Self {
        let hs = Arc::new(Mutex::new(Hs {
            access: access.to_string(),
            ..Hs::default()
        }));
        let app = Router::new()
            .route("/_matrix/client/versions", get(versions))
            .route("/_matrix/client/v3/account/whoami", get(whoami))
            .route("/_matrix/client/v3/refresh", post(refresh))
            .route("/_matrix/client/v3/sync", get(sync))
            .route("/_matrix/client/v3/keys/upload", post(keys_upload))
            .route(
                "/_matrix/client/v3/rooms/{room}/send/{kind}/{txn}",
                put(room_send),
            )
            .fallback(not_found)
            .with_state(hs.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { hs, url }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Hs> {
        self.hs.lock().unwrap()
    }
}

type Hss = State<Arc<Mutex<Hs>>>;

fn unknown_token() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "errcode": "M_UNKNOWN_TOKEN",
            "error": "the access token has expired",
            "soft_logout": true
        })),
    )
        .into_response()
}

fn authorized(hs: &Mutex<Hs>, headers: &HeaderMap) -> bool {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    presented == Some(hs.lock().unwrap().access.as_str())
}

async fn versions() -> Json<Value> {
    Json(json!({ "versions": ["v1.1", "v1.2", "v1.3", "v1.4", "v1.5", "v1.6"] }))
}

async fn whoami(State(hs): Hss, headers: HeaderMap) -> Response {
    if !authorized(&hs, &headers) {
        return unknown_token();
    }
    Json(json!({ "user_id": USER, "device_id": "balerix" })).into_response()
}

async fn refresh(State(hs): Hss, Json(body): Json<Value>) -> Response {
    let mut hs = hs.lock().unwrap();
    let offered = body["refresh_token"].as_str().unwrap_or_default();
    match hs.refresh.take() {
        Some((accepted, access, refresh)) if accepted == offered => {
            hs.refreshes += 1;
            hs.access = access.clone();
            Json(json!({ "access_token": access, "refresh_token": refresh })).into_response()
        }
        other => {
            hs.refresh = other;
            unknown_token()
        }
    }
}

async fn sync(
    State(hs): Hss,
    headers: HeaderMap,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    if !authorized(&hs, &headers) {
        return unknown_token();
    }
    let first = !query.iter().any(|(k, _)| k == "since");
    let rooms = {
        let mut hs = hs.lock().unwrap();
        if first { None } else { hs.syncs.pop_front() }
    };
    if rooms.is_none() && !first {
        // A long poll with nothing new, kept short.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let batch = {
        let mut hs = hs.lock().unwrap();
        if rooms.is_some() {
            hs.batch += 1;
        }
        hs.batch
    };
    let mut body = json!({ "next_batch": format!("s{batch}") });
    if let Some(rooms) = rooms {
        body["rooms"] = rooms;
    }
    Json(body).into_response()
}

async fn keys_upload() -> Json<Value> {
    Json(json!({ "one_time_key_counts": { "signed_curve25519": 50 } }))
}

async fn room_send(
    axum::extract::Path((_, _, txn)): axum::extract::Path<(String, String, String)>,
) -> Json<Value> {
    Json(json!({ "event_id": format!("$sent-{txn}") }))
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        // `M_NOT_FOUND`, so a state lookup such as a room's encryption
        // event reads as "no such state" rather than as a failure.
        Json(json!({ "errcode": "M_NOT_FOUND", "error": "not in the mock" })),
    )
        .into_response()
}

/// A fresh directory for one launch's state and crypto store, under
/// `TMPDIR` like everything else a test writes, removed when dropped. Bind
/// it for the whole test: the client keeps using it after `launch`.
struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(label: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "matrix-homeserver-it-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

fn cached(mock: &Mock, access: &str, refresh: &str) -> Session {
    Session {
        homeserver: mock.url.clone(),
        user_id: USER.into(),
        configured_user_id: USER.into(),
        device_id: "balerix".into(),
        access_token: Secret::new(access),
        refresh_token: Some(Secret::new(refresh)),
    }
}

struct Started {
    host: Host,
    launcher: MatrixLauncher,
    queue: Arc<Queue>,
    config: config::DaemonConfig,
}

/// A launcher over `fake`, with its scratch directory at `dir` and no
/// password configured: a cached session that does not work is fatal.
fn launcher(fake: &FakeHost, mock: &Mock, dir: &std::path::Path) -> Started {
    let host = Host::new(fake.env("matrix", dir)).unwrap();
    let counters = Counters::new(&Metrics::new("matrix")).unwrap();
    let queue = Queue::new(counters.events_dropped.clone());
    let config = config::parse_daemon(&json!({ "homeserver": mock.url, "userId": USER })).unwrap();
    Started {
        launcher: MatrixLauncher {
            host: host.clone(),
            counters,
            health: Health::new(),
        },
        host,
        queue,
        config,
    }
}

/// #261, the two halves of start-up apart, with no background task
/// running: `whoami` refreshes an expired cached token, the proven session
/// carries the rotated pair (what `launch` seals before it returns), and
/// the session-change subscription, taken before `whoami`, has already
/// heard the refresh. Subscribing after `whoami` fails the last assertion.
#[tokio::test(flavor = "multi_thread")]
async fn whoami_refreshes_an_expired_token_and_the_early_subscription_hears_it() {
    let mock = Mock::start("at-2").await;
    mock.lock().refresh = Some(("rt-1".into(), "at-2".into(), "rt-2".into()));
    let dir = scratch("prove");
    let config = config::parse_daemon(&json!({ "homeserver": mock.url, "userId": USER })).unwrap();

    let mut proven = restore_and_prove(&config, &dir.join("store"), &cached(&mock, "at-1", "rt-1"))
        .await
        .expect("the refreshed session proves itself");
    assert_eq!(
        mock.lock().refreshes,
        1,
        "whoami refreshed the expired token"
    );
    assert_eq!(proven.user_id, USER);
    assert_eq!(proven.session.access_token.expose(), "at-2");
    assert_eq!(
        proven.session.refresh_token.as_ref().map(Secret::expose),
        Some("rt-2")
    );
    assert!(
        matches!(
            proven.changes.try_recv(),
            Ok(SessionChange::TokensRefreshed)
        ),
        "the subscription existed before whoami's refresh"
    );
}

/// #261: an expired cached access token is refreshed by start-up's own
/// `whoami`. The rotated pair has to be sealed by the time `launch`
/// returns, because the homeserver has spent the old refresh token: a
/// restart from the old pair would be refused, and with no password
/// configured the plugin would exit `REVOKED`.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_refreshed_during_start_up_is_sealed_and_the_next_start_uses_it() {
    let mock = Mock::start("at-2").await;
    mock.lock().refresh = Some(("rt-1".into(), "at-2".into(), "rt-2".into()));
    let fake = FakeHost::start("tok", json!({}), Vec::new()).await;

    let first_dir = scratch("first");
    let first = launcher(&fake, &mock, &first_dir);
    session::store(&first.host, &cached(&mock, "at-1", "rt-1"))
        .await
        .unwrap();
    first
        .launcher
        .launch(first.config.clone(), first.queue.clone())
        .await
        .expect("the refreshed session proves itself");
    assert_eq!(
        mock.lock().refreshes,
        1,
        "whoami refreshed the expired token"
    );

    let sealed = session::load(&first.host).await.unwrap().unwrap();
    assert_eq!(sealed.access_token.expose(), "at-2");
    assert_eq!(
        sealed.refresh_token.as_ref().map(Secret::expose),
        Some("rt-2"),
        "the rotated refresh token is the one sealed"
    );

    // A restart: a new process with its own store, the same sealed KV. The
    // old refresh token is spent, so only the re-sealed pair can start it.
    let second_dir = scratch("second");
    let second = launcher(&fake, &mock, &second_dir);
    second
        .launcher
        .launch(second.config.clone(), second.queue.clone())
        .await
        .expect("the next start restores the re-sealed pair");
    assert_eq!(mock.lock().refreshes, 1, "no second refresh was needed");
}

const ROOM: &str = "!crew:mock";
const RAHUL: &str = "@rahul:mock";
const ROOT: &str = "$root:mock";

fn reply(id: &str, ts: u64, body: &str) -> Value {
    reply_from(RAHUL, id, ts, body)
}

fn reply_from(sender: &str, id: &str, ts: u64, body: &str) -> Value {
    json!({
        "type": "m.room.message",
        "event_id": id,
        "sender": sender,
        "origin_server_ts": ts,
        "content": {
            "msgtype": "m.text",
            "body": body,
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": ROOT,
                "is_falling_back": true,
                "m.in_reply_to": { "event_id": ROOT }
            }
        }
    })
}

/// Our own membership changing to `membership`, by `sender`, from `prev`.
fn member(id: &str, ts: u64, sender: &str, membership: &str, prev: Option<&str>) -> Value {
    let mut event = json!({
        "type": "m.room.member",
        "event_id": id,
        "sender": sender,
        "state_key": USER,
        "origin_server_ts": ts,
        "content": { "membership": membership }
    });
    if let Some(prev) = prev {
        event["unsigned"] = json!({ "prev_content": { "membership": prev } });
    }
    event
}

fn joined(state: Vec<Value>, timeline: Vec<Value>) -> Value {
    joined_limited(state, timeline, false)
}

fn joined_limited(state: Vec<Value>, timeline: Vec<Value>, limited: bool) -> Value {
    json!({ "join": { ROOM: {
        "state": { "events": state },
        "timeline": { "events": timeline, "limited": limited, "prev_batch": "p0" }
    }}})
}

fn create() -> Value {
    json!({
        "type": "m.room.create", "event_id": "$create:mock", "sender": RAHUL,
        "state_key": "", "origin_server_ts": 1, "content": { "creator": RAHUL }
    })
}

/// A launched plugin with alice's thread in `ROOM`, and the actor
/// configured, ready for scripted syncs.
async fn launched_with_thread(fake: &FakeHost, mock: &Mock, dir: &Scratch) -> Started {
    let started = launcher(fake, mock, dir);
    session::store(&started.host, &cached(mock, "at-1", "rt-1"))
        .await
        .unwrap();
    // The thread the replies belong to, as an earlier `SessionStart` left it.
    started
        .host
        .kv_put(
            "thread/f/c/alice",
            &serde_json::to_vec(&json!({
                "session_id": "s1", "root": ROOT, "room": ROOM, "closed": false
            }))
            .unwrap(),
            false,
        )
        .await
        .unwrap();
    started
        .queue
        .push(Command::Configure(started.config.clone()))
        .unwrap();
    // Alice is active, as the daemon re-activates every agent at start.
    started
        .queue
        .push(Command::Activate {
            agent: "f/c/alice".into(),
            config: config::parse_agent(&json!({})).unwrap(),
        })
        .unwrap();
    started
        .launcher
        .launch(started.config.clone(), started.queue.clone())
        .await
        .unwrap();
    started
}

/// Waits until `n` actions reached alice, failing after 20 s.
async fn routed(fake: &FakeHost, n: usize) -> Vec<PluginAction> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while fake.actions_for("f/c/alice").len() < n {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out; routed so far: {:?}",
            fake.actions_for("f/c/alice")
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    fake.actions_for("f/c/alice")
}

fn text(t: &str) -> PluginAction {
    PluginAction::SendText {
        text: t.into(),
        submit: true,
    }
}

/// #262 over the real pump: the bot is in the crew room and routes a
/// reply; it is kicked and invited back; the join sync carries the room's
/// recent history — the reply it already routed, and one sent while it was
/// out — and neither reaches the agent. Every reply in a later sync does,
/// whatever its timestamp: one from a homeserver whose clock runs ahead,
/// then one from a homeserver whose clock runs behind, stamped even before
/// the bot's join. A later sync that
/// delivers an already handled event id again routes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejoin_routes_nothing_from_the_history_its_join_sync_carries() {
    let mock = Mock::start("at-1").await;
    let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
    let dir = scratch("rejoin");
    let started = launched_with_thread(&fake, &mock, &dir).await;
    let create = create();
    {
        let mut hs = mock.lock();
        // In the room, and a reply that is routed.
        hs.syncs.push_back(joined(
            vec![create],
            vec![
                member("$join1:mock", 100, USER, "join", None),
                reply("$r1:mock", 200, "first"),
            ],
        ));
        // Kicked.
        hs.syncs.push_back(json!({ "leave": { ROOM: {
            "state": { "events": [] },
            "timeline": { "events": [
                member("$kick:mock", 300, RAHUL, "leave", Some("join"))
            ]}
        }}}));
        // Invited back.
        hs.syncs.push_back(json!({ "invite": { ROOM: {
            "invite_state": { "events": [{
                "type": "m.room.member", "sender": RAHUL, "state_key": USER,
                "content": { "membership": "invite" }
            }]}
        }}}));
        // The join sync, with the history since before the kick.
        hs.syncs.push_back(joined(
            vec![],
            vec![
                reply("$r1:mock", 200, "first"),
                member("$kick:mock", 300, RAHUL, "leave", Some("join")),
                reply("$r2:mock", 350, "while kicked"),
                member("$invite:mock", 400, RAHUL, "invite", Some("leave")),
                member("$join2:mock", 500, USER, "join", Some("invite")),
            ],
        ));
        // Live again: a reply stamped by a clock far ahead.
        let ahead = reply_from("@ann:fast", "$r3:fast", 900_000, "new reply");
        hs.syncs.push_back(joined(vec![], vec![ahead.clone()]));
        // The same event again, then a reply stamped by a clock behind.
        hs.syncs.push_back(joined(
            vec![],
            vec![
                ahead,
                reply_from("@bob:slow", "$r4:slow", 450, "slow clock"),
            ],
        ));
    }

    routed(&fake, 3).await;
    assert_eq!(
        fake.actions_for("f/c/alice"),
        vec![text("first"), text("new reply"), text("slow clock")],
        "the join sync's history and the redelivery routed nothing; both \
         live replies routed, out of timestamp order"
    );
    assert_eq!(
        started
            .launcher
            .counters
            .inbound
            .with_label_values(&["before_join"])
            .get(),
        2,
        "the pump dropped both history messages, without queueing them"
    );
}

/// A gappy join: the bot joined, then more happened than one timeline
/// holds, so the response's timeline is `limited` and the join is only in
/// its state section. Nothing in that timeline is history — it all follows
/// the join — so every reply is routed, even one stamped earlier than the
/// join by a homeserver whose clock runs behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_gappy_join_routes_everything_its_timeline_carries() {
    let mock = Mock::start("at-1").await;
    let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
    let dir = scratch("gappy");
    let _started = launched_with_thread(&fake, &mock, &dir).await;

    mock.lock().syncs.push_back(joined_limited(
        vec![
            create(),
            member("$join:mock", 100, USER, "join", Some("invite")),
        ],
        vec![
            reply_from("@bob:slow", "$g1:slow", 50, "gappy one"),
            reply("$g2:mock", 200, "gappy two"),
        ],
        true,
    ));
    assert_eq!(
        routed(&fake, 2).await,
        vec![text("gappy one"), text("gappy two")]
    );
}
