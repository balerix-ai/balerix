//! The `matrix-sdk` adapter (Spec G §12.3): the only file that knows Matrix
//! types. It implements `MatrixPort` for the actor, and `Launcher` for the
//! plugin, which is where login, the store and the inbound pump are started.
//!
//! Nothing here decides anything. Every rule this plugin follows is decided
//! in a module above — `session::plan`, `routing`, `render`, `actor` — each
//! of which is tested against the port or against a fake host. This file is
//! a translation between `matrix-sdk`'s types and the port's, and the one
//! place in the crate that knows a Matrix error from any other.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use balerix_plugin_sdk::Host;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::{RequestConfig, SyncSettings};
use matrix_sdk::room::Room;
use matrix_sdk::ruma::api::client::filter::FilterDefinition;
use matrix_sdk::ruma::api::client::room::create_room;
use matrix_sdk::ruma::api::client::sync::sync_events;
use matrix_sdk::ruma::api::error::{ErrorKind, RetryAfter};
use matrix_sdk::ruma::events::InitialStateEvent;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::relation::{Annotation, Thread};
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
};
use matrix_sdk::ruma::{EventId, RoomId, UserId, uint};
use matrix_sdk::store::{RoomLoadSettings, StateStoreDataKey};
use matrix_sdk::{Client, HttpError, RoomState, SessionChange, SessionMeta, SessionTokens};

use crate::actor::{Actor, Command, Counters, Health, Queue};
use crate::config::{DaemonConfig, Secret};
use crate::matrix::{Inbound, MatrixError, MatrixPort};
use crate::plugin::Launcher;
use crate::session::{self, Plan, Session};

/// What a rate limit costs when the homeserver refuses to say. The actor
/// sleeps this before its one retry, so it has to be a number; every
/// homeserver that implements `M_LIMIT_EXCEEDED` sends its own.
const DEFAULT_RETRY_MS: u64 = 1_000;
/// The longest delay the adapter will pass on. What stands between a wild
/// `retry_after` and a stalled plugin is the actor, which sits out only a
/// short delay inline (`actor::MAX_INLINE_RETRY`) and declines a long one
/// rather than holding every crew's events still; this clamp keeps a
/// timestamp years out from reaching it as a number at all.
const MAX_RETRY_MS: u64 = 60_000;
/// How long the inbound pump waits before its first reconnect, and the most
/// it will ever wait between two.
const SYNC_BACKOFF_MIN: Duration = Duration::from_secs(1);
const SYNC_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// How many syncs in a row must fail authentication before the pump gives
/// up. Giving up is permanent until the plugin restarts, and a bare 401
/// from an intermediary — a body that is not even Matrix JSON still carries
/// its status into `classify` — must not cost that. A token that is really
/// dead fails every time and still stops the pump, seconds later.
const AUTH_FAILURES_BEFORE_STOPPING: u32 = 3;

/// The `MatrixPort` the actor drives. `user_id` is the homeserver's own
/// rendering of this account's id, taken from `whoami`, because the actor
/// compares it byte for byte against the `sender` of an inbound event to
/// keep the plugin from answering itself.
pub struct MatrixClient {
    client: Client,
    user_id: String,
}

/// A client on the homeserver in `config`, with its state and crypto store
/// under `store_dir`. Does not authenticate.
async fn build(config: &DaemonConfig, store_dir: &Path) -> Result<Client, MatrixError> {
    Client::builder()
        .homeserver_url(&config.homeserver)
        .sqlite_store(store_dir, None)
        // Without this the client never refreshes: on `M_UNKNOWN_TOKEN` it
        // broadcasts and gives up, and `SessionChange::TokensRefreshed`,
        // which `reseal_on_refresh` waits for, is only ever sent by the
        // refresh this flag gates. `request_refresh_token()` at login only
        // advertises that we support refreshing (G-8).
        .handle_refresh_tokens()
        // The plan puts the retry in the actor: one retry, after the delay
        // the homeserver itself named (rule 1), with the queue moving in
        // between. Left at its default the SDK swallows a rate limit or a
        // 5xx into an exponential backoff of up to fifteen minutes inside a
        // single call, so the actor's retry — and the tests written for it —
        // would never run.
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .map_err(|e| MatrixError::Other(format!("building the client: {e}")))
}

/// Restores `session` into a fresh client. No network call.
async fn restore(
    config: &DaemonConfig,
    store_dir: &Path,
    session: &Session,
) -> Result<Client, String> {
    let client = build(config, store_dir).await.map_err(|e| e.to_string())?;
    let user_id = UserId::parse(&session.user_id)
        .map_err(|e| format!("the cached session's user id is not a Matrix id: {e}"))?;
    let restored = MatrixSession {
        meta: SessionMeta {
            user_id,
            device_id: session.device_id.as_str().into(),
        },
        tokens: SessionTokens {
            access_token: session.access_token.expose().to_string(),
            refresh_token: session
                .refresh_token
                .as_ref()
                .map(|t| t.expose().to_string()),
        },
    };
    client
        .matrix_auth()
        .restore_session(restored, RoomLoadSettings::default())
        .await
        .map_err(|e| format!("restoring the cached session: {e}"))?;
    Ok(client)
}

/// Password login with the configured device id and display name, asking
/// for a refresh token (G-8). Returns the client and the session to seal.
async fn login(
    config: &DaemonConfig,
    store_dir: &Path,
    password: &Secret,
) -> Result<(Client, Session), String> {
    let client = build(config, store_dir).await.map_err(|e| e.to_string())?;
    let response = client
        .matrix_auth()
        .login_username(&config.user_id, password.expose())
        .device_id(&config.device_id)
        .initial_device_display_name(&config.device_name)
        .request_refresh_token()
        .send()
        .await
        .map_err(|e| format!("logging in as {}: {e}", config.user_id))?;
    let session = Session {
        homeserver: config.homeserver.clone(),
        // The homeserver's spelling and the operator's, side by side: the
        // first is what a restore and the loop guard need, the second is
        // what `session::plan` has to compare against `config.userId`.
        user_id: response.user_id.to_string(),
        configured_user_id: config.user_id.clone(),
        device_id: response.device_id.to_string(),
        access_token: Secret::new(response.access_token),
        refresh_token: response.refresh_token.map(Secret::new),
    };
    Ok((client, session))
}

/// `GET /_matrix/client/v3/account/whoami`: the call that proves the
/// credentials before the daemon is told the plugin is ready.
async fn whoami(client: &Client) -> Result<String, MatrixError> {
    match client.whoami().await {
        Ok(response) => Ok(response.user_id.to_string()),
        Err(e) => Err(classify_http(&e, format!("whoami: {e}"))),
    }
}

/// A failure from a call that returns `HttpResult`.
fn classify_http(e: &HttpError, message: String) -> MatrixError {
    // With `handle_refresh_tokens` on, a refresh the homeserver rejected
    // comes back as its own variant rather than as a Matrix error, so
    // `as_client_api_error` sees nothing. The session is dead either way,
    // and only `Auth` takes the launcher's session-clearing path.
    if matches!(e, HttpError::RefreshToken(_)) {
        return MatrixError::Auth(message);
    }
    classify(e.as_client_api_error(), message)
}

/// A failure from a call that returns the SDK's own `Result`.
fn classify_error(e: &matrix_sdk::Error, message: String) -> MatrixError {
    match e {
        matrix_sdk::Error::Http(http) => classify_http(http, message),
        other => classify(other.as_client_api_error(), message),
    }
}

/// Every `matrix-sdk` failure becomes a `MatrixError` here, and nowhere else
/// in the crate. A rate limit keeps the homeserver's own delay, because the
/// actor sleeps exactly that before its single retry; a rejected token
/// becomes `Auth`, because that is the variant the launcher clears the
/// cached session for. `message` is the caller's context plus the error's
/// own text: a Matrix error carries the server's `errcode` and message and
/// never a credential, and the access token travels in a header, so no URL
/// in a transport error carries one either.
fn classify(api: Option<&matrix_sdk::ruma::api::error::Error>, message: String) -> MatrixError {
    let Some(api) = api else {
        return MatrixError::Other(message);
    };
    match api.error_kind() {
        Some(ErrorKind::LimitExceeded(limit)) => MatrixError::RateLimited {
            retry_after_ms: retry_after_ms(limit.retry_after),
        },
        Some(ErrorKind::UnknownToken(_)) => MatrixError::Auth(message),
        // `M_MISSING_TOKEN` and a bare 401 land here together.
        _ if api.status_code.as_u16() == 401 => MatrixError::Auth(message),
        _ => MatrixError::Other(message),
    }
}

/// The homeserver may name a delay or a wall-clock instant; the port speaks
/// only in milliseconds from now, clamped to `MAX_RETRY_MS`.
fn retry_after_ms(retry_after: Option<RetryAfter>) -> u64 {
    let delay = match retry_after {
        Some(RetryAfter::Delay(delay)) => delay,
        Some(RetryAfter::DateTime(when)) => {
            when.duration_since(SystemTime::now()).unwrap_or_default()
        }
        None => Duration::from_millis(DEFAULT_RETRY_MS),
    };
    u64::try_from(delay.as_millis())
        .unwrap_or(u64::MAX)
        .min(MAX_RETRY_MS)
}

/// Whether a room in this state has to be joined before anything can be
/// sent to it. `None` is a room the state store has never heard of, which
/// on a fresh store is every room until the first sync lands; every state
/// but `Joined` is a room the homeserver would refuse a send in.
fn needs_join(state: Option<RoomState>) -> bool {
    state != Some(RoomState::Joined)
}

impl MatrixClient {
    /// A room the client can send to. The state store holds every joined
    /// room, so the common case resolves a room id from the KV map without
    /// a network call. Anything else is joined first, which is what makes
    /// G-4's pinned room work: an operator who creates a room and invites
    /// the bot leaves it in the *invited* state, where every send is
    /// refused by the homeserver, and on a fresh store the room is not in
    /// the state store at all until the first sync lands, so the lookup
    /// fails outright and the first events after an install are dropped.
    ///
    /// Joining is the adapter's job, not the port's: `MatrixPort`'s
    /// contract is "give me a room I can send to". `join_room_by_id` works
    /// whether or not the store knows the room and is idempotent for a room
    /// we are already in, so it covers both cases with one call, and a
    /// refusal (no invite, banned, a rate limit) comes back through
    /// `classify_error` as the `MatrixError` the caller already handles.
    async fn room(&self, room: &str) -> Result<Room, MatrixError> {
        let id =
            RoomId::parse(room).map_err(|e| MatrixError::Other(format!("room id {room}: {e}")))?;
        if let Some(known) = self.client.get_room(&id)
            && !needs_join(Some(known.state()))
        {
            return Ok(known);
        }
        tracing::info!("matrix: joining room {room}");
        self.client
            .join_room_by_id(&id)
            .await
            .map_err(|e| classify_error(&e, format!("joining room {room}: {e}")))
    }
}

impl MatrixPort for MatrixClient {
    fn user_id(&self) -> &str {
        &self.user_id
    }

    async fn create_room(&self, name: &str, invite: &[String]) -> Result<String, MatrixError> {
        let mut invited = Vec::with_capacity(invite.len());
        for user in invite {
            invited.push(
                UserId::parse(user)
                    .map_err(|e| MatrixError::Other(format!("invite {user}: {e}")))?,
            );
        }
        let mut request = create_room::v3::Request::new();
        request.name = Some(name.to_string());
        request.invite = invited;
        request.preset = Some(create_room::v3::RoomPreset::PrivateChat);
        request.initial_state = vec![
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        ];
        let room = self
            .client
            .create_room(request)
            .await
            .map_err(|e| classify_error(&e, format!("create room {name}: {e}")))?;
        Ok(room.room_id().to_string())
    }

    async fn send(
        &self,
        room: &str,
        thread_root: Option<&str>,
        markdown: &str,
    ) -> Result<String, MatrixError> {
        let target = self.room(room).await?;
        let mut content = RoomMessageEventContent::text_markdown(markdown);
        if let Some(root) = thread_root {
            let root = EventId::parse(root)
                .map_err(|e| MatrixError::Other(format!("thread root {root}: {e}")))?;
            // `Thread::plain` carries the in-reply-to fallback, pointed at
            // the root: the plugin does not track the latest event of a
            // thread, and a client that does not understand `m.thread`
            // renders the message as a reply to the root either way.
            content.relates_to = Some(Relation::Thread(Thread::plain(root.clone(), root)));
        }
        let sent = target
            .send(content)
            .await
            .map_err(|e| classify_error(&e, format!("send to {room}: {e}")))?;
        Ok(sent.response.event_id.to_string())
    }

    async fn react(&self, room: &str, event_id: &str, key: &str) -> Result<(), MatrixError> {
        let target = self.room(room).await?;
        let event_id = EventId::parse(event_id)
            .map_err(|e| MatrixError::Other(format!("event id {event_id}: {e}")))?;
        let content = ReactionEventContent::new(Annotation::new(event_id, key.to_string()));
        target
            .send(content)
            .await
            .map_err(|e| classify_error(&e, format!("react in {room}: {e}")))?;
        Ok(())
    }
}

/// Counts one failed sync attempt: `errors_total{kind="sync"}` for every
/// one, and `errors_total{kind="auth"}` as well when the homeserver
/// rejected the session (Spec G §10 names both labels). These are the two
/// the operator of a plugin that has gone deaf sees, and a time series
/// matters more here than the pump's one log line: the failures repeat,
/// and the log is sparse on purpose.
fn count_sync_failure(counters: &Counters, error: &MatrixError) {
    counters.errors.with_label_values(&["sync"]).inc();
    if matches!(error, MatrixError::Auth(_)) {
        counters.errors.with_label_values(&["auth"]).inc();
    }
}

/// The inbound pump: a sync loop that reconnects for as long as the process
/// lives, pushing `Command::Inbound` onto `queue` for every text message in
/// a room the account is in. It gives up only when the homeserver rejects
/// the session, which no amount of reconnecting would mend, and it fails
/// `health` on the way out so `plugin list` says so.
async fn start_inbound_pump(client: Client, queue: Arc<Queue>, counters: Counters, health: Health) {
    // The returned handle only exists to remove the handler again, which
    // nothing here ever does: the pump lives as long as the process.
    client.add_event_handler(move |event: OriginalSyncRoomMessageEvent, room: Room| {
        let queue = queue.clone();
        async move {
            // Everything the actor never needs to see is dropped here: a
            // reaction, a state event, an emote, a room we only watch from
            // an invite, and an empty body.
            if room.state() != RoomState::Joined {
                return;
            }
            let MessageType::Text(text) = &event.content.msgtype else {
                return;
            };
            if text.body.trim().is_empty() {
                return;
            }
            let thread_root = match &event.content.relates_to {
                Some(Relation::Thread(thread)) => Some(thread.event_id.to_string()),
                _ => None,
            };
            queue.push(Command::Inbound(Inbound {
                room: room.room_id().to_string(),
                event_id: event.event_id.to_string(),
                // The same ruma `OwnedUserId` rendering `whoami` gave the
                // port's `user_id`, so the actor's byte comparison holds.
                sender: event.sender.to_string(),
                thread_root,
                body: text.body.clone(),
            }));
        }
    });

    // A homeserver can be down for an hour; the plugin should still be
    // reading replies when it comes back. The SDK's own retry is off (it
    // would swallow the rate limit the actor is written to handle), so the
    // reconnect lives here, and only here.
    let mut backoff = SYNC_BACKOFF_MIN;
    let mut failures: u64 = 0;
    let mut auth_failures: u32 = 0;
    loop {
        let started = Instant::now();
        let Err(e) = sync(&client).await else {
            // `Client::sync` returns `Ok` only if a sync callback asked it
            // to stop, and ours never does.
            tracing::error!("matrix: the sync loop ended; no reply can arrive");
            return;
        };
        // A sync that ran longer than the cap was a working connection, so
        // the next outage starts its backoff from the bottom, and whatever
        // ends this one is the first failure of a new episode.
        if started.elapsed() > SYNC_BACKOFF_MAX {
            backoff = SYNC_BACKOFF_MIN;
            failures = 0;
            auth_failures = 0;
        }
        // The one failure retrying cannot fix, and the one an operator has
        // to act on — but only once it has happened often enough in a row to
        // be the session rather than something in the way. Any other
        // outcome, and any sync that lasted, starts the count over.
        // Stopping here leaves the launcher's §5.2 step 4 path to clear the
        // cached session at the next start.
        let classified = classify_error(&e, format!("sync: {e}"));
        count_sync_failure(&counters, &classified);
        if let MatrixError::Auth(message) = classified {
            auth_failures += 1;
            if auth_failures >= AUTH_FAILURES_BEFORE_STOPPING {
                let message = format!(
                    "{message}; rejected {auth_failures} times in a row, so no reply can \
                     arrive until the plugin is restarted with a session the homeserver \
                     accepts"
                );
                tracing::error!("matrix: {message}");
                // Nothing sets the cell back to ok except a newly created or
                // newly pinned crew room, so this stands until an operator
                // acts: `Plugin::health` reads it straight through to the
                // daemon's health poll and `plugin list`.
                health.fail(message);
                return;
            }
        } else {
            auth_failures = 0;
        }
        failures += 1;
        // Loud once, then sparse: a homeserver down overnight must not fill
        // the plugin's log.
        if failures == 1 || failures.is_multiple_of(10) {
            tracing::warn!(
                "matrix: sync failed ({failures} in a row), retrying in {backoff:?}: {e}"
            );
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(SYNC_BACKOFF_MAX);
    }
}

/// One attempt at syncing: the history-free first sync when the store holds
/// no batch token, then the live sync, which returns only on failure.
///
/// §9.3: a first sync must not replay history — every message in it would be
/// a fresh instruction to an agent. With no batch token in the store, take
/// one sync with a filter that asks for no timeline events at all and start
/// the live sync from the token it returns. The store keeps that token, so
/// both a reconnect and a restart resume instead of replaying.
async fn sync(client: &Client) -> Result<(), matrix_sdk::Error> {
    if client
        .state_store()
        .get_kv_data(StateStoreDataKey::SyncToken)
        .await?
        .is_none()
    {
        let mut filter = FilterDefinition::empty();
        filter.room.timeline.limit = Some(uint!(0));
        let settings =
            SyncSettings::new().filter(sync_events::v3::Filter::FilterDefinition(filter));
        client.sync_once(settings).await?;
    }
    // `SyncSettings::new()` resumes from the token the store holds, which is
    // the one the filtered sync above just wrote.
    client.sync(SyncSettings::new()).await
}

/// `matrix-sdk` rotates the access token when it spends the refresh token,
/// which makes the sealed record stale. Without this, a restart after a
/// rotation logs in from the password again, or fails when the password has
/// been taken out of `plugins.yaml`.
async fn reseal_on_refresh(host: Host, client: Client, session: Session) {
    let mut changes = client.subscribe_to_session_changes();
    loop {
        match changes.recv().await {
            Ok(SessionChange::TokensRefreshed) => {
                let Some(tokens) = client.session_tokens() else {
                    continue;
                };
                let rotated = Session {
                    access_token: Secret::new(tokens.access_token),
                    refresh_token: tokens.refresh_token.map(Secret::new),
                    ..session.clone()
                };
                if let Err(e) = session::store(&host, &rotated).await {
                    tracing::warn!("matrix: re-sealing the refreshed session: {e}");
                }
            }
            Ok(SessionChange::UnknownToken(_)) => {
                // With refreshing on, this means the refresh itself was
                // refused: the cached record is now worthless and only a
                // password can replace it (§5.2 step 4, at the next start).
                tracing::warn!(
                    "matrix: the homeserver rejected our session and it could not be \
                     refreshed; configure a password so the next start can log in again"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Starts the actor and the inbound pump against a real homeserver.
pub struct MatrixLauncher {
    pub host: Host,
    pub counters: Counters,
    pub health: Health,
}

impl MatrixLauncher {
    async fn remember(&self, session: &Session) -> Result<(), String> {
        session::store(&self.host, session)
            .await
            .map_err(|e| format!("kv: {e}"))
    }
}

impl Launcher for MatrixLauncher {
    async fn launch(&self, config: DaemonConfig, queue: Arc<Queue>) -> Result<(), String> {
        // The state and crypto store live under the plugin's scratch
        // directory, which survives a restart and is removed only by
        // `plugin remove --purge` (Spec G §3).
        let store_dir = self.host.env().scratch.join("store");
        let cached = session::load(&self.host)
            .await
            .map_err(|e| format!("kv: {e}"))?;
        let plan = session::plan(cached, &config)?;

        // Restore or log in, then prove the credentials with `whoami`
        // before the daemon is told the plugin is ready (G-14).
        let (client, session, user_id) = match plan {
            Plan::Restore(cached) => {
                let client = restore(&config, &store_dir, &cached).await?;
                match whoami(&client).await {
                    Ok(user_id) => (client, *cached, user_id),
                    Err(MatrixError::Auth(e)) => {
                        // §5.2 step 4: the cached session was rejected, most
                        // likely a revoked device. Drop it and log in once
                        // more if there is a password to log in with.
                        let Some(password) = config.password.clone() else {
                            return Err(session::REVOKED.to_string());
                        };
                        tracing::warn!(
                            "matrix: the cached session was rejected ({e}); logging in again"
                        );
                        drop(client);
                        session::clear(&self.host)
                            .await
                            .map_err(|e| format!("kv: {e}"))?;
                        let (client, session) = login(&config, &store_dir, &password).await?;
                        let user_id = whoami(&client).await.map_err(|e| e.to_string())?;
                        self.remember(&session).await?;
                        (client, session, user_id)
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            Plan::Login { password } => {
                let (client, session) = login(&config, &store_dir, &password).await?;
                let user_id = whoami(&client).await.map_err(|e| e.to_string())?;
                self.remember(&session).await?;
                tracing::warn!(
                    "matrix: logged in with the configured password and cached the session; \
                     the password can now be removed from plugins.yaml"
                );
                (client, session, user_id)
            }
        };

        let port = MatrixClient {
            client: client.clone(),
            user_id,
        };
        let mut actor = Actor::new(
            self.host.clone(),
            port,
            self.counters.clone(),
            self.health.clone(),
        );
        actor.load().await;
        tokio::spawn(actor.run(queue.clone()));
        tokio::spawn(reseal_on_refresh(
            self.host.clone(),
            client.clone(),
            session,
        ));
        tokio::spawn(start_inbound_pump(
            client,
            queue,
            self.counters.clone(),
            self.health.clone(),
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one decision in `MatrixClient::room` that can be made without a
    /// homeserver: a pinned room the operator invited the bot to arrives
    /// `Invited`, and on a fresh store it is not in the store at all — both
    /// have to be joined, or every send in that room is refused by the
    /// server (G-4). The join call itself is exercised by
    /// `scripts/verify-matrix.sh`.
    #[test]
    fn every_state_but_joined_has_to_be_joined_first() {
        assert!(!needs_join(Some(RoomState::Joined)));
        for state in [
            RoomState::Invited,
            RoomState::Left,
            RoomState::Knocked,
            RoomState::Banned,
        ] {
            assert!(needs_join(Some(state)), "{state:?} should be joined first");
        }
        assert!(needs_join(None), "a room the store has never seen");
    }

    /// Spec G §10's `errors_total` names `sync` and `auth`, and only the
    /// pump can emit either: `sync` counts every failed attempt, `auth` the
    /// subset the homeserver rejected the session for. Between them they
    /// are what says the plugin has gone deaf.
    #[test]
    fn a_failed_sync_counts_sync_and_a_rejected_session_counts_auth_too() {
        let metrics = balerix_plugin_sdk::Metrics::new("matrix");
        let counters = Counters::new(&metrics).unwrap();
        let sync = || counters.errors.with_label_values(&["sync"]).get();
        let auth = || counters.errors.with_label_values(&["auth"]).get();

        count_sync_failure(&counters, &MatrixError::Other("connection refused".into()));
        assert_eq!((sync(), auth()), (1, 0), "a transient failure is not auth");

        count_sync_failure(&counters, &MatrixError::RateLimited { retry_after_ms: 5 });
        assert_eq!((sync(), auth()), (2, 0));

        count_sync_failure(&counters, &MatrixError::Auth("sync: 401".into()));
        assert_eq!((sync(), auth()), (3, 1), "a rejected session counts both");

        let text = metrics.render().unwrap();
        for label in ["kind=\"sync\"", "kind=\"auth\""] {
            assert!(text.contains(label), "missing {label} in\n{text}");
        }
    }
}
