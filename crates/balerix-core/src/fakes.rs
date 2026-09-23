//! In-memory ports for tests (Phase 2 spec §3.5). Always compiled: they are
//! small, dependency-free, and Phase 3's tests need them too.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use balerix_api::{
    CredentialBundle, EntryKind, FleetSpec, GitSettings, Timestamp, TreeEntry,
    WORKSPACE_FILE_LIMIT, WorkspaceDiff, WorkspaceTree, WorkspaceVersion, check_path,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::agent::{CrewRef, ResolvedAgent};
use crate::name::{AgentId, AgentName, FleetName};
use crate::plugin::ResolvedPlugin;
use crate::ports::{
    AgentRunner, Clock, CredentialSource, CrewTools, FleetResolver, HookTarget, Keep, LaunchPlan,
    MaterializeError, Materializer, ObservedState, ProcessState, PtyStream, RunnerError,
    SystemToolchain, WorkspaceError, WorkspaceReader,
};
use crate::repo::RepoRef;

#[derive(Default)]
struct Recorder {
    calls: Vec<String>,
    fail_next: Vec<(String, String, String)>, // (method, id, stderr)
}

impl Recorder {
    /// Records the call; returns the stderr to fail with, if one was armed.
    fn record(&mut self, method: &str, id: &str) -> Option<String> {
        self.calls.push(format!("{method} {id}"));
        let pos = self
            .fail_next
            .iter()
            .position(|(m, i, _)| m == method && i == id)?;
        Some(self.fail_next.remove(pos).2)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
pub struct FakeMaterializer {
    rec: Mutex<Recorder>,
    #[allow(clippy::type_complexity)]
    crew_tools: Mutex<Vec<(String, Vec<String>, Vec<String>)>>,
}

impl FakeMaterializer {
    pub fn calls(&self) -> Vec<String> {
        lock(&self.rec).calls.clone()
    }
    pub fn fail_next(&self, method: &str, id: &str, stderr: &str) {
        lock(&self.rec)
            .fail_next
            .push((method.into(), id.into(), stderr.into()));
    }
    /// Each `ensure_crew` call as (crew, fleet tools, crew tools), with
    /// each table flattened to sorted `name=version` strings.
    pub fn crew_tools(&self) -> Vec<(String, Vec<String>, Vec<String>)> {
        lock(&self.crew_tools).clone()
    }
    fn check(&self, method: &str, id: &str, tool: &str) -> Result<(), MaterializeError> {
        match lock(&self.rec).record(method, id) {
            Some(stderr) => Err(MaterializeError::Tool {
                id: id.into(),
                tool: tool.into(),
                subcommand: method.into(),
                args: vec![],
                stderr,
            }),
            None => Ok(()),
        }
    }
}

impl Materializer for FakeMaterializer {
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        _: &RepoRef,
        _: &str,
        _: &GitSettings,
        _: &CredentialBundle,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        let flat = |t: &BTreeMap<String, String>| {
            t.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
        };
        lock(&self.crew_tools).push((crew.to_string(), flat(tools.fleet), flat(tools.crew)));
        self.check("ensure_crew", &crew.to_string(), "git")
    }
    fn materialize(
        &self,
        agent: &ResolvedAgent,
        _: &CredentialBundle,
        _: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let id = agent.id.to_string();
        self.check("materialize", &id, "mise")?;
        let root = PathBuf::from("/fake").join(&id);
        Ok(LaunchPlan {
            cwd: root.join("workspace"),
            env: BTreeMap::new(),
            argv: vec!["fake".into()],
            script: root.join("launch.sh"),
        })
    }
    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        self.check("remove_agent", &agent.to_string(), "rm")
    }
    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError> {
        let tag = format!("{crew} repos={} sessions={}", keep.repos, keep.sessions);
        self.check("remove_crew", &tag, "rm").map_err(|e| match e {
            MaterializeError::Tool {
                tool,
                subcommand,
                args,
                stderr,
                ..
            } => MaterializeError::Tool {
                id: crew.to_string(),
                tool,
                subcommand,
                args,
                stderr,
            },
            other => other,
        })
    }
    fn materialize_plugin(
        &self,
        plugin: &ResolvedPlugin,
        _: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let id = plugin.id().to_string();
        self.check("materialize_plugin", &id, "mise")?;
        Ok(LaunchPlan {
            cwd: plugin.package.clone(),
            env: BTreeMap::new(),
            argv: vec!["fake-plugin".into()],
            script: PathBuf::from("/fake").join(&id).join("launch.sh"),
        })
    }
    fn purge_plugin(&self, name: &AgentName) -> Result<(), MaterializeError> {
        self.check("purge_plugin", name.as_str(), "rm")
    }
}

#[derive(Default)]
struct PtyBuf {
    data: VecDeque<u8>,
    closed: bool,
}

/// One fake terminal's shared end: what the writer wrote, until the
/// reader takes it; `closed` ends the reader with EOF and the writer with
/// `BrokenPipe`.
#[derive(Default)]
struct PtyShared {
    buf: Mutex<PtyBuf>,
    cv: Condvar,
}

impl PtyShared {
    fn close(&self) {
        lock(&self.buf).closed = true;
        self.cv.notify_all();
    }
}

struct FakePtyReader(Arc<PtyShared>);

impl Read for FakePtyReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut b = lock(&self.0.buf);
        while b.data.is_empty() && !b.closed {
            b = self.0.cv.wait(b).unwrap_or_else(|e| e.into_inner());
        }
        if b.data.is_empty() {
            return Ok(0);
        }
        let n = out.len().min(b.data.len());
        for (slot, byte) in out.iter_mut().zip(b.data.drain(..n)) {
            *slot = byte;
        }
        Ok(n)
    }
}

struct FakePtyWriter {
    shared: Arc<PtyShared>,
    /// Every write fails: the runner's side broke under the session.
    broken: bool,
}

impl Write for FakePtyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.broken {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty write failed",
            ));
        }
        let mut b = lock(&self.shared.buf);
        if b.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pty closed"));
        }
        b.data.extend(bytes);
        self.shared.cv.notify_all();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Where the runner's side of an attached stream fails, for the 1011
/// paths (plugins spec §18.4): `FakeRunner::fault_next_attach`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyFault {
    /// `reader()` fails.
    Reader,
    /// `writer()` fails.
    Writer,
    /// The writer is handed out but every write fails.
    Write,
}

/// The fake's terminal: an echo. Bytes written come back on the reader,
/// resizes are recorded on the runner, `FakeRunner::close_attach` (or a
/// drop) ends it.
pub struct FakePty {
    shared: Arc<PtyShared>,
    agent: String,
    resizes: Arc<Mutex<Vec<(String, u16, u16)>>>,
    /// The runner's open attaches, so a drop takes its own entry out.
    attached: Arc<Mutex<BTreeMap<String, Arc<PtyShared>>>>,
    /// The port says the writer is taken once, as `TmuxAttach` enforces.
    writer_taken: AtomicBool,
    fault: Option<PtyFault>,
}

impl PtyStream for FakePty {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        if self.fault == Some(PtyFault::Reader) {
            return Err(io::Error::other("no reader: the pty is gone"));
        }
        Ok(Box::new(FakePtyReader(self.shared.clone())))
    }
    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        if self.writer_taken.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other("the writer was already taken"));
        }
        if self.fault == Some(PtyFault::Writer) {
            return Err(io::Error::other("no writer: the pty is gone"));
        }
        Ok(Box::new(FakePtyWriter {
            shared: self.shared.clone(),
            broken: self.fault == Some(PtyFault::Write),
        }))
    }
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        lock(&self.resizes).push((self.agent.clone(), cols, rows));
        Ok(())
    }
}

impl Drop for FakePty {
    fn drop(&mut self) {
        self.shared.close();
        // Only this stream's own entry: a newer attach for the same agent
        // has replaced it and stays open.
        let mut attached = lock(&self.attached);
        if attached
            .get(&self.agent)
            .is_some_and(|open| Arc::ptr_eq(open, &self.shared))
        {
            attached.remove(&self.agent);
        }
    }
}

#[derive(Default)]
pub struct FakeRunner {
    rec: Mutex<Recorder>,
    state: Mutex<ObservedState>,
    next_pid: Mutex<u32>,
    /// While set, every `observe()` call fails (spec §3.2: a failed
    /// `observe()` is logged, counted, leaves status unchanged, and the
    /// next tick retries at the resync cadence).
    fail_observe: AtomicBool,
    attached: Arc<Mutex<BTreeMap<String, Arc<PtyShared>>>>,
    resizes: Arc<Mutex<Vec<(String, u16, u16)>>>,
    /// Consumed, oldest first, by the next `attach` calls.
    faults: Mutex<VecDeque<PtyFault>>,
}

impl FakeRunner {
    pub fn calls(&self) -> Vec<String> {
        lock(&self.rec).calls.clone()
    }
    pub fn fail_next(&self, method: &str, id: &str, stderr: &str) {
        lock(&self.rec)
            .fail_next
            .push((method.into(), id.into(), stderr.into()));
    }
    pub fn set_state(&self, id: &AgentId, s: ProcessState) {
        lock(&self.state).set(id, s);
    }
    pub fn observed(&self) -> ObservedState {
        lock(&self.state).clone()
    }
    /// Makes every subsequent `observe()` fail (or, passed `false`, stops
    /// failing) until changed again.
    pub fn set_fail_observe(&self, fail: bool) {
        self.fail_observe.store(fail, Ordering::SeqCst);
    }
    /// The next `attach` succeeds but its stream fails as `fault` says.
    pub fn fault_next_attach(&self, fault: PtyFault) {
        lock(&self.faults).push_back(fault);
    }
    /// Ends the reader of the latest attach for `agent`; `false` if none
    /// is open (a dropped stream took its own entry out).
    pub fn close_attach(&self, agent: &AgentId) -> bool {
        match lock(&self.attached).remove(&agent.to_string()) {
            Some(shared) => {
                shared.close();
                true
            }
            None => false,
        }
    }
    pub fn resizes(&self) -> Vec<(String, u16, u16)> {
        lock(&self.resizes).clone()
    }
    fn check(&self, method: &str, id: &str) -> Result<(), RunnerError> {
        match lock(&self.rec).record(method, id) {
            Some(stderr) => Err(RunnerError::Tool {
                id: id.into(),
                subcommand: method.into(),
                args: vec![],
                stderr,
            }),
            None => Ok(()),
        }
    }
}

impl AgentRunner for FakeRunner {
    fn ensure_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.check("ensure_crew", &crew.to_string())?;
        lock(&self.state)
            .crews
            .entry(crew.crew.clone())
            .or_default();
        Ok(())
    }
    fn ensure_agent(&self, agent: &AgentId, _: &LaunchPlan) -> Result<(), RunnerError> {
        self.check("ensure_agent", &agent.to_string())?;
        let pid = {
            let mut p = lock(&self.next_pid);
            *p += 1;
            *p
        };
        lock(&self.state).set(agent, ProcessState::Running { pid });
        Ok(())
    }
    fn stop_agent(&self, agent: &AgentId) -> Result<(), RunnerError> {
        self.check("stop_agent", &agent.to_string())?;
        lock(&self.state).remove(agent);
        Ok(())
    }
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        self.check("stop_crew", &crew.to_string())?;
        lock(&self.state).crews.remove(&crew.crew);
        Ok(())
    }
    fn observe(&self, fleet: &FleetName) -> Result<ObservedState, RunnerError> {
        self.check("observe", fleet.as_str())?;
        if self.fail_observe.load(Ordering::SeqCst) {
            return Err(RunnerError::Tool {
                id: fleet.as_str().into(),
                subcommand: "observe".into(),
                args: vec![],
                stderr: "fake observe failure".into(),
            });
        }
        Ok(self.observed())
    }
    fn send_text(&self, agent: &AgentId, text: &str, submit: bool) -> Result<(), RunnerError> {
        self.check("send_text", &format!("{agent} {text:?} submit={submit}"))
    }
    fn send_keys(
        &self,
        agent: &AgentId,
        steps: &[balerix_api::KeyStep],
        delay: std::time::Duration,
    ) -> Result<(), RunnerError> {
        let shown: Vec<String> = steps
            .iter()
            .map(|s| match s {
                balerix_api::KeyStep::Key(k) => format!("{k:?}").to_lowercase(),
                balerix_api::KeyStep::Text(t) => format!("{t:?}"),
            })
            .collect();
        self.check(
            "send_keys",
            &format!(
                "{agent} [{}] delay={}ms",
                shown.join(","),
                delay.as_millis()
            ),
        )
    }
    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError> {
        let id = agent.to_string();
        self.check("attach", &id)?;
        let shared = Arc::new(PtyShared::default());
        lock(&self.attached).insert(id.clone(), shared.clone());
        let fault = lock(&self.faults).pop_front();
        Ok(Box::new(FakePty {
            shared,
            agent: id,
            resizes: self.resizes.clone(),
            attached: self.attached.clone(),
            writer_taken: AtomicBool::new(false),
            fault,
        }))
    }
}

struct FakeTree {
    diff: WorkspaceDiff,
    files: BTreeMap<String, Vec<u8>>,
}

/// An in-memory `WorkspaceReader`: per agent, the diff to answer and a
/// flat map of relative path → bytes from which `read_file` and
/// `list_dir` answer. An agent never `set` has no workspace.
#[derive(Default)]
pub struct FakeWorkspace {
    rec: Mutex<Recorder>,
    agents: Mutex<BTreeMap<String, FakeTree>>,
}

impl FakeWorkspace {
    pub fn set(&self, agent: &AgentId, diff: WorkspaceDiff, files: BTreeMap<String, Vec<u8>>) {
        lock(&self.agents).insert(agent.to_string(), FakeTree { diff, files });
    }
    pub fn calls(&self) -> Vec<String> {
        lock(&self.rec).calls.clone()
    }
    fn with<T>(
        &self,
        agent: &AgentId,
        f: impl FnOnce(&FakeTree) -> Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        let agents = lock(&self.agents);
        match agents.get(&agent.to_string()) {
            Some(tree) => f(tree),
            None => Err(WorkspaceError::Missing(agent.to_string())),
        }
    }
}

/// The entries directly under `path` in a flat path map; `None` when
/// nothing lives there.
fn tree_of(files: &BTreeMap<String, Vec<u8>>, path: &str) -> Option<WorkspaceTree> {
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    let mut entries: BTreeMap<String, TreeEntry> = BTreeMap::new();
    for (key, bytes) in files {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        match rest.split_once('/') {
            Some((dir, _)) => {
                entries.entry(dir.to_string()).or_insert(TreeEntry {
                    name: dir.to_string(),
                    kind: EntryKind::Dir,
                    size: None,
                });
            }
            None => {
                entries.insert(
                    rest.to_string(),
                    TreeEntry {
                        name: rest.to_string(),
                        kind: EntryKind::File,
                        size: Some(bytes.len() as u64),
                    },
                );
            }
        }
    }
    if entries.is_empty() && !path.is_empty() {
        return None;
    }
    Some(WorkspaceTree {
        path: path.to_string(),
        entries: entries.into_values().collect(),
    })
}

/// The fakes' fingerprint: `sha256(head \0 (path \0 bytes \0)*)` over the
/// file map, so a `set` with different bytes changes it. The SDK's
/// `FakeHost` derives the same value (it may not depend on this crate),
/// and the `workspace-version.json` fixture holds it for its files.
pub fn fake_fingerprint(head: &str, files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut h = Sha256::new();
    h.update(head.as_bytes());
    h.update([0]);
    for (path, bytes) in files {
        h.update(path.as_bytes());
        h.update([0]);
        h.update(bytes);
        h.update([0]);
    }
    hex::encode(h.finalize())
}

impl WorkspaceReader for FakeWorkspace {
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        let _ = lock(&self.rec).record("diff", &format!("{agent} {base_ref}"));
        self.with(agent, |t| {
            Ok(WorkspaceDiff {
                base_ref: base_ref.to_string(),
                ..t.diff.clone()
            })
        })
    }
    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError> {
        let _ = lock(&self.rec).record("read_file", &format!("{agent} {path}"));
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        self.with(agent, |t| match t.files.get(path) {
            Some(bytes) if bytes.len() as u64 > WORKSPACE_FILE_LIMIT => {
                Err(WorkspaceError::TooLarge {
                    limit: WORKSPACE_FILE_LIMIT,
                })
            }
            Some(bytes) => Ok(bytes.clone()),
            None if tree_of(&t.files, path).is_some() => Err(WorkspaceError::NotAFile),
            None => Err(WorkspaceError::NoSuchPath),
        })
    }
    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError> {
        let _ = lock(&self.rec).record("list_dir", &format!("{agent} {path}"));
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        self.with(agent, |t| {
            if t.files.contains_key(path) {
                return Err(WorkspaceError::NotADirectory);
            }
            tree_of(&t.files, path).ok_or(WorkspaceError::NoSuchPath)
        })
    }
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        let _ = lock(&self.rec).record("version", &format!("{agent} {base_ref}"));
        self.with(agent, |t| {
            Ok(WorkspaceVersion {
                head: t.diff.head.clone(),
                fingerprint: fake_fingerprint(&t.diff.head, &t.files),
            })
        })
    }
}

pub struct FakeClock(Mutex<Timestamp>);

impl FakeClock {
    pub fn new(t: Timestamp) -> Self {
        Self(Mutex::new(t))
    }
    pub fn set(&self, t: Timestamp) {
        *lock(&self.0) = t;
    }
    pub fn advance(&self, secs: u64) {
        let mut t = lock(&self.0);
        *t = t.plus_secs(secs);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        *lock(&self.0)
    }
}

/// A `SystemToolchain` that fails a fixed number of times before succeeding.
/// `calls()` is how the actor's retry behaviour is asserted.
pub struct FakeSystemToolchain {
    remaining_failures: Mutex<usize>,
    forever: bool,
    calls: Mutex<usize>,
    succeed_first: Mutex<usize>,
}

impl FakeSystemToolchain {
    pub fn ready() -> Self {
        Self {
            remaining_failures: Mutex::new(0),
            forever: false,
            calls: Mutex::new(0),
            succeed_first: Mutex::new(usize::MAX),
        }
    }
    pub fn failing(times: usize) -> Self {
        Self {
            remaining_failures: Mutex::new(times),
            forever: false,
            calls: Mutex::new(0),
            succeed_first: Mutex::new(usize::MAX),
        }
    }
    pub fn always_failing() -> Self {
        Self {
            remaining_failures: Mutex::new(0),
            forever: true,
            calls: Mutex::new(0),
            succeed_first: Mutex::new(usize::MAX),
        }
    }
    /// Succeeds `ok` times, then fails forever. Drives the "readiness is not
    /// a latch" test (Spec F, F-4).
    pub fn ready_then_failing(ok: usize) -> Self {
        Self {
            remaining_failures: Mutex::new(0),
            forever: false,
            calls: Mutex::new(0),
            succeed_first: Mutex::new(ok),
        }
    }
    pub fn calls(&self) -> usize {
        *lock(&self.calls)
    }
}

impl SystemToolchain for FakeSystemToolchain {
    fn ensure_system_pool(&self) -> Result<(), MaterializeError> {
        *lock(&self.calls) += 1;

        let mut first = lock(&self.succeed_first);
        if *first > 0 && *first != usize::MAX {
            *first -= 1;
            return Ok(());
        }
        if *first == 0 {
            return Err(MaterializeError::Invalid {
                id: "system".to_string(),
                message: "fake system pool failure".to_string(),
            });
        }
        drop(first);

        let mut left = lock(&self.remaining_failures);
        if self.forever || *left > 0 {
            *left = left.saturating_sub(1);
            return Err(MaterializeError::Invalid {
                id: "system".to_string(),
                message: "fake system pool failure".to_string(),
            });
        }
        Ok(())
    }
}

/// `FleetResolver` for daemon tests: answers a fixed spec, renamed to the
/// requested fleet so one fake serves several names, or a fixed error;
/// records every `(fleet, file)` it was asked to resolve.
#[derive(Default)]
pub struct FakeResolver {
    answer: Mutex<Option<Result<FleetSpec, String>>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl FakeResolver {
    pub fn answering(spec: FleetSpec) -> Self {
        let r = Self::default();
        r.set(Ok(spec));
        r
    }
    pub fn failing(message: &str) -> Self {
        let r = Self::default();
        r.set(Err(message.to_string()));
        r
    }
    pub fn set(&self, answer: Result<FleetSpec, String>) {
        *lock(&self.answer) = Some(answer);
    }
    /// `(fleet name, file)` per call, in order.
    pub fn calls(&self) -> Vec<(String, Value)> {
        lock(&self.calls).clone()
    }
}

impl FleetResolver for FakeResolver {
    fn resolve(&self, file: &Value, name: &FleetName) -> Result<FleetSpec, String> {
        lock(&self.calls).push((name.to_string(), file.clone()));
        match lock(&self.answer).clone() {
            Some(Ok(mut spec)) => {
                spec.name = name.to_string();
                Ok(spec)
            }
            Some(Err(e)) => Err(e),
            None => Err("name: no resolver answer configured".to_string()),
        }
    }
}

/// `CredentialSource` for daemon tests: a fixed bundle (empty by default)
/// or a fixed error, and a count of loads.
pub struct FakeCredentials {
    answer: Mutex<Result<CredentialBundle, String>>,
    calls: AtomicUsize,
}

impl Default for FakeCredentials {
    fn default() -> Self {
        Self::new(CredentialBundle::default())
    }
}

impl FakeCredentials {
    pub fn new(bundle: CredentialBundle) -> Self {
        Self {
            answer: Mutex::new(Ok(bundle)),
            calls: AtomicUsize::new(0),
        }
    }
    pub fn failing(message: &str) -> Self {
        let c = Self::default();
        c.set(Err(message.to_string()));
        c
    }
    pub fn set(&self, answer: Result<CredentialBundle, String>) {
        *lock(&self.answer) = answer;
    }
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CredentialSource for FakeCredentials {
    fn load(&self) -> Result<CredentialBundle, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.answer).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc;
    use std::time::Duration;

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    /// The fake is the contract twin the server tests run against: a
    /// double take passes against it only if it would pass on tmux.
    #[test]
    fn the_fake_pty_hands_out_its_writer_once() {
        let r = FakeRunner::default();
        let pty = r.attach(&id("f/c/a")).unwrap();
        assert!(pty.writer().is_ok());
        assert!(pty.writer().is_err(), "PtyStream::writer: taken once");
        assert!(
            pty.reader().is_ok() && pty.reader().is_ok(),
            "readers clone"
        );
    }

    #[test]
    fn a_dropped_attach_is_no_longer_there_to_close() {
        let r = FakeRunner::default();
        let a = id("f/c/a");
        let pty = r.attach(&a).unwrap();
        drop(pty);
        assert!(!r.close_attach(&a), "the drop closed it; nothing left");
        // a newer attach for the same agent is untouched by an older drop
        let first = r.attach(&a).unwrap();
        let second = r.attach(&a).unwrap();
        drop(first);
        assert!(r.close_attach(&a), "the second attach was still open");
        drop(second);
        assert!(!r.close_attach(&a));
    }

    #[test]
    fn runner_tracks_windows_and_records_calls() {
        let r = FakeRunner::default();
        let crew: CrewRef = "f/c".parse().unwrap();
        r.ensure_crew(&crew).unwrap();
        r.ensure_agent(
            &id("f/c/a"),
            &LaunchPlan {
                cwd: "/x".into(),
                env: BTreeMap::new(),
                argv: vec![],
                script: "/x/l".into(),
            },
        )
        .unwrap();
        assert_eq!(
            r.observe(&"f".parse().unwrap()).unwrap().get(&id("f/c/a")),
            Some(&ProcessState::Running { pid: 1 })
        );
        r.stop_agent(&id("f/c/a")).unwrap();
        assert_eq!(r.observed().get(&id("f/c/a")), None);
        assert_eq!(
            r.calls(),
            vec![
                "ensure_crew f/c",
                "ensure_agent f/c/a",
                "observe f",
                "stop_agent f/c/a"
            ]
        );
    }

    #[test]
    fn fail_next_fails_exactly_once_for_the_named_call() {
        let m = FakeMaterializer::default();
        m.fail_next("remove_agent", "f/c/a", "boom");
        let e = m.remove_agent(&id("f/c/a")).unwrap_err();
        assert_eq!(e.to_string(), "f/c/a: rm remove_agent: boom");
        assert!(m.remove_agent(&id("f/c/a")).is_ok());
        assert!(m.remove_agent(&id("f/c/b")).is_ok());
    }

    #[test]
    fn plugin_calls_are_recorded_and_failable() {
        let m = FakeMaterializer::default();
        let p = ResolvedPlugin {
            name: "web".parse().unwrap(),
            package: "/pkg/web".into(),
            manifest: serde_json::from_value(serde_json::json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": "web",
                "version": "0.1.0", "protocol": 1, "start": "serve"
            }))
            .unwrap(),
            config: serde_json::json!({}),
            digest: None,
        };
        let host = HookTarget {
            url: "http://127.0.0.1:1".into(),
            secret: "tok".into(),
        };
        let plan = m.materialize_plugin(&p, &host).unwrap();
        assert_eq!(plan.cwd, PathBuf::from("/pkg/web"));
        m.fail_next("purge_plugin", "web", "busy");
        assert_eq!(
            m.purge_plugin(&"web".parse().unwrap())
                .unwrap_err()
                .to_string(),
            "web: rm purge_plugin: busy"
        );
        assert_eq!(
            m.calls(),
            vec!["materialize_plugin balerix/plugins/web", "purge_plugin web"]
        );
    }

    #[test]
    fn clock_advances() {
        let c = FakeClock::new(Timestamp(100));
        c.advance(5);
        assert_eq!(c.now(), Timestamp(105));
        c.set(Timestamp(1));
        assert_eq!(c.now(), Timestamp(1));
    }

    #[test]
    fn the_fake_system_toolchain_counts_calls_and_recovers_after_n_failures() {
        let tc = FakeSystemToolchain::failing(2);
        assert!(tc.ensure_system_pool().is_err(), "first attempt fails");
        assert!(tc.ensure_system_pool().is_err(), "second attempt fails");
        assert!(tc.ensure_system_pool().is_ok(), "third attempt succeeds");
        assert_eq!(tc.calls(), 3);

        let always = FakeSystemToolchain::always_failing();
        assert!(always.ensure_system_pool().is_err());
        assert!(always.ensure_system_pool().is_err());
        assert_eq!(always.calls(), 2);

        let ok = FakeSystemToolchain::ready();
        assert!(ok.ensure_system_pool().is_ok());
    }

    /// Drains `reader` on a thread: one message per read, ending at EOF or
    /// an error. A fake that never wakes its reader then fails `next` after
    /// a bound instead of hanging the test (and its mutation run).
    fn reads(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<io::Result<Vec<u8>>> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8];
            loop {
                let r = reader.read(&mut buf).map(|n| buf[..n].to_vec());
                let done = !matches!(&r, Ok(v) if !v.is_empty());
                if tx.send(r).is_err() || done {
                    break;
                }
            }
        });
        rx
    }

    fn next(rx: &mpsc::Receiver<io::Result<Vec<u8>>>) -> Vec<u8> {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("the fake reader stalled")
            .expect("the fake reader failed")
    }

    #[test]
    fn set_fail_observe_fails_every_observe_until_cleared() {
        let r = FakeRunner::default();
        let fleet: FleetName = "f".parse().unwrap();
        r.set_fail_observe(true);
        let e = r.observe(&fleet).unwrap_err().to_string();
        assert!(e.contains("fake observe failure"), "{e}");
        assert!(r.observe(&fleet).is_err(), "keeps failing until cleared");
        r.set_fail_observe(false);
        assert!(r.observe(&fleet).is_ok());
    }

    #[test]
    fn send_text_is_recorded_and_failable() {
        let r = FakeRunner::default();
        r.send_text(&id("f/c/a"), "hi", true).unwrap();
        assert_eq!(r.calls(), vec!["send_text f/c/a \"hi\" submit=true"]);
        r.fail_next("send_text", "f/c/a \"no\" submit=false", "no window");
        assert!(r.send_text(&id("f/c/a"), "no", false).is_err());
    }

    #[test]
    fn send_keys_is_recorded_and_failable() {
        use balerix_api::{Key, KeyStep};
        let r = FakeRunner::default();
        let steps = [
            KeyStep::Key(Key::Down),
            KeyStep::Key(Key::Down),
            KeyStep::Text("teal-ish".into()),
            KeyStep::Key(Key::Enter),
        ];
        r.send_keys(&id("f/c/a"), &steps, std::time::Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            r.calls(),
            vec!["send_keys f/c/a [down,down,\"teal-ish\",enter] delay=100ms"]
        );
        r.fail_next("send_keys", "f/c/a [escape] delay=20ms", "no window");
        assert!(
            r.send_keys(
                &id("f/c/a"),
                &[KeyStep::Key(Key::Escape)],
                std::time::Duration::from_millis(20)
            )
            .is_err()
        );
    }

    #[test]
    fn the_fake_pty_echoes_records_resizes_and_closes() {
        let r = FakeRunner::default();
        let pty = r.attach(&id("f/c/a")).unwrap();
        let rx = reads(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        writer.write_all(b"hi").unwrap();
        assert_eq!(next(&rx), b"hi");
        pty.resize(120, 40).unwrap();
        assert_eq!(r.resizes(), vec![("f/c/a".to_string(), 120, 40)]);
        assert!(r.close_attach(&id("f/c/a")), "an open attach was closed");
        assert!(!r.close_attach(&id("f/c/a")), "and only once");
        assert_eq!(next(&rx), b"", "EOF after close");
        assert_eq!(
            writer.write_all(b"x").unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert!(r.calls().contains(&"attach f/c/a".to_string()));
        // dropping the stream closes it too
        let pty = r.attach(&id("f/c/b")).unwrap();
        let rx = reads(pty.reader().unwrap());
        drop(pty);
        assert_eq!(next(&rx), b"", "EOF after drop");
        r.fail_next("attach", "f/c/c", "no window");
        assert!(r.attach(&id("f/c/c")).is_err());
    }

    #[test]
    fn the_fake_workspace_answers_from_what_the_test_set() {
        use balerix_api::{EntryKind, FileDiff, FileStatus, WorkspaceDiff};
        let w = FakeWorkspace::default();
        assert_eq!(
            w.diff(&id("f/c/a"), "origin/main"),
            Err(WorkspaceError::Missing("f/c/a".into()))
        );
        let diff = WorkspaceDiff {
            base_ref: String::new(),
            merge_base: "m".into(),
            head: "h".into(),
            files: vec![FileDiff {
                path: "src/lib.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                uncommitted: true,
                binary: false,
                patch: "p".into(),
                truncated: false,
            }],
            truncated: false,
        };
        w.set(
            &id("f/c/a"),
            diff.clone(),
            BTreeMap::from([
                ("src/lib.rs".to_string(), b"fn a() {}\n".to_vec()),
                ("src/sub/x.rs".to_string(), b"x".to_vec()),
                ("README".to_string(), b"hi\n".to_vec()),
            ]),
        );
        let got = w.diff(&id("f/c/a"), "origin/main").unwrap();
        assert_eq!(got.base_ref, "origin/main", "the base the caller asked for");
        assert_eq!(got.files, diff.files);
        assert_eq!(
            w.read_file(&id("f/c/a"), "src/lib.rs").unwrap(),
            b"fn a() {}\n".to_vec()
        );
        assert_eq!(
            w.read_file(&id("f/c/a"), "nope"),
            Err(WorkspaceError::NoSuchPath)
        );
        assert_eq!(
            w.read_file(&id("f/c/a"), "src"),
            Err(WorkspaceError::NotAFile)
        );
        assert_eq!(
            w.read_file(&id("f/c/a"), "../x"),
            Err(WorkspaceError::InvalidPath("\"..\" segment".into()))
        );
        let big = vec![0u8; (balerix_api::WORKSPACE_FILE_LIMIT + 1) as usize];
        w.set(
            &id("f/c/b"),
            diff.clone(),
            BTreeMap::from([("big".to_string(), big)]),
        );
        assert_eq!(
            w.read_file(&id("f/c/b"), "big"),
            Err(WorkspaceError::TooLarge {
                limit: balerix_api::WORKSPACE_FILE_LIMIT
            })
        );
        let root = w.list_dir(&id("f/c/a"), "").unwrap();
        assert_eq!(root.path, "");
        let names: Vec<(&str, EntryKind, Option<u64>)> = root
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.kind, e.size))
            .collect();
        assert_eq!(
            names,
            vec![
                ("README", EntryKind::File, Some(3)),
                ("src", EntryKind::Dir, None)
            ]
        );
        let src = w.list_dir(&id("f/c/a"), "src").unwrap();
        assert_eq!(src.entries.len(), 2);
        assert_eq!(src.entries[1].name, "sub");
        assert_eq!(
            w.list_dir(&id("f/c/a"), "README"),
            Err(WorkspaceError::NotADirectory)
        );
        assert_eq!(
            w.list_dir(&id("f/c/a"), "nope"),
            Err(WorkspaceError::NoSuchPath)
        );
        assert_eq!(
            w.list_dir(&id("f/c/z"), ""),
            Err(WorkspaceError::Missing("f/c/z".into()))
        );
        assert!(w.calls().contains(&"diff f/c/a origin/main".to_string()));
        assert!(
            w.calls()
                .contains(&"read_file f/c/a src/lib.rs".to_string())
        );
        assert_eq!(
            WorkspaceError::TooLarge { limit: 1 << 20 }.to_string(),
            "file larger than 1 MiB"
        );
        assert_eq!(
            WorkspaceError::InvalidPath("absolute".into()).to_string(),
            "workspace: invalid path: absolute"
        );
        assert_eq!(
            WorkspaceError::Filter {
                key: "filter.lfs.clean".into()
            }
            .to_string(),
            "repository config sets filter.lfs.clean; workspace diff refused"
        );
        let v = w.version(&id("f/c/a"), "origin/main").unwrap();
        assert_eq!(v.head, "h");
        assert_eq!(v.fingerprint.len(), 64);
        assert!(v.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            w.version(&id("f/c/a"), "origin/main").unwrap().fingerprint,
            v.fingerprint,
            "stable"
        );
        w.set(
            &id("f/c/a"),
            diff.clone(),
            BTreeMap::from([("src/lib.rs".to_string(), b"fn a() {}\nfn b() {}\n".to_vec())]),
        );
        assert_ne!(
            w.version(&id("f/c/a"), "origin/main").unwrap().fingerprint,
            v.fingerprint,
            "different bytes, different fingerprint"
        );
        assert_eq!(
            w.version(&id("f/c/z"), "origin/main"),
            Err(WorkspaceError::Missing("f/c/z".into()))
        );
        assert!(w.calls().contains(&"version f/c/a origin/main".to_string()));
    }

    #[test]
    fn the_fake_resolver_answers_under_the_requested_name_and_records_files() {
        let r = FakeResolver::default();
        let name: FleetName = "f".parse().unwrap();
        assert_eq!(
            r.resolve(&json!({}), &name),
            Err("name: no resolver answer configured".to_string())
        );
        r.set(Ok(FleetSpec {
            name: "other".into(),
            ..Default::default()
        }));
        let spec = r.resolve(&json!({ "kind": "Fleet" }), &name).unwrap();
        assert_eq!(spec.name, "f", "the fixed spec is renamed to the request");
        r.set(Err("crews.c.repo: invalid repo".into()));
        assert_eq!(
            r.resolve(&json!({}), &name),
            Err("crews.c.repo: invalid repo".to_string())
        );
        assert_eq!(
            r.calls(),
            vec![
                ("f".to_string(), json!({})),
                ("f".to_string(), json!({ "kind": "Fleet" })),
                ("f".to_string(), json!({})),
            ]
        );
        assert_eq!(
            FakeResolver::failing("x").resolve(&json!({}), &name),
            Err("x".to_string())
        );
        assert_eq!(
            FakeResolver::answering(FleetSpec::default())
                .resolve(&json!({}), &name)
                .unwrap()
                .name,
            "f"
        );
    }

    #[test]
    fn the_fake_credentials_answer_a_bundle_or_an_error_and_count_calls() {
        let c = FakeCredentials::default();
        assert_eq!(c.load(), Ok(CredentialBundle::default()));
        let bundle = CredentialBundle {
            gh_token: Some("gho_x".into()),
            ..CredentialBundle::default()
        };
        let c = FakeCredentials::new(bundle.clone());
        assert_eq!(c.load(), Ok(bundle));
        c.set(Err("/home/op/.claude/settings.json: invalid JSON".into()));
        assert!(c.load().is_err());
        assert_eq!(c.calls(), 2);
        assert!(FakeCredentials::failing("no").load().is_err());
    }
}
