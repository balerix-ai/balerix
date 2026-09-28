//! The actor (Spec M §8): the one task that owns every piece of state
//! and executes the command loop. Generic over `GitHubPort` (M-11).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use balerix_api::HookEvent;
use balerix_plugin_common::delivery::Deliveries;
use balerix_plugin_common::metrics::Shared;
use balerix_plugin_common::pending::Questions;
use balerix_plugin_common::phases::PhaseChange;
use balerix_plugin_common::render;
use balerix_plugin_sdk::metrics::{IntCounter, IntCounterVec, IntGauge};
use balerix_plugin_sdk::{Host, Metrics, SdkError};
use tokio::time::Instant;

use crate::config::{AgentConfig, COMMENT_LIMIT, DaemonConfig, Kind};
use crate::github::{CONFUSED, EYES, GitHubError, GitHubPort, IssueInfo, Permission, Target};
use crate::mention;
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
}

/// A review held while a question is open (§9).
#[derive(Debug, Clone)]
#[allow(dead_code)] // Task 8
struct Held {
    reviewer: String,
    message: String,
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
    #[allow(dead_code)] // Task 8
    rows_dirty: HashMap<String, ()>,
    permissions: HashMap<(String, String), (Permission, Instant)>,
    held: HashMap<String, Vec<Held>>,
    ended_notice: HashMap<String, ()>,
    /// What the first prompt needs, kept from `start` until `SessionStart`.
    first_prompts: HashMap<String, FirstPrompt>,
    /// Markers evicted inside a sync path, reported on the next tick.
    #[allow(dead_code)] // Task 8
    unconfirmed_queue: Vec<Sent>,
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
            } // Task 8
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
                    &format!("fleet name {fleet} already stands for {other}"),
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
            self.refuse(installation, repo, number, target, &e.to_string(), "daemon")
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
            Some(id) => Some(id),
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

    fn github_failed(&self, kind: &str, e: &GitHubError) {
        self.counters.errors.with_label_values(&[kind]).inc();
        tracing::warn!("github: {kind}: {e}");
        if matches!(e, GitHubError::Auth(_)) {
            self.counters.errors.with_label_values(&["auth"]).inc();
            self.health.fail(format!("github: {e}"));
        }
    }

    // ---- Task 8: outbound, inbound, review and ending paths ----

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
        // Task 8
        let _ = (agent, installation, repo, number, asker, comment_id, body);
    }

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
        // Task 8
        let _ = (
            installation,
            repo,
            number,
            author,
            review_id,
            state,
            body,
            commit,
        );
    }

    async fn end(&mut self, agent: &str, installation: u64, reason: &str) {
        // Task 8
        let _ = (agent, installation, reason);
    }

    async fn on_event(&mut self, _event: HookEvent) {
        // Task 8
    }

    async fn on_phase(&mut self, _change: PhaseChange) {
        // Task 8
    }

    async fn on_tick(&mut self) {
        // Task 8
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
#[allow(dead_code)] // Task 8
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
    use balerix_plugin_sdk::testing::FakeHost;
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
            "fleet name gh-acme-api already stands for acme/api."
        );
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
}
