//! The actor (Spec M §8): the one task that owns every piece of state
//! and executes the command loop. Generic over `GitHubPort` (M-11).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::{HookEvent, Key, KeyStep, PluginAction};
use balerix_plugin_common::delivery::{self, Deliveries, Kind as BodyKind};
use balerix_plugin_common::metrics::Shared;
use balerix_plugin_common::pending::{OpenQuestion, Questions};
use balerix_plugin_common::phases::PhaseChange;
use balerix_plugin_common::question;
use balerix_plugin_common::render;
use balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge};
use balerix_plugin_sdk::{Host, Metrics, SdkError};
use serde_json::Value;
use tokio::time::Instant;

use crate::config::{AgentConfig, COMMENT_LIMIT, DaemonConfig, Kind};
use crate::github::{
    CONFUSED, EYES, GitHubError, GitHubPort, HOORAY, IssueInfo, MINUS_ONE, PLUS_ONE, Permission,
    Target,
};
use crate::mention;
use crate::prompt;
use crate::repo_config;
use crate::session::{self, Session, Sessions};
use crate::status::Status;
use crate::webhook::{Author, WebhookEvent};

pub use balerix_plugin_common::queue::{Health, QUEUE};
pub type Queue = balerix_plugin_common::queue::Queue<Command>;

/// `main` pushes `Tick` this often: delivery expiry, status flushes, the
/// idle check, row write-back (§8.7, §8.3, §10).
pub const TICK: Duration = Duration::from_secs(5);
/// A status edit waits this long for more lines (§8.3).
pub const STATUS_COALESCE: Duration = Duration::from_secs(2);
/// What the first prompt waits beyond `confirmWindow` before it is
/// reported unconfirmed: it is sent on `SessionStart`, and a cold start
/// under nono has swallowed an Enter 20 s after that and taken one at
/// 60 s (#99). Enter is pressed again all the while.
pub const START_UP_ALLOWANCE: Duration = Duration::from_secs(60);
/// The collaborator permission cache (§8.1).
pub const PERMISSION_TTL: Duration = Duration::from_secs(300);
/// `retry_once` waits at most this long on `Retry-After`.
const MAX_INLINE_RETRY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Configure(DaemonConfig),
    Activate { agent: String, config: AgentConfig },
    Deactivate { agent: String },
    Events(Vec<HookEvent>),
    Phases(Vec<PhaseChange>),
    Webhook(WebhookEvent),
    Tick,
}

#[derive(Debug, Clone)]
pub struct Counters {
    pub messages_sent: IntCounterVec,
    pub events_dropped: IntCounter,
    pub inbound: IntCounterVec,
    pub errors: IntCounterVec,
    pub answers_mismatched: IntCounter,
    pub deliveries: IntCounterVec,
    pub webhooks: IntCounterVec,
    pub sessions_open: IntGauge,
    pub applies: IntCounterVec,
    pub status_edits: IntCounterVec,
}

impl Counters {
    pub fn new(metrics: &Metrics) -> Result<Self, SdkError> {
        let shared = Shared::new(metrics)?;
        Ok(Self {
            messages_sent: shared.messages_sent,
            events_dropped: shared.events_dropped,
            inbound: shared.inbound,
            errors: shared.errors,
            answers_mismatched: shared.answers_mismatched,
            deliveries: shared.deliveries,
            webhooks: metrics.int_counter_vec(
                "webhooks_total",
                "Webhook deliveries, by event and outcome",
                &["event", "outcome"],
            )?,
            sessions_open: metrics.int_gauge(
                "sessions_open",
                "Issues and pull requests with a live agent",
            )?,
            applies: metrics.int_counter_vec(
                "applies_total",
                "Fleet applies, by outcome",
                &["outcome"],
            )?,
            status_edits: metrics.int_counter_vec(
                "status_edits_total",
                "Status comment edits, by outcome",
                &["outcome"],
            )?,
        })
    }
}

/// Where a routed prompt's 👍 or note goes (§8.7).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sent {
    repo: String,
    installation: u64,
    number: u64,
    target: Target,
    agent: String,
    asker: String,
    /// What it was given before the expiry note; the note names it.
    window: Duration,
}

/// Why a routed prompt went unconfirmed (§8.7, §17): the note names its
/// reason, and only expiry claims a duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unconfirmed {
    /// Its `confirmWindow` passed with the agent idle.
    Expired,
    /// Claude took a later prompt first.
    Skipped,
    /// Pushed out past `MAX_PENDING`.
    Evicted,
}

/// A review held while a question is open (§9).
#[derive(Debug, Clone)]
struct Held {
    reviewer: String,
    message: String,
}

/// What `track_question` did with an event (Spec J §5): the posting half
/// reads this instead of asking `questions` again (matrix's shape).
enum Tracking {
    /// Nothing that opens or closes a question.
    Other,
    /// A `PreToolUse` whose dialog parsed, and is now open.
    Opened(Vec<question::Question>),
    /// A `PreToolUse` for `AskUserQuestion` that is not a dialog.
    Unparsed,
    /// A `PostToolUse` for `AskUserQuestion`, with the record it closed.
    Closed(Option<OpenQuestion>),
}

pub struct Actor<G: GitHubPort> {
    host: Host,
    port: G,
    counters: Counters,
    health: Health,
    config: Option<DaemonConfig>,
    slug: String,
    pending: Vec<Command>,
    agents: HashMap<String, AgentConfig>,
    sessions: Sessions,
    questions: Questions,
    deliveries: Deliveries<Sent>,
    statuses: HashMap<String, Status>,
    status_dirty: HashMap<String, Instant>,
    rows_dirty: HashMap<String, ()>,
    permissions: HashMap<(String, String), (Permission, Instant)>,
    held: HashMap<String, Vec<Held>>,
    ended_notice: HashMap<String, ()>,
    /// What the first prompt needs, kept from `start` until `SessionStart`.
    first_prompts: HashMap<String, FirstPrompt>,
    /// Markers evicted inside a sync path, reported on the next tick.
    unconfirmed_queue: Vec<(Sent, Unconfirmed)>,
    /// Wall-clock seconds; a test pins it.
    now_secs: fn() -> u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl<G: GitHubPort> Actor<G> {
    pub fn new(host: Host, port: G, counters: Counters, health: Health, slug: String) -> Self {
        Self {
            host,
            port,
            counters,
            health,
            slug,
            config: None,
            pending: Vec::new(),
            agents: HashMap::new(),
            sessions: Sessions::default(),
            questions: Questions::default(),
            deliveries: Deliveries::default(),
            statuses: HashMap::new(),
            status_dirty: HashMap::new(),
            rows_dirty: HashMap::new(),
            permissions: HashMap::new(),
            held: HashMap::new(),
            ended_notice: HashMap::new(),
            first_prompts: HashMap::new(),
            unconfirmed_queue: Vec::new(),
            now_secs: unix_now,
        }
    }

    pub async fn load(&mut self) {
        match Sessions::load(&self.host).await {
            Ok(s) => self.sessions = s,
            Err(e) => tracing::warn!("github: loading sessions: {e}"),
        }
        match Questions::load(&self.host).await {
            Ok(q) => self.questions = q,
            Err(e) => tracing::warn!("github: loading questions: {e}"),
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
            Command::Activate { agent, config } => self.on_activate(agent, config).await,
            // The daemon deactivates every agent an apply drops, including
            // the one `end` just removed. Memory is forgotten either way;
            // the KV row goes only while it is still open (the daemon took
            // away an agent the plugin did not end). A closed row stays:
            // §10's "dropped on Deactivate" loses to its own "row kept with
            // closed = true … the resume case", which the ended reply and
            // the resume need.
            Command::Deactivate { agent } => {
                self.agents.remove(&agent);
                self.questions.clear(&self.host, &agent).await;
                self.deliveries.forget(&agent);
                self.statuses.remove(&agent);
                self.status_dirty.remove(&agent);
                self.held.remove(&agent);
                if self.sessions.get(&agent).is_some_and(|r| !r.closed)
                    && let Err(e) = self.sessions.remove(&self.host, &agent).await
                {
                    tracing::warn!("github: forgetting {agent}: {e}");
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
            Command::Webhook(ev) => self.on_webhook(ev).await,
            Command::Tick => self.on_tick().await,
        }
    }

    fn cfg(&self) -> &DaemonConfig {
        // `handle` buffers everything until `Configure`; this is only
        // reached after it.
        self.config
            .as_ref()
            .unwrap_or_else(|| unreachable!("configured"))
    }

    fn publish_gauges(&self) {
        self.counters
            .sessions_open
            .set(self.sessions.open_count() as i64);
    }

    /// §5: a row missing from KV is rebuilt from the activation, with
    /// `status_comment` unset; the first status edit then posts anew.
    async fn on_activate(&mut self, agent: String, config: AgentConfig) {
        if self.sessions.get(&agent).is_none()
            && let (Some(kind), Some(number)) = (config.kind, config.number)
            && let Ok(Some(repo)) =
                session::repo_of_fleet(&self.host, session::fleet_of(&agent)).await
        {
            let row = Session {
                repo,
                installation: 0,
                kind,
                number,
                head: None,
                base: None,
                status_comment: None,
                session_id: None,
                last_activity: (self.now_secs)(),
                closed: false,
            };
            if let Err(e) = self.sessions.set(&self.host, &agent, row).await {
                tracing::warn!("github: rebuilding row for {agent}: {e}");
            }
        }
        self.agents.insert(agent, config);
        self.publish_gauges();
    }

    // ---- §8.1 who is heard ----

    async fn permitted(&mut self, installation: u64, repo: &str, author: &Author) -> bool {
        if author.bot
            || author
                .login
                .eq_ignore_ascii_case(&format!("{}[bot]", self.slug))
            || author.login.eq_ignore_ascii_case(&self.slug)
        {
            return false;
        }
        let key = (repo.to_string(), author.login.clone());
        let now = Instant::now();
        if let Some((p, until)) = self.permissions.get(&key)
            && *until > now
        {
            return p.may_prompt();
        }
        let p = match retry_once(|| self.port.permission(installation, repo, &author.login)).await {
            Ok(p) => p,
            Err(GitHubError::NotFound) => Permission::None,
            Err(e) => {
                self.github_failed("permission", &e);
                return false;
            }
        };
        self.permissions.insert(key, (p, now + PERMISSION_TTL));
        p.may_prompt()
    }

    // ---- §7 → §8 dispatch ----

    async fn on_webhook(&mut self, ev: WebhookEvent) {
        match ev {
            WebhookEvent::IssueOpened {
                repo,
                installation,
                number,
                author,
                title,
                body,
                url,
            } => {
                self.on_opening(
                    installation,
                    &repo,
                    Kind::Issue,
                    number,
                    author,
                    &title,
                    &body,
                    &url,
                    None,
                    Target::Issue(number),
                    None,
                )
                .await;
            }
            WebhookEvent::PrOpened {
                repo,
                installation,
                number,
                author,
                title,
                body,
                url,
                head,
                head_repo,
                base,
            } => {
                self.on_opening(
                    installation,
                    &repo,
                    Kind::Pr,
                    number,
                    author,
                    &title,
                    &body,
                    &url,
                    Some((head, head_repo, base)),
                    Target::Issue(number),
                    None,
                )
                .await;
            }
            WebhookEvent::Comment {
                repo,
                installation,
                number,
                author,
                comment_id,
                body,
                is_pr,
            } => {
                self.on_comment(
                    installation,
                    &repo,
                    number,
                    author,
                    comment_id,
                    &body,
                    is_pr,
                )
                .await;
            }
            WebhookEvent::Closed {
                repo,
                installation,
                number,
                merged,
            } => {
                if let Some(agent) = self.sessions.by_number(&repo, number).map(str::to_string) {
                    self.refresh_installation(&agent, installation);
                    self.end(
                        &agent,
                        installation,
                        if merged { "merged" } else { "closed" },
                    )
                    .await;
                }
            }
            WebhookEvent::ReviewSubmitted {
                repo,
                installation,
                number,
                author,
                review_id,
                state,
                body,
                commit,
            } => {
                self.on_review(
                    installation,
                    &repo,
                    number,
                    author,
                    review_id,
                    &state,
                    &body,
                    &commit,
                )
                .await;
            }
        }
    }

    /// An issue or PR body that mentions the App starts a session.
    #[allow(clippy::too_many_arguments)]
    async fn on_opening(
        &mut self,
        installation: u64,
        repo: &str,
        kind: Kind,
        number: u64,
        author: Author,
        title: &str,
        body: &str,
        url: &str,
        pr: Option<(String, String, String)>,
        target: Target,
        comment: Option<&str>,
    ) {
        if !mention::mentions(body, &self.slug)
            && !comment.is_some_and(|c| mention::mentions(c, &self.slug))
        {
            return;
        }
        if !self.permitted(installation, repo, &author).await {
            self.counters
                .inbound
                .with_label_values(&["unpermitted"])
                .inc();
            return;
        }
        self.start(
            installation,
            repo,
            kind,
            number,
            &author.login,
            title,
            body,
            url,
            pr,
            target,
            comment,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn on_comment(
        &mut self,
        installation: u64,
        repo: &str,
        number: u64,
        author: Author,
        comment_id: u64,
        body: &str,
        is_pr: bool,
    ) {
        let count = |c: &Counters, outcome: &str| c.inbound.with_label_values(&[outcome]).inc();
        if author.bot
            || author
                .login
                .eq_ignore_ascii_case(&format!("{}[bot]", self.slug))
        {
            count(&self.counters, "own_or_bot");
            return;
        }
        if !self.permitted(installation, repo, &author).await {
            count(&self.counters, "unpermitted");
            return;
        }
        let live = self.sessions.by_number(repo, number).map(str::to_string);
        if let Some(agent) = &live {
            self.refresh_installation(agent, installation);
        }
        let closed = live
            .as_deref()
            .and_then(|a| self.sessions.get(a))
            .is_some_and(|r| r.closed);
        let mentioned = mention::mentions(body, &self.slug);
        match (live, closed) {
            (Some(agent), false) => {
                self.on_message(
                    &agent,
                    installation,
                    repo,
                    number,
                    &author.login,
                    comment_id,
                    body,
                )
                .await
            }
            (Some(agent), true) if !mentioned => {
                count(&self.counters, "ended");
                self.react(installation, repo, Target::Comment(comment_id), CONFUSED)
                    .await;
                if self.ended_notice.insert(agent.clone(), ()).is_none() {
                    self.post(
                        installation,
                        repo,
                        number,
                        &format!(
                            "this session has ended; mention @{} to start a new one",
                            self.slug
                        ),
                        "notice",
                    )
                    .await;
                }
            }
            _ if mentioned => {
                // No live session: start one. A comment carries no title
                // or body, so the issue is read (§8.2's prompt needs it,
                // and a PR's head decides the fork check and the branch).
                let info = match retry_once(|| self.port.issue(installation, repo, number)).await {
                    Ok(i) => i,
                    Err(e) => {
                        self.github_failed("issue", &e);
                        return;
                    }
                };
                let kind = if is_pr || info.pr.is_some() {
                    Kind::Pr
                } else {
                    Kind::Issue
                };
                let IssueInfo {
                    title,
                    body: issue_body,
                    url,
                    pr,
                } = info;
                self.start(
                    installation,
                    repo,
                    kind,
                    number,
                    &author.login,
                    &title,
                    &issue_body,
                    &url,
                    pr,
                    Target::Comment(comment_id),
                    Some(body),
                )
                .await;
            }
            _ => count(&self.counters, "unknown_number"),
        }
    }

    // ---- §8.2 starting ----

    #[allow(clippy::too_many_arguments)]
    async fn start(
        &mut self,
        installation: u64,
        repo: &str,
        kind: Kind,
        number: u64,
        asker: &str,
        title: &str,
        body: &str,
        url: &str,
        pr: Option<(String, String, String)>,
        target: Target,
        comment: Option<&str>,
    ) {
        self.react(installation, repo, target, EYES).await;
        if let Some((_, head_repo, _)) = &pr
            && !head_repo.eq_ignore_ascii_case(repo)
        {
            self.post(
                installation,
                repo,
                number,
                "sessions on pull requests from forks are not supported",
                "notice",
            )
            .await;
            self.react(installation, repo, target, CONFUSED).await;
            return;
        }
        let fleet = mention::fleet_name(repo);
        match session::repo_of_fleet(&self.host, &fleet).await {
            Ok(Some(other)) if !other.eq_ignore_ascii_case(repo) => {
                self.post(
                    installation,
                    repo,
                    number,
                    &format!(
                        "fleet name {fleet} already stands for {other}; clear the plugin's KV key \
                         repo/{fleet} (plugins/github/kv/repo/{fleet} under the daemon's state \
                         root) to reuse it"
                    ),
                    "notice",
                )
                .await;
                self.react(installation, repo, target, CONFUSED).await;
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("github: reading repo/{fleet}: {e}");
                return;
            }
        }
        let agent = format!("{fleet}/repo/{}", kind.agent_name(number));
        let old_status_comment = self.sessions.get(&agent).and_then(|r| r.status_comment);
        let resumed = self.sessions.get(&agent).is_some();
        let head = pr.as_ref().map(|(h, _, _)| h.clone());
        let base = pr.as_ref().map(|(_, _, b)| b.clone());

        // §6: the file, then the apply with this session added
        let mut live = self.sessions.live_in(&fleet);
        live.retain(|l| l.number != number);
        live.push(repo_config::Live {
            kind,
            number,
            head: head.clone(),
        });
        let default_branch = match retry_once(|| self.port.default_branch(installation, repo)).await
        {
            Ok(b) => b,
            Err(e) => {
                self.github_failed("default_branch", &e);
                return;
            }
        };
        let text = match retry_once(|| {
            self.port
                .read_file(installation, repo, &self.cfg().config_path, &default_branch)
        })
        .await
        {
            Ok(Some(t)) => t,
            Ok(None) => {
                self.refuse(
                    installation,
                    repo,
                    number,
                    target,
                    &format!("{} not found on {default_branch}", self.cfg().config_path),
                    "config",
                )
                .await;
                return;
            }
            Err(e) => {
                self.github_failed("read_file", &e);
                return;
            }
        };
        let file = match repo_config::prepare(&text, repo, &fleet, &default_branch, &live) {
            Ok(f) => f,
            Err(m) => {
                self.refuse(installation, repo, number, target, &m, "config")
                    .await;
                return;
            }
        };
        if let Err(e) = self.host.apply_fleet(&fleet, &file).await {
            tracing::warn!("github: applying {fleet} with {agent}: {e}");
            self.refuse(
                installation,
                repo,
                number,
                target,
                &apply_refusal(&e),
                "daemon",
            )
            .await;
            return;
        }
        self.counters.applies.with_label_values(&["ok"]).inc();
        self.health.ok();
        if let Err(e) = session::set_repo_of_fleet(&self.host, &fleet, repo).await {
            tracing::warn!("github: writing repo/{fleet}: {e}");
        }

        // §8.3: the status comment, then the row. A resumed session keeps
        // its comment (one per session, edited in place); a new one posts.
        let mut status = self
            .statuses
            .remove(&agent)
            .unwrap_or_else(|| Status::new(&agent));
        status.phase = "starting".into();
        status.push(
            (self.now_secs)(),
            if resumed { "resuming" } else { "starting" },
        );
        let rendered = status.render();
        let status_comment = match old_status_comment {
            Some(id) => {
                self.status_dirty
                    .entry(agent.clone())
                    .or_insert_with(Instant::now);
                Some(id)
            }
            None => match retry_once(|| self.port.comment(installation, repo, number, &rendered))
                .await
            {
                Ok(id) => Some(id),
                Err(e) => {
                    self.github_failed("comment", &e);
                    None
                }
            },
        };
        self.statuses.insert(agent.clone(), status);
        let row = Session {
            repo: repo.to_string(),
            installation,
            kind,
            number,
            head,
            base: base.clone(),
            status_comment,
            session_id: None,
            last_activity: (self.now_secs)(),
            closed: false,
        };
        if let Err(e) = self.sessions.set(&self.host, &agent, row).await {
            tracing::warn!("github: storing row for {agent}: {e}");
        }
        self.ended_notice.remove(&agent);
        // The first prompt waits for `SessionStart` (§8.2); what it needs is kept here.
        self.first_prompts.insert(
            agent.clone(),
            FirstPrompt {
                title: title.to_string(),
                body: body.to_string(),
                url: url.to_string(),
                branch: head_or_default(&fleet, kind, number, pr.as_ref()),
                base: base.unwrap_or_else(|| default_branch.clone()),
                asker: asker.to_string(),
                comment: comment.map(str::to_string),
                resumed,
                target,
            },
        );
        self.publish_gauges();
    }

    async fn refuse(
        &mut self,
        installation: u64,
        repo: &str,
        number: u64,
        target: Target,
        message: &str,
        outcome: &str,
    ) {
        self.counters.applies.with_label_values(&[outcome]).inc();
        self.post(installation, repo, number, message, "notice")
            .await;
        self.react(installation, repo, target, CONFUSED).await;
    }

    /// A row rebuilt by `on_activate` carries `installation: 0`, and an App
    /// reinstall changes every id: a verified webhook naming the row
    /// brings it up to date.
    fn refresh_installation(&mut self, agent: &str, installation: u64) {
        if let Some(r) = self.sessions.get_mut(agent)
            && r.installation != installation
        {
            r.installation = installation;
            self.rows_dirty.insert(agent.to_string(), ());
        }
    }

    fn github_failed(&self, kind: &str, e: &GitHubError) {
        self.counters.errors.with_label_values(&[kind]).inc();
        tracing::warn!("github: {kind}: {e}");
        if matches!(e, GitHubError::Auth(_)) {
            self.counters.errors.with_label_values(&["auth"]).inc();
            self.health.fail(format!("github: {e}"));
        }
    }

    // ---- §8.4 inbound while live ----

    #[allow(clippy::too_many_arguments)]
    async fn on_message(
        &mut self,
        agent: &str,
        installation: u64,
        repo: &str,
        number: u64,
        asker: &str,
        comment_id: u64,
        body: &str,
    ) {
        self.touch(agent);
        if self.questions.is_open(agent) {
            self.on_answer(agent, installation, repo, number, comment_id, body)
                .await;
            return;
        }
        let action = PluginAction::SendText {
            text: body.to_string(),
            submit: true,
        };
        match self.host.action(agent, &action).await {
            Ok(()) => {
                self.counters.inbound.with_label_values(&["routed"]).inc();
                self.react(installation, repo, Target::Comment(comment_id), EYES)
                    .await;
                self.track(
                    agent,
                    installation,
                    repo,
                    number,
                    asker,
                    Target::Comment(comment_id),
                    body,
                    Duration::ZERO,
                );
            }
            Err(e) => {
                self.counters
                    .inbound
                    .with_label_values(&["send_failed"])
                    .inc();
                self.counters.errors.with_label_values(&["send_text"]).inc();
                self.post(
                    installation,
                    repo,
                    number,
                    &format!("not delivered to {agent}: {e}"),
                    "notice",
                )
                .await;
                self.react(installation, repo, Target::Comment(comment_id), MINUS_ONE)
                    .await;
            }
        }
    }

    /// §8.7: a prose prompt is tracked; a slash command is not.
    #[allow(clippy::too_many_arguments)]
    fn track(
        &mut self,
        agent: &str,
        installation: u64,
        repo: &str,
        number: u64,
        asker: &str,
        target: Target,
        body: &str,
        allowance: Duration,
    ) {
        match delivery::classify(body) {
            BodyKind::Command => {
                self.counters
                    .deliveries
                    .with_label_values(&["command"])
                    .inc();
            }
            BodyKind::Prompt => {
                let window = self
                    .agents
                    .get(agent)
                    .map_or(crate::config::DEFAULT_CONFIRM_WINDOW, |c| c.confirm_window);
                // zero stays zero: never reported
                let window = if window.is_zero() {
                    window
                } else {
                    window + allowance
                };
                let sent = Sent {
                    repo: repo.into(),
                    installation,
                    number,
                    target,
                    agent: agent.into(),
                    asker: asker.into(),
                    window,
                };
                if let Some(evicted) =
                    self.deliveries
                        .sent(agent, body, sent, Instant::now(), window)
                {
                    self.unconfirmed_later(evicted);
                }
            }
        }
    }

    /// #99: a prompt sent while Claude's TUI was starting sits in the
    /// composer with its Enter lost, so an idle agent with an unconfirmed
    /// prompt gets Enter once more. A refusal is counted and nothing
    /// else: the prompt's own expiry is what its asker hears about.
    async fn press_enter(&mut self, agent: &str) {
        let delay_ms = self
            .agents
            .get(agent)
            .map_or(balerix_api::DEFAULT_KEY_DELAY_MS, |c| c.key_delay_ms);
        let action = PluginAction::SendKeys {
            steps: vec![KeyStep::Key(Key::Enter)],
            delay_ms,
        };
        match self.host.action(agent, &action).await {
            Ok(()) => self
                .counters
                .deliveries
                .with_label_values(&["nudged"])
                .inc(),
            Err(e) => {
                self.counters.errors.with_label_values(&["send_keys"]).inc();
                tracing::warn!("github: Enter again for {agent}: {e}");
            }
        }
    }

    fn unconfirmed_later(&mut self, sent: Sent) {
        self.unconfirmed_queue.push((sent, Unconfirmed::Evicted));
    }

    async fn unconfirmed(&mut self, sent: Sent, why: Unconfirmed) {
        self.counters
            .deliveries
            .with_label_values(&["unconfirmed"])
            .inc();
        self.react(sent.installation, &sent.repo, sent.target, CONFUSED)
            .await;
        let line = match why {
            Unconfirmed::Expired => format!(
                "prompt from @{} not confirmed after {}s",
                sent.asker,
                sent.window.as_secs()
            ),
            Unconfirmed::Skipped => format!(
                "prompt from @{} skipped: Claude took a later one first",
                sent.asker
            ),
            Unconfirmed::Evicted => {
                format!("prompt from @{} dropped: too many pending", sent.asker)
            }
        };
        self.line(&sent.agent, &line);
    }

    async fn on_answer(
        &mut self,
        agent: &str,
        installation: u64,
        repo: &str,
        number: u64,
        comment_id: u64,
        body: &str,
    ) {
        use balerix_plugin_common::answer::{self, Reaction};
        use balerix_plugin_common::pending::Stage;
        let Some(open) = self.questions.get(agent).cloned() else {
            return;
        };
        let delay_ms = self
            .agents
            .get(agent)
            .map_or(balerix_api::DEFAULT_KEY_DELAY_MS, |c| c.key_delay_ms);
        let d = answer::on_reply(&open, body, delay_ms);
        // The executor contract of `Decision` (Spec K §4), with comments as
        // posts and reactions on the operator's comment.
        let mut echo = None;
        if let Some(text) = &d.post {
            match self
                .post(installation, repo, number, text, "question")
                .await
            {
                Some(id) => echo = Some(id.to_string()),
                None if d.gates_on_post() => {
                    self.counters
                        .inbound
                        .with_label_values(&["send_failed"])
                        .inc();
                    self.questions.set_stage(agent, Stage::Open);
                    self.react(installation, repo, Target::Comment(comment_id), MINUS_ONE)
                        .await;
                    return;
                }
                None => {}
            }
        }
        if let Some(action) = &d.send
            && let Err(e) = self.host.action(agent, action).await
        {
            self.counters
                .inbound
                .with_label_values(&["send_failed"])
                .inc();
            self.counters.errors.with_label_values(&["send_keys"]).inc();
            self.questions.set_stage(agent, Stage::Open);
            self.post(
                installation,
                repo,
                number,
                &format!("not delivered to {agent}: {e}"),
                "notice",
            )
            .await;
            self.react(installation, repo, Target::Comment(comment_id), MINUS_ONE)
                .await;
            return;
        }
        let stage = if d.gates_on_post() {
            d.stage.with_echo(echo)
        } else {
            d.stage
        };
        self.questions.set_stage(agent, stage);
        if let Some(outcome) = d.outcome {
            self.counters.inbound.with_label_values(&[outcome]).inc();
        }
        if let Some(r) = d.react {
            let content = match r {
                Reaction::Ack => PLUS_ONE,
                Reaction::Refused => CONFUSED,
                Reaction::Failed => MINUS_ONE,
                Reaction::Confirmed => HOORAY,
            };
            self.react(installation, repo, Target::Comment(comment_id), content)
                .await;
        }
    }

    // ---- §8.5 outbound ----

    async fn on_event(&mut self, event: HookEvent) {
        let Some(config) = self.agents.get(&event.agent).cloned() else {
            return;
        };
        let Some(row) = self.sessions.get(&event.agent).cloned() else {
            return;
        };
        self.touch(&event.agent);
        // §8.7 and J §5: both run above every early return.
        let outcome = self.deliveries.on_event(&event, Instant::now());
        for s in outcome.skipped {
            self.unconfirmed(s, Unconfirmed::Skipped).await;
        }
        if let Some(s) = outcome.confirmed {
            self.counters
                .deliveries
                .with_label_values(&["confirmed"])
                .inc();
            self.react(s.installation, &s.repo, s.target, PLUS_ONE)
                .await;
        }
        let tracking = self.track_question(&event).await;
        // §10: an ended session posts nothing more; the tracking above
        // still ran.
        if row.closed || !config.enabled {
            return;
        }
        let (installation, repo, number) = (row.installation, row.repo.clone(), row.number);

        if let Some(session) = &event.session_id
            && row.session_id.as_deref() != Some(session)
        {
            if let Some(r) = self.sessions.get_mut(&event.agent) {
                r.session_id = Some(session.clone());
            }
            self.rows_dirty.insert(event.agent.clone(), ());
            self.statuses
                .entry(event.agent.clone())
                .or_insert_with(|| Status::new(&event.agent))
                .session = Some(session.clone());
        }
        match event.name.as_str() {
            "SessionStart" => {
                let source = event
                    .payload
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                self.line(&event.agent, &format!("session started ({source})"));
                if let Some(fp) = self.first_prompts.remove(&event.agent) {
                    self.send_first_prompt(&event.agent, installation, &repo, number, row.kind, fp)
                        .await;
                }
                return;
            }
            "SessionEnd" => {
                self.line(&event.agent, &status_line(&event));
                return;
            }
            "Stop" => {
                let message = event
                    .payload
                    .get("last_assistant_message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if message.is_empty() {
                    self.line(&event.agent, "turn finished");
                } else {
                    self.post(installation, &repo, number, &message, "turn")
                        .await;
                    if config.wants("Stop") {
                        self.line(&event.agent, "turn finished");
                    }
                }
                return;
            }
            _ => {}
        }
        if self
            .on_question_event(installation, &repo, number, &event, tracking)
            .await
        {
            return;
        }
        if config.wants(&event.name) {
            self.line(&event.agent, &status_line(&event));
        }
    }

    async fn send_first_prompt(
        &mut self,
        agent: &str,
        installation: u64,
        repo: &str,
        number: u64,
        kind: Kind,
        fp: FirstPrompt,
    ) {
        let s = prompt::Start {
            repo,
            kind,
            number,
            title: &fp.title,
            url: &fp.url,
            body: &fp.body,
            branch: &fp.branch,
            base: &fp.base,
            asker: &fp.asker,
            comment: fp.comment.as_deref(),
            resumed: fp.resumed,
        };
        let text = prompt::first(&s);
        let action = PluginAction::SendText {
            text: text.clone(),
            submit: true,
        };
        match self.host.action(agent, &action).await {
            Ok(()) => self.track(
                agent,
                installation,
                repo,
                number,
                &fp.asker,
                fp.target,
                &text,
                START_UP_ALLOWANCE,
            ),
            Err(e) => {
                self.counters.errors.with_label_values(&["send_text"]).inc();
                self.post(
                    installation,
                    repo,
                    number,
                    &format!("not delivered to {agent}: {e}"),
                    "notice",
                )
                .await;
                self.react(installation, repo, fp.target, MINUS_ONE).await;
            }
        }
    }

    /// Matrix's `track_question`, verbatim in shape (Spec J §5, §7.1),
    /// plus the release of a held review once the dialog is gone (§9).
    async fn track_question(&mut self, event: &HookEvent) -> Tracking {
        if matches!(
            event.name.as_str(),
            "Stop" | "UserPromptSubmit" | "SessionStart" | "SessionEnd"
        ) {
            self.questions.clear(&self.host, &event.agent).await;
            if event.name == "Stop" || event.name == "UserPromptSubmit" {
                self.release_held(&event.agent).await;
            }
            return Tracking::Other;
        }
        if event.payload.get("tool_name").and_then(Value::as_str) != Some(question::TOOL) {
            return Tracking::Other;
        }
        match event.name.as_str() {
            "PreToolUse" => {
                let input = event
                    .payload
                    .get("tool_input")
                    .cloned()
                    .unwrap_or(Value::Null);
                match question::parse(&input) {
                    Some(qs) => {
                        self.questions
                            .open(&self.host, &event.agent, &input, qs.clone())
                            .await;
                        Tracking::Opened(qs)
                    }
                    None => Tracking::Unparsed,
                }
            }
            "PostToolUse" => {
                let closed = self.questions.clear(&self.host, &event.agent).await;
                self.release_held(&event.agent).await;
                Tracking::Closed(closed)
            }
            _ => Tracking::Other,
        }
    }

    /// Matrix's `on_question_event`, with comments for posts and
    /// reactions on the echo. `true` when handled here.
    async fn on_question_event(
        &mut self,
        installation: u64,
        repo: &str,
        number: u64,
        event: &HookEvent,
        tracking: Tracking,
    ) -> bool {
        use balerix_plugin_common::answer::{self, Verdict};
        match tracking {
            Tracking::Opened(qs) => {
                let body = render::question_message(&qs)
                    .replace(
                        "Reply with a number or a label.",
                        "Reply with a comment: a number or a label.",
                    )
                    .replace(
                        "Reply with one line per question, in order: a number or a label.",
                        "Reply with a comment, one line per question, in order: a number or a label.",
                    );
                if self
                    .post(installation, repo, number, &body, "question")
                    .await
                    .is_some()
                {
                    self.questions.mark_posted(&event.agent);
                }
                true
            }
            Tracking::Unparsed | Tracking::Closed(None) => false,
            Tracking::Closed(Some(open)) => {
                let answers = event
                    .payload
                    .pointer("/tool_response/answers")
                    .cloned()
                    .unwrap_or(Value::Null);
                match answer::on_closed(&open, &answers) {
                    Verdict::Confirmed { echo } => {
                        if let Some(id) = echo.and_then(|e| e.parse::<u64>().ok()) {
                            self.react(installation, repo, Target::Comment(id), HOORAY)
                                .await;
                        }
                    }
                    Verdict::Mismatch { message } => {
                        self.counters.answers_mismatched.inc();
                        self.post(installation, repo, number, &message, "question")
                            .await;
                    }
                    Verdict::AnsweredAtTerminal { message } => {
                        self.post(installation, repo, number, &message, "question")
                            .await;
                    }
                    Verdict::Nothing => {}
                }
                true
            }
            Tracking::Other => {
                event.name == "Notification"
                    && event
                        .payload
                        .get("notification_type")
                        .and_then(Value::as_str)
                        == Some("permission_prompt")
                    && self.questions.suppress_permission_prompt(&event.agent)
            }
        }
    }

    // ---- §9 reviews ----

    #[allow(clippy::too_many_arguments)]
    async fn on_review(
        &mut self,
        installation: u64,
        repo: &str,
        number: u64,
        author: Author,
        review_id: u64,
        state: &str,
        body: &str,
        commit: &str,
    ) {
        let Some(agent) = self.sessions.by_number(repo, number).map(str::to_string) else {
            return;
        };
        self.refresh_installation(&agent, installation);
        let Some(row) = self.sessions.get(&agent).cloned() else {
            return;
        };
        if row.closed || row.kind != Kind::Pr || !self.permitted(installation, repo, &author).await
        {
            return;
        }
        let comments = match retry_once(|| {
            self.port
                .review_comments(installation, repo, number, review_id)
        })
        .await
        {
            Ok(c) => c,
            Err(e) => {
                self.github_failed("review_comments", &e);
                return;
            }
        };
        let base = row.base.clone().unwrap_or_else(|| "main".into());
        let Some(message) = prompt::review(&author.login, state, commit, body, &comments, &base)
        else {
            return;
        };
        if self.questions.is_open(&agent) {
            self.held.entry(agent.clone()).or_default().push(Held {
                reviewer: author.login.clone(),
                message,
            });
            self.line(&agent, &format!("review from @{} held", author.login));
            return;
        }
        self.deliver_review(&agent, &author.login, message).await;
    }

    async fn deliver_review(&mut self, agent: &str, reviewer: &str, message: String) {
        let action = PluginAction::SendText {
            text: message,
            submit: true,
        };
        match self.host.action(agent, &action).await {
            Ok(()) => self.line(agent, &format!("review from @{reviewer} delivered")),
            Err(e) => {
                self.counters.errors.with_label_values(&["send_text"]).inc();
                self.line(
                    agent,
                    &format!("review from @{reviewer} not delivered: {e}"),
                );
            }
        }
    }

    async fn release_held(&mut self, agent: &str) {
        if self.questions.is_open(agent) {
            return;
        }
        for h in self.held.remove(agent).unwrap_or_default() {
            self.deliver_review(agent, &h.reviewer, h.message).await;
        }
    }

    // ---- §10 ending ----

    async fn end(&mut self, agent: &str, installation: u64, reason: &str) {
        let Some(row) = self.sessions.get(agent).cloned() else {
            return;
        };
        if row.closed {
            return;
        }
        let fleet = session::fleet_of(agent).to_string();
        let mut live = self.sessions.live_in(&fleet);
        live.retain(|l| l.number != row.number);
        // §11: a refile that fails is reported on the issue and the row
        // stays open, touched so the idle check retries after another
        // `idleTimeout` rather than every tick; the agent is still running.
        let default_branch =
            match retry_once(|| self.port.default_branch(installation, &row.repo)).await {
                Ok(b) => b,
                Err(e) => {
                    self.github_failed("default_branch", &e);
                    self.refile_failed(agent, &row, &github_refusal("default_branch"))
                        .await;
                    return;
                }
            };
        let text = match retry_once(|| {
            self.port.read_file(
                installation,
                &row.repo,
                &self.cfg().config_path,
                &default_branch,
            )
        })
        .await
        {
            Ok(t) => t.unwrap_or_default(),
            Err(e) => {
                self.github_failed("read_file", &e);
                self.refile_failed(agent, &row, &github_refusal("read_file"))
                    .await;
                return;
            }
        };
        let file = match repo_config::prepare(&text, &row.repo, &fleet, &default_branch, &live) {
            Ok(f) => f,
            Err(m) => {
                tracing::warn!("github: {fleet}: {m}");
                self.counters.applies.with_label_values(&["config"]).inc();
                self.refile_failed(agent, &row, &m).await;
                return;
            }
        };
        if let Err(e) = self.host.apply_fleet(&fleet, &file).await {
            tracing::warn!("github: applying {fleet} without {agent}: {e}");
            self.counters.applies.with_label_values(&["daemon"]).inc();
            self.health.fail(format!("github: applying {fleet}: {e}"));
            self.refile_failed(agent, &row, &apply_refusal(&e)).await;
            return;
        }
        self.counters.applies.with_label_values(&["ok"]).inc();
        self.health.ok();
        self.line(agent, reason);
        // The apply makes the daemon deactivate the agent, which drops its
        // status: the final line goes out now, not on a later tick.
        self.flush_status(agent).await;
        if let Some(r) = self.sessions.get_mut(agent) {
            r.closed = true;
        }
        self.rows_dirty.insert(agent.to_string(), ());
        self.deliveries.forget(agent);
        self.held.remove(agent);
        self.questions.clear(&self.host, agent).await;
        self.publish_gauges();
    }

    /// `end` could not remove the agent: say so once on the issue and
    /// leave the row open for a later try.
    async fn refile_failed(&mut self, agent: &str, row: &Session, message: &str) {
        self.post(row.installation, &row.repo, row.number, message, "notice")
            .await;
        self.touch(agent);
    }

    // ---- the tick ----

    async fn on_tick(&mut self) {
        for (s, why) in std::mem::take(&mut self.unconfirmed_queue) {
            self.unconfirmed(s, why).await;
        }
        for s in self.deliveries.expire(Instant::now()) {
            self.unconfirmed(s, Unconfirmed::Expired).await;
        }
        for agent in self.deliveries.nudge(Instant::now()) {
            self.press_enter(&agent).await;
        }
        // idle (§10)
        let idle = self.cfg().idle_timeout;
        if !idle.is_zero() {
            let now = (self.now_secs)();
            let stale: Vec<(String, u64)> = self
                .sessions
                .agents()
                .into_iter()
                .filter_map(|a| {
                    let r = self.sessions.get(&a)?;
                    let installation = r.installation;
                    (!r.closed && now.saturating_sub(r.last_activity) > idle.as_secs())
                        .then_some((a, installation))
                })
                .collect();
            for (agent, installation) in stale {
                let after = idle_wording(idle);
                let reason = format!(
                    "stopped after {after} idle — mention @{} to resume",
                    self.slug
                );
                self.end(&agent, installation, &reason).await;
            }
        }
        // status flushes (§8.3)
        let now = Instant::now();
        let due: Vec<String> = self
            .status_dirty
            .iter()
            .filter(|(_, at)| now.duration_since(**at) >= STATUS_COALESCE)
            .map(|(a, _)| a.clone())
            .collect();
        for agent in due {
            self.flush_status(&agent).await;
        }
        // row write-back (§8.4)
        for agent in std::mem::take(&mut self.rows_dirty).into_keys() {
            if let Some(row) = self.sessions.get(&agent).cloned()
                && let Err(e) = self.sessions.set(&self.host, &agent, row).await
            {
                tracing::warn!("github: writing row for {agent}: {e}");
            }
        }
    }

    // ---- §8.3 the status comment ----

    fn touch(&mut self, agent: &str) {
        if let Some(r) = self.sessions.get_mut(agent) {
            r.last_activity = (self.now_secs)();
        }
        self.rows_dirty.insert(agent.to_string(), ());
    }

    fn line(&mut self, agent: &str, text: &str) {
        let at = (self.now_secs)();
        self.statuses
            .entry(agent.to_string())
            .or_insert_with(|| Status::new(agent))
            .push(at, text);
        self.status_dirty
            .entry(agent.to_string())
            .or_insert_with(Instant::now);
    }

    async fn flush_status(&mut self, agent: &str) {
        self.status_dirty.remove(agent);
        let (Some(row), Some(status)) =
            (self.sessions.get(agent).cloned(), self.statuses.get(agent))
        else {
            return;
        };
        let body = status.render();
        if let Some(id) = row.status_comment {
            match retry_once(|| {
                self.port
                    .edit_comment(row.installation, &row.repo, id, &body)
            })
            .await
            {
                Ok(()) => {
                    self.counters.status_edits.with_label_values(&["ok"]).inc();
                    return;
                }
                // deleted: post a new one below
                Err(GitHubError::NotFound) => {}
                Err(e) => {
                    self.counters
                        .status_edits
                        .with_label_values(&["failed"])
                        .inc();
                    // retried on the next change (§8.3), not every tick
                    self.github_failed("edit_comment", &e);
                    return;
                }
            }
        }
        match retry_once(|| {
            self.port
                .comment(row.installation, &row.repo, row.number, &body)
        })
        .await
        {
            Ok(id) => {
                self.counters
                    .status_edits
                    .with_label_values(&["reposted"])
                    .inc();
                if let Some(r) = self.sessions.get_mut(agent) {
                    r.status_comment = Some(id);
                }
                self.rows_dirty.insert(agent.into(), ());
            }
            Err(e) => self.github_failed("comment", &e),
        }
    }

    async fn on_phase(&mut self, change: PhaseChange) {
        let Some(config) = self.agents.get(&change.agent).cloned() else {
            return;
        };
        if self.sessions.get(&change.agent).is_none() {
            return;
        }
        let phase = format!("{:?}", change.to).to_lowercase();
        self.statuses
            .entry(change.agent.clone())
            .or_insert_with(|| Status::new(&change.agent))
            .phase = phase;
        if config.phases {
            self.line(&change.agent, &render::phase_message(&change));
        } else {
            self.status_dirty
                .entry(change.agent.clone())
                .or_insert_with(Instant::now);
        }
    }

    // ---- posting helpers ----

    /// Posts `body` split into comments (§8.5); a failed part stops the
    /// rest. Answers the first comment's id.
    async fn post(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
        body: &str,
        kind: &str,
    ) -> Option<u64> {
        let max_parts = self
            .config
            .as_ref()
            .map_or(crate::config::DEFAULT_MAX_PARTS, |c| c.max_parts);
        let mut first = None;
        for part in render::split(body, COMMENT_LIMIT, max_parts) {
            match retry_once(|| self.port.comment(installation, repo, number, &part)).await {
                Ok(id) => {
                    self.counters.messages_sent.with_label_values(&[kind]).inc();
                    self.health.ok();
                    first.get_or_insert(id);
                }
                Err(e) => {
                    self.github_failed("comment", &e);
                    return first;
                }
            }
        }
        first
    }

    async fn react(&self, installation: u64, repo: &str, target: Target, content: &str) {
        if let Err(e) = self.port.react(installation, repo, target, content).await {
            self.counters.errors.with_label_values(&["react"]).inc();
            tracing::warn!("github: reacting {content} on {target:?}: {e}");
        }
    }
}

/// What the first prompt needs, kept from the start until `SessionStart`.
#[derive(Debug, Clone)]
struct FirstPrompt {
    title: String,
    body: String,
    url: String,
    branch: String,
    base: String,
    asker: String,
    comment: Option<String>,
    resumed: bool,
    target: Target,
}

/// An event's status line (§8.3): common's `event_message`, whose bold
/// markers are for a chat thread, as the plain text the spec shows
/// (`needs you: Claude needs your permission to use Bash`).
/// What an `apply_fleet` failure posts on the issue (§6): the daemon's
/// 400 carries the resolver's message and goes out verbatim; anything
/// else is a fixed line, the detail in the plugin log.
fn apply_refusal(e: &SdkError) -> String {
    match e {
        SdkError::Status { status: 400, .. } => e.to_string(),
        _ => "the daemon refused the apply; see the plugin log".into(),
    }
}

/// What a GitHub call failure posts on the issue: a fixed line naming the
/// call, the detail in the plugin log (`github_failed`).
fn github_refusal(kind: &str) -> String {
    format!("github: {kind} failed; see the plugin log")
}

/// `idleTimeout` as the idle line says it: whole hours, else whole
/// minutes, else seconds.
fn idle_wording(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn status_line(event: &HookEvent) -> String {
    render::event_message(event).replace("**", "")
}

fn head_or_default(
    fleet: &str,
    kind: Kind,
    number: u64,
    pr: Option<&(String, String, String)>,
) -> String {
    pr.map(|(h, _, _)| h.clone())
        .unwrap_or_else(|| format!("balerix/{fleet}/repo/{}", kind.agent_name(number)))
}

/// One retry after `Retry-After`, when it is short (matrix's rule).
async fn retry_once<T, F, Fut>(call: F) -> Result<T, GitHubError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, GitHubError>>,
{
    let attempt = call().await;
    if let Err(GitHubError::RateLimited { retry_after_ms }) = attempt {
        let delay = Duration::from_millis(retry_after_ms);
        if delay > MAX_INLINE_RETRY {
            tracing::warn!("github: rate limited for {retry_after_ms} ms; not waiting");
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
    use crate::github::fake::{Call, FakePort};
    use balerix_plugin_sdk::testing::{FakeHost, event};
    use serde_json::json;

    pub(super) const FILE: &str = "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo:\n    repo: acme/api\n    agents: {}\n";

    fn counters() -> Counters {
        Counters::new(&Metrics::new("github")).unwrap()
    }

    pub(super) fn daemon_config() -> DaemonConfig {
        crate::config::parse_daemon(&json!({ "appId": 1, "privateKey": "k", "webhookSecret": "s" }))
            .unwrap()
    }

    pub(super) async fn actor() -> (FakeHost, FakePort, Actor<FakePort>) {
        let fake = FakeHost::start("tok", json!({}), Vec::new()).await;
        let host = Host::new(fake.env("github", std::path::Path::new("scratch"))).unwrap();
        let port = FakePort::new("balerix");
        port.set_permission("acme/api", "alice", Permission::Write);
        port.set_permission("acme/api", "bob", Permission::Maintain);
        port.set_permission("acme/api", "eve", Permission::Read);
        port.set_file("acme/api", "main", ".balerix.yaml", FILE);
        let mut a = Actor::new(
            host,
            port.clone(),
            counters(),
            Health::new(),
            "balerix".into(),
        );
        a.now_secs = || 14 * 3600 + 2 * 60;
        a.handle(Command::Configure(daemon_config())).await;
        (fake, port, a)
    }

    pub(super) fn user(login: &str) -> Author {
        Author {
            login: login.into(),
            bot: false,
        }
    }

    pub(super) fn comment(number: u64, login: &str, id: u64, body: &str) -> Command {
        Command::Webhook(WebhookEvent::Comment {
            repo: "acme/api".into(),
            installation: 7,
            number,
            author: user(login),
            comment_id: id,
            body: body.into(),
            is_pr: false,
        })
    }

    pub(super) fn reactions(calls: &[Call]) -> Vec<(Target, String)> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::React {
                    target, content, ..
                } => Some((*target, content.clone())),
                _ => None,
            })
            .collect()
    }

    pub(super) fn comments(calls: &[Call]) -> Vec<(u64, String)> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::Comment { number, body, .. } => Some((*number, body.clone())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_mention_by_a_collaborator_starts_a_session_and_the_applied_file_carries_the_agent() {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix please look at this"))
            .await;
        let applied = fake.applied_fleets();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].0, "gh-acme-api");
        let file = &applied[0].1;
        assert_eq!(file["name"], "gh-acme-api");
        assert_eq!(file["crews"]["repo"]["ref"], "main");
        assert_eq!(
            file["crews"]["repo"]["agents"]["issue-12"]["plugins"]["github"],
            json!({ "kind": "issue", "number": 12 })
        );
        assert_eq!(
            reactions(&port.calls()),
            vec![(Target::Comment(5), EYES.to_string())]
        );
        let c = comments(&port.calls());
        assert_eq!(c.len(), 1, "the status comment: {c:?}");
        assert!(
            c[0].1
                .starts_with("**balerix** · `gh-acme-api/repo/issue-12` · **starting**"),
            "{}",
            c[0].1
        );
        assert!(c[0].1.contains("- 14:02 starting"));
        let row = fake.kv_json("session/gh-acme-api/repo/issue-12").unwrap();
        assert_eq!(row["status_comment"], 1001);
        assert_eq!(
            fake.kv_json("repo/gh-acme-api"),
            None,
            "the repo key is bytes, not JSON"
        );
        assert_eq!(
            fake.kv().get("repo/gh-acme-api").map(|(b, _)| b.clone()),
            Some(b"acme/api".to_vec())
        );
        assert!(
            fake.actions_for("gh-acme-api/repo/issue-12").is_empty(),
            "no prompt before SessionStart"
        );
        assert_eq!(a.sessions.open_count(), 1);
    }

    #[tokio::test]
    async fn a_non_collaborator_a_bot_and_the_apps_own_comments_are_silent() {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "eve", 5, "@balerix go")).await;
        a.handle(comment(12, "nobody", 6, "@balerix go")).await;
        a.handle(Command::Webhook(WebhookEvent::Comment {
            repo: "acme/api".into(),
            installation: 7,
            number: 12,
            author: Author {
                login: "dependabot[bot]".into(),
                bot: true,
            },
            comment_id: 7,
            body: "@balerix go".into(),
            is_pr: false,
        }))
        .await;
        a.handle(Command::Webhook(WebhookEvent::Comment {
            repo: "acme/api".into(),
            installation: 7,
            number: 12,
            author: Author {
                login: "balerix[bot]".into(),
                bot: true,
            },
            comment_id: 8,
            body: "**balerix** status".into(),
            is_pr: false,
        }))
        .await;
        assert!(fake.applied_fleets().is_empty());
        assert!(reactions(&port.calls()).is_empty() && comments(&port.calls()).is_empty());
        assert_eq!(
            a.counters.inbound.with_label_values(&["unpermitted"]).get(),
            2
        );
        assert_eq!(
            a.counters.inbound.with_label_values(&["own_or_bot"]).get(),
            2
        );
    }

    #[tokio::test]
    async fn a_fork_pr_is_refused_and_a_pr_gets_its_head_as_branch() {
        let (fake, port, mut a) = actor().await;
        let pr = |head_repo: &str| {
            Command::Webhook(WebhookEvent::PrOpened {
                repo: "acme/api".into(),
                installation: 7,
                number: 34,
                author: user("alice"),
                title: "t".into(),
                body: "@balerix review".into(),
                url: "u".into(),
                head: "feature/x".into(),
                head_repo: head_repo.into(),
                base: "main".into(),
            })
        };
        a.handle(pr("someone/api")).await;
        assert!(fake.applied_fleets().is_empty());
        assert_eq!(
            comments(&port.calls()),
            vec![(
                34,
                "sessions on pull requests from forks are not supported".into()
            )]
        );
        assert_eq!(
            reactions(&port.calls()),
            vec![
                (Target::Issue(34), EYES.into()),
                (Target::Issue(34), CONFUSED.into())
            ]
        );
        port.take_calls();
        a.handle(pr("acme/api")).await;
        let file = &fake.applied_fleets()[0].1;
        assert_eq!(
            file["crews"]["repo"]["agents"]["pr-34"]["branch"],
            "feature/x"
        );
    }

    #[tokio::test]
    async fn a_config_error_is_posted_once_and_applies_nothing() {
        let (fake, port, mut a) = actor().await;
        port.set_file(
            "acme/api",
            "main",
            ".balerix.yaml",
            "apiVersion: balerix/v1\nkind: Fleet\ncrews:\n  repo: { repo: other/thing }\n",
        );
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        assert!(fake.applied_fleets().is_empty());
        let c = comments(&port.calls());
        assert_eq!(c.len(), 1);
        assert!(
            c[0].1
                .starts_with(".balerix.yaml: crews.repo.repo: expected acme/api"),
            "{}",
            c[0].1
        );
        assert_eq!(
            reactions(&port.calls()).last().map(|r| r.1.as_str()),
            Some(CONFUSED)
        );
        assert!(fake.kv_json("session/gh-acme-api/repo/issue-12").is_none());
        assert_eq!(a.counters.applies.with_label_values(&["config"]).get(), 1);
    }

    #[tokio::test]
    async fn a_daemon_refusal_is_posted_verbatim() {
        let (fake, port, mut a) = actor().await;
        fake.fail_manage(Some((400, "defaults.sandbox: not allowed in a plugin-applied fleet file; the host's default applies")));
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        let c = comments(&port.calls());
        assert_eq!(
            c[0].1,
            "daemon: HTTP 400: defaults.sandbox: not allowed in a plugin-applied fleet file; the host's default applies"
        );
        assert_eq!(a.counters.applies.with_label_values(&["daemon"]).get(), 1);
    }

    #[tokio::test]
    async fn a_name_collision_is_refused() {
        let (fake, port, mut a) = actor().await;
        let host = Host::new(fake.env("github", std::path::Path::new("scratch"))).unwrap();
        // a repository whose name sanitises the same already owns the fleet name
        session::set_repo_of_fleet(&host, "gh-acme-api", "acme/api.")
            .await
            .unwrap();
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        assert!(fake.applied_fleets().is_empty());
        assert_eq!(
            comments(&port.calls())[0].1,
            "fleet name gh-acme-api already stands for acme/api.; clear the plugin's KV key \
             repo/gh-acme-api (plugins/github/kv/repo/gh-acme-api under the daemon's state \
             root) to reuse it"
        );
    }

    #[tokio::test]
    async fn a_daemon_failure_other_than_400_posts_a_fixed_line() {
        let (fake, port, mut a) = actor().await;
        fake.fail_manage(Some((500, "disk full")));
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        let c = comments(&port.calls());
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].1, "the daemon refused the apply; see the plugin log");
        assert!(!c[0].1.contains("disk full"));
        assert_eq!(a.counters.applies.with_label_values(&["daemon"]).get(), 1);
    }

    #[tokio::test]
    async fn a_fork_pr_is_refused_on_the_comment_path() {
        let (fake, port, mut a) = actor().await;
        port.set_issue(
            "acme/api",
            40,
            IssueInfo {
                pr: Some(("feature/x".into(), "someone/api".into(), "main".into())),
                ..Default::default()
            },
        );
        a.handle(comment(40, "alice", 9, "@balerix review this"))
            .await;
        assert!(fake.applied_fleets().is_empty());
        assert_eq!(
            comments(&port.calls()),
            vec![(
                40,
                "sessions on pull requests from forks are not supported".into()
            )]
        );
        assert_eq!(
            reactions(&port.calls()),
            vec![
                (Target::Comment(9), EYES.into()),
                (Target::Comment(9), CONFUSED.into())
            ]
        );
    }

    #[test]
    fn the_idle_wording_keeps_whole_units_only() {
        assert_eq!(idle_wording(Duration::from_secs(90 * 60)), "90m");
        assert_eq!(idle_wording(Duration::from_secs(2 * 3600)), "2h");
        assert_eq!(idle_wording(Duration::from_secs(30)), "30s");
        assert_eq!(idle_wording(Duration::from_secs(90)), "90s");
    }

    #[tokio::test]
    async fn the_permission_check_is_cached_for_five_minutes() {
        let (_fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        a.handle(comment(12, "alice", 6, "and this")).await;
        let checks = port
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Permission { .. }))
            .count();
        assert_eq!(checks, 1);
    }

    #[tokio::test]
    async fn a_mention_in_the_issue_body_starts_and_reacts_on_the_issue() {
        let (fake, port, mut a) = actor().await;
        a.handle(Command::Webhook(WebhookEvent::IssueOpened {
            repo: "acme/api".into(),
            installation: 7,
            number: 12,
            author: user("bob"),
            title: "Bug".into(),
            body: "@balerix fix".into(),
            url: "u".into(),
        }))
        .await;
        assert_eq!(fake.applied_fleets().len(), 1);
        assert_eq!(
            reactions(&port.calls())[0],
            (Target::Issue(12), EYES.into())
        );
    }

    #[tokio::test]
    async fn deactivate_drops_an_open_row_but_keeps_a_closed_one() {
        let (fake, _port, mut a) = actor().await;
        let agent = "gh-acme-api/repo/issue-12";
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        a.handle(Command::Deactivate {
            agent: agent.into(),
        })
        .await;
        assert!(
            fake.kv_json("session/gh-acme-api/repo/issue-12").is_none(),
            "an open row the daemon took away goes"
        );
        a.handle(comment(12, "alice", 6, "@balerix again")).await;
        a.handle(Command::Activate {
            agent: agent.into(),
            config: AgentConfig::default(),
        })
        .await;
        a.sessions.get_mut(agent).unwrap().closed = true;
        a.handle(Command::Deactivate {
            agent: agent.into(),
        })
        .await;
        assert!(
            fake.kv_json("session/gh-acme-api/repo/issue-12").is_some(),
            "a closed row stays for the resume"
        );
        assert!(!a.agents.contains_key(agent));
    }

    #[tokio::test]
    async fn an_auth_failure_on_the_permission_check_fails_health() {
        let (fake, port, mut a) = actor().await;
        port.fail_next(GitHubError::Auth("bad token".into()));
        a.handle(comment(12, "alice", 5, "@balerix go")).await;
        assert!(a.health.get().is_err());
        assert_eq!(a.counters.errors.with_label_values(&["auth"]).get(), 1);
        assert_eq!(
            a.counters.errors.with_label_values(&["permission"]).get(),
            1
        );
        assert!(fake.applied_fleets().is_empty());
        assert!(comments(&port.calls()).is_empty() && reactions(&port.calls()).is_empty());
    }

    fn actions_text(a: &PluginAction) -> String {
        match a {
            PluginAction::SendText { text, .. } => text.clone(),
            _ => String::new(),
        }
    }

    /// A live session on issue 12 whose first prompt went out and was
    /// confirmed by its `UserPromptSubmit`, with the calls cleared.
    async fn started() -> (FakeHost, FakePort, Actor<FakePort>) {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix please look"))
            .await;
        a.handle(Command::Activate {
            agent: AGENT.into(),
            config: crate::config::parse_agent(&json!({ "kind": "issue", "number": 12 })).unwrap(),
        })
        .await;
        let mut e = event(AGENT, "SessionStart", json!({ "source": "startup" }));
        e.session_id = Some("0199aa11-abcd".into());
        a.handle(Command::Events(vec![e])).await;
        let first = actions_text(&fake.actions_for(AGENT)[0]);
        a.handle(Command::Events(vec![during(
            "UserPromptSubmit",
            json!({ "prompt": first }),
        )]))
        .await;
        a.handle(Command::Events(vec![during("Stop", json!({}))]))
            .await;
        port.take_calls();
        (fake, port, a)
    }
    const AGENT: &str = "gh-acme-api/repo/issue-12";

    fn during(name: &str, payload: Value) -> HookEvent {
        let mut e = event(AGENT, name, payload);
        e.session_id = Some("0199aa11-abcd".into());
        e
    }

    fn edits(calls: &[Call]) -> Vec<String> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::EditComment { body, .. } => Some(body.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn the_first_prompt_goes_out_on_session_start_and_never_before() {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix please look"))
            .await;
        a.handle(Command::Activate {
            agent: AGENT.into(),
            config: crate::config::parse_agent(&json!({ "kind": "issue", "number": 12 })).unwrap(),
        })
        .await;
        a.handle(Command::Events(vec![during(
            "Notification",
            json!({ "message": "early" }),
        )]))
        .await;
        assert!(fake.actions_for(AGENT).is_empty());
        a.handle(Command::Events(vec![during(
            "SessionStart",
            json!({ "source": "startup" }),
        )]))
        .await;
        let actions = fake.actions_for(AGENT);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            PluginAction::SendText { text, submit } => {
                assert!(*submit);
                insta::assert_snapshot!(text);
                assert!(
                    text.contains("issue #12")
                        && text.contains("@alice asked:")
                        && text.contains("please look")
                );
            }
            other => panic!("{other:?}"),
        }
        // status edits are coalesced and flushed on the tick (§8.3); the
        // clock is paused only now, after the fake host's real round trips
        tokio::time::pause();
        tokio::time::advance(STATUS_COALESCE).await;
        a.handle(Command::Tick).await;
        // the status comment was edited, not re-posted: header names the session and the line
        let e = edits(&port.calls());
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(
            e[0].contains("session `0199aa11`") && e[0].contains("session started (startup)"),
            "{}",
            e[0]
        );
        // the mention's 👍 arrives with the matching submit (§8.7)
        a.handle(Command::Events(vec![during(
            "UserPromptSubmit",
            json!({ "prompt": actions_text(&actions[0]) }),
        )]))
        .await;
        assert_eq!(
            reactions(&port.calls()).last(),
            Some(&(Target::Comment(5), PLUS_ONE.to_string()))
        );
    }

    /// A tick `d` later, on a running clock: a paused one jumps to the
    /// HTTP client's timeout while a host round trip waits on real I/O,
    /// which would age the prompts under test by ten seconds a call.
    async fn tick_after(a: &mut Actor<FakePort>, d: Duration) {
        tokio::time::pause();
        tokio::time::advance(d).await;
        tokio::time::resume();
        a.handle(Command::Tick).await;
    }

    /// #99: a prompt sent on `SessionStart` reaches the composer while
    /// Claude's TUI is starting and loses its Enter.
    #[tokio::test]
    async fn an_unconfirmed_first_prompt_gets_enter_again_until_claude_takes_it() {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix please look"))
            .await;
        a.handle(Command::Activate {
            agent: AGENT.into(),
            config: crate::config::parse_agent(
                &json!({ "kind": "issue", "number": 12, "keyDelayMs": 40 }),
            )
            .unwrap(),
        })
        .await;
        a.handle(Command::Events(vec![during(
            "SessionStart",
            json!({ "source": "startup" }),
        )]))
        .await;
        let first = fake.actions_for(AGENT);
        assert_eq!(first.len(), 1, "{first:?}");
        let enter = PluginAction::SendKeys {
            steps: vec![KeyStep::Key(Key::Enter)],
            delay_ms: 40,
        };

        tick_after(&mut a, Duration::from_secs(4)).await;
        assert_eq!(fake.actions_for(AGENT).len(), 1, "too early for an Enter");
        tick_after(&mut a, Duration::from_secs(1)).await;
        assert_eq!(fake.actions_for(AGENT)[1..], *std::slice::from_ref(&enter));
        tick_after(&mut a, Duration::from_secs(5)).await;
        assert_eq!(fake.actions_for(AGENT)[1..], [enter.clone(), enter.clone()]);

        // Claude takes it: 👍, and the turn's end brings no further Enter
        a.handle(Command::Events(vec![during(
            "UserPromptSubmit",
            json!({ "prompt": actions_text(&first[0]) }),
        )]))
        .await;
        assert_eq!(
            reactions(&port.calls()).last(),
            Some(&(Target::Comment(5), PLUS_ONE.to_string()))
        );
        a.handle(Command::Events(vec![during("Stop", json!({}))]))
            .await;
        tick_after(&mut a, Duration::from_secs(60)).await;
        assert_eq!(fake.actions_for(AGENT).len(), 3);
        assert!(
            !edits(&port.calls())
                .iter()
                .any(|e| e.contains("not confirmed")),
            "{:?}",
            edits(&port.calls())
        );
    }

    /// #99: a cold start under nono swallowed an Enter 20 s after
    /// `SessionStart` and took one at 60 s, so the first prompt is not
    /// reported at `confirmWindow` alone.
    #[tokio::test]
    async fn the_first_prompt_has_a_start_up_allowance_before_it_is_reported() {
        let (fake, port, mut a) = actor().await;
        a.handle(comment(12, "alice", 5, "@balerix please look"))
            .await;
        a.handle(Command::Activate {
            agent: AGENT.into(),
            config: crate::config::parse_agent(&json!({ "kind": "issue", "number": 12 })).unwrap(),
        })
        .await;
        a.handle(Command::Events(vec![during(
            "SessionStart",
            json!({ "source": "startup" }),
        )]))
        .await;
        tick_after(&mut a, Duration::from_secs(31)).await;
        assert!(
            !reactions(&port.calls()).contains(&(Target::Comment(5), CONFUSED.into())),
            "past confirmWindow, inside the allowance"
        );
        assert_eq!(fake.actions_for(AGENT).len(), 2, "the prompt and one Enter");
        tick_after(&mut a, Duration::from_secs(5)).await;
        assert_eq!(fake.actions_for(AGENT).len(), 3, "Enter again at 36 s");
        tick_after(&mut a, Duration::from_secs(54)).await;
        assert_eq!(
            reactions(&port.calls()).last(),
            Some(&(Target::Comment(5), CONFUSED.into()))
        );
        tick_after(&mut a, STATUS_COALESCE).await;
        assert!(
            edits(&port.calls())
                .last()
                .unwrap()
                .contains("prompt from @alice not confirmed after 90s"),
            "{:?}",
            edits(&port.calls()).last()
        );
    }

    #[tokio::test]
    async fn a_refused_enter_is_counted_and_the_prompt_still_expires() {
        let (fake, port, mut a) = started().await;
        a.handle(comment(12, "bob", 9, "hello")).await;
        fake.fail_actions(Some("window gone"));
        let errors = a.counters.errors.with_label_values(&["send_keys"]).get();
        tick_after(&mut a, Duration::from_secs(5)).await;
        assert_eq!(
            a.counters.errors.with_label_values(&["send_keys"]).get(),
            errors + 1
        );
        assert!(
            !comments(&port.calls())
                .iter()
                .any(|(_, b)| b.starts_with("not delivered")),
            "a nudge is not the user's message: no note for it"
        );
        tick_after(&mut a, Duration::from_secs(26)).await;
        assert_eq!(
            reactions(&port.calls()).last(),
            Some(&(Target::Comment(9), CONFUSED.into()))
        );
    }

    #[tokio::test]
    async fn a_permitted_comment_while_live_is_sent_eyes_then_plus_one() {
        let (fake, port, mut a) = started().await;
        a.handle(comment(12, "bob", 9, "also check the tests"))
            .await;
        assert_eq!(
            fake.actions_for(AGENT).last(),
            Some(&PluginAction::SendText {
                text: "also check the tests".into(),
                submit: true
            })
        );
        assert_eq!(
            reactions(&port.calls()),
            vec![(Target::Comment(9), EYES.into())]
        );
        a.handle(Command::Events(vec![during(
            "UserPromptSubmit",
            json!({ "prompt": "also  check the tests" }),
        )]))
        .await;
        assert_eq!(
            reactions(&port.calls()),
            vec![
                (Target::Comment(9), EYES.into()),
                (Target::Comment(9), PLUS_ONE.into())
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_send_posts_the_error_and_minus_one() {
        let (fake, port, mut a) = started().await;
        fake.fail_actions(Some("window gone"));
        a.handle(comment(12, "bob", 9, "hello")).await;
        assert!(
            comments(&port.calls())
                .iter()
                .any(|(_, b)| b.starts_with("not delivered to gh-acme-api/repo/issue-12: "))
        );
        assert_eq!(
            reactions(&port.calls()),
            vec![(Target::Comment(9), MINUS_ONE.into())]
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_prompt_gets_confused_and_a_status_line() {
        let (_fake, port, mut a) = started().await;
        a.handle(comment(12, "bob", 9, "hello")).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        a.handle(Command::Tick).await;
        assert_eq!(
            reactions(&port.calls()).last(),
            Some(&(Target::Comment(9), CONFUSED.into()))
        );
        tokio::time::advance(STATUS_COALESCE).await;
        a.handle(Command::Tick).await;
        assert!(
            edits(&port.calls())
                .last()
                .unwrap()
                .contains("prompt from @bob not confirmed after 30s")
        );
    }

    #[tokio::test]
    async fn a_slash_command_gets_eyes_only() {
        let (_fake, port, mut a) = started().await;
        a.handle(comment(12, "bob", 9, "/compact")).await;
        assert_eq!(
            reactions(&port.calls()),
            vec![(Target::Comment(9), EYES.into())]
        );
        assert_eq!(a.deliveries.pending(AGENT), 0);
    }

    #[tokio::test]
    async fn a_stop_posts_the_message_and_a_long_one_is_split_and_a_failed_part_stops_the_rest() {
        let (_fake, port, mut a) = started().await;
        a.handle(Command::Events(vec![during(
            "Stop",
            json!({ "last_assistant_message": "Done: I fixed the refund path." }),
        )]))
        .await;
        assert_eq!(
            comments(&port.calls()),
            vec![(12, "Done: I fixed the refund path.".into())]
        );
        port.take_calls();
        let long = "abcd\n".repeat(30_000);
        a.handle(Command::Events(vec![during(
            "Stop",
            json!({ "last_assistant_message": long }),
        )]))
        .await;
        let c = comments(&port.calls());
        assert!(c.len() >= 2 && c.len() <= 10, "{}", c.len());
        assert!(c[0].1.ends_with(&format!("(1/{})", c.len())));
        port.take_calls();
        port.fail_next(GitHubError::Other("boom".into()));
        a.handle(Command::Events(vec![during(
            "Stop",
            json!({ "last_assistant_message": "abcd\n".repeat(30_000) }),
        )]))
        .await;
        assert!(
            comments(&port.calls()).is_empty(),
            "the first part failed; nothing after it"
        );
        // an empty message posts nothing and adds a status line
        port.take_calls();
        a.handle(Command::Events(vec![during("Stop", json!({}))]))
            .await;
        assert!(comments(&port.calls()).is_empty());
        assert!(a.statuses[AGENT].render().contains("turn finished"));
    }

    #[tokio::test]
    async fn the_status_comment_is_edited_not_reposted_and_coalesced_and_reposted_when_deleted() {
        let (_fake, port, mut a) = started().await;
        tokio::time::pause();
        a.handle(Command::Events(vec![during("Notification", json!({ "message": "Claude needs your permission to use Bash", "notification_type": "permission_prompt" }))])).await;
        a.handle(Command::Events(vec![during(
            "Notification",
            json!({ "message": "second" }),
        )]))
        .await;
        assert!(edits(&port.calls()).is_empty(), "coalesced: nothing yet");
        tokio::time::advance(STATUS_COALESCE).await;
        a.handle(Command::Tick).await;
        let e = edits(&port.calls());
        assert_eq!(e.len(), 1, "one edit for two lines");
        assert!(
            e[0].contains("needs you: Claude needs your permission to use Bash")
                && e[0].contains("- 14:02 needs you: second")
        );
        assert!(comments(&port.calls()).is_empty());
        port.take_calls();
        port.delete_comment(1001);
        a.handle(Command::Events(vec![during(
            "Notification",
            json!({ "message": "third" }),
        )]))
        .await;
        tokio::time::advance(STATUS_COALESCE).await;
        a.handle(Command::Tick).await;
        assert_eq!(comments(&port.calls()).len(), 1, "a new status comment");
        assert_eq!(a.sessions.get(AGENT).unwrap().status_comment, Some(1002));
    }

    #[tokio::test]
    async fn a_question_round_trip_through_common() {
        use balerix_plugin_common::question::fixtures::{color, input};
        let (fake, port, mut a) = started().await;
        a.handle(Command::Events(vec![during(
            "PreToolUse",
            json!({ "tool_name": "AskUserQuestion", "tool_input": input(&[color()]) }),
        )]))
        .await;
        let c = comments(&port.calls());
        assert_eq!(c.len(), 1);
        assert!(
            c[0].1.contains("**question**")
                && c[0].1.contains("Reply with a comment: a number or a label"),
            "{}",
            c[0].1
        );
        port.take_calls();
        a.handle(comment(12, "alice", 20, "3")).await;
        assert!(matches!(
            fake.actions_for(AGENT).last(),
            Some(PluginAction::SendKeys { .. })
        ));
        let c = comments(&port.calls());
        assert_eq!(c[0].1, "**answering** Color → Blue");
        assert_eq!(
            reactions(&port.calls()),
            vec![(Target::Comment(20), PLUS_ONE.into())]
        );
        let echo = c[0].0; // the echo's number; its id is the last minted
        let _ = echo;
        port.take_calls();
        a.handle(Command::Events(vec![during("PostToolUse", json!({ "tool_name": "AskUserQuestion", "tool_response": { "answers": { "Which color?": "Blue" } } }))])).await;
        assert_eq!(
            reactions(&port.calls()),
            vec![(Target::Comment(1003), HOORAY.into())],
            "✓ on the echo"
        );
    }

    #[tokio::test]
    async fn a_review_renders_one_message_and_waits_behind_an_open_question() {
        use crate::github::ReviewComment;
        use balerix_plugin_common::question::fixtures::{color, input};
        let (fake, port, mut a) = actor().await;
        let pr = Command::Webhook(WebhookEvent::PrOpened {
            repo: "acme/api".into(),
            installation: 7,
            number: 34,
            author: user("alice"),
            title: "t".into(),
            body: "@balerix review".into(),
            url: "u".into(),
            head: "feature/x".into(),
            head_repo: "acme/api".into(),
            base: "main".into(),
        });
        a.handle(pr).await;
        let agent = "gh-acme-api/repo/pr-34";
        a.handle(Command::Activate {
            agent: agent.into(),
            config: crate::config::parse_agent(&json!({ "kind": "pr", "number": 34 })).unwrap(),
        })
        .await;
        let mut e = event(agent, "SessionStart", json!({ "source": "startup" }));
        e.session_id = Some("s".into());
        a.handle(Command::Events(vec![e])).await;
        port.set_review_comments(
            "acme/api",
            9,
            vec![ReviewComment {
                path: "src/lib.rs".into(),
                side: "RIGHT".into(),
                line: Some(42),
                original_line: None,
                diff_hunk: "+    let x = foo();".into(),
                body: "panics".into(),
            }],
        );
        let review = || {
            Command::Webhook(WebhookEvent::ReviewSubmitted {
                repo: "acme/api".into(),
                installation: 7,
                number: 34,
                author: user("bob"),
                review_id: 9,
                state: "changes_requested".into(),
                body: "Close.".into(),
                commit: "3f9c2a1dead".into(),
            })
        };
        a.handle(review()).await;
        let sent = fake.actions_for(agent);
        let text = actions_text(sent.last().unwrap());
        assert!(
            text.starts_with("Review by @bob: changes requested, at 3f9c2a1\n"),
            "{text}"
        );
        assert!(text.contains("src/lib.rs line 42 (new)") && text.contains("Overall:\nClose."));
        assert!(
            a.statuses[agent]
                .render()
                .contains("review from @bob delivered")
        );
        // held behind a question
        let mut q = event(
            agent,
            "PreToolUse",
            json!({ "tool_name": "AskUserQuestion", "tool_input": input(&[color()]) }),
        );
        q.session_id = Some("s".into());
        a.handle(Command::Events(vec![q])).await;
        let before = fake.actions_for(agent).len();
        a.handle(review()).await;
        assert_eq!(fake.actions_for(agent).len(), before, "held");
        assert!(a.statuses[agent].render().contains("review from @bob held"));
        let mut done = event(
            agent,
            "PostToolUse",
            json!({ "tool_name": "AskUserQuestion", "tool_response": { "answers": {} } }),
        );
        done.session_id = Some("s".into());
        a.handle(Command::Events(vec![done])).await;
        assert_eq!(
            fake.actions_for(agent).len(),
            before + 1,
            "delivered when the question cleared"
        );
        // a stranger's review is ignored
        a.handle(Command::Webhook(WebhookEvent::ReviewSubmitted {
            repo: "acme/api".into(),
            installation: 7,
            number: 34,
            author: user("eve"),
            review_id: 10,
            state: "approved".into(),
            body: "lgtm".into(),
            commit: "c".into(),
        }))
        .await;
        assert_eq!(fake.actions_for(agent).len(), before + 1);
    }

    #[tokio::test]
    async fn close_removes_the_agent_and_a_later_comment_is_told_the_session_ended() {
        let (fake, port, mut a) = started().await;
        a.handle(Command::Webhook(WebhookEvent::Closed {
            repo: "acme/api".into(),
            installation: 7,
            number: 12,
            merged: false,
        }))
        .await;
        let applied = fake.applied_fleets();
        assert_eq!(applied.len(), 2);
        assert_eq!(
            applied[1].1["crews"]["repo"]["agents"],
            json!({}),
            "applied without the agent"
        );
        assert!(a.sessions.get(AGENT).unwrap().closed);
        assert!(a.statuses[AGENT].render().contains("closed"));
        port.take_calls();
        a.handle(comment(12, "bob", 30, "one more thing")).await;
        a.handle(comment(12, "bob", 31, "and another")).await;
        assert_eq!(
            reactions(&port.calls()),
            vec![
                (Target::Comment(30), CONFUSED.into()),
                (Target::Comment(31), CONFUSED.into())
            ]
        );
        assert_eq!(
            comments(&port.calls()),
            vec![(
                12,
                "this session has ended; mention @balerix to start a new one".into()
            )],
            "once"
        );
        assert!(
            fake.actions_for(AGENT).len() == 1,
            "nothing sent after the close"
        );
    }

    #[tokio::test]
    async fn the_idle_tick_ends_the_session_and_the_next_mention_resumes_with_the_branch_line() {
        let (fake, port, mut a) = started().await;
        a.now_secs = || 14 * 3600 + 2 * 60 + 2 * 3600 + 1;
        a.handle(Command::Tick).await;
        assert!(a.sessions.get(AGENT).unwrap().closed);
        assert!(
            a.statuses[AGENT]
                .render()
                .contains("stopped after 2h idle — mention @balerix to resume")
        );
        assert_eq!(fake.applied_fleets().len(), 2);
        port.take_calls();
        a.handle(comment(12, "alice", 40, "@balerix continue"))
            .await;
        assert_eq!(fake.applied_fleets().len(), 3);
        assert!(!a.sessions.get(AGENT).unwrap().closed);
        a.handle(Command::Events(vec![during(
            "SessionStart",
            json!({ "source": "startup" }),
        )]))
        .await;
        let text = actions_text(fake.actions_for(AGENT).last().unwrap());
        assert!(text.contains("Earlier work on this issue is on branch balerix/gh-acme-api/repo/issue-12; continue from it."), "{text}");
    }

    #[tokio::test]
    async fn a_ninety_minute_idle_timeout_says_90m() {
        let (_fake, _port, mut a) = started().await;
        a.config.as_mut().unwrap().idle_timeout = Duration::from_secs(90 * 60);
        a.now_secs = || 14 * 3600 + 2 * 60 + 90 * 60 + 1;
        a.handle(Command::Tick).await;
        assert!(a.sessions.get(AGENT).unwrap().closed);
        let r = a.statuses[AGENT].render();
        assert!(
            r.contains("stopped after 90m idle — mention @balerix to resume"),
            "{r}"
        );
    }

    #[tokio::test]
    async fn a_webhook_refreshes_a_rows_installation() {
        let (_fake, _port, mut a) = started().await;
        a.sessions.get_mut(AGENT).unwrap().installation = 0;
        a.handle(comment(12, "bob", 60, "carry on")).await;
        assert_eq!(a.sessions.get(AGENT).unwrap().installation, 7);
    }

    #[tokio::test]
    async fn phases_update_the_header_and_add_a_line_when_wanted() {
        let (_fake, _port, mut a) = started().await;
        a.handle(Command::Phases(vec![PhaseChange {
            agent: AGENT.into(),
            from: balerix_api::AgentPhase::Starting,
            to: balerix_api::AgentPhase::Ready,
            message: String::new(),
        }]))
        .await;
        let r = a.statuses[AGENT].render();
        assert!(
            r.contains("· **ready**") && r.contains("phase **Starting** to **Ready**"),
            "{r}"
        );
        a.handle(Command::Phases(vec![PhaseChange {
            agent: "other/x/y".into(),
            from: balerix_api::AgentPhase::Ready,
            to: balerix_api::AgentPhase::Dead,
            message: "gone".into(),
        }]))
        .await;
        assert!(!a.statuses.contains_key("other/x/y"));
    }

    #[tokio::test]
    async fn rows_survive_an_actor_restart_over_the_same_host() {
        let (fake, port, a) = started().await;
        drop(a);
        let host = Host::new(fake.env("github", std::path::Path::new("scratch"))).unwrap();
        let mut b = Actor::new(
            host,
            port.clone(),
            counters(),
            Health::new(),
            "balerix".into(),
        );
        b.handle(Command::Configure(daemon_config())).await;
        b.load().await;
        assert_eq!(b.sessions.by_number("acme/api", 12), Some(AGENT));
        b.handle(comment(12, "bob", 50, "still here?")).await;
        assert_eq!(
            fake.actions_for(AGENT).len(),
            2,
            "routed to the reloaded row"
        );
    }

    #[tokio::test]
    async fn a_skipped_prompt_gets_confused_and_names_its_reason() {
        let (_fake, port, mut a) = started().await;
        a.handle(comment(12, "bob", 9, "first thing")).await;
        a.handle(comment(12, "bob", 10, "second thing")).await;
        port.take_calls();
        a.handle(Command::Events(vec![during(
            "UserPromptSubmit",
            json!({ "prompt": "second thing" }),
        )]))
        .await;
        assert_eq!(
            reactions(&port.calls()),
            vec![
                (Target::Comment(9), CONFUSED.into()),
                (Target::Comment(10), PLUS_ONE.into())
            ]
        );
        let r = a.statuses[AGENT].render();
        assert!(
            r.contains("prompt from @bob skipped: Claude took a later one first"),
            "{r}"
        );
        assert!(!r.contains("not confirmed after"), "{r}");
    }

    #[tokio::test]
    async fn the_final_line_reaches_the_status_comment_before_the_deactivate() {
        let (_fake, port, mut a) = started().await;
        a.handle(Command::Webhook(WebhookEvent::Closed {
            repo: "acme/api".into(),
            installation: 7,
            number: 12,
            merged: false,
        }))
        .await;
        a.handle(Command::Deactivate {
            agent: AGENT.into(),
        })
        .await;
        let e = edits(&port.calls());
        assert!(e.last().is_some_and(|b| b.ends_with(" closed")), "{e:?}");
    }

    #[tokio::test]
    async fn a_failed_refile_on_close_is_reported_and_the_row_stays_open() {
        let (fake, port, mut a) = started().await;
        port.set_file("acme/api", "main", ".balerix.yaml", "");
        a.handle(Command::Webhook(WebhookEvent::Closed {
            repo: "acme/api".into(),
            installation: 7,
            number: 12,
            merged: false,
        }))
        .await;
        assert_eq!(
            comments(&port.calls()).len(),
            1,
            "one notice: {:?}",
            comments(&port.calls())
        );
        assert!(!a.sessions.get(AGENT).unwrap().closed);
        assert_eq!(a.counters.applies.with_label_values(&["config"]).get(), 1);
        assert_eq!(fake.applied_fleets().len(), 1, "no apply without the agent");
        // still open: a turn still posts
        port.take_calls();
        a.handle(Command::Events(vec![during(
            "Stop",
            json!({ "last_assistant_message": "still here" }),
        )]))
        .await;
        assert_eq!(comments(&port.calls()), vec![(12, "still here".into())]);
    }

    #[tokio::test]
    async fn a_closed_row_posts_no_turn() {
        let (_fake, port, mut a) = started().await;
        a.handle(Command::Webhook(WebhookEvent::Closed {
            repo: "acme/api".into(),
            installation: 7,
            number: 12,
            merged: true,
        }))
        .await;
        port.take_calls();
        a.handle(Command::Events(vec![during(
            "Stop",
            json!({ "last_assistant_message": "late" }),
        )]))
        .await;
        assert!(comments(&port.calls()).is_empty());
    }
}
