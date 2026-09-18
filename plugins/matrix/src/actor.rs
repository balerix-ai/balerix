//! The one task that owns every piece of mutable state (Spec G-10), the
//! bounded drop-oldest queue that feeds it (G-11), and the counters and
//! health cell it publishes through.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use balerix_api::{HookEvent, OBSERVER_QUEUE, PluginAction};
use balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge};
use balerix_plugin_sdk::{Host, Metrics, SdkError};
use serde_json::Value;
use tokio::sync::Notify;

use crate::config::{AgentConfig, DaemonConfig};
use crate::matrix::{ACK, CONFIRMED, FAILED, Inbound, MatrixError, MatrixPort, REFUSED};
use crate::pending::{Questions, Stage};
use crate::question;
use crate::render::{self, PhaseChange};
use crate::routing::{Maps, Thread, crew_of};

/// Queue depth, the same as the daemon's own observer queues.
pub const QUEUE: usize = OBSERVER_QUEUE;

/// How long a crew waits after its room creation failed, and the most it
/// will ever wait. Without this the actor issues one `create_room` per hook
/// event for a crew whose creation can never succeed — wrong rights, a bad
/// invite id — which for a busy crew is tens to hundreds of calls a minute
/// against a homeserver that has already refused: exactly how a rate limit
/// is earned.
const CREATE_COOLDOWN_MIN: Duration = Duration::from_secs(5);
const CREATE_COOLDOWN_MAX: Duration = Duration::from_secs(300);

/// The longest homeserver-requested delay `retry_once` will sit out inline.
/// It sleeps inside the single actor task, so everything for every crew
/// waits behind it — and while it waits the drop-oldest queue discards
/// events for crews that had nothing to do with the rate limit. A longer
/// delay is not honoured by waiting: the call fails, the caller counts it,
/// and the next event tries again, spaced by the per-crew cooldown for a
/// creation and by the event stream itself for a send. Blocking the only
/// actor for up to a minute is the worse of the two.
const MAX_INLINE_RETRY: Duration = Duration::from_secs(3);

/// A crew whose room creation failed: when it may be attempted again, and
/// the wait that produced that instant, which doubles on each further
/// failure up to `CREATE_COOLDOWN_MAX`.
#[derive(Debug, Clone)]
struct Cooldown {
    until: Instant,
    wait: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Configure(DaemonConfig),
    Activate { agent: String, config: AgentConfig },
    Deactivate { agent: String },
    Events(Vec<HookEvent>),
    Phases(Vec<PhaseChange>),
    Inbound(Inbound),
}

/// A bounded queue that drops its oldest entry rather than blocking its
/// producer: `observe` is a daemon-to-plugin HTTP call and must return.
pub struct Queue {
    inner: Mutex<VecDeque<Command>>,
    notify: Notify,
    dropped: IntCounter,
}

impl Queue {
    pub fn new(dropped: IntCounter) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::with_capacity(QUEUE)),
            notify: Notify::new(),
            dropped,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Command>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push(&self, command: Command) {
        {
            let mut q = self.lock();
            if q.len() >= QUEUE {
                q.pop_front();
                self.dropped.inc();
            }
            q.push_back(command);
        }
        // `notify_one` stores a permit when nobody is waiting, so a pop
        // that arrives afterwards returns at once: no lost wakeups.
        self.notify.notify_one();
    }

    pub async fn pop(&self) -> Command {
        loop {
            if let Some(command) = self.lock().pop_front() {
                return command;
            }
            self.notify.notified().await;
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The metric families of Spec G §10.
#[derive(Debug, Clone)]
pub struct Counters {
    pub messages_sent: IntCounterVec,
    pub events_dropped: IntCounter,
    pub inbound: IntCounterVec,
    pub rooms: IntGauge,
    pub threads_open: IntGauge,
    pub errors: IntCounterVec,
    pub answers_mismatched: IntCounter,
}

impl Counters {
    pub fn new(metrics: &Metrics) -> Result<Self, SdkError> {
        Ok(Self {
            messages_sent: metrics.int_counter_vec(
                "messages_sent_total",
                "Messages sent to Matrix, by kind",
                &["kind"],
            )?,
            events_dropped: metrics.int_counter(
                "events_dropped_total",
                "Commands dropped because the queue was full",
            )?,
            inbound: metrics.int_counter_vec(
                "inbound_total",
                "Matrix messages seen, by what became of them",
                &["outcome"],
            )?,
            rooms: metrics.int_gauge("rooms", "Crew rooms the plugin knows")?,
            threads_open: metrics
                .int_gauge("threads_open", "Agent sessions with an open thread")?,
            errors: metrics.int_counter_vec(
                "errors_total",
                "Matrix failures, by kind",
                &["kind"],
            )?,
            answers_mismatched: metrics.int_counter(
                "answers_mismatched_total",
                "Answers Claude recorded differently from what the thread chose",
            )?,
        })
    }
}

/// What `Plugin::health` reports. The actor writes it; the plugin reads it.
#[derive(Debug, Clone, Default)]
pub struct Health(Arc<Mutex<Option<String>>>);

impl Health {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn ok(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    pub fn fail(&self, message: String) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
    }
    pub fn get(&self) -> Result<(), String> {
        match self.0.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(m) => Err(m),
            None => Ok(()),
        }
    }
}

/// The one owner of every mutable piece of state (Spec G-10). Generic over
/// the port rather than holding a trait object, because the port's methods
/// return `impl Future` and so are not dyn-compatible (G-13).
pub struct Actor<M: MatrixPort> {
    host: Host,
    port: M,
    counters: Counters,
    health: Health,
    config: Option<DaemonConfig>,
    /// Commands that arrived before `Configure`. The daemon may `activate`
    /// before the SDK's `configure` returns, so nothing may be dropped.
    pending: Vec<Command>,
    agents: HashMap<String, AgentConfig>,
    maps: Maps,
    /// The `AskUserQuestion` each agent is waiting on (Spec J §7).
    questions: Questions,
    /// Crews whose room creation the homeserver refused, and when each may
    /// be tried again. Only failures are kept: a crew whose room exists is
    /// answered from `maps` and never reaches the creation path again.
    cooldowns: HashMap<String, Cooldown>,
}

impl<M: MatrixPort> Actor<M> {
    pub fn new(host: Host, port: M, counters: Counters, health: Health) -> Self {
        Self {
            host,
            port,
            counters,
            health,
            config: None,
            pending: Vec::new(),
            agents: HashMap::new(),
            maps: Maps::new(),
            questions: Questions::default(),
            cooldowns: HashMap::new(),
        }
    }

    /// Whether this crew's room creation is still inside the wait its last
    /// failure earned.
    fn cooling_down(&self, crew: &str) -> bool {
        self.cooldowns
            .get(crew)
            .is_some_and(|c| Instant::now() < c.until)
    }

    /// Records a refused creation, doubling the wait the crew has already
    /// served, up to the cap.
    fn cool_down(&mut self, crew: &str) {
        let wait = match self.cooldowns.get(crew) {
            Some(c) => (c.wait * 2).min(CREATE_COOLDOWN_MAX),
            None => CREATE_COOLDOWN_MIN,
        };
        self.cooldowns.insert(
            crew.to_string(),
            Cooldown {
                until: Instant::now() + wait,
                wait,
            },
        );
    }

    /// Restores the room and thread maps from KV, so a restart resumes.
    pub async fn load(&mut self) {
        match Maps::load(&self.host).await {
            Ok(maps) => self.maps = maps,
            Err(e) => tracing::warn!("matrix: loading maps: {e}"),
        }
        match Questions::load(&self.host).await {
            Ok(questions) => self.questions = questions,
            Err(e) => tracing::warn!("matrix: loading questions: {e}"),
        }
        self.publish_gauges();
    }

    pub async fn run(mut self, queue: Arc<Queue>) {
        loop {
            let command = queue.pop().await;
            self.handle(command).await;
        }
    }

    pub async fn handle(&mut self, command: Command) {
        if let Command::Configure(config) = command {
            self.config = Some(config);
            for buffered in std::mem::take(&mut self.pending) {
                Box::pin(self.handle(buffered)).await;
            }
            return;
        }
        if self.config.is_none() {
            self.pending.push(command);
            return;
        }
        match command {
            Command::Configure(_) => unreachable!("handled above"),
            Command::Activate { agent, config } => {
                self.agents.insert(agent, config);
            }
            Command::Deactivate { agent } => {
                self.agents.remove(&agent);
                self.questions.clear(&self.host, &agent).await;
                if let Err(e) = self.maps.forget(&self.host, &agent).await {
                    tracing::warn!("matrix: forgetting {agent}: {e}");
                }
                self.publish_gauges();
            }
            Command::Events(events) => {
                for event in events {
                    self.on_event(event).await;
                }
            }
            Command::Phases(changes) => {
                for change in changes {
                    self.on_phase(change).await;
                }
            }
            Command::Inbound(message) => self.on_inbound(message).await,
        }
    }

    fn publish_gauges(&self) {
        self.counters.rooms.set(self.maps.rooms_len() as i64);
        self.counters
            .threads_open
            .set(self.maps.open_threads() as i64);
    }

    /// The crew's room: known, then pinned, then created. `None` means the
    /// homeserver refused, which is reported and tried again on a later
    /// event — not the very next one, if the refusal earned a cooldown.
    async fn room_for(&mut self, agent: &str) -> Option<String> {
        let crew = crew_of(agent)?.to_string();
        if let Some(room) = self.maps.room(&crew) {
            return Some(room.to_string());
        }
        let (pinned, invite) = {
            let config = self.config.as_ref()?;
            (config.rooms.get(&crew).cloned(), config.invite.clone())
        };
        let room = match pinned {
            Some(room) => room,
            None => {
                if self.cooling_down(&crew) {
                    // The homeserver refused this crew's room recently.
                    // Asking again on every hook event is what turns one
                    // misconfiguration into a flood; the health cell still
                    // carries the failure, so nothing is hidden by waiting.
                    tracing::debug!("matrix: room creation for {crew} is cooling down");
                    return None;
                }
                let name = format!("balerix {crew}");
                // Bound before the match: the closure borrows `self`, and
                // the arms below need it back to record the outcome.
                let created = retry_once(|| self.port.create_room(&name, &invite)).await;
                match created {
                    Ok(room) => {
                        self.cooldowns.remove(&crew);
                        // The only proven round trip on this path, so the
                        // only place the health cell may go green again.
                        // The pinned arm above contacts no homeserver at
                        // all: clearing there would let the first event of
                        // a crew with a pre-pinned room wipe the failure
                        // the inbound pump recorded when it gave up on a
                        // dead session, while no reply can still arrive.
                        self.health.ok();
                        room
                    }
                    Err(e) => {
                        self.cool_down(&crew);
                        self.counters
                            .errors
                            .with_label_values(&["create_room"])
                            .inc();
                        self.health.fail(format!("create room for {crew}: {e}"));
                        tracing::warn!("matrix: create room for {crew}: {e}");
                        return None;
                    }
                }
            }
        };
        if let Err(e) = self.maps.set_room(&self.host, &crew, &room).await {
            tracing::warn!("matrix: storing room for {crew}: {e}");
        }
        self.publish_gauges();
        Some(room)
    }

    /// One send. `None` means the message did not land, and the caller must
    /// not go on to post anything that depends on it.
    async fn send(
        &self,
        room: &str,
        thread_root: Option<&str>,
        body: &str,
        kind: &str,
    ) -> Option<String> {
        // A body past the limit becomes several messages rather than one
        // truncated one (Spec G §8). The first part's id is the one the
        // caller keeps: a thread roots on its opening message.
        let max_parts = self
            .config
            .as_ref()
            .map(|c| c.max_parts)
            .unwrap_or(crate::config::DEFAULT_MAX_PARTS);
        let mut first = None;
        for part in render::split(body, max_parts) {
            match retry_once(|| self.port.send(room, thread_root, &part)).await {
                Ok(id) => {
                    self.counters.messages_sent.with_label_values(&[kind]).inc();
                    first.get_or_insert(id);
                }
                Err(e) => {
                    self.counters.errors.with_label_values(&["send"]).inc();
                    tracing::warn!("matrix: send to {room}: {e}");
                    // Stopping beats a gap in the middle of a reply.
                    return first;
                }
            }
        }
        first
    }

    async fn on_event(&mut self, event: HookEvent) {
        let Some(config) = self.agents.get(&event.agent).cloned() else {
            return;
        };
        if !config.enabled {
            return;
        }
        // Spec J §7.1: these prove no dialog is on screen any more. Done
        // before the thread logic, which returns early for `SessionStart`.
        if matches!(
            event.name.as_str(),
            "Stop" | "UserPromptSubmit" | "SessionStart" | "SessionEnd"
        ) {
            self.questions.clear(&self.host, &event.agent).await;
        }
        let Some(room) = self.room_for(&event.agent).await else {
            return;
        };
        let session = event.session_id.clone().unwrap_or_default();

        // A thread is opened for a new session id, and for an agent we
        // have no thread for at all: the plugin may have started mid
        // session, and an event must never be dropped for want of a root.
        let opening = match self.maps.thread(&event.agent) {
            None => true,
            Some(thread) => !session.is_empty() && thread.session_id != session,
        };
        if opening {
            let source = event
                .payload
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("already running");
            // `thread_root` interpolates the hook payload's `source`, so
            // like every other body it goes through the 4000-character cut.
            let body = render::thread_root(&event.agent, &session, source);
            let Some(root) = self.send(&room, None, &body, "root").await else {
                return;
            };
            let thread = Thread {
                session_id: session,
                root,
                room: room.clone(),
                closed: false,
            };
            if let Err(e) = self.maps.set_thread(&self.host, &event.agent, thread).await {
                // `Maps` writes the store before memory, so memory still
                // holds whatever thread this one was replacing. Going on
                // would re-read *that* root and file this session's
                // message under the previous session's thread. Dropping
                // one message beats putting it in the wrong conversation.
                tracing::warn!("matrix: storing thread for {}: {e}", event.agent);
                return;
            }
            self.publish_gauges();
            // The root already says the session started.
            if event.name == "SessionStart" {
                return;
            }
        } else if let Some(mut thread) =
            self.maps.thread(&event.agent).filter(|t| t.closed).cloned()
        {
            // The session is emitting again after a `SessionEnd` — an
            // operator resuming it — so its thread is alive whatever the
            // record says. Left closed, the user would watch the agent
            // work in a thread where inbound routing refuses every reply
            // as stale (Spec G-12), and `threads_open` would undercount
            // from here on. A failed write leaves only the flag stale,
            // the root being unchanged, so this posts anyway rather than
            // dropping the event.
            thread.closed = false;
            if let Err(e) = self.maps.set_thread(&self.host, &event.agent, thread).await {
                tracing::warn!("matrix: reopening thread for {}: {e}", event.agent);
            }
            self.publish_gauges();
        }

        if self.on_question_event(&config, &room, &event).await {
            return;
        }
        if !config.wants(&event.name) {
            return;
        }
        let Some(root) = self.maps.thread(&event.agent).map(|t| t.root.clone()) else {
            return;
        };
        let body = render::event_message(&event);
        self.send(&room, Some(&root), &body, "event").await;

        if event.name == "SessionEnd" {
            if let Err(e) = self.maps.close_thread(&self.host, &event.agent).await {
                tracing::warn!("matrix: closing thread for {}: {e}", event.agent);
            }
            self.publish_gauges();
        }
    }

    /// The `AskUserQuestion` lifecycle (Spec J §5, §7.3). `true` when the
    /// event was handled here and must not also post as a generic line.
    async fn on_question_event(
        &mut self,
        config: &AgentConfig,
        room: &str,
        event: &HookEvent,
    ) -> bool {
        let is_question =
            event.payload.get("tool_name").and_then(Value::as_str) == Some(question::TOOL);
        // The question is the detailed form of "needs you", so either event
        // being wanted shows it. Tracking below never depends on this (J-7).
        let shown = config.wants("Notification") || config.wants("PreToolUse");
        let root = self.maps.thread(&event.agent).map(|t| t.root.clone());
        match event.name.as_str() {
            "PreToolUse" if is_question => {
                let input = event
                    .payload
                    .get("tool_input")
                    .cloned()
                    .unwrap_or(Value::Null);
                let Some(questions) = question::parse(&input) else {
                    return false; // posts as `running AskUserQuestion`, as before
                };
                let body = render::question_message(&questions);
                self.questions
                    .open(&self.host, &event.agent, &input, questions)
                    .await;
                if shown && let Some(root) = root {
                    self.send(room, Some(&root), &body, "question").await;
                }
                true
            }
            "Notification"
                if self.questions.is_open(&event.agent)
                    && event
                        .payload
                        .get("notification_type")
                        .and_then(Value::as_str)
                        == Some("permission_prompt") =>
            {
                true // says nothing the question has not
            }
            "PostToolUse" if is_question => {
                let Some(open) = self.questions.clear(&self.host, &event.agent).await else {
                    return false;
                };
                let answers = event
                    .payload
                    .pointer("/tool_response/answers")
                    .cloned()
                    .unwrap_or(Value::Null);
                let recorded = question::describe_recorded(&open.questions, &answers);
                match open.stage {
                    Stage::Sent {
                        selections: Some(selections),
                        echo,
                    } => {
                        if question::recorded_matches(&open.questions, &selections, &answers) {
                            if let Some(echo) = echo {
                                self.react_to(room, &echo, CONFIRMED).await;
                            }
                        } else {
                            // Posted whatever the filter says: it answers
                            // the operator's own action.
                            self.counters.answers_mismatched.inc();
                            let body = format!(
                                "**recorded answer differs** — Claude recorded {recorded}; \
                                 you chose {}. Tell the agent if that matters.",
                                question::describe(&open.questions, &selections)
                            );
                            if let Some(root) = root {
                                self.send(room, Some(&root), &body, "question").await;
                            }
                        }
                    }
                    Stage::Sent {
                        selections: None, ..
                    } => {}
                    Stage::Open | Stage::Confirming { .. } => {
                        if shown && let Some(root) = root {
                            let body = format!("**answered at the terminal** {recorded}");
                            self.send(room, Some(&root), &body, "question").await;
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    async fn on_phase(&mut self, change: PhaseChange) {
        let Some(config) = self.agents.get(&change.agent).cloned() else {
            return;
        };
        if !config.enabled || !config.phases {
            return;
        }
        let Some(room) = self.room_for(&change.agent).await else {
            return;
        };
        let root = self.maps.thread(&change.agent).map(|t| t.root.clone());
        let body = render::phase_message(&change);
        self.send(&room, root.as_deref(), &body, "phase").await;
    }

    /// A reply in a live thread of ours becomes a `send_text` (Spec G §9).
    /// Every other shape is counted under its own outcome, and the ones a
    /// person should see get a reaction.
    async fn on_inbound(&mut self, message: Inbound) {
        let count = |outcome: &str| self.counters.inbound.with_label_values(&[outcome]).inc();

        if message.sender == self.port.user_id() {
            count("own_message");
            return;
        }
        let Some(root) = message.thread_root.clone() else {
            count("not_a_thread");
            self.react(&message, REFUSED).await;
            return;
        };
        let Some(agent) = self.maps.route(&message.room, &root).map(str::to_string) else {
            // A thread in a room we are in that is not one of ours. Silent
            // on purpose: reacting to every unrelated thread would be noise.
            count("unknown_thread");
            return;
        };
        if self.maps.thread(&agent).is_some_and(|t| t.closed) {
            count("stale_thread");
            self.react(&message, REFUSED).await;
            return;
        }

        // Spec J-7: while a question is open a reply is an answer, never a
        // prompt. `send_text` here would type the body into the dialog and
        // its Enter would pick whatever row is highlighted.
        if self.questions.is_open(&agent) {
            self.on_answer(&agent, &root, &message).await;
            return;
        }

        let action = PluginAction::SendText {
            text: message.body.clone(),
            submit: true,
        };
        match self.host.action(&agent, &action).await {
            Ok(()) => {
                count("routed");
                self.react(&message, ACK).await;
            }
            Err(e) => {
                count("send_failed");
                self.counters.errors.with_label_values(&["send_text"]).inc();
                let body = format!("**not delivered to {agent}:** {e}");
                self.send(&message.room, Some(&root), &body, "notice").await;
                self.react(&message, FAILED).await;
            }
        }
    }

    /// A thread reply while `agent` has a question open (Spec J §7.2).
    async fn on_answer(&mut self, agent: &str, root: &str, message: &Inbound) {
        let count = |outcome: &str| self.counters.inbound.with_label_values(&[outcome]).inc();
        let Some(open) = self.questions.get(agent).cloned() else {
            return;
        };
        match &open.stage {
            Stage::Sent { .. } => {
                count("answer_refused");
                let body = "an answer is already on its way; wait for the agent.";
                self.send(&message.room, Some(root), body, "question").await;
                self.react(message, REFUSED).await;
                return;
            }
            Stage::Confirming { selections, echo } => {
                match message.body.trim().to_ascii_lowercase().as_str() {
                    "yes" | "y" => {
                        count("confirmed");
                        let (selections, echo) = (selections.clone(), echo.clone());
                        self.deliver(
                            agent,
                            root,
                            message,
                            &open.questions,
                            Some(selections),
                            echo,
                        )
                        .await;
                        return;
                    }
                    "no" | "n" => {
                        self.questions.set_stage(agent, Stage::Open);
                        self.react(message, ACK).await;
                        return;
                    }
                    _ => {} // anything else is a fresh answer, matched below
                }
            }
            Stage::Open => {}
        }

        match question::match_reply(&open.questions, &message.body) {
            Err(question::Refusal(reason)) => {
                count("answer_refused");
                self.questions.set_stage(agent, Stage::Open);
                self.send(&message.room, Some(root), &reason, "question")
                    .await;
                self.react(message, REFUSED).await;
            }
            Ok(question::Matched::Skip) => {
                let echo = self
                    .send(
                        &message.room,
                        Some(root),
                        "**declining the question**",
                        "question",
                    )
                    .await;
                self.deliver(agent, root, message, &open.questions, None, echo)
                    .await;
            }
            Ok(question::Matched::Answers { selections, exact }) => {
                let chosen = question::describe(&open.questions, &selections);
                if exact {
                    let body = format!("**answering** {chosen}");
                    let echo = self
                        .send(&message.room, Some(root), &body, "question")
                        .await;
                    self.deliver(
                        agent,
                        root,
                        message,
                        &open.questions,
                        Some(selections),
                        echo,
                    )
                    .await;
                } else {
                    count("confirm_asked");
                    let body = format!("**I read that as** {chosen}. Reply **yes** to send.");
                    let echo = self
                        .send(&message.room, Some(root), &body, "question")
                        .await;
                    self.questions
                        .set_stage(agent, Stage::Confirming { selections, echo });
                }
            }
        }
    }

    /// Sends the keys. `selections` is `None` for a `skip`.
    async fn deliver(
        &mut self,
        agent: &str,
        root: &str,
        message: &Inbound,
        questions: &[question::Question],
        selections: Option<Vec<question::Selection>>,
        echo: Option<String>,
    ) {
        let count = |outcome: &str| self.counters.inbound.with_label_values(&[outcome]).inc();
        let steps = match &selections {
            Some(selections) => question::plan(questions, selections),
            None => question::skip_plan(),
        };
        let delay_ms = self
            .agents
            .get(agent)
            .map(|c| c.key_delay_ms)
            .unwrap_or(balerix_api::DEFAULT_KEY_DELAY_MS);
        let action = PluginAction::SendKeys { steps, delay_ms };
        // The daemon would refuse it; say why here, before anything is sent.
        if let Err(reason) = action.validate() {
            count("answer_refused");
            self.questions.set_stage(agent, Stage::Open);
            let body = format!(
                "this answer needs more keystrokes than can be sent from here ({reason}); \
                 answer at the terminal."
            );
            self.send(&message.room, Some(root), &body, "question")
                .await;
            self.react(message, REFUSED).await;
            return;
        }
        match self.host.action(agent, &action).await {
            Ok(()) => {
                count(if selections.is_some() {
                    "answered"
                } else {
                    "skipped"
                });
                self.questions
                    .set_stage(agent, Stage::Sent { selections, echo });
                self.react(message, ACK).await;
            }
            Err(e) => {
                count("send_failed");
                self.counters.errors.with_label_values(&["send_keys"]).inc();
                self.questions.set_stage(agent, Stage::Open);
                let body = format!("**not delivered to {agent}:** {e}");
                self.send(&message.room, Some(root), &body, "notice").await;
                self.react(message, FAILED).await;
            }
        }
    }

    async fn react(&self, message: &Inbound, key: &str) {
        self.react_to(&message.room, &message.event_id, key).await;
    }

    async fn react_to(&self, room: &str, event_id: &str, key: &str) {
        if let Err(e) = self.port.react(room, event_id, key).await {
            self.counters.errors.with_label_values(&["react"]).inc();
            tracing::warn!("matrix: reacting to {event_id}: {e}");
        }
    }
}

/// One port call, retried once on a rate limit after the delay the
/// homeserver itself asked for — but only when that delay is short enough
/// to be worth standing the whole actor still for (`MAX_INLINE_RETRY`).
/// A longer delay is returned to the caller, which counts it and moves on.
/// Room creation is rate limited as readily as a message, so both go
/// through here.
async fn retry_once<T, F, Fut>(call: F) -> Result<T, MatrixError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, MatrixError>>,
{
    let attempt = call().await;
    if let Err(MatrixError::RateLimited { retry_after_ms }) = attempt {
        let delay = Duration::from_millis(retry_after_ms);
        if delay > MAX_INLINE_RETRY {
            tracing::warn!(
                "matrix: the homeserver asked for {delay:?}, too long to hold the actor \
                 for; the next event will try again"
            );
            return attempt;
        }
        tokio::time::sleep(delay).await;
        return call().await;
    }
    attempt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::fake::{Call, FakePort};
    use balerix_plugin_sdk::testing::{FakeHost, event};
    use balerix_plugin_sdk::{Host, Metrics};
    use serde_json::json;

    fn counters() -> Counters {
        Counters::new(&Metrics::new("matrix")).unwrap()
    }

    fn deactivate(agent: &str) -> Command {
        Command::Deactivate {
            agent: agent.to_string(),
        }
    }

    #[tokio::test]
    async fn the_queue_is_fifo_and_wakes_a_waiting_pop() {
        let c = counters();
        let q = Queue::new(c.events_dropped.clone());
        let popper = {
            let q = q.clone();
            tokio::spawn(async move { q.pop().await })
        };
        // Hand control to the scheduler so the popper actually runs, finds
        // the queue empty, and parks on `notified()` before the push below
        // exercises the wake path. Nothing here can assert that it parked —
        // the queue is empty either way — so the yield is what makes the
        // test reach that path at all, and the pop returning below is what
        // proves the wake happened.
        tokio::task::yield_now().await;
        q.push(deactivate("f/c/a"));
        assert_eq!(popper.await.unwrap(), deactivate("f/c/a"));

        q.push(deactivate("one"));
        q.push(deactivate("two"));
        assert_eq!(q.pop().await, deactivate("one"));
        assert_eq!(q.pop().await, deactivate("two"));
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_oldest_and_counts_it() {
        let c = counters();
        let q = Queue::new(c.events_dropped.clone());
        for i in 0..QUEUE {
            q.push(deactivate(&format!("a{i}")));
        }
        assert_eq!(q.len(), QUEUE);
        assert_eq!(c.events_dropped.get(), 0);

        q.push(deactivate("newest"));
        assert_eq!(q.len(), QUEUE, "capacity is held");
        assert_eq!(c.events_dropped.get(), 1);
        assert_eq!(
            q.pop().await,
            deactivate("a1"),
            "the oldest was dropped, not the newest"
        );
    }

    #[test]
    fn health_starts_ok_and_reports_the_last_failure_until_cleared() {
        let h = Health::new();
        assert_eq!(h.get(), Ok(()));
        h.fail("create room for f/c: no rights".into());
        assert_eq!(h.get(), Err("create room for f/c: no rights".into()));
        h.ok();
        assert_eq!(h.get(), Ok(()));
    }

    #[test]
    fn every_metric_family_carries_the_plugin_prefix() {
        let m = Metrics::new("matrix");
        let c = Counters::new(&m).unwrap();
        c.messages_sent.with_label_values(&["event"]).inc();
        c.inbound.with_label_values(&["routed"]).inc();
        c.errors.with_label_values(&["send"]).inc();
        c.rooms.set(2);
        c.threads_open.set(3);
        c.events_dropped.inc();
        c.answers_mismatched.inc();
        let text = m.render().unwrap();
        for family in [
            "balerix_plugin_matrix_messages_sent_total",
            "balerix_plugin_matrix_events_dropped_total",
            "balerix_plugin_matrix_inbound_total",
            "balerix_plugin_matrix_rooms",
            "balerix_plugin_matrix_threads_open",
            "balerix_plugin_matrix_errors_total",
            "balerix_plugin_matrix_answers_mismatched_total",
        ] {
            assert!(text.contains(family), "missing {family} in\n{text}");
        }
    }

    fn daemon_config() -> DaemonConfig {
        crate::config::parse_daemon(&json!({
            "homeserver": "https://h",
            "userId": "@balerix:example.org",
            "password": "pw",
            "invite": ["@rahul:example.org"]
        }))
        .unwrap()
    }

    fn agent_config(events: &[&str]) -> AgentConfig {
        crate::config::parse_agent(&json!({ "events": events })).unwrap()
    }

    fn started(agent: &str, session: &str, source: &str) -> HookEvent {
        let mut e = event(agent, "SessionStart", json!({ "source": source }));
        e.session_id = Some(session.to_string());
        e
    }

    fn during(agent: &str, session: &str, name: &str, payload: serde_json::Value) -> HookEvent {
        let mut e = event(agent, name, payload);
        e.session_id = Some(session.to_string());
        e
    }

    /// An actor holding the health cell the test also keeps, for the two
    /// tests that assert on what does and does not clear a failure.
    async fn actor_watching_health() -> (FakeHost, FakePort, Actor<FakePort>, Health) {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        let port = FakePort::new("@balerix:example.org");
        let health = Health::new();
        let a = Actor::new(host, port.clone(), counters(), health.clone());
        (fake, port, a, health)
    }

    async fn actor() -> (FakeHost, FakePort, Actor<FakePort>) {
        let (fake, port, a, _health) = actor_watching_health().await;
        (fake, port, a)
    }

    /// The event id `FakePort` returned for the root send recorded at
    /// `index`: it mints `$evt<n>:fake` on its nth successful call, so the
    /// id is knowable from the call sequence alone. Asserting a child
    /// against *this* is the point — a child threaded under the session id,
    /// the room id or any other non-empty string satisfies `is_some()` but
    /// lands nowhere a Matrix client would show it.
    fn minted_root(calls: &[Call], index: usize) -> String {
        assert!(
            matches!(
                calls.get(index),
                Some(Call::Send {
                    thread_root: None,
                    ..
                })
            ),
            "call {index} is not a thread root: {calls:?}"
        );
        format!("$evt{}:fake", index + 1)
    }

    fn creations(calls: &[Call]) -> usize {
        calls
            .iter()
            .filter(|c| matches!(c, Call::CreateRoom { .. }))
            .count()
    }

    fn sends(calls: &[Call]) -> Vec<(Option<String>, String)> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::Send {
                    thread_root, body, ..
                } => Some((thread_root.clone(), body.clone())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn commands_before_configure_are_buffered_and_replayed_in_order() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert!(port.calls().is_empty(), "nothing before configure");

        a.handle(Command::Configure(daemon_config())).await;
        let calls = port.calls();
        assert!(
            matches!(calls.first(), Some(Call::CreateRoom { .. })),
            "the room comes first: {calls:?}"
        );
        assert_eq!(sends(&calls).len(), 1, "then the thread root");
        assert_eq!(sends(&calls)[0].0, None, "the root is not a thread reply");
    }

    #[tokio::test]
    async fn two_agents_in_one_crew_share_a_single_room() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        for name in ["alice", "bob"] {
            a.handle(Command::Activate {
                agent: format!("f/c/{name}"),
                config: agent_config(&["Notification"]),
            })
            .await;
            a.handle(Command::Events(vec![started(
                &format!("f/c/{name}"),
                "s1",
                "startup",
            )]))
            .await;
        }
        let calls = port.calls();
        assert_eq!(creations(&calls), 1, "one room per crew");
        // Not just two roots and one creation: both roots have to have gone
        // to that one room. `sends` drops the room, so read it here.
        let rooms: Vec<&String> = calls
            .iter()
            .filter_map(|c| match c {
                Call::Send {
                    room,
                    thread_root: None,
                    ..
                } => Some(room),
                _ => None,
            })
            .collect();
        assert_eq!(rooms.len(), 2, "one root per agent: {calls:?}");
        assert_eq!(rooms[0], rooms[1], "and both in the same room");
    }

    #[tokio::test]
    async fn a_root_exists_before_any_child_and_children_are_thread_replies() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "Notification",
                json!({ "message": "needs permission" }),
            ),
        ]))
        .await;

        let calls = port.calls();
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&calls, 1);
        let s = sends(&calls);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].0, None, "root first");
        assert!(s[0].1.contains("session"), "{}", s[0].1);
        assert_eq!(
            s[1].0,
            Some(root),
            "the child hangs off the id the root send returned"
        );
        assert!(s[1].1.contains("needs permission"), "{}", s[1].1);
    }

    #[tokio::test]
    async fn a_same_id_session_start_reuses_the_thread_and_a_new_id_opens_another() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&port.take_calls(), 1);

        a.handle(Command::Events(vec![started("f/c/alice", "s1", "compact")]))
            .await;
        let s = sends(&port.take_calls());
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].0,
            Some(root),
            "a compaction posts inside the very thread the root opened"
        );
        assert!(s[0].1.contains("restarted"), "{}", s[0].1);

        a.handle(Command::Events(vec![started("f/c/alice", "s2", "clear")]))
            .await;
        let s = sends(&port.take_calls());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, None, "a new session id opens a new root");
    }

    #[tokio::test]
    async fn session_end_closes_the_thread_and_the_filter_drops_unwanted_events() {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "PreToolUse",
                json!({ "tool_name": "Bash" }),
            ),
            during(
                "f/c/alice",
                "s1",
                "SessionEnd",
                json!({ "reason": "clear" }),
            ),
        ]))
        .await;

        let bodies: Vec<String> = sends(&port.calls()).into_iter().map(|(_, b)| b).collect();
        assert_eq!(bodies.len(), 2, "PreToolUse is filtered out: {bodies:?}");
        assert!(bodies[1].contains("session ended"), "{}", bodies[1]);

        let stored = fake.kv_json("thread/f/c/alice").unwrap();
        assert_eq!(stored["closed"], true);
    }

    #[tokio::test]
    async fn a_session_start_after_a_session_end_reopens_the_same_thread() {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "SessionEnd",
                json!({ "reason": "clear" }),
            ),
        ]))
        .await;
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&port.take_calls(), 1);
        assert_eq!(fake.kv_json("thread/f/c/alice").unwrap()["closed"], true);

        // The operator resumes that very session.
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "resume")]))
            .await;

        let s = sends(&port.take_calls());
        assert_eq!(s.len(), 1, "the same session opens no second root");
        assert_eq!(s[0].0, Some(root), "the restart posts inside that thread");
        let stored = fake.kv_json("thread/f/c/alice").unwrap();
        assert_eq!(
            stored["closed"], false,
            "and the thread is open again, so a reply in it still routes"
        );

        // A later event posts in the same thread and leaves it open.
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "Notification",
            json!({ "message": "needs permission" }),
        )]))
        .await;
        assert_eq!(fake.kv_json("thread/f/c/alice").unwrap()["closed"], false);
    }

    #[tokio::test]
    async fn a_disabled_agent_and_an_unknown_agent_post_nothing() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: crate::config::parse_agent(&json!({ "enabled": false })).unwrap(),
        })
        .await;
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            started("f/c/ghost", "s1", "startup"),
        ]))
        .await;
        assert!(port.calls().is_empty(), "{:?}", port.calls());
    }

    #[tokio::test]
    async fn a_pinned_room_is_used_instead_of_creating_one() {
        let (_fake, port, mut a) = actor().await;
        let mut cfg = daemon_config();
        cfg.rooms.insert("f/c".into(), "!pinned:example.org".into());
        a.handle(Command::Configure(cfg)).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;

        assert!(
            !port
                .calls()
                .iter()
                .any(|c| matches!(c, Call::CreateRoom { .. })),
            "no room was created"
        );
        assert!(matches!(
            port.calls().first(),
            Some(Call::Send { room, .. }) if room == "!pinned:example.org"
        ));
    }

    /// `health.ok()` may only ever follow a proven round trip to the
    /// homeserver. The pinned-room path contacts no server, so the first
    /// event for a crew with a pre-pinned room must not clear the failure
    /// the inbound pump recorded when it gave up on a dead session: no
    /// reply can still arrive, and `plugin list` has to keep saying so.
    #[tokio::test]
    async fn a_pinned_room_does_not_clear_a_failure_it_never_disproved() {
        let (_fake, port, mut a, health) = actor_watching_health().await;
        let mut cfg = daemon_config();
        cfg.rooms.insert("f/c".into(), "!pinned:example.org".into());
        a.handle(Command::Configure(cfg)).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        let deaf = "sync: the homeserver rejected our session".to_string();
        health.fail(deaf.clone());

        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;

        assert_eq!(sends(&port.calls()).len(), 1, "the event was still posted");
        assert_eq!(
            health.get(),
            Err(deaf),
            "a path that reached no homeserver cleared the failure"
        );
    }

    /// A refused room creation earns a cooldown, and the very next event
    /// must not ask again: a busy crew fires tens to hundreds of events a
    /// minute, and a room the homeserver will not create refuses every one
    /// of them, which is how the plugin earns a rate limit. Each further
    /// refusal doubles the wait; a success clears both the cooldown and the
    /// health failure.
    #[tokio::test]
    async fn a_failed_room_creation_cools_down_before_it_is_tried_again() {
        let (_fake, port, mut a, health) = actor_watching_health().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        async fn start(a: &mut Actor<FakePort>) {
            a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
                .await;
        }
        // The cooldown runs on the real clock, which the actor has no seam
        // for, so the test moves the deadline rather than waiting it out.
        let expire = |a: &mut Actor<FakePort>| {
            for c in a.cooldowns.values_mut() {
                c.until = Instant::now();
            }
        };

        // `FakePort` records only the calls it lets through, so a recorded
        // `CreateRoom` is the proof an attempt was made and a cooldown the
        // proof one was refused.
        port.fail_next(crate::matrix::MatrixError::Other("no rights".into()));
        start(&mut a).await;
        assert!(health.get().is_err(), "the failure is visible");
        assert_eq!(
            a.cooldowns["f/c"].wait, CREATE_COOLDOWN_MIN,
            "the refusal earned the first wait"
        );
        assert!(port.take_calls().is_empty());

        start(&mut a).await;
        assert!(
            port.take_calls().is_empty(),
            "the next event asked the homeserver again inside the cooldown"
        );

        expire(&mut a);
        port.fail_next(crate::matrix::MatrixError::Other("no rights".into()));
        start(&mut a).await;
        assert_eq!(
            a.cooldowns["f/c"].wait,
            CREATE_COOLDOWN_MIN * 2,
            "the wait let one attempt through, and its refusal waits twice as long"
        );

        expire(&mut a);
        start(&mut a).await;
        let calls = port.take_calls();
        assert_eq!(creations(&calls), 1);
        assert_eq!(sends(&calls).len(), 1, "and the thread root follows");
        assert_eq!(health.get(), Ok(()), "a created room clears the failure");
        assert!(
            a.cooldowns.is_empty(),
            "and the crew is out of its cooldown"
        );
    }

    /// `retry_once` sleeps inside the one actor task, so a long delay would
    /// stand every crew's events still and let the drop-oldest queue throw
    /// them away. Past `MAX_INLINE_RETRY` the call returns instead, and the
    /// caller counts the error and moves on.
    #[tokio::test]
    async fn a_long_rate_limit_delay_returns_instead_of_holding_the_actor() {
        let calls = std::cell::Cell::new(0);
        let long = MatrixError::RateLimited {
            retry_after_ms: 60_000,
        };
        let started = Instant::now();
        let result: Result<(), MatrixError> = retry_once(|| {
            calls.set(calls.get() + 1);
            let error = long.clone();
            async move { Err(error) }
        })
        .await;

        assert_eq!(result, Err(long));
        assert_eq!(calls.get(), 1, "it must not have retried");
        assert!(
            started.elapsed() < MAX_INLINE_RETRY,
            "the actor slept the delay out: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_failed_root_send_posts_nothing_and_stores_no_thread() {
        let (fake, port, mut a) = actor().await;
        let mut cfg = daemon_config();
        // Pinning the room means the call that fails below is the thread
        // root's send and not the room creation.
        cfg.rooms.insert("f/c".into(), "!pinned:example.org".into());
        a.handle(Command::Configure(cfg)).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;

        port.fail_next(crate::matrix::MatrixError::Other("no rights".into()));
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;

        assert!(sends(&port.calls()).is_empty(), "{:?}", port.calls());
        assert!(
            !fake.kv().contains_key("thread/f/c/alice"),
            "a thread whose root never landed is not a thread"
        );
    }

    #[tokio::test]
    async fn a_child_in_the_same_batch_as_a_failed_root_is_never_posted_rootless() {
        let (fake, port, mut a) = actor().await;
        let mut cfg = daemon_config();
        cfg.rooms.insert("f/c".into(), "!pinned:example.org".into());
        a.handle(Command::Configure(cfg)).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;

        port.fail_next(crate::matrix::MatrixError::Other("no rights".into()));
        a.handle(Command::Events(vec![
            started("f/c/alice", "s1", "startup"),
            during(
                "f/c/alice",
                "s1",
                "Notification",
                json!({ "message": "needs permission" }),
            ),
        ]))
        .await;

        // The `SessionStart`'s root never landed and nothing was stored for
        // it, so the notification had to open a root of its own before it
        // could post: had the actor kept a thread despite the failed send,
        // the first thing recorded here would be a child under a root that
        // does not exist in the room.
        let calls = port.calls();
        let root = minted_root(&calls, 0);
        let s = sends(&calls);
        assert_eq!(s.len(), 2, "a root, then the notification: {calls:?}");
        assert_eq!(s[1].0, Some(root.clone()), "under the root that landed");
        assert!(s[1].1.contains("needs permission"), "{}", s[1].1);
        assert_eq!(
            fake.kv_json("thread/f/c/alice").unwrap()["root"],
            json!(root),
            "and the stored root is the one that landed, not the one that failed"
        );
    }

    /// A body past the limit arrives as several ordered messages in the
    /// same thread, not one truncated message.
    #[tokio::test]
    async fn a_long_body_is_split_across_ordered_thread_messages() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Stop"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        let root = minted_root(&port.take_calls(), 1);

        let long = "abcd\n".repeat(3000);
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "Stop",
            json!({ "last_assistant_message": long }),
        )]))
        .await;

        let s = sends(&port.take_calls());
        assert!(
            s.len() > 1,
            "a long body must split, got {} message(s)",
            s.len()
        );
        for (i, (thread, body)) in s.iter().enumerate() {
            assert_eq!(
                thread.as_deref(),
                Some(root.as_str()),
                "part {} left the thread",
                i + 1
            );
            assert!(
                body.ends_with(&format!("({}/{})", i + 1, s.len())),
                "part {} is out of order or unmarked",
                i + 1
            );
        }
    }

    /// Nothing is posted after a part fails: a gap in the middle of a
    /// reply reads worse than a reply that visibly stops.
    #[tokio::test]
    async fn a_failed_part_stops_the_rest_of_the_body() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Stop"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        port.take_calls();

        // Fails the first part; `retry_once` burns its one retry on it.
        port.fail_next(crate::matrix::MatrixError::Other("nope".into()));
        port.fail_next(crate::matrix::MatrixError::Other("nope again".into()));
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "Stop",
            json!({ "last_assistant_message": "abcd\n".repeat(3000) }),
        )]))
        .await;

        let s = sends(&port.take_calls());
        assert!(
            s.is_empty(),
            "the run must stop at the failed part, got {} message(s)",
            s.len()
        );
    }

    #[tokio::test]
    async fn a_rate_limited_send_is_retried_once() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification"]),
        })
        .await;
        // The room and the thread root have to exist first, or the call the
        // rate limit lands on would be `create_room`, not a send.
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&port.take_calls(), 1);

        port.fail_next(crate::matrix::MatrixError::RateLimited { retry_after_ms: 1 });
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "Notification",
            json!({ "message": "needs permission" }),
        )]))
        .await;

        let calls = port.take_calls();
        assert!(
            calls.iter().all(|c| matches!(c, Call::Send { .. })),
            "the rate limit landed on a send, not a room creation: {calls:?}"
        );
        let s = sends(&calls);
        assert_eq!(s.len(), 1, "the retry got through");
        assert_eq!(s[0].0, Some(root), "and it is still the thread reply");
        assert!(s[0].1.contains("needs permission"), "{}", s[0].1);
    }

    #[tokio::test]
    async fn a_rate_limited_room_creation_is_retried_once() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        // Nothing has been posted yet, so the first port call — and the one
        // the rate limit lands on — is the room creation.
        port.fail_next(crate::matrix::MatrixError::RateLimited { retry_after_ms: 1 });
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;

        let calls = port.calls();
        assert!(
            matches!(calls.first(), Some(Call::CreateRoom { .. })),
            "the retry created the room: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| matches!(c, Call::CreateRoom { .. }))
                .count(),
            1,
            "and only one room came of it"
        );
        assert_eq!(sends(&calls).len(), 1, "the thread root follows it");
    }

    #[tokio::test]
    async fn a_phase_change_posts_into_the_thread_when_there_is_one() {
        let (_fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        // Call 0 is the room creation, so the root is call 1.
        let root = minted_root(&port.take_calls(), 1);
        a.handle(Command::Phases(vec![PhaseChange {
            agent: "f/c/alice".into(),
            from: balerix_api::AgentPhase::Ready,
            to: balerix_api::AgentPhase::Dead,
            message: "window gone".into(),
        }]))
        .await;
        let s = sends(&port.calls());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, Some(root), "in the agent's own thread");
        assert!(s[0].1.contains("window gone"), "{}", s[0].1);
    }

    #[tokio::test]
    async fn deactivate_forgets_the_agent_and_its_thread() {
        let (fake, _port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        assert!(fake.kv().contains_key("thread/f/c/alice"));
        a.handle(deactivate("f/c/alice")).await;
        assert!(!fake.kv().contains_key("thread/f/c/alice"));
    }

    use crate::matrix::{ACK, FAILED, REFUSED};
    use balerix_api::PluginAction;

    fn inbound(room: &str, root: Option<&str>, sender: &str, body: &str) -> Inbound {
        Inbound {
            room: room.to_string(),
            event_id: "$msg:fake".into(),
            sender: sender.to_string(),
            thread_root: root.map(str::to_string),
            body: body.to_string(),
        }
    }

    fn reactions(calls: &[Call]) -> Vec<String> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::React { key, .. } => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    /// An actor with one live thread; returns the room and its root.
    async fn with_thread() -> (FakeHost, FakePort, Actor<FakePort>, String, String) {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&[]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        let (room, root) = match port.calls().last() {
            Some(Call::Send { room, .. }) => (room.clone(), "$evt2:fake".to_string()),
            other => panic!("expected a root send, got {other:?}"),
        };
        port.take_calls();
        (fake, port, a, room, root)
    }

    use crate::matrix::CONFIRMED;
    use crate::pending::Stage;
    use crate::question::fixtures::{color, size};

    fn asks(agent: &str, session: &str, questions: &[serde_json::Value]) -> HookEvent {
        during(
            agent,
            session,
            "PreToolUse",
            json!({ "tool_name": "AskUserQuestion", "tool_input": { "questions": questions } }),
        )
    }

    fn answered(agent: &str, session: &str, answers: serde_json::Value) -> HookEvent {
        during(
            agent,
            session,
            "PostToolUse",
            json!({ "tool_name": "AskUserQuestion", "tool_response": { "answers": answers } }),
        )
    }

    /// `with_thread`, for an agent that posts `Notification` and `Stop`.
    async fn with_question_thread() -> (FakeHost, FakePort, Actor<FakePort>, String, String) {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Configure(daemon_config())).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: agent_config(&["Notification", "Stop"]),
        })
        .await;
        a.handle(Command::Events(vec![started("f/c/alice", "s1", "startup")]))
            .await;
        let room = match port.calls().last() {
            Some(Call::Send { room, .. }) => room.clone(),
            other => panic!("expected a root send, got {other:?}"),
        };
        port.take_calls();
        (fake, port, a, room, "$evt2:fake".to_string())
    }

    #[tokio::test]
    async fn a_question_is_posted_in_the_thread_and_the_permission_line_is_suppressed() {
        let (fake, port, mut a, _room, root) = with_question_thread().await;
        a.handle(Command::Events(vec![
            asks("f/c/alice", "s1", &[color()]),
            during(
                "f/c/alice",
                "s1",
                "Notification",
                json!({ "notification_type": "permission_prompt", "message": "Claude needs your permission" }),
            ),
        ]))
        .await;
        let sent = sends(&port.calls());
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0.as_deref(), Some(root.as_str()));
        assert!(
            sent[0].1.starts_with("**question** · Color"),
            "{}",
            sent[0].1
        );
        assert!(sent[0].1.contains("3. **Blue**"));
        assert!(
            fake.kv_json("question/f/c/alice").is_some(),
            "mirrored to KV"
        );

        // with no question open the same notification posts as before
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "Stop",
            json!({}),
        )]))
        .await;
        port.take_calls();
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "Notification",
            json!({ "notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash" }),
        )]))
        .await;
        assert!(
            sends(&port.calls())[0]
                .1
                .contains("needs your permission to use Bash")
        );
    }

    #[tokio::test]
    async fn a_question_is_tracked_even_when_the_event_filter_hides_it() {
        let (fake, port, mut a, _room, _root) = with_thread().await; // events: []
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        assert!(sends(&port.calls()).is_empty(), "nothing posted");
        assert!(
            a.questions.is_open("f/c/alice"),
            "but J-7's protection is on"
        );
        assert!(fake.kv_json("question/f/c/alice").is_some());
    }

    #[tokio::test]
    async fn every_clearing_event_closes_the_question() {
        for (name, payload) in [
            ("Stop", json!({})),
            ("UserPromptSubmit", json!({ "prompt": "x" })),
            ("SessionEnd", json!({ "reason": "clear" })),
            ("SessionStart", json!({ "source": "clear" })),
        ] {
            let (fake, _port, mut a, _room, _root) = with_question_thread().await;
            a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
                .await;
            assert!(a.questions.is_open("f/c/alice"));
            a.handle(Command::Events(vec![during(
                "f/c/alice",
                "s1",
                name,
                payload,
            )]))
            .await;
            assert!(!a.questions.is_open("f/c/alice"), "{name}");
            assert!(fake.kv_json("question/f/c/alice").is_none(), "{name}");
        }
        let (_fake, _port, mut a, _room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        a.handle(deactivate("f/c/alice")).await;
        assert!(!a.questions.is_open("f/c/alice"), "deactivate");
    }

    #[tokio::test]
    async fn an_answer_given_at_the_terminal_is_reported() {
        let (_fake, port, mut a, _room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks(
            "f/c/alice",
            "s1",
            &[color(), size()],
        )]))
        .await;
        port.take_calls();
        a.handle(Command::Events(vec![answered(
            "f/c/alice",
            "s1",
            json!({ "Which color?": "Blue", "Which size?": "Medium" }),
        )]))
        .await;
        assert_eq!(
            sends(&port.calls())[0].1,
            "**answered at the terminal** Color → Blue · Size → Medium"
        );
        assert!(!a.questions.is_open("f/c/alice"));
    }

    #[tokio::test]
    async fn a_sent_answer_is_confirmed_on_the_echo_or_reported_when_it_differs() {
        let chosen = vec![crate::question::Selection {
            options: vec![2],
            other: None,
        }];
        let sent = |echo: &str| Stage::Sent {
            selections: Some(chosen.clone()),
            echo: Some(echo.to_string()),
        };

        let (_fake, port, mut a, room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        a.questions.set_stage("f/c/alice", sent("$echo:fake"));
        port.take_calls();
        a.handle(Command::Events(vec![answered(
            "f/c/alice",
            "s1",
            json!({ "Which color?": "Blue" }),
        )]))
        .await;
        assert_eq!(
            port.calls(),
            vec![Call::React {
                room: room.clone(),
                event_id: "$echo:fake".into(),
                key: CONFIRMED.into()
            }],
            "a reaction, not another message"
        );

        let (_fake, port, mut a, _room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        a.questions.set_stage("f/c/alice", sent("$echo:fake"));
        port.take_calls();
        a.handle(Command::Events(vec![answered(
            "f/c/alice",
            "s1",
            json!({ "Which color?": "Red" }),
        )]))
        .await;
        let body = &sends(&port.calls())[0].1;
        assert!(body.starts_with("**recorded answer differs**"), "{body}");
        assert!(
            body.contains("Color → Red") && body.contains("Color → Blue"),
            "{body}"
        );
        assert_eq!(a.counters.answers_mismatched.get(), 1);
    }

    #[tokio::test]
    async fn an_open_question_survives_an_actor_restart() {
        let (fake, _port, mut a, _room, _root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", &[color()])]))
            .await;
        drop(a);
        let host = Host::new(fake.env("matrix", std::path::Path::new("scratch"))).unwrap();
        let mut again = Actor::new(
            host,
            FakePort::new("@balerix:example.org"),
            counters(),
            Health::new(),
        );
        again.load().await;
        assert!(again.questions.is_open("f/c/alice"));
    }

    #[tokio::test]
    async fn a_thread_reply_becomes_a_submitted_send_text_and_is_acknowledged() {
        let (fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound(
            &room,
            Some(&root),
            "@rahul:example.org",
            "run the tests",
        )))
        .await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendText {
                text: "run the tests".into(),
                submit: true,
            }]
        );
        assert_eq!(reactions(&port.calls()), vec![ACK.to_string()]);
    }

    #[tokio::test]
    async fn our_own_message_is_ignored_without_a_reaction() {
        let (fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Inbound(inbound(
            &room,
            Some(&root),
            "@balerix:example.org",
            "a message we sent",
        )))
        .await;
        assert!(fake.actions_for("f/c/alice").is_empty());
        assert!(port.calls().is_empty(), "no reaction on our own message");
    }

    #[tokio::test]
    async fn a_room_level_message_is_refused_and_an_unknown_thread_is_silent() {
        let (fake, port, mut a, room, _root) = with_thread().await;
        a.handle(Command::Inbound(inbound(
            &room,
            None,
            "@rahul:example.org",
            "hello room",
        )))
        .await;
        assert!(fake.actions_for("f/c/alice").is_empty());
        assert_eq!(reactions(&port.take_calls()), vec![REFUSED.to_string()]);

        a.handle(Command::Inbound(inbound(
            &room,
            Some("$someone-elses-thread"),
            "@rahul:example.org",
            "not ours",
        )))
        .await;
        assert!(
            port.calls().is_empty(),
            "a thread we do not own gets no reaction"
        );
    }

    #[tokio::test]
    async fn a_reply_in_a_closed_thread_is_refused() {
        let (fake, port, mut a, room, root) = with_thread().await;
        a.handle(Command::Events(vec![during(
            "f/c/alice",
            "s1",
            "SessionEnd",
            json!({ "reason": "clear" }),
        )]))
        .await;
        port.take_calls();

        a.handle(Command::Inbound(inbound(
            &room,
            Some(&root),
            "@rahul:example.org",
            "too late",
        )))
        .await;
        assert!(
            fake.actions_for("f/c/alice").is_empty(),
            "nothing reaches the agent"
        );
        assert_eq!(reactions(&port.calls()), vec![REFUSED.to_string()]);
    }

    #[tokio::test]
    async fn a_rejected_send_text_is_reported_in_the_thread() {
        let (fake, port, mut a, room, root) = with_thread().await;
        fake.fail_actions(Some("no such window"));
        a.handle(Command::Inbound(inbound(
            &room,
            Some(&root),
            "@rahul:example.org",
            "run the tests",
        )))
        .await;
        let calls = port.calls();
        assert_eq!(reactions(&calls), vec![FAILED.to_string()]);
        let notice = sends(&calls);
        assert_eq!(notice.len(), 1, "the failure is posted in the thread");
        assert_eq!(notice[0].0, Some(root), "in the thread, not the room");
        assert!(notice[0].1.contains("no such window"), "{}", notice[0].1);
    }

    /// A long notice splits like any other body: every part fits the
    /// limit, and all of them stay in the thread. It used to be cut to
    /// one message, which threw the tail of the error away.
    #[tokio::test]
    async fn a_long_daemon_error_is_split_across_parts_before_it_is_posted() {
        let (fake, port, mut a, room, root) = with_thread().await;
        let long_error = "x".repeat(crate::render::BODY_LIMIT * 2);
        fake.fail_actions(Some(&long_error));
        a.handle(Command::Inbound(inbound(
            &room,
            Some(&root),
            "@rahul:example.org",
            "run the tests",
        )))
        .await;
        let notice = sends(&port.calls());
        assert!(
            notice.len() > 1,
            "a long notice splits: {} part(s)",
            notice.len()
        );
        for (i, (thread, body)) in notice.iter().enumerate() {
            assert_eq!(
                thread.as_deref(),
                Some(root.as_str()),
                "part {} left the thread",
                i + 1
            );
            assert!(
                body.len() <= crate::render::BODY_LIMIT,
                "part {} is {} bytes",
                i + 1,
                body.len()
            );
        }
    }

    use balerix_api::{Key, KeyStep};

    /// A thread with `questions` open; calls so far are cleared.
    async fn asked(
        questions: &[serde_json::Value],
    ) -> (FakeHost, FakePort, Actor<FakePort>, String, String) {
        let (fake, port, mut a, room, root) = with_question_thread().await;
        a.handle(Command::Events(vec![asks("f/c/alice", "s1", questions)]))
            .await;
        port.take_calls();
        (fake, port, a, room, root)
    }

    async fn reply(a: &mut Actor<FakePort>, room: &str, root: &str, body: &str) {
        a.handle(Command::Inbound(inbound(
            room,
            Some(root),
            "@rahul:example.org",
            body,
        )))
        .await;
    }

    fn down_enter(downs: usize) -> Vec<KeyStep> {
        let mut steps = vec![KeyStep::Key(Key::Down); downs];
        steps.push(KeyStep::Key(Key::Enter));
        steps
    }

    #[tokio::test]
    async fn an_exact_reply_is_echoed_then_sent_as_keys_and_never_as_text() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "3").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: down_enter(2),
                delay_ms: 100,
            }]
        );
        assert_eq!(
            sends(&port.calls()),
            vec![(Some(root.clone()), "**answering** Color → Blue".to_string())]
        );
        assert_eq!(reactions(&port.calls()), vec![ACK.to_string()]);
        assert_eq!(
            a.questions.get("f/c/alice").unwrap().stage,
            Stage::Sent {
                selections: Some(vec![crate::question::Selection {
                    options: vec![2],
                    other: None
                }]),
                echo: Some("$evt4:fake".into()),
            }
        );

        // the recorded answer then lands as a ✅ on that echo
        port.take_calls();
        a.handle(Command::Events(vec![answered(
            "f/c/alice",
            "s1",
            json!({ "Which color?": "Blue" }),
        )]))
        .await;
        assert_eq!(
            port.calls(),
            vec![Call::React {
                room,
                event_id: "$evt4:fake".into(),
                key: CONFIRMED.into()
            }]
        );
    }

    #[tokio::test]
    async fn the_agents_key_delay_is_used() {
        let (fake, _port, mut a, room, root) = asked(&[color()]).await;
        a.handle(Command::Activate {
            agent: "f/c/alice".into(),
            config: crate::config::parse_agent(
                &json!({ "events": ["Notification"], "keyDelayMs": 250 }),
            )
            .unwrap(),
        })
        .await;
        reply(&mut a, &room, &root, "1").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: down_enter(0),
                delay_ms: 250,
            }]
        );
    }

    #[tokio::test]
    async fn an_inexact_reply_asks_first_and_yes_sends_it() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "gre").await;
        assert!(
            fake.actions_for("f/c/alice").is_empty(),
            "nothing typed yet"
        );
        assert_eq!(
            sends(&port.calls())[0].1,
            "**I read that as** Color → Green. Reply **yes** to send."
        );
        assert!(matches!(
            a.questions.get("f/c/alice").unwrap().stage,
            Stage::Confirming { .. }
        ));

        port.take_calls();
        reply(&mut a, &room, &root, " Yes ").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: down_enter(1),
                delay_ms: 100,
            }]
        );
        assert!(sends(&port.calls()).is_empty(), "no second echo");
        assert_eq!(
            a.questions.get("f/c/alice").unwrap().stage,
            Stage::Sent {
                selections: Some(vec![crate::question::Selection {
                    options: vec![1],
                    other: None
                }]),
                echo: Some("$evt4:fake".into()),
            },
            "the ✅ goes on the echo that asked"
        );
    }

    #[tokio::test]
    async fn no_drops_the_confirmation_and_another_reply_replaces_it() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "gre").await;
        reply(&mut a, &room, &root, "no").await;
        assert_eq!(a.questions.get("f/c/alice").unwrap().stage, Stage::Open);
        assert!(fake.actions_for("f/c/alice").is_empty());

        reply(&mut a, &room, &root, "gre").await;
        port.take_calls();
        reply(&mut a, &room, &root, "blue").await; // not yes/no: a fresh answer
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: down_enter(2),
                delay_ms: 100,
            }]
        );
        assert_eq!(sends(&port.calls())[0].1, "**answering** Color → Blue");
    }

    #[tokio::test]
    async fn prose_is_refused_and_nothing_reaches_the_agent() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "purple please").await;
        assert!(fake.actions_for("f/c/alice").is_empty(), "the §1 hazard");
        assert_eq!(reactions(&port.calls()), vec![REFUSED.to_string()]);
        let body = &sends(&port.calls())[0].1;
        assert!(
            body.contains("matches nothing") && body.contains("1. Red"),
            "{body}"
        );
        assert_eq!(a.questions.get("f/c/alice").unwrap().stage, Stage::Open);
    }

    #[tokio::test]
    async fn skip_declines_with_one_escape() {
        let (fake, port, mut a, room, root) = asked(&[color(), size()]).await;
        reply(&mut a, &room, &root, "skip").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps: vec![KeyStep::Key(Key::Escape)],
                delay_ms: 100,
            }]
        );
        assert_eq!(sends(&port.calls())[0].1, "**declining the question**");
        assert_eq!(
            a.questions.get("f/c/alice").unwrap().stage,
            Stage::Sent {
                selections: None,
                echo: Some("$evt4:fake".into()),
            }
        );
    }

    #[tokio::test]
    async fn one_reply_answers_several_questions() {
        let (fake, port, mut a, room, root) = asked(&[color(), size()]).await;
        reply(&mut a, &room, &root, "blue\nmedium").await;
        let mut steps = down_enter(2);
        steps.extend(down_enter(1));
        steps.push(KeyStep::Key(Key::Enter)); // the review screen
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendKeys {
                steps,
                delay_ms: 100,
            }]
        );
        assert_eq!(
            sends(&port.calls())[0].1,
            "**answering** Color → Blue · Size → Medium"
        );
    }

    #[tokio::test]
    async fn a_reply_while_keys_are_on_their_way_is_refused() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        reply(&mut a, &room, &root, "1").await;
        port.take_calls();
        reply(&mut a, &room, &root, "2").await;
        assert_eq!(fake.actions_for("f/c/alice").len(), 1, "only the first");
        assert_eq!(reactions(&port.calls()), vec![REFUSED.to_string()]);
        assert!(sends(&port.calls())[0].1.contains("already on its way"));
    }

    #[tokio::test]
    async fn a_failed_send_keys_is_reported_and_the_question_stays_open() {
        let (fake, port, mut a, room, root) = asked(&[color()]).await;
        fake.fail_actions(Some("no window"));
        reply(&mut a, &room, &root, "1").await;
        assert_eq!(reactions(&port.calls()), vec![FAILED.to_string()]);
        let bodies = sends(&port.calls());
        assert!(
            bodies[1].1.starts_with("**not delivered to f/c/alice:**"),
            "{bodies:?}"
        );
        assert_eq!(a.questions.get("f/c/alice").unwrap().stage, Stage::Open);
    }

    #[tokio::test]
    async fn with_no_question_open_a_reply_is_still_a_prompt() {
        let (fake, _port, mut a, room, root) = with_question_thread().await;
        reply(&mut a, &room, &root, "3").await;
        assert_eq!(
            fake.actions_for("f/c/alice"),
            vec![PluginAction::SendText {
                text: "3".into(),
                submit: true,
            }]
        );
    }
}
