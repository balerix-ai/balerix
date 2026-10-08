//! The sidecar (Spec O §6.2, §6.3): a one-agent daemon. The same planner,
//! materializer and runner the daemon uses on one machine, over a pod
//! layout and a shared tmux socket; Claude's hooks forwarded; one outbound
//! link to the Daemon carrying `send_text`, `send_keys`, `stop`,
//! `restart`, `attach` and the workspace reads.

use std::collections::{BTreeMap, BTreeSet};
use std::future::IntoFuture;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use balerix_api::{
    AgentStatus, CredentialBundle, FleetStatus, GitSettings, Keep, LinkStatus, Timestamp,
};
use balerix_core::reconcile::{ReconcileContext, agent_ready, reconcile_pass, set_desired};
use balerix_core::{
    AgentId, AgentName, AgentRunner, Clock, CrewRef, CrewTools, HookTarget, LaunchPlan,
    MaterializeError, Materializer, ProcessState, ReconcilePolicy, RepoRef, ResolvedAgent,
    ResolvedPlugin, WorkspaceReader,
};
use balerix_runtime::layout::{PodLayout, PodMounts};
use balerix_runtime::sandbox::sandbox_self_test;
use balerix_runtime::{Runtime, SocketPolicy, StateLayout, TmuxRunner, ToolPaths, UnixSockets};
use std::path::Path;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::cli::SidecarArgs;
use crate::hooks::{self, FORWARD_BUDGET, Hooks};
use crate::link::{self, Control, LinkDeps};
use crate::{bundle, run, state, tls};

/// How long the `agent` container may take to bring the tmux server up
/// after the start marker (image pull is before this; it is only the
/// container start and `new-session`).
pub const SERVER_WAIT: Duration = Duration::from_secs(120);
/// The planner's cadence when nothing is due (the daemon's `RESYNC`).
pub const RESYNC: Duration = Duration::from_secs(30);
const MIN_TICK: Duration = Duration::from_secs(1);
const READY_EVENT: &str = "SessionStart";
/// After a hook event, when its forward has failed or succeeded for
/// certain: the hook route hands the event over before forwarding it, so
/// the failure count is final only once the budget is spent.
const RECOUNT_AFTER: Duration = FORWARD_BUDGET.saturating_add(Duration::from_millis(250));

struct WallClock;

impl Clock for WallClock {
    fn now(&self) -> Timestamp {
        Timestamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        )
    }
}

/// `Runtime` as the planner's materializer in a pod. Removal never runs
/// here: the planner removes only agents and crews the fleet does not
/// want, and the sidecar's fleet is its one agent, so a removal is a
/// stray window or session on the shared socket, with nothing of the
/// sidecar's on disk. `Runtime`'s removal would harvest into the crew
/// cache, which a pod mounts read-only (the harvest is a Job's, Spec O
/// §8.4). The planner's `Stop` before it has already ended the stray.
struct PodMaterializer(Arc<Runtime>);

impl Materializer for PodMaterializer {
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        repo: &RepoRef,
        git_ref: &str,
        git: &GitSettings,
        creds: &CredentialBundle,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        self.0.ensure_crew(crew, repo, git_ref, git, creds, tools)
    }
    fn materialize(
        &self,
        agent: &ResolvedAgent,
        creds: &CredentialBundle,
        hooks: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        self.0.materialize(agent, creds, hooks)
    }
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        tracing::warn!(%agent, "not this sidecar's agent; nothing of it to remove");
        Ok(())
    }
    fn remove_crew(&self, crew: &CrewRef, _keep: Keep) -> Result<(), MaterializeError> {
        tracing::warn!(%crew, "not this sidecar's crew; nothing of it to remove");
        Ok(())
    }
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        host: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        self.0.materialize_plugin(plugin, host)
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        self.0.purge_plugin(name)
    }
}

/// Spec O §10.3: `https://<authority>`, or `http://` under the hidden
/// `--allow-plain-http` (tests). Anything else is refused rather than
/// handed on: `link::ws_url` maps exactly these two schemes.
pub fn check_daemon_url(url: &str, allow_plain_http: bool) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://").filter(|_| allow_plain_http));
    match rest {
        Some(authority) if !authority.is_empty() => Ok(()),
        _ => bail!("daemon_url must be https:// (Spec O §10.3)"),
    }
}

/// The hook listener (§7.1): loopback only, Claude is in the same pod.
pub async fn bind_hooks(port: u16) -> Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("cannot bind 127.0.0.1:{port}"))
}

pub async fn main(args: SidecarArgs) -> Result<()> {
    // first: the sidecar is its container's pid 1, which has no default
    // action for SIGTERM, and materialising or waiting for the agent
    // container's server can take minutes
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = sidecar(args) => r,
        _ = term.recv() => {
            tracing::info!("SIGTERM; the agent container ends the tree");
            Ok(())
        }
    }
}

/// The pod's Unix socket policy, resolved once at start-up (`auto`: there
/// is no bundle field, see the spec's deviation note). Under `Deny` the
/// `balerix` binary must support `sandbox-exec`: the agent image pairs
/// this sidecar with the base image's `balerix`, which may be older.
async fn resolve_socket_policy(tools: &ToolPaths, run_dir: &Path) -> Result<SocketPolicy> {
    let (tools, scratch) = (tools.clone(), run_dir.join("socket-probe"));
    tokio::task::spawn_blocking(move || {
        let (policy, why) = balerix_runtime::resolve_socket_policy(UnixSockets::Auto, || {
            balerix_runtime::socket_policy::probe_mediation(&tools, &scratch)
        })
        .map_err(|e| anyhow!("{e}"))?;
        match policy {
            SocketPolicy::Open => tracing::warn!("{why}"),
            _ => tracing::info!("{why}"),
        }
        // fails closed: no agent runs without the policy it was given
        if policy == SocketPolicy::Deny {
            balerix_runtime::socket_policy::check_sandbox_exec(&tools.balerix)
                .map_err(|e| anyhow!("unix_sockets = \"deny\": {e}"))?;
        }
        Ok(policy)
    })
    .await?
}

async fn sidecar(args: SidecarArgs) -> Result<()> {
    let loaded = bundle::load(&args.bundle)?;
    let id = loaded.id.clone();
    let daemon_url = loaded.bundle.daemon_url.clone();
    check_daemon_url(&daemon_url, args.allow_plain_http)?;
    let tls = tls::client_config(&args.ca)?;
    let layout = StateLayout::pod(
        PodMounts {
            agent: args.agent_dir.clone(),
            shared: args.shared_dir.clone(),
            run: args.run_dir.clone(),
        },
        &id,
    );
    let pod: PodLayout = layout.pod_layout().cloned().context("pod layout")?;
    let balerix = match &args.balerix {
        Some(p) => p.clone(),
        None => run::on_path("balerix").context("balerix is not on PATH; pass --balerix")?,
    };
    let tools = ToolPaths::discover_in(&std::env::var_os("PATH").unwrap_or_default(), &balerix)
        .map_err(|e| anyhow!("{e} (the sidecar needs git, gh, mise, nono and tmux on PATH)"))?;
    let socket_policy = resolve_socket_policy(&tools, &args.run_dir).await?;
    let runtime =
        Arc::new(Runtime::new(layout.clone(), tools.clone()).with_socket_policy(socket_policy));
    let runner = Arc::new(TmuxRunner::at_socket(tools.tmux.clone(), pod.tmux_socket()));

    // §7.1: the hook listener first; its port goes into the profile's
    // `open_port` and its URL into `settings.json`, through `HookTarget`.
    // Claude's secret is the sidecar's own: the bundle token stays in this
    // container (§10.4)
    let state_dir = state::dir(&args.agent_dir);
    let listener = bind_hooks(args.hook_port).await?;
    let hook_target = HookTarget {
        url: format!("http://{}", listener.local_addr()?),
        secret: state::hook_secret(&state_dir)?,
    };

    // §6.2 step 1: the files, as the daemon makes them
    {
        let (rt, agent, creds, hooks) = (
            runtime.clone(),
            loaded.agent.clone(),
            loaded.bundle.credentials.clone(),
            hook_target.clone(),
        );
        tokio::task::spawn_blocking(move || -> Result<()> {
            let empty = BTreeMap::new();
            rt.ensure_crew(
                &agent.id.crew_ref(),
                &agent.repo,
                &agent.git_ref,
                &agent.git,
                &creds,
                CrewTools {
                    fleet: &empty,
                    crew: &empty,
                },
            )?;
            rt.materialize(&agent, &creds, &hooks)?;
            Ok(())
        })
        .await??;
    }
    // §6.2 step 2: the sandbox self-test; its failure is the termination
    // message (`SandboxUnavailable: …` where Landlock is denied)
    {
        let (tools, paths) = (tools.clone(), layout.agent(&id));
        tokio::task::spawn_blocking(move || sandbox_self_test(&tools, &paths))
            .await?
            .map_err(|e| anyhow!("{e}"))?;
    }
    // §6.2 step 3: the marker, then the server the agent container starts
    std::fs::create_dir_all(&args.run_dir)?;
    std::fs::write(pod.start_marker(), format!("{}\n", id.crew_ref()))?;
    wait_for_server(&runner, &id).await?;
    tracing::info!(agent = %id, "materialised; tmux server up");

    // the long-lived parts: hooks, the link, the planner
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<String>();
    let failures = Arc::new(AtomicU64::new(0));
    let hooks_state = Arc::new(Hooks {
        id: id.clone(),
        secret: hook_target.secret.clone(),
        token: loaded.bundle.token.clone(),
        daemon_url: daemon_url.clone(),
        http: tls::http_client(&tls, Duration::from_secs(10))?,
        failures: failures.clone(),
        events: events_tx,
    });
    tokio::spawn(axum::serve(listener, hooks::router(hooks_state)).into_future());
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Control>();
    let (status_tx, status_rx) = watch::channel(LinkStatus {
        status: AgentStatus::default(),
        pid: None,
        hook_failures: 0,
    });
    // started after the first pass: a link's first status frame is what
    // the Daemon reconciles its stopped set against (§7.4), so it must be
    // the pass's answer, not the placeholder above
    let mut link = Some(Arc::new(LinkDeps {
        id: id.clone(),
        token: loaded.bundle.token.clone(),
        daemon_url,
        tls,
        runner: runner.clone() as Arc<dyn AgentRunner>,
        workspace: runtime.clone() as Arc<dyn WorkspaceReader>,
        control: control_tx,
        status: status_rx,
    }));

    let fleet = Arc::new(loaded.fleet);
    let creds = Arc::new(loaded.bundle.credentials.clone());
    let policy = Arc::new(ReconcilePolicy::default());
    let clock = Arc::new(WallClock);
    let materializer = Arc::new(PodMaterializer(runtime.clone()));
    // §6.3: what the last sidecar left, so a stop holds across a restart
    // and a healthy Claude is not relaunched
    let saved = state::load(&state_dir)?;
    let mut status = saved.status;
    set_desired(&mut status, 1);
    let mut stopped: BTreeSet<AgentId> = BTreeSet::new();
    if saved.stopped {
        stopped.insert(id.clone());
    }
    let mut recount: Option<tokio::time::Instant> = None;
    loop {
        let (new_status, pid, clean) = {
            let (fleet, creds, policy, clock, materializer, runner, hooks, stopped, id) = (
                fleet.clone(),
                creds.clone(),
                policy.clone(),
                clock.clone(),
                materializer.clone(),
                runner.clone(),
                hook_target.clone(),
                stopped.clone(),
                id.clone(),
            );
            let mut status = std::mem::take(&mut status);
            tokio::task::spawn_blocking(move || {
                let hooks = |_: &AgentId| hooks.clone();
                let ctx = ReconcileContext {
                    fleet: &id.fleet,
                    desired: Some(&fleet),
                    keep: Keep::default(),
                    stopped: &stopped,
                    materializer: materializer.as_ref(),
                    runner: runner.as_ref(),
                    creds: &creds,
                    hooks: &hooks,
                    policy: &policy,
                    clock: clock.as_ref(),
                };
                let clean = match reconcile_pass(&mut status, &ctx) {
                    Ok((plan, report)) => {
                        for (step, err) in &report.failures {
                            tracing::warn!(step = %step, "step failed: {err}");
                        }
                        tracing::debug!(steps = plan.len(), failed = report.failures.len(), "pass");
                        report.all_ok()
                    }
                    Err(e) => {
                        tracing::error!("pass failed: {e}");
                        false
                    }
                };
                let pid = match runner.observe(&id.fleet) {
                    Ok(observed) => match observed.get(&id) {
                        Some(ProcessState::Running { pid }) => Some(*pid),
                        _ => None,
                    },
                    Err(_) => None,
                };
                (status, pid, clean)
            })
            .await?
        };
        status = new_status;
        save(&state_dir, &status, &stopped)?;
        ready_marker(&pod, &status, &id)?;
        publish(&status_tx, &status, &id, pid, &failures);
        if let Some(deps) = link.take() {
            tokio::spawn(link::run(deps));
        }

        let wake = tokio::time::sleep(next_deadline(&status, clean, clock.now()));
        tokio::pin!(wake);
        loop {
            tokio::select! {
                () = &mut wake => break,
                Some(c) = control_rx.recv() => {
                    match c {
                        Control::Stop => {
                            stopped.insert(id.clone());
                        }
                        Control::Restart => {
                            stopped.remove(&id);
                        }
                    }
                    save(&state_dir, &status, &stopped)?;
                    break;
                }
                Some(name) = events_rx.recv() => {
                    if name == READY_EVENT {
                        tracing::info!(agent = %id, "agent ready ({READY_EVENT} received)");
                        agent_ready(&mut status, &id, clock.now());
                        save(&state_dir, &status, &stopped)?;
                        ready_marker(&pod, &status, &id)?;
                    }
                    publish(&status_tx, &status, &id, pid, &failures);
                    recount = Some(tokio::time::Instant::now() + RECOUNT_AFTER);
                }
                () = sleep_until(recount), if recount.is_some() => {
                    recount = None;
                    publish(&status_tx, &status, &id, pid, &failures);
                }
            }
        }
    }
}

fn save(dir: &std::path::Path, status: &FleetStatus, stopped: &BTreeSet<AgentId>) -> Result<()> {
    state::save(
        dir,
        &state::Saved {
            stopped: !stopped.is_empty(),
            status: status.clone(),
        },
    )
}

async fn sleep_until(at: Option<tokio::time::Instant>) {
    if let Some(at) = at {
        tokio::time::sleep_until(at).await;
    }
}

/// `observe` lists the crew's session once the agent container's server
/// is up with the anchor window.
async fn wait_for_server(runner: &Arc<TmuxRunner>, id: &AgentId) -> Result<()> {
    let deadline = tokio::time::Instant::now() + SERVER_WAIT;
    loop {
        let (r, fleet, crew) = (runner.clone(), id.fleet.clone(), id.crew.clone());
        let up = tokio::task::spawn_blocking(move || r.observe(&fleet))
            .await?
            .map(|o| o.crews.contains_key(&crew))
            .unwrap_or(false);
        if up {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the agent container did not start the tmux server within {} s",
                SERVER_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The agent's status as a frame, sent only when it changed.
fn publish(
    tx: &watch::Sender<LinkStatus>,
    status: &FleetStatus,
    id: &AgentId,
    pid: Option<u32>,
    failures: &AtomicU64,
) {
    let frame = LinkStatus {
        status: status
            .agents
            .get(&id.to_string())
            .cloned()
            .unwrap_or_default(),
        pid,
        hook_failures: failures.load(Ordering::Relaxed),
    };
    tx.send_if_modified(|current| {
        if *current == frame {
            false
        } else {
            *current = frame;
            true
        }
    });
}

/// `<run>/ready` exists exactly while the agent is `Ready` (the pod's
/// exec readiness probe).
fn ready_marker(pod: &PodLayout, status: &FleetStatus, id: &AgentId) -> Result<()> {
    let ready = status
        .agents
        .get(&id.to_string())
        .is_some_and(|a| a.phase == balerix_api::AgentPhase::Ready);
    let marker = pod.ready_marker();
    if ready {
        if !marker.exists() {
            std::fs::write(&marker, "ready\n")?;
        }
    } else if let Err(e) = std::fs::remove_file(&marker)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e.into());
    }
    Ok(())
}

/// The daemon actor's `deadline`: the next restart if one is due, the
/// resync otherwise; a dirty pass waits the resync out.
fn next_deadline(status: &FleetStatus, clean: bool, now: Timestamp) -> Duration {
    if !clean {
        return RESYNC;
    }
    match status
        .agents
        .values()
        .filter_map(|a| a.next_restart_at)
        .min()
    {
        Some(due) => Duration::from_secs(due.0.saturating_sub(now.0))
            .max(MIN_TICK)
            .min(RESYNC),
        None => RESYNC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::AgentPhase;

    #[test]
    fn the_deadline_follows_the_next_restart_within_the_floor_and_the_cap() {
        let mut s = FleetStatus::default();
        assert_eq!(next_deadline(&s, true, Timestamp(100)), RESYNC);
        assert_eq!(next_deadline(&s, false, Timestamp(100)), RESYNC);
        s.entry("f/c/a").next_restart_at = Some(Timestamp(105));
        assert_eq!(
            next_deadline(&s, true, Timestamp(100)),
            Duration::from_secs(5)
        );
        s.entry("f/c/a").next_restart_at = Some(Timestamp(100));
        assert_eq!(next_deadline(&s, true, Timestamp(100)), MIN_TICK);
        s.entry("f/c/a").next_restart_at = Some(Timestamp(10_000));
        assert_eq!(next_deadline(&s, true, Timestamp(100)), RESYNC);
        // `SessionStart` before a due restart: nothing is due any more
        s.entry("f/c/a").next_restart_at = Some(Timestamp(105));
        agent_ready(&mut s, &"f/c/a".parse().unwrap(), Timestamp(101));
        assert_eq!(s.agents["f/c/a"].phase, AgentPhase::Ready);
        assert_eq!(next_deadline(&s, true, Timestamp(101)), RESYNC);
    }

    /// §10.3: the scheme is required, not merely `http://` refused.
    #[test]
    fn the_daemon_url_must_be_https() {
        assert!(check_daemon_url("https://d:7643", false).is_ok());
        for bad in [
            "http://d:7643",
            "HTTP://d:7643",
            "ws://d:7643",
            "wss://d:7643",
            "d:7643",
            "https://",
        ] {
            let e = check_daemon_url(bad, false).unwrap_err();
            assert_eq!(e.to_string(), "daemon_url must be https:// (Spec O §10.3)");
        }
        assert!(check_daemon_url("http://127.0.0.1:1", true).is_ok());
        assert!(check_daemon_url("ws://127.0.0.1:1", true).is_err());
    }

    #[test]
    fn the_ready_marker_follows_the_phase() {
        let dir = crate::test_dir();
        let id: AgentId = "f/c/a".parse().unwrap();
        let pod = PodLayout {
            mounts: PodMounts {
                agent: dir.path().join("agent"),
                shared: dir.path().join("shared"),
                run: dir.path().to_path_buf(),
            },
            id: id.clone(),
        };
        let mut s = FleetStatus::default();
        ready_marker(&pod, &s, &id).unwrap();
        assert!(!pod.ready_marker().exists());
        s.entry("f/c/a").phase = AgentPhase::Ready;
        ready_marker(&pod, &s, &id).unwrap();
        assert!(pod.ready_marker().is_file());
        s.entry("f/c/a").phase = AgentPhase::Stopped;
        ready_marker(&pod, &s, &id).unwrap();
        assert!(!pod.ready_marker().exists());
    }

    /// Spec O §7.1: Claude is in the pod; nothing outside it reaches the
    /// hook route.
    #[tokio::test]
    async fn the_hook_listener_is_on_loopback_only() {
        let listener = bind_hooks(0).await.unwrap();
        assert_eq!(
            listener.local_addr().unwrap().ip(),
            std::net::IpAddr::from([127, 0, 0, 1])
        );
    }

    /// A stray window's removal neither fails the pass nor touches a file:
    /// `Runtime`'s would run git to harvest into the read-only cache.
    #[test]
    fn removal_in_a_pod_touches_nothing() {
        let dir = crate::test_dir();
        let id: AgentId = "f/c/a".parse().unwrap();
        let layout = StateLayout::pod(
            PodMounts {
                agent: dir.path().join("agent"),
                shared: dir.path().join("shared"),
                run: dir.path().join("run"),
            },
            &id,
        );
        let stray: AgentId = "f/c/b".parse().unwrap();
        let objects = layout.crew(&id.crew_ref()).cache_objects();
        let stray_root = layout.agent(&stray).root;
        for d in [&objects, &stray_root] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("kept"), "").unwrap();
        }
        let none = std::path::PathBuf::from("/nonexistent");
        let tools = ToolPaths {
            git: none.clone(),
            gh: none.clone(),
            mise: none.clone(),
            nono: none.clone(),
            tmux: none.clone(),
            balerix: none,
        };
        let m = PodMaterializer(Arc::new(Runtime::new(layout, tools)));
        m.remove_agent(&stray).unwrap();
        let stray_crew = CrewRef {
            fleet: "f".parse().unwrap(),
            crew: "x".parse().unwrap(),
        };
        m.remove_crew(&stray_crew, Keep::default()).unwrap();
        assert!(objects.join("kept").is_file());
        assert!(stray_root.join("kept").is_file());
    }
}
