//! `WorkspaceReader` over real git (Spec C §3.2): the agent's clone
//! against the crew's base, one file, one listing. Reads only, with the
//! repository's config escape hatches closed — an agent can write its clone's
//! `.git/config` and `.gitattributes`, and this code runs as the daemon.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, openat, readlinkat, statat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use balerix_api::{
    EntryKind, FileDiff, FileStatus, TreeEntry, WORKSPACE_FILE_COUNT_LIMIT, WORKSPACE_FILE_LIMIT,
    WORKSPACE_PATCH_LIMIT, WorkspaceDiff, WorkspaceTree, WorkspaceVersion, check_path,
};
use balerix_core::{AgentId, MaterializeError, WorkspaceError, WorkspaceReader};

use crate::layout::{AgentPaths, CrewPaths};
use crate::materializer::Runtime;
use crate::sandbox::{lexical, write_git_profile};
use crate::tools::{CmdOutput, TIMEOUT_GRACE};
use crate::workspace::{
    GitFailure, GitLog, StdoutSink, git_read_hint, sandboxed_git, sandboxed_git_script,
};

/// The most a `version` may take, its sandbox start and the end of a
/// timed-out one included (#213): its four git calls are cheap, so past
/// this something is hung (a lock, a filesystem that stopped answering),
/// and the review page's poll, which holds one `version` per agent in
/// flight, would wait on it for good. Under the plugin SDK's 10 s request
/// timeout, so the plugin gets the daemon's answer (`TimedOut`, a 504)
/// rather than a timeout of its own.
pub const VERSION_BUDGET: Duration = Duration::from_secs(6);

/// `VERSION_BUDGET` for a whole `diff`, shared by its sandbox starts (the
/// listing, the patches, and a file diffed on its own): each start gets
/// what is left. Under the SDK's 30 s for a diff.
pub const DIFF_BUDGET: Duration = Duration::from_secs(25);

/// One read under way: its name in a `TimedOut`, its budget, and when
/// that runs out.
#[derive(Debug, Clone, Copy)]
struct ReadOp {
    name: &'static str,
    budget: Duration,
    by: Instant,
}

impl ReadOp {
    fn version() -> Self {
        Self::new("version", VERSION_BUDGET)
    }

    fn diff() -> Self {
        Self::new("diff", DIFF_BUDGET)
    }

    fn new(name: &'static str, budget: Duration) -> Self {
        Self {
            name,
            budget,
            by: Instant::now() + budget,
        }
    }

    fn left(self) -> Duration {
        self.by.saturating_duration_since(Instant::now())
    }

    /// The `Cmd::timeout` for the next sandbox start: what is left less
    /// `TIMEOUT_GRACE`, so a start ended at it is gone by the budget's
    /// end; `None` once nothing is.
    fn start_deadline(self) -> Option<Duration> {
        Some(self.left().saturating_sub(TIMEOUT_GRACE)).filter(|d| !d.is_zero())
    }

    fn timed_out(self, id: &str) -> WorkspaceError {
        WorkspaceError::TimedOut {
            id: id.to_string(),
            subcommand: self.name.to_string(),
            after: self.budget,
        }
    }
}

/// Command-line config beats every config file: whatever an agent wrote
/// into its clone's `.git/config`, no program runs from it here (the rest
/// of the hardening is `workspace::sandboxed_git`'s and the git profile's).
/// The prefixes are pinned so the combined diff's `diff --git a/… b/…`
/// lines are the ones `patch_header` expects, whatever the clone sets.
const CONFIG: &[&str] = &[
    "-c",
    "core.quotePath=true",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicPrefix=false",
    "-c",
    "diff.srcPrefix=a/",
    "-c",
    "diff.dstPrefix=b/",
];
/// On every `diff`: no external diff driver, no textconv, no colour, and no
/// descent into a nested repository. An explicit `--submodule=short` beats a
/// `diff.submodule = diff` an agent wrote into the shared config, so a
/// gitlink prints its two hashes and git runs nothing inside the nested
/// repository — where the `-c` overrides above do apply (they travel in
/// `GIT_CONFIG_PARAMETERS`) but these argv flags do not, leaving that
/// repository's own `diff.external` free to run as the daemon; the
/// `FILTER_KEYS` probe never reads a nested repository's config either.
/// `--ignore-submodules=dirty` drops the `git status` child the default
/// dirty check spawns there as well; the cost is that a submodule dirty only
/// in its own worktree is not flagged `uncommitted`.
const DIFF_FLAGS: &[&str] = &[
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--submodule=short",
    "--ignore-submodules=dirty",
];

/// Config keys whose value is a program git runs on `diff` (a clean
/// filter through `.gitattributes`) or on checkout; `--no-ext-diff` and
/// `--no-textconv` do not cover them, so a diff is refused instead. A
/// matching `extensions.worktreeconfig` (git lowercases variable names in
/// `--name-only` output, whatever case the file used) means the worktree
/// may have a `config.worktree` file the `--local` read below cannot see
/// — balerix never enables `worktreeConfig`, so one set here is not
/// something balerix sets, and a diff is refused outright. `git
/// sparse-checkout init/set` (and `scalar`) enable it too, so the remedy is
/// `git sparse-checkout disable` rather than unsetting the key by hand.
/// A promisor remote (`extensions.partialclone`, or any
/// `remote.<x>.promisor`; the alternation is `workspace::PROMISOR_KEYS`)
/// would make a diff over a missing object fetch it from `remote.<x>.url`
/// as the daemon and run `remote.<x>.uploadpack`; `GIT_NO_LAZY_FETCH=1`
/// (the git profile's `set_vars`) closes that on git 2.45.1 and later, the
/// profile's blocked network on any git (#108), and the refusal says why
/// (#67).
const FILTER_KEYS: &str = r"^(filter\..*\.(clean|smudge|process)|extensions\.worktreeconfig|extensions\.partialclone|remote\..*\.promisor)$";

impl Runtime {
    /// The agent's paths and its crew's; `Missing` when the worktree
    /// directory does not exist (not materialized, or purged).
    fn workspace_of(&self, agent: &AgentId) -> Result<(AgentPaths, CrewPaths), WorkspaceError> {
        let paths = self.layout.agent(agent);
        if !paths.workspace.is_dir() {
            return Err(WorkspaceError::Missing(agent.to_string()));
        }
        Ok((paths, self.layout.crew(&agent.crew_ref())))
    }

    /// `workspace_of`, plus the git profile the git calls below run
    /// under, written for this request (`write_git_profile`; validated
    /// only when its bytes changed).
    fn sandboxed_workspace_of(
        &self,
        agent: &AgentId,
    ) -> Result<(AgentPaths, CrewPaths), WorkspaceError> {
        let (paths, crew) = self.workspace_of(agent)?;
        let id = agent.to_string();
        write_git_profile(
            &self.tools,
            &id,
            &paths,
            &crew,
            &self.git_read,
            self.socket_policy,
        )
        .map_err(|e| match e {
            MaterializeError::Io { path, message, .. } => WorkspaceError::Io { path, message },
            MaterializeError::Tool {
                subcommand,
                args,
                stderr,
                ..
            } => WorkspaceError::Tool {
                id: id.clone(),
                subcommand,
                args,
                stderr,
            },
            other => WorkspaceError::Tool {
                id: id.clone(),
                subcommand: "git profile".into(),
                args: Vec::new(),
                stderr: other.to_string(),
            },
        })?;
        Ok((paths, crew))
    }

    /// One git call in the worktree, under the git profile
    /// (`workspace::sandboxed_git`, #108): it reads the clone and the crew
    /// cache's objects and nothing else, so a clone pointed at another
    /// repository shows the operator nothing of it. Logged to the crew's
    /// `git.log` with the argv, stderr and the exit status but not the
    /// stdout: a review page's `diff.json` carries up to 500 patches at
    /// 256 KiB each, and full-output logging would grow `git.log` by the
    /// whole diff on every fetch. `CONFIG` goes first; the profile's
    /// `set_vars` and `sandboxed_git`'s `-c` pairs are the rest of the
    /// hardening. Only for the rare file the combined diff cannot name;
    /// the rest is two scripts (`DIFF_LIST_SCRIPT`, `DIFF_PATCH_SCRIPT`).
    fn inspect_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        paths: &AgentPaths,
        op: ReadOp,
        args: &[&str],
    ) -> Result<String, WorkspaceError> {
        let Some(deadline) = op.start_deadline() else {
            return Err(op.timed_out(id));
        };
        let mut argv: Vec<&str> = CONFIG.to_vec();
        argv.extend(args);
        sandboxed_git(
            &self.tools,
            self.socket_policy,
            crew,
            paths,
            &argv,
            &[0],
            GitLog::ArgvOnly,
            Some(deadline),
        )
        .map(|out| out.stdout)
        .map_err(|f| self.read_failure(id, op, f))
    }

    /// A failed sandbox start as the read's error: `TimedOut` past its
    /// deadline, else the start's `Tool` error. No hint here: a start
    /// that fails outright has no git exit code to judge by.
    fn read_failure(&self, id: &str, op: ReadOp, f: GitFailure) -> WorkspaceError {
        if f.timed_out.is_some() {
            return op.timed_out(id);
        }
        WorkspaceError::Tool {
            id: id.to_string(),
            subcommand: f.subcommand,
            args: f.args,
            stderr: f.stderr,
        }
    }

    /// `e` with the `sandbox.git_read` hint before its stderr (#214) when
    /// `cannot_run`: git exited 126 (the shell could not execute it) or
    /// 127 (it could not load what it needs), and only when the sandbox
    /// itself starts (`workspace::git_read_hint`, asked within what is
    /// left of `op`), as the clone step's canary says it (#111). Never
    /// from stderr's text: a git that ran and could not read a file the
    /// agent made unreadable prints `Permission denied` too. Any other
    /// error is returned as it is.
    fn hinted(
        &self,
        crew: &CrewPaths,
        paths: &AgentPaths,
        op: ReadOp,
        e: WorkspaceError,
        cannot_run: bool,
    ) -> WorkspaceError {
        match e {
            WorkspaceError::Tool {
                id,
                subcommand,
                args,
                stderr,
            } if cannot_run && !op.left().is_zero() => {
                let hint = git_read_hint(&self.tools, self.socket_policy, crew, paths, op.left());
                let stderr = match hint {
                    Some(hint) => format!("{hint}: {stderr}"),
                    None => stderr,
                };
                WorkspaceError::Tool {
                    id,
                    subcommand,
                    args,
                    stderr,
                }
            }
            other => other,
        }
    }

    /// One of the reader's scripts in one sandbox start
    /// (`workspace::sandboxed_git_script`) within what is left of `op`, git's
    /// options `CONFIG`'s; a failure of the start itself is reported as
    /// `sh`'s.
    #[allow(clippy::too_many_arguments)]
    fn inspect_script(
        &self,
        id: &str,
        crew: &CrewPaths,
        paths: &AgentPaths,
        op: ReadOp,
        script: &'static str,
        leading: &[&str],
        accepted: &[i32],
        sink: Option<StdoutSink<'_>>,
    ) -> Result<CmdOutput, WorkspaceError> {
        let Some(deadline) = op.start_deadline() else {
            return Err(op.timed_out(id));
        };
        sandboxed_git_script(
            &self.tools,
            self.socket_policy,
            crew,
            paths,
            script,
            leading,
            CONFIG,
            accepted,
            sink,
            deadline,
        )
        .map_err(|f| self.read_failure(id, op, f))
    }

    /// A listing script (`VERSION_SCRIPT` with `VERSION_LISTING`,
    /// `DIFF_LIST_SCRIPT` with `DIFF_LISTING`) against `base_ref`: its
    /// stdout, or the refusal or the failed stage reported as that git
    /// call on its own would have been. The caller parses the stdout
    /// (`parse_listing`; `unexpected_listing` when it does not parse).
    fn run_listing(
        &self,
        id: &str,
        crew: &CrewPaths,
        paths: &AgentPaths,
        op: ReadOp,
        (script, listing): (&'static str, &[&str]),
        base_ref: &str,
    ) -> Result<String, WorkspaceError> {
        let out = self.inspect_script(
            id,
            crew,
            paths,
            op,
            script,
            &[base_ref, FILTER_KEYS],
            &[0, VERSION_FILTER, 81, 82, 83, 84, GIT_CANNOT_RUN],
            None,
        )?;
        let tool_error =
            |subcommand: &str, args: Vec<String>, stderr: String| WorkspaceError::Tool {
                id: id.to_string(),
                subcommand: subcommand.to_string(),
                args,
                stderr,
            };
        // each stage's own git arguments, as `inspect_git` reports them
        let stage_args = |stage: &str| -> Vec<String> {
            let mut args: Vec<&str> = CONFIG.to_vec();
            let merge_base = out.stdout.split('\0').next().unwrap_or_default();
            match stage {
                "config" => args.extend(PROBE.iter().chain([&FILTER_KEYS])),
                "merge-base" => args.extend(["merge-base", base_ref, "HEAD"]),
                "diff" => {
                    args.push("diff");
                    args.extend(DIFF_FLAGS);
                    args.extend(listing);
                    args.push(merge_base);
                }
                _ => args.extend(STATUS),
            }
            args.into_iter().map(str::to_string).collect()
        };
        if out.code == VERSION_FILTER {
            return Err(match filter_key(&out.stdout) {
                Some(key) => WorkspaceError::Filter { key },
                None => tool_error("config", stage_args("config"), out.stdout.clone()),
            });
        }
        if let Some(stage) = version_stage(out.code) {
            return Err(self.hinted(
                crew,
                paths,
                op,
                tool_error(stage, stage_args(stage), out.stderr),
                out.code == GIT_CANNOT_RUN,
            ));
        }
        Ok(out.stdout)
    }

    /// `DIFF_PATCH_SCRIPT` over `untracked` (and the combined diff, when
    /// `mode` is not `none`), in as many starts as `PATCH_ARGS_BUDGET`
    /// needs (one, unless the paths are very long): the combined diff's
    /// sections and each untracked file's raw patch, in order. A combined
    /// diff that fails, or a `--no-index` that exits other than 0 or 1
    /// (1 is "they differ", and also what git answers, with a message,
    /// for a path it cannot read as a file, such as a symlink to a
    /// directory: an empty patch, as before), fails the read as that git
    /// call would have.
    #[allow(clippy::too_many_arguments)]
    fn run_patches(
        &self,
        id: &str,
        crew: &CrewPaths,
        paths: &AgentPaths,
        op: ReadOp,
        merge_base: &str,
        mode: &str,
        pathspecs: &[&str],
        untracked: &[&str],
    ) -> Result<(BTreeMap<String, String>, Vec<String>), WorkspaceError> {
        // the first start carries the pathspecs too: they count against
        // its budget, so the untracked paths after them may start later
        let used = pathspecs.iter().map(|p| p.len() + 1).sum();
        let mut batches = batches(untracked, PATCH_ARGS_BUDGET, used);
        if mode != "none" && batches.is_empty() {
            batches.push(&[]);
        }
        let mut combined = BTreeMap::new();
        let mut patches = Vec::with_capacity(untracked.len());
        for (n, batch) in batches.into_iter().enumerate() {
            let (mode, pathspecs) = if n == 0 {
                (mode, pathspecs)
            } else {
                ("none", &[][..])
            };
            let (nt, nu) = (pathspecs.len().to_string(), batch.len().to_string());
            let mut leading: Vec<&str> = vec![merge_base, FILTER_KEYS, mode, &nt, &nu];
            leading.extend(pathspecs);
            leading.extend(batch);
            let mut reader = SectionReader::new(mode != "none", batch.len());
            let out = self.inspect_script(
                id,
                crew,
                paths,
                op,
                DIFF_PATCH_SCRIPT,
                &leading,
                &[0, VERSION_FILTER, 81, GIT_CANNOT_RUN],
                Some(&mut |chunk| reader.feed(chunk)),
            )?;
            let probe_args = || -> Vec<String> {
                CONFIG
                    .iter()
                    .chain(PROBE)
                    .chain([&FILTER_KEYS])
                    .map(|a| (*a).to_string())
                    .collect()
            };
            if out.code == VERSION_FILTER {
                return Err(match filter_key(&out.stderr) {
                    Some(key) => WorkspaceError::Filter { key },
                    None => WorkspaceError::Tool {
                        id: id.to_string(),
                        subcommand: "config".into(),
                        args: probe_args(),
                        stderr: out.stderr,
                    },
                });
            }
            if matches!(out.code, 81 | GIT_CANNOT_RUN) {
                return Err(self.hinted(
                    crew,
                    paths,
                    op,
                    WorkspaceError::Tool {
                        id: id.to_string(),
                        subcommand: "config".into(),
                        args: probe_args(),
                        stderr: out.stderr,
                    },
                    out.code == GIT_CANNOT_RUN,
                ));
            }
            let sections = reader.finish().ok_or_else(|| WorkspaceError::Tool {
                id: id.to_string(),
                subcommand: "sh".into(),
                args: Vec::new(),
                stderr: format!("unexpected output from the patch script: {}", out.stderr),
            })?;
            // the script's stderr is every section's: say which one failed
            let failed = |args: Vec<&str>, what: String, code: Option<i32>| {
                let mut argv: Vec<&str> = CONFIG.to_vec();
                argv.extend(args);
                self.hinted(
                    crew,
                    paths,
                    op,
                    WorkspaceError::Tool {
                        id: id.to_string(),
                        subcommand: "diff".into(),
                        args: argv.into_iter().map(str::to_string).collect(),
                        stderr: format!(
                            "{what} failed (stderr of the whole patch script follows): {}",
                            out.stderr
                        ),
                    },
                    matches!(code, Some(126 | 127)),
                )
            };
            let mut codes = sections.codes.into_iter();
            if let Some(split) = sections.combined {
                let code = codes.next();
                if code != Some(0) {
                    let mut args = Vec::new();
                    if mode == "paths" {
                        args.push("--literal-pathspecs");
                    }
                    args.push("diff");
                    args.extend(DIFF_FLAGS);
                    args.extend(["-U3", "--find-renames", merge_base]);
                    if mode == "paths" {
                        args.push("--");
                        args.extend(pathspecs);
                    }
                    return Err(failed(args, "the combined diff".into(), code));
                }
                combined = split;
            }
            for (patch, (code, path)) in sections.untracked.into_iter().zip(codes.zip(batch)) {
                if !matches!(code, 0 | 1) {
                    let mut args = vec!["diff"];
                    args.extend(DIFF_FLAGS);
                    args.extend(["--no-index", "-U3", "--", "/dev/null", path]);
                    return Err(failed(
                        args,
                        format!("the diff of untracked {path:?} (exit {code})"),
                        Some(code),
                    ));
                }
                patches.push(patch);
            }
        }
        Ok((combined, patches))
    }
}

/// What one `status --porcelain=v2 -z --branch --untracked-files=all`
/// says: HEAD (`None` on an unborn branch), every path that differs from
/// HEAD in the index or the worktree (both sides of a rename), and every
/// untracked path. One call where there were three (#108).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub head: Option<String>,
    pub changed: BTreeSet<String>,
    pub untracked: BTreeSet<String>,
}

const STATUS: &[&str] = &[
    "status",
    "--porcelain=v2",
    "-z",
    "--branch",
    "--no-ahead-behind",
    "--untracked-files=all",
    "--ignore-submodules=dirty",
];

/// Parses `STATUS`'s output. Records are NUL-terminated and paths are
/// not quoted; a path is everything after the record's fixed fields, so
/// a space in it is kept. A type `2` (rename or copy) record is followed
/// by its original path as a record of its own.
pub fn parse_status(z: &str) -> Status {
    let mut status = Status::default();
    let mut records = z.split('\0').filter(|r| !r.is_empty());
    let path_after = |record: &str, fields: usize| {
        record
            .splitn(fields + 1, ' ')
            .nth(fields)
            .map(str::to_string)
    };
    while let Some(record) = records.next() {
        let mut kind = record.splitn(2, ' ');
        match kind.next() {
            Some("#") => {
                if let Some(oid) = record.strip_prefix("# branch.oid ") {
                    status.head = (oid != "(initial)").then(|| oid.to_string());
                }
            }
            Some("1") => status.changed.extend(path_after(record, 8)),
            Some("2") => {
                status.changed.extend(path_after(record, 9));
                status.changed.extend(records.next().map(str::to_string));
            }
            Some("u") => status.changed.extend(path_after(record, 10)),
            Some("?") => status.untracked.extend(path_after(record, 1)),
            _ => {}
        }
    }
    status
}

/// A combined patch split per file as it streams in, keyed by its
/// `diff --git` line, keeping at most `cap` bytes of each section. A
/// typechange is two sections under one header (a deletion, then a
/// creation), kept together as `git diff -- <path>` prints them. No
/// content line can start `diff --git `: every one carries a ` `, `+`,
/// `-` or `\` first. A header met again after another one is ambiguous
/// (two files that print the same line) and is dropped, so both fall back
/// to a diff of their own.
pub struct PatchSplitter {
    cap: usize,
    sections: BTreeMap<String, Vec<u8>>,
    ambiguous: BTreeSet<String>,
    current: Option<String>,
    at_line_start: bool,
    /// The start of the current line while it may still be a header.
    probe: Option<Vec<u8>>,
}

/// A header line is a path pair; one longer than this is kept as content.
const HEADER_MAX: usize = 64 * 1024;
const HEADER: &[u8] = b"diff --git ";

impl PatchSplitter {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            sections: BTreeMap::new(),
            ambiguous: BTreeSet::new(),
            current: None,
            at_line_start: true,
            probe: None,
        }
    }

    pub fn feed(&mut self, mut chunk: &[u8]) {
        while !chunk.is_empty() {
            let (segment, rest) = match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => chunk.split_at(i + 1),
                None => (chunk, &[][..]),
            };
            chunk = rest;
            let ends_line = segment.last() == Some(&b'\n');
            if self.at_line_start {
                self.probe = Some(Vec::new());
            }
            self.at_line_start = ends_line;
            let Some(mut probe) = self.probe.take() else {
                self.append(segment);
                continue;
            };
            probe.extend_from_slice(segment);
            let could_be = probe.len() < HEADER.len() && HEADER.starts_with(&probe)
                || probe.starts_with(HEADER) && probe.len() <= HEADER_MAX;
            if !could_be {
                self.append(&probe);
            } else if ends_line {
                if probe.starts_with(HEADER) {
                    self.start_section(&probe);
                } else {
                    self.append(&probe);
                }
            } else {
                self.probe = Some(probe);
            }
        }
    }

    fn start_section(&mut self, line: &[u8]) {
        let key = String::from_utf8_lossy(line.strip_suffix(b"\n").unwrap_or(line)).into_owned();
        if self.current.as_ref() != Some(&key) && self.sections.contains_key(&key) {
            self.ambiguous.insert(key.clone());
        }
        self.current = Some(key);
        self.append(line);
    }

    fn append(&mut self, bytes: &[u8]) {
        let Some(key) = &self.current else {
            return;
        };
        let section = self.sections.entry(key.clone()).or_default();
        let room = self.cap.saturating_sub(section.len());
        section.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// The sections, each at most `cap` bytes, ambiguous ones left out.
    pub fn finish(mut self) -> BTreeMap<String, String> {
        if let Some(probe) = self.probe.take() {
            if probe.starts_with(HEADER) {
                self.start_section(&probe);
            } else {
                self.append(&probe);
            }
        }
        let ambiguous = self.ambiguous;
        self.sections
            .into_iter()
            .filter(|(k, _)| !ambiguous.contains(k))
            .map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned()))
            .collect()
    }
}

/// `PatchSplitter` over a whole patch held in memory, with no cap.
pub fn split_patch(raw: &str) -> BTreeMap<String, String> {
    let mut splitter = PatchSplitter::new(usize::MAX);
    splitter.feed(raw.as_bytes());
    splitter.finish()
}

/// Bytes of a section the combined diff keeps: `shape_patch` cuts at the
/// last line end within `WORKSPACE_PATCH_LIMIT`, so what lies past the
/// limit only tells it that the patch was longer. The three extra bytes
/// keep a UTF-8 character the cap splits out of that window.
const SECTION_CAP: usize = WORKSPACE_PATCH_LIMIT + 4;

/// The `diff --git` line git prints for `file` with the `a/` and `b/`
/// prefixes `CONFIG` pins; `None` when git would quote either path
/// (`core.quotePath`), or when either holds ` b/`, which makes the line
/// ambiguous (`p` → `q b/r` and `p b/q` → `r` both print `diff --git a/p
/// b/q b/r`): the caller diffs that file on its own instead.
fn patch_header(file: &FileDiff) -> Option<String> {
    let old = file.old_path.as_deref().unwrap_or(&file.path);
    let plain = |p: &str| {
        !p.contains(" b/")
            && !p
                .bytes()
                .any(|b| !(0x20..0x7f).contains(&b) || b == b'"' || b == b'\\')
    };
    (plain(old) && plain(&file.path)).then(|| format!("diff --git a/{old} b/{}", file.path))
}

/// `diff --name-status -z --find-renames` → one `FileDiff` per record,
/// patches empty. `R`/`C` records carry two paths; a torn pair ends the
/// parse.
pub fn parse_name_status(z: &str) -> Vec<FileDiff> {
    let mut out = Vec::new();
    let mut parts = z.split('\0').filter(|p| !p.is_empty());
    while let Some(code) = parts.next() {
        let status = match code.chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            Some('R') => FileStatus::Renamed,
            Some('C') => FileStatus::Copied,
            Some('T') => FileStatus::Typechange,
            _ => FileStatus::Modified,
        };
        let (old_path, path) = match status {
            FileStatus::Renamed | FileStatus::Copied => match (parts.next(), parts.next()) {
                (Some(old), Some(new)) => (Some(old.to_string()), new.to_string()),
                _ => break,
            },
            _ => match parts.next() {
                Some(p) => (None, p.to_string()),
                None => break,
            },
        };
        out.push(FileDiff {
            path,
            old_path,
            status,
            uncommitted: false,
            binary: false,
            patch: String::new(),
            truncated: false,
        });
    }
    out
}

/// `(patch, binary, truncated)`: a binary diff keeps no patch; a long one
/// is cut at the last line boundary under `WORKSPACE_PATCH_LIMIT`.
pub fn shape_patch(raw: String) -> (String, bool, bool) {
    if raw
        .lines()
        .any(|l| l.starts_with("Binary files ") || l.starts_with("GIT binary patch"))
    {
        return (String::new(), true, false);
    }
    if raw.len() <= WORKSPACE_PATCH_LIMIT {
        return (raw, false, false);
    }
    // Search bytes, not chars: `raw[..WORKSPACE_PATCH_LIMIT]` would panic if
    // the limit lands inside a multi-byte character. `\n` is ASCII and never
    // appears inside one, so a byte search for it is a valid boundary.
    let end = raw.as_bytes()[..WORKSPACE_PATCH_LIMIT]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    (raw[..end].to_string(), false, true)
}

/// The patch that stands in for an untracked path that is neither a
/// regular file nor a symlink: a FIFO, a socket, a device, a directory
/// (a nested repository is one), or a path one of whose directories is
/// not one any more (`untracked_patch`).
pub const SKIPPED_NOT_A_FILE: &str = "not a regular file, skipped\n";

/// The most symlinks `untracked_patch` follows for one link's target: the
/// kernel's own limit.
const SYMLINK_HOPS: usize = 40;

/// What one no-follow lookup found (`lookup`).
#[derive(Debug, PartialEq, Eq)]
enum Leaf {
    Regular,
    /// The link's target, as `readlinkat` gave it.
    Symlink(Vec<u8>),
    /// Nothing there, the last name or a directory before it.
    Missing,
    /// Anything else: a FIFO, a socket, a device, a directory.
    Other,
    /// A name on the way that is not a directory (a symlink included),
    /// or a lookup that failed otherwise.
    Blocked,
}

/// `names` looked up below the worktree's directory `root`, following no
/// symlink: each directory on the way opened `O_PATH | O_NOFOLLOW |
/// O_DIRECTORY` from the one before, the last name `statat`ed with
/// `AT_SYMLINK_NOFOLLOW`, and a symlink there read with `readlinkat`. A
/// directory swapped for a symlink after git listed the path is
/// `Blocked`, never followed.
fn lookup(root: &OwnedFd, names: &[&OsStr]) -> Leaf {
    let Some((last, dirs)) = names.split_last() else {
        return Leaf::Other;
    };
    let mut dir: Option<OwnedFd> = None;
    for name in dirs {
        let at = dir.as_ref().unwrap_or(root);
        match openat(
            at,
            *name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => dir = Some(fd),
            Err(e) if e == Errno::NOENT => return Leaf::Missing,
            Err(_) => return Leaf::Blocked,
        }
    }
    let at = dir.as_ref().unwrap_or(root);
    let stat = match statat(at, *last, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(e) if e == Errno::NOENT => return Leaf::Missing,
        Err(_) => return Leaf::Blocked,
    };
    match FileType::from_raw_mode(stat.st_mode) {
        FileType::RegularFile => Leaf::Regular,
        FileType::Symlink => match readlinkat(at, *last, Vec::new()) {
            Ok(target) => Leaf::Symlink(target.into_bytes()),
            Err(_) => Leaf::Blocked,
        },
        _ => Leaf::Other,
    }
}

/// `rel`'s names, or `None` when it holds anything else (`..`, `.`, a
/// root): git names untracked paths plainly, and a path that does not is
/// refused rather than walked.
fn plain_names(rel: &Path) -> Option<Vec<&OsStr>> {
    rel.components()
        .map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}

/// Whether `--no-index` may be given the symlink at `names` whose target
/// is `target`: only when it resolves, inside the worktree, to a regular
/// file or to nothing, for which git prints the target text. git follows
/// a link to a directory or a FIFO (and reads the FIFO, blocking until
/// something writes to it), so anything else is not given to it, and
/// neither is a target outside the worktree, which is never looked at.
/// The resolution is lexical from the link's directory (whose names are
/// real directories: `lookup` opened them) and each step a `lookup`, so
/// a directory symlink on the way stops it. `root_path` is the worktree's
/// canonical path, for an absolute target.
fn link_is_safe(root: &OwnedFd, root_path: &Path, names: &[&OsStr], target: &[u8]) -> bool {
    let mut dir: Vec<OsString> = names[..names.len().saturating_sub(1)]
        .iter()
        .map(|n| n.to_os_string())
        .collect();
    let mut target = target.to_vec();
    for _ in 0..SYMLINK_HOPS {
        let t = Path::new(OsStr::from_bytes(&target));
        let path: Vec<OsString> = if t.is_absolute() {
            let Ok(inside) = lexical(t).strip_prefix(root_path).map(Path::to_path_buf) else {
                return false;
            };
            match plain_names(&inside) {
                Some(n) => n.into_iter().map(OsStr::to_os_string).collect(),
                None => return false,
            }
        } else {
            let mut path = dir.clone();
            for c in t.components() {
                match c {
                    Component::Normal(name) => path.push(name.to_os_string()),
                    // above the worktree's root is outside it
                    Component::ParentDir if path.pop().is_none() => return false,
                    _ => {}
                }
            }
            path
        };
        if path.is_empty() {
            // the worktree's root: a directory
            return false;
        }
        let refs: Vec<&OsStr> = path.iter().map(OsString::as_os_str).collect();
        match lookup(root, &refs) {
            Leaf::Regular | Leaf::Missing => return true,
            Leaf::Symlink(next) => {
                dir = path[..path.len() - 1].to_vec();
                target = next;
            }
            Leaf::Other | Leaf::Blocked => return false,
        }
    }
    false
}

/// What stands in for an untracked path's `--no-index` patch (#213), or
/// `None` when the path may go to git: a regular file, a path that is not
/// there any more (git reports it, as before), or a symlink that is safe
/// to (`link_is_safe`). Another symlink, one out of the worktree included,
/// gets the patch git would print for it, built here from the link's own
/// target text (`symlink_patch`); the target is never touched. Anything
/// else is `SKIPPED_NOT_A_FILE`. `root` is the worktree's directory and
/// `root_path` its canonical path. A path changed between this and the
/// patch start is the read's budget to bound.
fn untracked_patch(root: &OwnedFd, root_path: &Path, rel: &str) -> Option<String> {
    let Some(names) = plain_names(Path::new(rel)) else {
        return Some(SKIPPED_NOT_A_FILE.to_string());
    };
    match lookup(root, &names) {
        Leaf::Regular | Leaf::Missing => None,
        Leaf::Symlink(target) => {
            (!link_is_safe(root, root_path, &names, &target)).then(|| symlink_patch(rel, &target))
        }
        Leaf::Other | Leaf::Blocked => Some(SKIPPED_NOT_A_FILE.to_string()),
    }
}

/// `prefix` and `path` as git's header lines print them: plain, or in
/// double quotes with C escapes when `core.quotePath` (which `CONFIG`
/// pins on) would quote it.
fn git_quoted(prefix: &str, path: &str) -> String {
    let needs = |b: u8| !(0x20..0x7f).contains(&b) || b == b'"' || b == b'\\';
    if !path.bytes().any(needs) {
        return format!("{prefix}{path}");
    }
    let mut out = String::from("\"");
    out.push_str(prefix);
    for b in path.bytes() {
        match b {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            b'\r' => out.push_str("\\r"),
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b if needs(b) => out.push_str(&format!("\\{b:03o}")),
            b => out.push(char::from(b)),
        }
    }
    out.push('"');
    out
}

/// The patch `git diff --no-index /dev/null <link>` prints for a new
/// symlink: its mode and its target text as the one added line, without
/// the `index` line (the target's blob id, which would take a SHA-1).
pub fn symlink_patch(path: &str, target: &[u8]) -> String {
    let target = String::from_utf8_lossy(target);
    let lines: Vec<&str> = target.split_inclusive('\n').collect();
    let mut out = format!(
        "diff --git {} {}\nnew file mode 120000\n--- /dev/null\n+++ {}\n",
        git_quoted("a/", path),
        git_quoted("b/", path),
        git_quoted("b/", path)
    );
    out.push_str(&match lines.len() {
        1 => "@@ -0,0 +1 @@\n".to_string(),
        n => format!("@@ -0,0 +1,{n} @@\n"),
    });
    for line in &lines {
        out.push('+');
        out.push_str(line);
    }
    if !target.ends_with('\n') {
        out.push_str("\n\\ No newline at end of file\n");
    }
    out
}

/// One changed or untracked path as the fingerprint sees it: `(size,
/// mtime, mtime_nsec)` from `symlink_metadata`, or `None` when the `stat`
/// failed for any reason — the path vanished between the listing and the
/// `stat` (a deleted tracked file always does), or a parent is not
/// searchable, or a loop: every error hashes as `missing`, so the result
/// is still deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStat {
    pub path: String,
    pub stat: Option<(u64, i64, i64)>,
}

/// The most paths `version` stats (#18): a worktree with a large
/// un-ignored tree (a build output, a vendored checkout) would otherwise
/// cost one `stat` per path on every poll of every open review page.
pub const VERSION_PATH_CAP: usize = 2000;

/// At most `VERSION_PATH_CAP` paths, stat'ed under `workspace` and
/// returned sorted, and how many paths there were in all. The cap fills
/// with the `tracked` changes first (sorted), then the `untracked` paths not
/// among them (sorted), so a large untracked tree cannot push an edit to a
/// tracked file out of the fingerprint. The kept entries are sorted by path
/// whichever side they came from, so a `git add` that moves a path from
/// untracked to tracked, and changes no bytes, still changes nothing. A
/// path git named is held to `check_path` first, as every other read's is
/// (#19): one that fails is neither stat'ed nor counted. That includes a
/// valid Linux file name with a `\` in it (`check_path` refuses
/// backslashes): such a file is left out of the fingerprint, so an edit to
/// it alone does not refresh the review page.
pub fn stat_paths(
    workspace: &Path,
    tracked: BTreeSet<String>,
    untracked: BTreeSet<String>,
) -> (Vec<PathStat>, usize) {
    let untracked: Vec<String> = untracked
        .into_iter()
        .filter(|p| !tracked.contains(p))
        .collect();
    let paths: Vec<String> = tracked
        .into_iter()
        .chain(untracked)
        .filter(|p| check_path(p).is_ok())
        .collect();
    let total = paths.len();
    let mut entries: Vec<PathStat> = paths
        .into_iter()
        .take(VERSION_PATH_CAP)
        .map(|path| {
            let stat = std::fs::symlink_metadata(workspace.join(&path))
                .ok()
                .map(|m| (m.len(), m.mtime(), m.mtime_nsec()));
            PathStat { path, stat }
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    (entries, total)
}

/// `hex(sha256(head \0 merge_base \0 (path \0 size \0 mtime.nsec \0 |
/// path \0 missing \0)* [\0 capped \0 total \0]))` (Spec D §2.3). Entries
/// are hashed in the order given; callers pass them sorted. When `total`
/// is more than the entries (`VERSION_PATH_CAP`), a marker and the count
/// follow, so the fingerprint still changes when a path past the cap
/// comes or goes; it starts with the NUL no entry's (non-empty) path can.
pub fn fingerprint_of(head: &str, merge_base: &str, entries: &[PathStat], total: usize) -> String {
    let mut h = Sha256::new();
    h.update(head.as_bytes());
    h.update([0]);
    h.update(merge_base.as_bytes());
    h.update([0]);
    for e in entries {
        h.update(e.path.as_bytes());
        h.update([0]);
        match e.stat {
            Some((size, mtime, nsec)) => {
                h.update(size.to_string().as_bytes());
                h.update([0]);
                h.update(format!("{mtime}.{nsec:09}").as_bytes());
            }
            None => h.update(b"missing"),
        }
        h.update([0]);
    }
    if total > entries.len() {
        h.update(b"\0capped\0");
        h.update(total.to_string().as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

fn io_error(path: &Path, e: std::io::Error) -> WorkspaceError {
    WorkspaceError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// `symlink_metadata`: never follows the final component.
fn meta_of(path: &Path) -> Result<std::fs::Metadata, WorkspaceError> {
    std::fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            WorkspaceError::NoSuchPath
        } else {
            io_error(path, e)
        }
    })
}

/// Defence in depth against a symlinked ancestor: the canonical path must
/// still sit under the canonical worktree.
fn confine(workspace: &Path, full: &Path) -> Result<(), WorkspaceError> {
    let root = workspace
        .canonicalize()
        .map_err(|e| io_error(workspace, e))?;
    let real = full.canonicalize().map_err(|e| io_error(full, e))?;
    if real.starts_with(&root) {
        Ok(())
    } else {
        Err(WorkspaceError::InvalidPath("escapes the worktree".into()))
    }
}

/// The `FILTER_KEYS` probe, before the regex: the listing scripts
/// (`listing_start`) run exactly this, first on every git-running read.
const PROBE: &[&str] = &[
    "config",
    "--local",
    "--includes",
    "--name-only",
    "--get-regexp",
];

/// Whether `key` (as `config --name-only` prints it, lowercased) is one
/// `FILTER_KEYS` matches; the regex, spelled out without a regex engine.
fn is_filter_key(key: &str) -> bool {
    let between = |prefix: &str, suffixes: &[&str]| {
        key.strip_prefix(prefix)
            .is_some_and(|rest| suffixes.iter().any(|s| rest.ends_with(s)))
    };
    between("filter.", &[".clean", ".smudge", ".process"])
        || between("remote.", &[".promisor"])
        || key == "extensions.worktreeconfig"
        || key == "extensions.partialclone"
}

/// The first line of a probe's output that is a `FILTER_KEYS` key: a
/// warning git printed first (the version script folds stderr in) is
/// skipped, never taken for the key.
fn filter_key(probe: &str) -> Option<String> {
    probe
        .lines()
        .map(str::trim)
        .find(|k| is_filter_key(k))
        .map(str::to_string)
}

/// The start of `VERSION_SCRIPT` and `DIFF_LIST_SCRIPT`: `$1` is the
/// base ref, `$2` `FILTER_KEYS`, and the rest git with its options, so
/// nothing the caller passes is ever script text. The filter probe runs
/// first, before any call that reads an object: a match prints the key and exits
/// `VERSION_FILTER`; an exit 1 that printed anything (stderr is folded in)
/// is a failure, as `is_gits_answer` would judge it, and an exit 126 or 127
/// is `GIT_CANNOT_RUN`. Then `merge-base`,
/// printed with a NUL after it. Each git call fails with its own code
/// (`version_stage`).
macro_rules! listing_start {
    () => {
        r#"base=$1 keys=$2
shift 2
found=$("$@" config --local --includes --name-only --get-regexp "$keys" 2>&1)
case $? in
0) printf '%s' "$found"; exit 80 ;;
1) if [ -n "$found" ]; then printf '%s\n' "$found" >&2; exit 81; fi ;;
126|127) printf '%s\n' "$found" >&2; exit 85 ;;
*) printf '%s\n' "$found" >&2; exit 81 ;;
esac
mb=$("$@" merge-base "$base" HEAD) || exit 83
printf '%s\000' "$mb"
"#
    };
}

/// The end of both: a NUL after the listing, then `status` (`STATUS`).
macro_rules! listing_end {
    () => {
        r#"printf '\000'
"$@" status --porcelain=v2 -z --branch --no-ahead-behind --untracked-files=all --ignore-submodules=dirty || exit 82
"#
    };
}

/// `version`'s four git calls as one sandbox start (#174;
/// `workspace::sandboxed_git_script`): `listing_start`, the name-only diff
/// against the merge-base (`DIFF_FLAGS`, `VERSION_LISTING`) and
/// `listing_end`. On success stdout is `<merge-base> \0 <name-only -z> \0
/// <status -z>` (`parse_listing`). The `-c` pairs are `CONFIG`'s, passed in
/// `"$@"`; a unit test holds the flag lists to the constants.
const VERSION_SCRIPT: &str = concat!(
    listing_start!(),
    r#""$@" diff --no-ext-diff --no-textconv --no-color --submodule=short --ignore-submodules=dirty --name-only -z "$mb" || exit 84
"#,
    listing_end!()
);

/// `diff`'s listing in one sandbox start (#186): `VERSION_SCRIPT` with the
/// name-status diff (`DIFF_LISTING`) for the name-only one, so the same
/// stages, exits and output layout.
const DIFF_LIST_SCRIPT: &str = concat!(
    listing_start!(),
    r#""$@" diff --no-ext-diff --no-textconv --no-color --submodule=short --ignore-submodules=dirty --name-status -z --find-renames "$mb" || exit 84
"#,
    listing_end!()
);

/// The listing diff's own flags after `DIFF_FLAGS`, in each script.
const VERSION_LISTING: &[&str] = &["--name-only", "-z"];
const DIFF_LISTING: &[&str] = &["--name-status", "-z", "--find-renames"];

/// `diff`'s patches in one sandbox start (#186): the combined `-U3` diff of
/// the tracked files, then one `--no-index` diff per untracked file, each
/// section's stdout followed by the frame `\0<exit code>\n`
/// (`SectionReader`). No git patch line starts with a NUL (content lines
/// carry their ` `, `+`, `-` or `\` first, and a path cannot hold one), so
/// a frame is told from a section by where it starts. The arguments are
/// `$1` the merge-base; `$2` `FILTER_KEYS`; `$3` `none` (no combined
/// diff), `all` or `paths` (the combined diff restricted to literal
/// pathspecs); `$4` the number of pathspecs and `$5` of untracked paths;
/// then the pathspecs, the
/// untracked paths, and git with its options. POSIX `sh` has one list, so
/// `combined` builds its command line after the inputs from the words of
/// that list and drops the inputs (`for a do` walks the list as it was
/// when the loop began), and `pick` reads one path by its position;
/// functions get their own `"$@"`, so the list itself never changes. The
/// filter probe runs again first, in this start, so the window between
/// the probe and the reads it guards is no wider than one start: a match
/// exits `VERSION_FILTER`, a failed probe 81 (`GIT_CANNOT_RUN` when it
/// exited 126 or 127), the probe's output on
/// stderr (stdout is the sections). The
/// script's own exit is `printf`'s: the codes are in the frames.
const DIFF_PATCH_SCRIPT: &str = r#"mb=$1 keys=$2 mode=$3 nt=$4 nu=$5
shift 5
skip=$((nt + nu))
probe() {
shift "$skip"
"$@" config --local --includes --name-only --get-regexp "$keys" 2>&1
}
found=$(probe "$@")
case $? in
0) printf '%s\n' "$found" >&2; exit 80 ;;
1) if [ -n "$found" ]; then printf '%s\n' "$found" >&2; exit 81; fi ;;
126|127) printf '%s\n' "$found" >&2; exit 85 ;;
*) printf '%s\n' "$found" >&2; exit 81 ;;
esac
combined() {
k=$# i=0
for a do
i=$((i + 1))
if [ "$i" -gt "$skip" ]; then set -- "$@" "$a"; fi
done
if [ "$mode" = paths ]; then set -- "$@" --literal-pathspecs; fi
set -- "$@" diff --no-ext-diff --no-textconv --no-color --submodule=short --ignore-submodules=dirty -U3 --find-renames "$mb"
if [ "$mode" = paths ]; then
set -- "$@" --
i=0
for a do
i=$((i + 1))
if [ "$i" -gt "$nt" ]; then break; fi
set -- "$@" "$a"
done
fi
shift "$k"
"$@"
}
pick() { shift "$1"; p=$1; }
untracked() {
shift "$skip"
"$@" diff --no-ext-diff --no-textconv --no-color --submodule=short --ignore-submodules=dirty --no-index -U3 -- /dev/null "$p"
}
if [ "$mode" != none ]; then
combined "$@"
printf '\000%d\n' "$?"
fi
j=0
while [ "$j" -lt "$nu" ]; do
j=$((j + 1))
pick $((nt + j)) "$@"
untracked "$@"
printf '\000%d\n' "$?"
done
"#;

/// `paths` cut in order into runs of at most `budget` bytes (a byte more
/// per path, its NUL in the argument block), the first run sharing its
/// budget with `used` bytes of other arguments (the combined diff's
/// pathspecs). The first run is empty when not even the first path fits
/// beside those; a path over the budget alone is a run of its own. The
/// pathspecs themselves are never split: past the budget they go in one
/// start, as they did before #186.
fn batches<'a, 'p>(paths: &'a [&'p str], budget: usize, used: usize) -> Vec<&'a [&'p str]> {
    let mut out = Vec::new();
    let (mut start, mut bytes) = (0, used);
    for (i, p) in paths.iter().enumerate() {
        if bytes + p.len() + 1 > budget && (i > start || bytes > 0) {
            out.push(&paths[start..i]);
            (start, bytes) = (i, 0);
        }
        bytes += p.len() + 1;
    }
    if start < paths.len() {
        out.push(&paths[start..]);
    }
    out
}

/// The most bytes of paths one `DIFF_PATCH_SCRIPT` start takes as
/// arguments; untracked paths past it go to another start, so a list of
/// long paths stays under the kernel's argument limit.
const PATCH_ARGS_BUDGET: usize = 256 * 1024;

/// `VERSION_SCRIPT`'s exit when the probe found a key (on stdout).
const VERSION_FILTER: i32 = 80;

/// Every script's exit when its first git call, the filter probe, exited
/// 126 or 127: the shell could not execute git, or git could not load
/// what it needs (#214). A failed probe otherwise exits 81.
const GIT_CANNOT_RUN: i32 = 85;

/// The git subcommand behind one of `VERSION_SCRIPT`'s failure exits.
fn version_stage(code: i32) -> Option<&'static str> {
    match code {
        81 | GIT_CANNOT_RUN => Some("config"),
        82 => Some("status"),
        83 => Some("merge-base"),
        84 => Some("diff"),
        _ => None,
    }
}

/// A listing script's stdout (`VERSION_SCRIPT`, `DIFF_LIST_SCRIPT`): the
/// merge-base, the listing diff's records and `status`'s output. The
/// listing's records are non-empty and NUL-terminated, so the first empty
/// record ends them; `None` when that structure is not there.
fn parse_listing(out: &str) -> Option<(String, Vec<&str>, &str)> {
    let (merge_base, mut rest) = out.split_once('\0')?;
    let mut records = Vec::new();
    loop {
        let (record, after) = rest.split_once('\0')?;
        rest = after;
        if record.is_empty() {
            break;
        }
        records.push(record);
    }
    Some((merge_base.trim().to_string(), records, rest))
}

/// A listing script that exited 0 without its output's layout.
fn unexpected_listing(id: &str) -> WorkspaceError {
    WorkspaceError::Tool {
        id: id.to_string(),
        subcommand: "sh".into(),
        args: Vec::new(),
        stderr: "unexpected output from the listing script".into(),
    }
}

/// `VERSION_SCRIPT`'s stdout: the merge-base, the name-only paths and the
/// parsed status.
pub fn parse_version(out: &str) -> Option<(String, BTreeSet<String>, Status)> {
    let (merge_base, names, status) = parse_listing(out)?;
    Some((
        merge_base,
        names.into_iter().map(str::to_string).collect(),
        parse_status(status),
    ))
}

/// What one `DIFF_PATCH_SCRIPT` start prints, read as it streams: the
/// combined diff's section into a `PatchSplitter`, each untracked file's
/// into a buffer of its own (at most `SECTION_CAP` bytes, like the
/// splitter's), and each frame's exit code. A frame is a NUL at the start
/// of a line, the code, and a newline. Output that breaks that shape (a
/// frame that is not one, a section past the last expected) leaves the
/// reader `broken`.
pub struct SectionReader {
    combined: Option<PatchSplitter>,
    untracked: Vec<Vec<u8>>,
    wanted: usize,
    codes: Vec<i32>,
    at_line_start: bool,
    frame: Option<Vec<u8>>,
    broken: bool,
}

/// What a `SectionReader` read: the combined diff split per file (`None`
/// when there was none), the untracked files' patches in order, and the
/// codes, one per section.
pub struct Sections {
    pub combined: Option<BTreeMap<String, String>>,
    pub untracked: Vec<String>,
    pub codes: Vec<i32>,
}

impl SectionReader {
    pub fn new(combined: bool, untracked: usize) -> Self {
        Self {
            combined: combined.then(|| PatchSplitter::new(SECTION_CAP)),
            untracked: Vec::new(),
            wanted: untracked,
            codes: Vec::new(),
            at_line_start: true,
            frame: None,
            broken: false,
        }
    }

    fn expected(&self) -> usize {
        usize::from(self.combined.is_some()) + self.wanted
    }

    pub fn feed(&mut self, mut chunk: &[u8]) {
        while !chunk.is_empty() && !self.broken {
            if let Some(frame) = &mut self.frame {
                let end = chunk.iter().position(|b| *b == b'\n');
                frame.extend_from_slice(&chunk[..end.unwrap_or(chunk.len())]);
                // a minus sign and the ten digits of an `i32` fit
                if frame.len() > 12 {
                    self.broken = true;
                    return;
                }
                let Some(end) = end else {
                    return;
                };
                chunk = &chunk[end + 1..];
                let code = std::str::from_utf8(frame)
                    .ok()
                    .and_then(|c| c.parse::<i32>().ok());
                self.frame = None;
                self.at_line_start = true;
                match code {
                    Some(code) if self.codes.len() < self.expected() => self.codes.push(code),
                    _ => self.broken = true,
                }
                continue;
            }
            if self.at_line_start && chunk[0] == 0 {
                self.frame = Some(Vec::new());
                chunk = &chunk[1..];
                continue;
            }
            let end = chunk
                .iter()
                .position(|b| *b == b'\n')
                .map_or(chunk.len(), |i| i + 1);
            let (line, rest) = chunk.split_at(end);
            chunk = rest;
            self.at_line_start = line.last() == Some(&b'\n');
            self.content(line);
        }
    }

    fn content(&mut self, bytes: &[u8]) {
        let index = self.codes.len();
        let index = match &mut self.combined {
            Some(splitter) if index == 0 => {
                splitter.feed(bytes);
                return;
            }
            Some(_) => index - 1,
            None => index,
        };
        if index >= self.wanted {
            self.broken = true;
            return;
        }
        // the sections before it that printed nothing
        while self.untracked.len() <= index {
            self.untracked.push(Vec::new());
        }
        let section = &mut self.untracked[index];
        let room = SECTION_CAP.saturating_sub(section.len());
        section.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// The sections, or `None` when the output was not one section and
    /// one frame for each expected.
    pub fn finish(mut self) -> Option<Sections> {
        let expected = self.expected();
        if self.broken || self.frame.is_some() || self.codes.len() != expected {
            return None;
        }
        // the last sections, when they printed nothing
        while self.untracked.len() < self.wanted {
            self.untracked.push(Vec::new());
        }
        Some(Sections {
            combined: self.combined.map(PatchSplitter::finish),
            untracked: self
                .untracked
                .into_iter()
                .map(|v| String::from_utf8_lossy(&v).into_owned())
                .collect(),
            codes: self.codes,
        })
    }
}

impl WorkspaceReader for Runtime {
    /// Two sandbox starts (#108: each is ~55 ms idle, far more on a loaded
    /// host; #186): the listing (`DIFF_LIST_SCRIPT`: the filter probe,
    /// `merge-base`, `--name-status`, `status`), then the patches
    /// (`DIFF_PATCH_SCRIPT`: one combined `diff -U3` split per file, and one
    /// `--no-index` diff per untracked file) of the files the cap keeps. A
    /// file the combined diff does not name as expected is diffed on its
    /// own, one more start each.
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        let op = ReadOp::diff();
        let id = agent.to_string();
        let (paths, crew) = self.sandboxed_workspace_of(agent)?;
        let listing = self.run_listing(
            &id,
            &crew,
            &paths,
            op,
            (DIFF_LIST_SCRIPT, DIFF_LISTING),
            base_ref,
        )?;
        let (merge_base, records, status) =
            parse_listing(&listing).ok_or_else(|| unexpected_listing(&id))?;
        let status = parse_status(status);
        let mut name_status = records.join("\0");
        name_status.push('\0');
        let mut files = parse_name_status(&name_status);
        let head = status.head.unwrap_or_default();
        let all_tracked = files.len();
        let dirty = status.changed;
        let untracked = status.untracked;
        for path in &untracked {
            files.push(FileDiff {
                path: path.clone(),
                old_path: None,
                status: FileStatus::Added,
                uncommitted: true,
                binary: false,
                patch: String::new(),
                truncated: false,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let truncated = files.len() > WORKSPACE_FILE_COUNT_LIMIT;
        files.truncate(WORKSPACE_FILE_COUNT_LIMIT);

        // The tracked files that survived the cap, in one combined diff:
        // every one when none was cut, else those named as literal
        // pathspecs; and the untracked ones that survived it.
        let tracked: Vec<&FileDiff> = files
            .iter()
            .filter(|f| !untracked.contains(&f.path))
            .collect();
        let mode = if tracked.is_empty() {
            "none"
        } else if tracked.len() < all_tracked {
            "paths"
        } else {
            "all"
        };
        let mut pathspecs: Vec<&str> = Vec::new();
        if mode == "paths" {
            for f in &tracked {
                if let Some(old) = &f.old_path {
                    pathspecs.push(old);
                }
                pathspecs.push(&f.path);
            }
        }
        // an untracked FIFO (or a link to one) never reaches `--no-index`
        // (#213); the worktree is walked from its own directory, and one
        // that cannot be opened fails the read rather than go unchecked
        let root_path = paths
            .workspace
            .canonicalize()
            .map_err(|e| io_error(&paths.workspace, e))?;
        let root = openat(
            CWD,
            &root_path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| io_error(&root_path, e.into()))?;
        let mut standing_in: BTreeMap<String, String> = BTreeMap::new();
        let kept_untracked: Vec<&str> = files
            .iter()
            .filter(|f| untracked.contains(&f.path))
            .map(|f| f.path.as_str())
            .filter(|p| match untracked_patch(&root, &root_path, p) {
                Some(patch) => {
                    standing_in.insert((*p).to_string(), patch);
                    false
                }
                None => true,
            })
            .collect();
        let (mut sections, untracked_patches) = if mode == "none" && kept_untracked.is_empty() {
            (BTreeMap::new(), Vec::new())
        } else {
            self.run_patches(
                &id,
                &crew,
                &paths,
                op,
                &merge_base,
                mode,
                &pathspecs,
                &kept_untracked,
            )?
        };
        let mut untracked_patches = untracked_patches.into_iter();

        for f in &mut files {
            f.uncommitted = f.uncommitted
                || dirty.contains(&f.path)
                || f.old_path.as_deref().is_some_and(|o| dirty.contains(o));
            let raw = if let Some(patch) = standing_in.remove(&f.path) {
                patch
            } else if untracked.contains(&f.path) {
                // in the order `kept_untracked` gave them
                untracked_patches.next().unwrap_or_default()
            } else if let Some(section) = patch_header(f).and_then(|h| sections.remove(&h)) {
                section
            } else {
                // a path the combined patch does not name as expected
                // (quoted, or prefixes the clone's config changed): this
                // file on its own, as before
                let mut args: Vec<&str> = vec!["diff"];
                args.extend(DIFF_FLAGS);
                args.extend(["-U3", "--find-renames", merge_base.as_str(), "--"]);
                if let Some(old) = &f.old_path {
                    args.push(old);
                }
                args.push(&f.path);
                self.inspect_git(&id, &crew, &paths, op, &args)?
            };
            let (patch, binary, cut) = shape_patch(raw);
            f.patch = patch;
            f.binary = binary;
            f.truncated = cut;
        }
        Ok(WorkspaceDiff {
            base_ref: base_ref.to_string(),
            merge_base,
            head,
            files,
            truncated,
        })
    }

    fn read_file(&self, agent: &AgentId, path: &str) -> Result<Vec<u8>, WorkspaceError> {
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        let ws = self.workspace_of(agent)?.0.workspace;
        let full = ws.join(path);
        let meta = meta_of(&full)?;
        if !meta.is_file() {
            return Err(WorkspaceError::NotAFile);
        }
        confine(&ws, &full)?;
        // The checks above ran on a path; the agent owns the worktree and
        // can replace that file with a symlink to /etc/shadow between them
        // and the open. So open once and check the *handle*: it must be a
        // regular file and the very inode the `symlink_metadata` above
        // described, and the size check and the bytes both come from it.
        // (An `O_NOFOLLOW` open would say the same in one step, but that
        // needs a `libc` dependency this workspace does not have.)
        let file = std::fs::File::open(&full).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                WorkspaceError::NoSuchPath
            } else {
                io_error(&full, e)
            }
        })?;
        let opened = file.metadata().map_err(|e| io_error(&full, e))?;
        if !opened.is_file() || (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
            return Err(WorkspaceError::NotAFile);
        }
        let too_large = WorkspaceError::TooLarge {
            limit: WORKSPACE_FILE_LIMIT,
        };
        if opened.len() > WORKSPACE_FILE_LIMIT {
            return Err(too_large);
        }
        // The file can still grow between that `fstat` and the read, so the
        // read itself is bounded rather than trusting the size.
        let mut buf = Vec::new();
        file.take(WORKSPACE_FILE_LIMIT + 1)
            .read_to_end(&mut buf)
            .map_err(|e| io_error(&full, e))?;
        if buf.len() as u64 > WORKSPACE_FILE_LIMIT {
            return Err(too_large);
        }
        Ok(buf)
    }

    fn list_dir(&self, agent: &AgentId, path: &str) -> Result<WorkspaceTree, WorkspaceError> {
        check_path(path).map_err(WorkspaceError::InvalidPath)?;
        let ws = self.workspace_of(agent)?.0.workspace;
        let full = if path.is_empty() {
            ws.clone()
        } else {
            ws.join(path)
        };
        if !meta_of(&full)?.is_dir() {
            return Err(WorkspaceError::NotADirectory);
        }
        confine(&ws, &full)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&full).map_err(|e| io_error(&full, e))? {
            let entry = entry.map_err(|e| io_error(&full, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            // `DirEntry::metadata` does not follow symlinks.
            let m = entry.metadata().map_err(|e| io_error(&entry.path(), e))?;
            let kind = if m.is_file() {
                EntryKind::File
            } else if m.is_dir() {
                EntryKind::Dir
            } else if m.file_type().is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::Other
            };
            entries.push(TreeEntry {
                name,
                kind,
                size: m.is_file().then_some(m.len()),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(WorkspaceTree {
            path: path.to_string(),
            entries,
        })
    }
    /// The filter probe, `merge-base`, one name-only diff against it and
    /// one `status`, in one sandbox start (`VERSION_SCRIPT`, #174).
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        let op = ReadOp::version();
        let id = agent.to_string();
        let (paths, crew) = self.sandboxed_workspace_of(agent)?;
        let out = self.run_listing(
            &id,
            &crew,
            &paths,
            op,
            (VERSION_SCRIPT, VERSION_LISTING),
            base_ref,
        )?;
        let (merge_base, mut paths_seen, status) =
            parse_version(&out).ok_or_else(|| unexpected_listing(&id))?;
        let head = status.head.unwrap_or_default();
        paths_seen.extend(status.changed);
        // `BTreeSet`s: sorted and deduplicated, so the hash order is fixed.
        let (entries, total) = stat_paths(&paths.workspace, paths_seen, status.untracked);
        Ok(WorkspaceVersion {
            fingerprint: fingerprint_of(&head, &merge_base, &entries, total),
            head,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::FileStatus;

    /// #213: what may reach `--no-index`, judged inside the worktree
    /// without following a link out of it.
    #[test]
    fn only_regular_files_and_safe_links_reach_no_index() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("file"), "x").unwrap();
        let root_path = dir.path().join("ws");
        std::fs::create_dir_all(root_path.join("sub")).unwrap();
        let root_path = root_path.canonicalize().unwrap();
        std::fs::write(root_path.join("file"), "x").unwrap();
        std::fs::write(root_path.join("sub/inner"), "x").unwrap();
        let mkfifo = std::process::Command::new("mkfifo")
            .arg(root_path.join("fifo"))
            .status()
            .unwrap();
        assert!(mkfifo.success());
        let link = |target: &Path, name: &str| symlink(target, root_path.join(name)).unwrap();
        link(Path::new("file"), "to-file");
        link(Path::new("sub/../to-file"), "chain");
        link(&root_path.join("sub/inner"), "absolute-inside");
        link(Path::new("nowhere"), "dangling");
        link(Path::new("fifo"), "to-fifo");
        link(Path::new("sub"), "to-dir");
        link(Path::new("../outside/file"), "up-and-out");
        link(&outside.join("file"), "absolute-out");
        link(&outside, "out-dir");
        link(Path::new("loop-b"), "loop-a");
        link(Path::new("loop-a"), "loop-b");
        link(Path::new("to-dir/inner"), "through-a-dir-link");
        let root = openat(
            CWD,
            &root_path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let patch = |rel: &str| untracked_patch(&root, &root_path, rel);

        for keep in [
            "file",
            "sub/inner",
            "to-file",
            "chain",
            "absolute-inside",
            "dangling",
            "vanished",
            "gone/too",
        ] {
            assert_eq!(patch(keep), None, "{keep}");
        }
        for skipped in [
            "fifo",
            "sub",
            "sub/",
            // `..` is refused, never walked
            "sub/../file",
            // a directory link on the way is never followed
            "out-dir/file",
            "to-dir/inner",
        ] {
            assert_eq!(
                patch(skipped).as_deref(),
                Some(SKIPPED_NOT_A_FILE),
                "{skipped}"
            );
        }
        // the link's own text, as git would print it, the target untouched
        for (name, target) in [
            ("to-fifo", "fifo".to_string()),
            ("to-dir", "sub".to_string()),
            ("loop-a", "loop-b".to_string()),
            ("up-and-out", "../outside/file".to_string()),
            ("absolute-out", outside.join("file").display().to_string()),
            ("out-dir", outside.display().to_string()),
            ("through-a-dir-link", "to-dir/inner".to_string()),
        ] {
            assert_eq!(
                patch(name),
                Some(symlink_patch(name, target.as_bytes())),
                "{name}"
            );
        }
    }

    #[test]
    fn a_symlink_patch_is_gits_and_quotes_as_git_does() {
        assert_eq!(
            symlink_patch("l", b"/home/x/.ssh/id_rsa"),
            "diff --git a/l b/l\nnew file mode 120000\n--- /dev/null\n+++ b/l\n\
             @@ -0,0 +1 @@\n+/home/x/.ssh/id_rsa\n\\ No newline at end of file\n"
        );
        assert_eq!(
            symlink_patch("a\tb\u{e9}\"", b"t\nu"),
            "diff --git \"a/a\\tb\\303\\251\\\"\" \"b/a\\tb\\303\\251\\\"\"\nnew file mode 120000\n\
             --- /dev/null\n+++ \"b/a\\tb\\303\\251\\\"\"\n@@ -0,0 +1,2 @@\n+t\n+u\n\
             \\ No newline at end of file\n"
        );
    }

    #[test]
    fn name_status_z_parses_statuses_and_rename_pairs() {
        let z = "M\0README\0R100\0LICENSE\0COPYING\0A\0src/lib.rs\0D\0gone\0T\0link\0C75\0a\0b\0";
        let files = parse_name_status(z);
        let got: Vec<(&str, Option<&str>, FileStatus)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.old_path.as_deref(), f.status))
            .collect();
        assert_eq!(
            got,
            vec![
                ("README", None, FileStatus::Modified),
                ("COPYING", Some("LICENSE"), FileStatus::Renamed),
                ("src/lib.rs", None, FileStatus::Added),
                ("gone", None, FileStatus::Deleted),
                ("link", None, FileStatus::Typechange),
                ("b", Some("a"), FileStatus::Copied),
            ]
        );
        assert!(
            files
                .iter()
                .all(|f| !f.uncommitted && !f.binary && f.patch.is_empty())
        );
        assert!(parse_name_status("").is_empty());
        assert_eq!(
            parse_name_status("R100\0only-old\0").len(),
            0,
            "a torn pair is dropped"
        );
    }

    #[test]
    fn status_v2_gives_head_changed_and_untracked_paths() {
        let z = "# branch.oid 0123abcd\0# branch.head main\0\
                 1 .M N... 100644 100644 100644 aaa aaa a file with spaces\0\
                 2 R. N... 100644 100644 100644 bbb bbb R100 new name\0old name\0\
                 u UU N... 100644 100644 100644 100644 c1 c2 c3 conflicted\0\
                 ? notes/todo.txt\0";
        let s = parse_status(z);
        assert_eq!(s.head.as_deref(), Some("0123abcd"));
        assert_eq!(
            s.changed.iter().map(String::as_str).collect::<Vec<_>>(),
            ["a file with spaces", "conflicted", "new name", "old name"]
        );
        assert_eq!(
            s.untracked.iter().map(String::as_str).collect::<Vec<_>>(),
            ["notes/todo.txt"]
        );
        assert_eq!(parse_status("# branch.oid (initial)\0").head, None);
        assert_eq!(parse_status(""), Status::default());
    }

    #[test]
    fn a_combined_patch_splits_per_header_and_keeps_a_typechange_whole() {
        let a = "diff --git a/a b/a\nindex 1..2 100644\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-diff --git x\n+y\n";
        let t = "diff --git a/l b/l\ndeleted file mode 100644\n--- a/l\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n\
                 diff --git a/l b/l\nnew file mode 120000\n--- /dev/null\n+++ b/l\n@@ -0,0 +1 @@\n+a\n\\ No newline at end of file\n";
        let r = "diff --git a/old b/new\nsimilarity index 100%\nrename from old\nrename to new\n";
        let sections = split_patch(&format!("{a}{t}{r}"));
        assert_eq!(sections.len(), 3);
        assert_eq!(sections["diff --git a/a b/a"], a);
        assert_eq!(sections["diff --git a/l b/l"], t);
        assert_eq!(sections["diff --git a/old b/new"], r);
        assert!(split_patch("").is_empty());
    }

    /// The splitter reads a stream: any chunking gives the same sections,
    /// a header split across chunks included.
    #[test]
    fn the_splitter_is_indifferent_to_chunk_boundaries_and_caps_each_section() {
        let raw = "diff --git a/a b/a\n@@ -1 +1 @@\n-x\n+y\n\
                   diff --git a/l b/l\ndeleted file mode 100644\n-x\n\
                   diff --git a/l b/l\nnew file mode 120000\n+a\n\\ No newline at end of file\n\
                   diff --git a/b b/b\n+tail without newline";
        let whole = split_patch(raw);
        assert_eq!(whole.len(), 3);
        assert!(whole["diff --git a/b b/b"].ends_with("tail without newline"));
        for size in [1, 2, 5, 11, 12, 64] {
            let mut s = PatchSplitter::new(usize::MAX);
            for chunk in raw.as_bytes().chunks(size) {
                s.feed(chunk);
            }
            assert_eq!(s.finish(), whole, "chunks of {size}");
        }
        let mut capped = PatchSplitter::new(20);
        capped.feed(raw.as_bytes());
        let capped = capped.finish();
        assert!(capped.values().all(|v| v.len() <= 20), "{capped:?}");
        assert_eq!(capped["diff --git a/a b/a"], "diff --git a/a b/a\n@");
    }

    /// Two files that print the same header are left to diffs of their own.
    #[test]
    fn a_header_met_again_after_another_is_dropped() {
        let raw = "diff --git a/p b/q b/r\n+one\n\
                   diff --git a/s b/s\n+s\n\
                   diff --git a/p b/q b/r\n+two\n";
        let sections = split_patch(raw);
        assert_eq!(
            sections.keys().map(String::as_str).collect::<Vec<_>>(),
            ["diff --git a/s b/s"]
        );
    }

    /// Past the cap only the first `WORKSPACE_PATCH_LIMIT` bytes matter:
    /// a capped section shapes exactly as the whole one does.
    #[test]
    fn a_capped_section_shapes_as_the_whole_patch() {
        let line = format!("+{}é\n", "y".repeat(97));
        let whole = format!(
            "diff --git a/x b/x\n{}",
            line.repeat(WORKSPACE_PATCH_LIMIT / line.len() * 3)
        );
        let mut s = PatchSplitter::new(SECTION_CAP);
        for chunk in whole.as_bytes().chunks(7919) {
            s.feed(chunk);
        }
        let capped = s.finish().remove("diff --git a/x b/x").unwrap();
        assert!(capped.len() <= SECTION_CAP + 2, "{}", capped.len());
        assert_eq!(shape_patch(capped), shape_patch(whole));
    }

    #[test]
    fn a_header_is_expected_only_for_paths_git_does_not_quote() {
        let file = |path: &str, old: Option<&str>| FileDiff {
            path: path.into(),
            old_path: old.map(str::to_string),
            status: FileStatus::Modified,
            uncommitted: false,
            binary: false,
            patch: String::new(),
            truncated: false,
        };
        assert_eq!(
            patch_header(&file("a b", None)).as_deref(),
            Some("diff --git a/a b b/a b")
        );
        assert_eq!(
            patch_header(&file("new", Some("old"))).as_deref(),
            Some("diff --git a/old b/new")
        );
        assert_eq!(patch_header(&file("é", None)), None);
        assert_eq!(patch_header(&file("q\"", None)), None);
        assert_eq!(patch_header(&file("new", Some("t\tab"))), None);
        assert_eq!(patch_header(&file("q b/r", Some("p"))), None, "ambiguous");
        assert_eq!(patch_header(&file("r", Some("p b/q"))), None, "ambiguous");
    }

    #[test]
    fn a_patch_is_marked_binary_or_cut_at_a_line_boundary() {
        assert_eq!(
            shape_patch("diff --git a/x b/x\nBinary files a/x and b/x differ\n".into()),
            (String::new(), true, false)
        );
        let small = "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n".to_string();
        assert_eq!(shape_patch(small.clone()), (small, false, false));
        let line = format!("+{}\n", "y".repeat(98));
        let big = line.repeat(WORKSPACE_PATCH_LIMIT / 100 + 10);
        let (cut, binary, truncated) = shape_patch(big);
        assert!(!binary && truncated);
        assert!(cut.len() <= WORKSPACE_PATCH_LIMIT);
        assert!(cut.ends_with('\n'), "cut at a line boundary");
        assert_eq!(cut.len() % 100, 0);
    }

    /// `WORKSPACE_PATCH_LIMIT` (262144) falls inside the second byte of a
    /// 2-byte UTF-8 character with this fixture's line length (102 bytes:
    /// `+`, 50 × `é` at 2 bytes each, `\n`) — a byte-index slice would panic
    /// here; the cut must land on the `\n` byte boundary instead.
    #[test]
    fn a_patch_is_cut_on_a_byte_boundary_even_through_a_multi_byte_character() {
        let line = format!("+{}\n", "é".repeat(50));
        let big = line.repeat(WORKSPACE_PATCH_LIMIT / line.len() + 10);
        let (cut, binary, truncated) = shape_patch(big);
        assert!(!binary && truncated);
        assert!(cut.len() <= WORKSPACE_PATCH_LIMIT);
        assert!(cut.ends_with('\n'), "cut at a line boundary");
    }

    /// The scripts spell `STATUS`, `DIFF_FLAGS` and the probe out; they
    /// must not drift.
    #[test]
    fn the_scripts_run_the_same_flags_as_the_constants() {
        let diff = DIFF_FLAGS.join(" ");
        for (script, listing) in [
            (VERSION_SCRIPT, VERSION_LISTING),
            (DIFF_LIST_SCRIPT, DIFF_LISTING),
        ] {
            assert!(script.contains(&format!("\"$@\" {} ||", STATUS.join(" "))));
            assert!(script.contains(&format!(
                "\"$@\" diff {diff} {} \"$mb\" || exit 84",
                listing.join(" ")
            )));
            assert!(script.contains(&format!("\"$@\" {} \"$keys\"", PROBE.join(" "))));
            for code in 81..=84 {
                let stage = version_stage(code).unwrap();
                assert!(script.contains(&format!("exit {code}")), "{stage}");
            }
            assert!(script.contains(&format!("exit {VERSION_FILTER}")));
        }
        assert!(DIFF_PATCH_SCRIPT.contains(&format!(
            "set -- \"$@\" diff {diff} -U3 --find-renames \"$mb\"\n"
        )));
        assert!(DIFF_PATCH_SCRIPT.contains(&format!(
            "\"$@\" diff {diff} --no-index -U3 -- /dev/null \"$p\"\n"
        )));
        assert!(DIFF_PATCH_SCRIPT.contains(&format!("\"$@\" {} \"$keys\"", PROBE.join(" "))));
        for code in [VERSION_FILTER, 81] {
            assert!(
                DIFF_PATCH_SCRIPT.contains(&format!("exit {code}")),
                "{code}"
            );
        }
        // nothing but the positional parameters reaches a command
        for script in [VERSION_SCRIPT, DIFF_LIST_SCRIPT, DIFF_PATCH_SCRIPT] {
            assert!(!script.contains("eval"));
        }
        assert_eq!(version_stage(0), None);
        assert_eq!(version_stage(1), None);
    }

    /// The patch script's frames: a NUL at a line start, the code, a
    /// newline; whatever the chunking. A NUL inside a line is content.
    #[test]
    fn the_section_reader_splits_on_frames_at_line_starts() {
        let combined = "diff --git a/a b/a\n+x\0y\n";
        let raw = format!("{combined}\x000\n\x001\n+u\0\n\x001\n");
        for size in [1, 2, 3, 7, 64] {
            let mut r = SectionReader::new(true, 2);
            for chunk in raw.as_bytes().chunks(size) {
                r.feed(chunk);
            }
            let s = r.finish().unwrap();
            assert_eq!(s.codes, [0, 1, 1], "chunks of {size}");
            assert_eq!(s.combined.unwrap()["diff --git a/a b/a"], combined);
            assert_eq!(s.untracked, ["", "+u\0\n"], "an empty section first");
        }
        let untracked_only = |raw: &str, n| {
            let mut r = SectionReader::new(false, n);
            r.feed(raw.as_bytes());
            r.finish()
        };
        let s = untracked_only("a\n\x001\nb\n\x00128\n", 2).unwrap();
        assert_eq!(
            (s.untracked, s.codes),
            (vec!["a\n".into(), "b\n".into()], vec![1, 128])
        );
        assert!(s.combined.is_none());
        // a frame missing, one too many, one that is not a code, or cut
        assert!(untracked_only("a\n\x001\n", 2).is_none());
        assert!(untracked_only("\x001\n\x001\n", 1).is_none());
        assert!(untracked_only("\x001\nextra\n", 1).is_none());
        assert!(untracked_only("\x00x\n", 1).is_none());
        assert!(untracked_only("\x000", 1).is_none());
        assert!(untracked_only(&format!("\x00{}\n", "1".repeat(20)), 1).is_none());
        assert!(untracked_only("", 0).is_some());
    }

    /// `DIFF_PATCH_SCRIPT` under a plain `/bin/sh`, git a stub that prints
    /// its arguments: the probe goes first and stops the script, and each
    /// call gets exactly its own words, whatever the paths hold.
    #[test]
    fn the_patch_script_probes_first_and_passes_each_call_its_own_words() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("balerix-patch-script-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("git");
        std::fs::write(
            &stub,
            "#!/bin/sh\ncase \"$*\" in *--get-regexp*) printf '%s' \"$PROBE_SAYS\"; exit \"$PROBE_EXIT\" ;; esac\n\
             printf '%s|' \"$@\"; echo\ncase \"$*\" in *--no-index*) exit 1 ;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |probe: (&str, &str), args: &[&str]| {
            let out = std::process::Command::new("/bin/sh")
                .args(["-c", DIFF_PATCH_SCRIPT, "sh"])
                .args(args)
                .args([stub.to_str().unwrap(), "-c", "k=v"])
                .env("PROBE_SAYS", probe.0)
                .env("PROBE_EXIT", probe.1)
                .output()
                .unwrap();
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        };
        let args = [
            "MB",
            FILTER_KEYS,
            "paths",
            "2",
            "2",
            "a b",
            "-c",
            "u 1",
            "-x",
        ];
        let (code, stdout, stderr) = run(("", "1"), &args);
        assert_eq!(code, Some(0), "{stderr}");
        let flags = DIFF_FLAGS.join("|");
        assert_eq!(
            stdout,
            format!(
                "-c|k=v|--literal-pathspecs|diff|{flags}|-U3|--find-renames|MB|--|a b|-c|\n\x000\n\
                 -c|k=v|diff|{flags}|--no-index|-U3|--|/dev/null|u 1|\n\x001\n\
                 -c|k=v|diff|{flags}|--no-index|-U3|--|/dev/null|-x|\n\x001\n"
            )
        );
        let (code, stdout, _) = run(("", "1"), &["MB", FILTER_KEYS, "all", "0", "0"]);
        assert_eq!(code, Some(0));
        assert_eq!(
            stdout,
            format!("-c|k=v|diff|{flags}|-U3|--find-renames|MB|\n\x000\n")
        );
        // a key: exit 80, the key on stderr, no section run
        let (code, stdout, stderr) = run(("filter.x.clean", "0"), &args);
        assert_eq!((code, stdout.as_str()), (Some(VERSION_FILTER), ""));
        assert_eq!(filter_key(&stderr).as_deref(), Some("filter.x.clean"));
        // a failed probe, or a "no" that printed anything: 81
        assert_eq!(run(("fatal: bad", "128"), &args).0, Some(81));
        assert_eq!(run(("warning: odd", "1"), &args).0, Some(81));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn untracked_paths_are_batched_under_the_argument_budget() {
        let paths = ["aaa", "bb", "c", "dddddddd", "e"];
        assert_eq!(
            batches(&paths, 7, 0),
            [&paths[..2], &paths[2..3], &paths[3..4], &paths[4..]]
        );
        assert_eq!(batches(&paths, 100, 0), [&paths[..]]);
        assert!(batches(&[], 7, 0).is_empty());
        // the pathspecs' bytes count against the first start
        assert_eq!(
            batches(&paths, 7, 3),
            [&paths[..1], &paths[1..3], &paths[3..4], &paths[4..]]
        );
        assert_eq!(
            batches(&paths, 7, 4),
            [
                &paths[..0],
                &paths[..2],
                &paths[2..3],
                &paths[3..4],
                &paths[4..]
            ],
            "aaa does not fit beside 4 bytes: the first start is the combined diff alone"
        );
        assert_eq!(batches(&paths, 100, 90), [&paths[..3], &paths[3..]]);
        assert_eq!(
            batches(&paths, 7, 50)[0],
            &paths[..0],
            "pathspecs over the budget"
        );
        assert!(
            batches(&[], 7, 50).is_empty(),
            "the caller adds the combined diff's start"
        );
    }

    #[test]
    fn a_listing_splits_into_merge_base_records_and_status() {
        let out = "abc\0R100\0old\0new\0M\0x\0\0# branch.oid h\0";
        let (mb, records, status) = parse_listing(out).unwrap();
        assert_eq!((mb.as_str(), status), ("abc", "# branch.oid h\0"));
        assert_eq!(records, ["R100", "old", "new", "M", "x"]);
    }

    /// #19: a git-supplied path is held to `check_path` before the `stat`,
    /// like every other read: one that would leave the worktree, or reach
    /// into `.git`, is neither stat'ed nor counted.
    #[test]
    fn version_drops_paths_that_fail_check_path_before_the_stat() {
        let root = std::env::temp_dir().join(format!("balerix-stat-paths-{}", std::process::id()));
        let ws = root.join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join("ok"), "x").unwrap();
        std::fs::write(ws.join(".git/HEAD"), "x").unwrap();
        std::fs::write(root.join("outside"), "x").unwrap();
        let outside = root.join("outside").display().to_string();
        let paths = BTreeSet::from(
            [
                "ok",
                "../outside",
                outside.as_str(),
                ".git/HEAD",
                "a/./b",
                "a\\b",
            ]
            .map(String::from),
        );
        let (entries, total) = stat_paths(&ws, paths, BTreeSet::new());
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(total, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "ok");
        assert!(entries[0].stat.is_some());
    }

    /// The key is the first line that is one; a warning before it is not.
    #[test]
    fn the_filter_key_is_the_first_line_that_is_a_filter_key() {
        assert_eq!(
            filter_key("warning: something odd\nfilter.x.clean\n").as_deref(),
            Some("filter.x.clean")
        );
        for key in [
            "filter.lfs.smudge",
            "filter.a.b.process",
            "extensions.worktreeconfig",
            "extensions.partialclone",
            "remote.origin.promisor",
        ] {
            assert_eq!(filter_key(key).as_deref(), Some(key));
        }
        assert_eq!(filter_key("warning: only a warning\n"), None);
        assert_eq!(filter_key("filter.x.cleanup\nremote.x.url\n"), None);
        assert_eq!(filter_key(""), None);
    }

    #[test]
    fn the_version_output_splits_into_merge_base_names_and_status() {
        let out = "abc123\0README\0src/a b.rs\0\0# branch.oid def456\0? new\0";
        let (mb, names, status) = parse_version(out).unwrap();
        assert_eq!(mb, "abc123");
        assert_eq!(
            names.iter().map(String::as_str).collect::<Vec<_>>(),
            ["README", "src/a b.rs"]
        );
        assert_eq!(status.head.as_deref(), Some("def456"));
        assert_eq!(
            status
                .untracked
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["new"]
        );
        // no names
        let (mb, names, status) = parse_version("abc\0\0# branch.oid (initial)\0").unwrap();
        assert_eq!((mb.as_str(), names.len(), status.head), ("abc", 0, None));
        // cut short: no end to the names
        assert_eq!(parse_version("abc\0README\0"), None);
        assert_eq!(parse_version("abc"), None);
    }

    #[test]
    fn the_fingerprint_changes_with_every_input_and_is_stable() {
        let a = PathStat {
            path: "a".into(),
            stat: Some((1, 10, 500)),
        };
        let b = PathStat {
            path: "b".into(),
            stat: None,
        };
        let base = fingerprint_of("h", "m", &[a.clone(), b.clone()], 2);
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            fingerprint_of("h", "m", &[a.clone(), b.clone()], 2),
            "stable"
        );
        assert_ne!(
            base,
            fingerprint_of("H", "m", &[a.clone(), b.clone()], 2),
            "head"
        );
        assert_ne!(
            base,
            fingerprint_of("h", "M", &[a.clone(), b.clone()], 2),
            "merge-base"
        );
        assert_ne!(
            base,
            fingerprint_of("h", "m", std::slice::from_ref(&a), 1),
            "a path"
        );
        let bigger = PathStat {
            path: "a".into(),
            stat: Some((2, 10, 500)),
        };
        assert_ne!(
            base,
            fingerprint_of("h", "m", &[bigger, b.clone()], 2),
            "size"
        );
        let later = PathStat {
            path: "a".into(),
            stat: Some((1, 11, 500)),
        };
        assert_ne!(
            base,
            fingerprint_of("h", "m", &[later, b.clone()], 2),
            "mtime"
        );
        let nsec = PathStat {
            path: "a".into(),
            stat: Some((1, 10, 501)),
        };
        assert_ne!(
            base,
            fingerprint_of("h", "m", &[nsec, b.clone()], 2),
            "mtime nsec"
        );
        let present = PathStat {
            path: "b".into(),
            stat: Some((0, 0, 0)),
        };
        assert_ne!(
            base,
            fingerprint_of("h", "m", &[a.clone(), present], 2),
            "missing vs present"
        );
        assert_eq!(fingerprint_of("", "", &[], 0).len(), 64);
        // past the cap: the count is hashed, and only past it
        let both = [a, b];
        assert_eq!(
            fingerprint_of("h", "m", &both, 1),
            base,
            "no marker at or under"
        );
        let capped = fingerprint_of("h", "m", &both, 3);
        assert_ne!(capped, base, "a marker past the cap");
        assert_ne!(capped, fingerprint_of("h", "m", &both, 4), "the count");
    }

    /// #18: past `VERSION_PATH_CAP` only the first paths in sorted order
    /// are stat'ed; the total is still counted. A failed `stat` (here, no
    /// such directory) is `missing`.
    #[test]
    fn version_stats_at_most_the_cap_in_sorted_order() {
        let paths: BTreeSet<String> = (0..VERSION_PATH_CAP + 5)
            .map(|i| format!("p{i:05}"))
            .collect();
        let first: Vec<String> = paths.iter().take(VERSION_PATH_CAP).cloned().collect();
        let (entries, total) =
            stat_paths(Path::new("/nonexistent-balerix"), paths, BTreeSet::new());
        assert_eq!(total, VERSION_PATH_CAP + 5);
        assert_eq!(
            entries.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
            first
        );
        assert!(entries.iter().all(|e| e.stat.is_none()));
        let few = BTreeSet::from(["b".to_string(), "a".to_string()]);
        let (entries, total) = stat_paths(Path::new("/nonexistent-balerix"), few, BTreeSet::new());
        assert_eq!((entries.len(), total), (2, 2));
        assert_eq!(entries[0].path, "a");
        // tracked changes fill the cap first, whatever sorts before them
        let untracked: BTreeSet<String> = (0..VERSION_PATH_CAP)
            .map(|i| format!("0vendor/{i:05}"))
            .chain(["z".to_string()])
            .collect();
        let tracked = BTreeSet::from(["z".to_string(), "m".to_string()]);
        let (entries, total) = stat_paths(Path::new("/nonexistent-balerix"), tracked, untracked);
        assert_eq!(total, VERSION_PATH_CAP + 2, "z is counted once");
        assert_eq!(entries.len(), VERSION_PATH_CAP);
        assert!(entries.is_sorted_by(|a, b| a.path < b.path));
        let kept: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(
            kept.contains(&"m") && kept.contains(&"z"),
            "the tracked changes are kept"
        );
        assert_eq!(kept[0], "0vendor/00000");
        assert_eq!(
            kept[VERSION_PATH_CAP - 3],
            "0vendor/01997",
            "then untracked, in order"
        );
    }
}
