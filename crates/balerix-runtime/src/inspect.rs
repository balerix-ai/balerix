//! `WorkspaceReader` over real git (Spec C §3.2): the agent's clone
//! against the crew's base, one file, one listing. Reads only, with the
//! repository's config escape hatches closed — an agent can write its clone's
//! `.git/config` and `.gitattributes`, and this code runs as the daemon.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use sha2::{Digest, Sha256};

use balerix_api::{
    EntryKind, FileDiff, FileStatus, TreeEntry, WORKSPACE_FILE_COUNT_LIMIT, WORKSPACE_FILE_LIMIT,
    WORKSPACE_PATCH_LIMIT, WorkspaceDiff, WorkspaceTree, WorkspaceVersion, check_path,
};
use balerix_core::{AgentId, MaterializeError, WorkspaceError, WorkspaceReader};

use crate::layout::{AgentPaths, CrewPaths};
use crate::materializer::Runtime;
use crate::sandbox::write_git_profile;
use crate::workspace::{GitLog, sandboxed_git, sandboxed_git_script, sandboxed_git_streaming};

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
        write_git_profile(&self.tools, &id, &paths, &crew, &self.git_read).map_err(
            |e| match e {
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
            },
        )?;
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
    /// hardening.
    fn inspect_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        paths: &AgentPaths,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<String, WorkspaceError> {
        self.inspect_git_answer(id, crew, paths, args, accepted, false)
    }

    /// `inspect_git` for a call whose stdout goes to `sink` as it arrives
    /// (the combined diff); exit 0 only.
    fn inspect_git_streaming(
        &self,
        id: &str,
        crew: &CrewPaths,
        paths: &AgentPaths,
        args: &[&str],
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<(), WorkspaceError> {
        let mut argv: Vec<&str> = CONFIG.to_vec();
        argv.extend(args);
        sandboxed_git_streaming(&self.tools, crew, paths, &argv, sink)
            .map(|_| ())
            .map_err(|f| WorkspaceError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }

    /// `inspect_git`; with `lenient`, an accepted exit that printed on
    /// stderr is still the answer. Only for the `--no-index` diff of an
    /// untracked file: git exits 1 with `error: Could not access
    /// '<link>/null'` for a symlink to a directory, which used to be an
    /// empty patch, and should not fail the whole diff. A nono that fails
    /// there, after the calls before it ran, costs one empty patch.
    fn inspect_git_answer(
        &self,
        id: &str,
        crew: &CrewPaths,
        paths: &AgentPaths,
        args: &[&str],
        accepted: &[i32],
        lenient: bool,
    ) -> Result<String, WorkspaceError> {
        let mut argv: Vec<&str> = CONFIG.to_vec();
        argv.extend(args);
        match sandboxed_git(&self.tools, crew, paths, &argv, accepted, GitLog::ArgvOnly) {
            Ok(out) => Ok(out.stdout),
            Err(f) if lenient && f.unanswered.is_some() => {
                Ok(f.unanswered.map(|o| o.stdout).unwrap_or_default())
            }
            Err(f) => Err(WorkspaceError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            }),
        }
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

/// One changed or untracked path as the fingerprint sees it: `(size,
/// mtime, mtime_nsec)` from `symlink_metadata`, or `None` when the path
/// vanished between the listing and the `stat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStat {
    pub path: String,
    pub stat: Option<(u64, i64, i64)>,
}

/// `hex(sha256(head \0 merge_base \0 (path \0 size \0 mtime.nsec \0 |
/// path \0 missing \0)*))` (Spec D §2.3). Entries are hashed in the order
/// given; callers pass them sorted.
pub fn fingerprint_of(head: &str, merge_base: &str, entries: &[PathStat]) -> String {
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

/// The `FILTER_KEYS` probe, first on every git-running read: `Filter` when
/// the repository config names a program git would run.
#[allow(clippy::type_complexity)]
fn refuse_filters(
    id: &str,
    git: &dyn Fn(&[&str], &[i32]) -> Result<String, WorkspaceError>,
) -> Result<(), WorkspaceError> {
    let mut args = PROBE.to_vec();
    args.push(FILTER_KEYS);
    let filters = git(&args, &[0, 1])?;
    match filter_key(&filters) {
        Some(key) => Err(WorkspaceError::Filter { key }),
        None if filters.trim().is_empty() => Ok(()),
        // git exited 0, so something matched, but no line is a key
        None => Err(WorkspaceError::Tool {
            id: id.to_string(),
            subcommand: "config".into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            stderr: filters,
        }),
    }
}

/// The `FILTER_KEYS` probe, before the regex: `refuse_filters` and
/// `VERSION_SCRIPT` both run exactly this.
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

/// `version`'s four git calls as one sandbox start (#174;
/// `workspace::sandboxed_git_script`). `$1` is the base ref, `$2`
/// `FILTER_KEYS`, and the rest git with its options, so nothing the caller
/// passes is ever script text. The filter probe runs first, as in
/// `refuse_filters`: a match prints the key and exits `VERSION_FILTER`; an
/// exit 1 that printed anything (stderr is folded in) is a failure, as
/// `is_gits_answer` would judge it. Then `merge-base`, the name-only diff
/// against it (`DIFF_FLAGS`) and `status` (`STATUS`), each failing with its
/// own code (`version_stage`). On success stdout is `<merge-base> \0
/// <name-only -z> \0 <status -z>` (`parse_version`). The `-c` pairs are
/// `CONFIG`'s, passed in `"$@"`; a unit test holds the two flag lists to
/// the constants.
const VERSION_SCRIPT: &str = r#"base=$1 keys=$2
shift 2
found=$("$@" config --local --includes --name-only --get-regexp "$keys" 2>&1)
case $? in
0) printf '%s' "$found"; exit 80 ;;
1) if [ -n "$found" ]; then printf '%s\n' "$found" >&2; exit 81; fi ;;
*) printf '%s\n' "$found" >&2; exit 81 ;;
esac
mb=$("$@" merge-base "$base" HEAD) || exit 83
printf '%s\000' "$mb"
"$@" diff --no-ext-diff --no-textconv --no-color --submodule=short --ignore-submodules=dirty --name-only -z "$mb" || exit 84
printf '\000'
"$@" status --porcelain=v2 -z --branch --no-ahead-behind --untracked-files=all --ignore-submodules=dirty || exit 82
"#;

/// `VERSION_SCRIPT`'s exit when the probe found a key (on stdout).
const VERSION_FILTER: i32 = 80;

/// The git subcommand behind one of `VERSION_SCRIPT`'s failure exits.
fn version_stage(code: i32) -> Option<&'static str> {
    match code {
        81 => Some("config"),
        82 => Some("status"),
        83 => Some("merge-base"),
        84 => Some("diff"),
        _ => None,
    }
}

/// `VERSION_SCRIPT`'s stdout: the merge-base, the name-only paths and the
/// parsed status. The name-only records are non-empty and NUL-terminated,
/// so the first empty record ends them; `None` when that structure is not
/// there.
pub fn parse_version(out: &str) -> Option<(String, BTreeSet<String>, Status)> {
    let (merge_base, mut rest) = out.split_once('\0')?;
    let mut names = BTreeSet::new();
    loop {
        let (record, after) = rest.split_once('\0')?;
        rest = after;
        if record.is_empty() {
            break;
        }
        names.insert(record.to_string());
    }
    Some((merge_base.trim().to_string(), names, parse_status(rest)))
}

impl WorkspaceReader for Runtime {
    /// `refuse_filters`, one `status`, `merge-base`, `--name-status`, one
    /// combined `diff -U3` split per file, and one `--no-index` diff per
    /// untracked file (#108: each call is a sandbox start, ~55 ms).
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        let id = agent.to_string();
        let (paths, crew) = self.sandboxed_workspace_of(agent)?;
        let git = |args: &[&str], ok: &[i32]| self.inspect_git(&id, &crew, &paths, args, ok);
        refuse_filters(&id, &git)?;
        let status = parse_status(&git(STATUS, &[0])?);
        let merge_base = git(&["merge-base", base_ref, "HEAD"], &[0])?
            .trim()
            .to_string();
        let head = status.head.unwrap_or_default();
        let mut name_status_args: Vec<&str> = vec!["diff"];
        name_status_args.extend(DIFF_FLAGS);
        name_status_args.extend(["--name-status", "-z", "--find-renames", &merge_base]);
        let mut files = parse_name_status(&git(&name_status_args, &[0])?);
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

        // The tracked files that survived the cap, in one call: every
        // one when none was cut, else those named as literal pathspecs.
        let tracked: Vec<&FileDiff> = files
            .iter()
            .filter(|f| !untracked.contains(&f.path))
            .collect();
        let mut sections = BTreeMap::new();
        if !tracked.is_empty() {
            let mut args: Vec<&str> = Vec::new();
            if tracked.len() < all_tracked {
                args.push("--literal-pathspecs");
            }
            args.push("diff");
            args.extend(DIFF_FLAGS);
            args.extend(["-U3", "--find-renames", merge_base.as_str()]);
            if tracked.len() < all_tracked {
                args.push("--");
                for f in &tracked {
                    if let Some(old) = &f.old_path {
                        args.push(old);
                    }
                    args.push(&f.path);
                }
            }
            let mut splitter = PatchSplitter::new(SECTION_CAP);
            self.inspect_git_streaming(&id, &crew, &paths, &args, &mut |chunk| {
                splitter.feed(chunk)
            })?;
            sections = splitter.finish();
        }

        for f in &mut files {
            f.uncommitted = f.uncommitted
                || dirty.contains(&f.path)
                || f.old_path.as_deref().is_some_and(|o| dirty.contains(o));
            let combined = if untracked.contains(&f.path) {
                None
            } else {
                patch_header(f).and_then(|h| sections.remove(&h))
            };
            let raw = match combined {
                Some(section) => section,
                None => {
                    // untracked, or a path the combined patch does not
                    // name as expected (quoted, or prefixes the clone's
                    // config changed): this file on its own, as before
                    let mut args: Vec<&str> = vec!["diff"];
                    args.extend(DIFF_FLAGS);
                    if untracked.contains(&f.path) {
                        args.extend(["--no-index", "-U3", "--", "/dev/null", f.path.as_str()]);
                        self.inspect_git_answer(&id, &crew, &paths, &args, &[0, 1], true)?
                    } else {
                        args.extend(["-U3", "--find-renames", merge_base.as_str(), "--"]);
                        if let Some(old) = &f.old_path {
                            args.push(old);
                        }
                        args.push(&f.path);
                        git(&args, &[0])?
                    }
                }
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
        let id = agent.to_string();
        let (paths, crew) = self.sandboxed_workspace_of(agent)?;
        let tool_error =
            |subcommand: &str, args: Vec<String>, stderr: String| WorkspaceError::Tool {
                id: id.clone(),
                subcommand: subcommand.to_string(),
                args,
                stderr,
            };
        let out = sandboxed_git_script(
            &self.tools,
            &crew,
            &paths,
            VERSION_SCRIPT,
            &[base_ref, FILTER_KEYS],
            CONFIG,
            &[0, VERSION_FILTER, 81, 82, 83, 84],
        )
        .map_err(|f| tool_error(&f.subcommand, f.args, f.stderr))?;
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
                    args.extend(["--name-only", "-z", merge_base]);
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
            return Err(tool_error(stage, stage_args(stage), out.stderr));
        }
        let (merge_base, mut paths_seen, status) = parse_version(&out.stdout).ok_or_else(|| {
            tool_error(
                "sh",
                Vec::new(),
                format!("unexpected output from the version script: {}", out.stderr),
            )
        })?;
        let head = status.head.unwrap_or_default();
        paths_seen.extend(status.changed);
        paths_seen.extend(status.untracked);
        // `BTreeSet`: sorted and deduplicated, so the hash order is fixed.
        let entries: Vec<PathStat> = paths_seen
            .into_iter()
            .map(|path| {
                let stat = std::fs::symlink_metadata(paths.workspace.join(&path))
                    .ok()
                    .map(|m| (m.len(), m.mtime(), m.mtime_nsec()));
                PathStat { path, stat }
            })
            .collect();
        Ok(WorkspaceVersion {
            fingerprint: fingerprint_of(&head, &merge_base, &entries),
            head,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::FileStatus;

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

    /// The script spells `STATUS` and `DIFF_FLAGS` out; they must not drift.
    #[test]
    fn the_version_script_runs_the_same_flags_as_the_constants() {
        assert!(VERSION_SCRIPT.contains(&format!("\"$@\" {} ||", STATUS.join(" "))));
        assert!(VERSION_SCRIPT.contains(&format!(
            "\"$@\" diff {} --name-only -z \"$mb\" ||",
            DIFF_FLAGS.join(" ")
        )));
        assert!(VERSION_SCRIPT.contains(&format!("\"$@\" {} \"$keys\"", PROBE.join(" "))));
        // nothing but the positional parameters reaches a command
        assert!(!VERSION_SCRIPT.contains("eval"));
        for code in 81..=84 {
            let stage = version_stage(code).unwrap();
            assert!(VERSION_SCRIPT.contains(&format!("exit {code}")), "{stage}");
        }
        assert!(VERSION_SCRIPT.contains(&format!("exit {VERSION_FILTER}")));
        assert_eq!(version_stage(0), None);
        assert_eq!(version_stage(1), None);
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
        let base = fingerprint_of("h", "m", &[a.clone(), b.clone()]);
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            fingerprint_of("h", "m", &[a.clone(), b.clone()]),
            "stable"
        );
        assert_ne!(
            base,
            fingerprint_of("H", "m", &[a.clone(), b.clone()]),
            "head"
        );
        assert_ne!(
            base,
            fingerprint_of("h", "M", &[a.clone(), b.clone()]),
            "merge-base"
        );
        assert_ne!(
            base,
            fingerprint_of("h", "m", std::slice::from_ref(&a)),
            "a path"
        );
        let bigger = PathStat {
            path: "a".into(),
            stat: Some((2, 10, 500)),
        };
        assert_ne!(base, fingerprint_of("h", "m", &[bigger, b.clone()]), "size");
        let later = PathStat {
            path: "a".into(),
            stat: Some((1, 11, 500)),
        };
        assert_ne!(base, fingerprint_of("h", "m", &[later, b.clone()]), "mtime");
        let nsec = PathStat {
            path: "a".into(),
            stat: Some((1, 10, 501)),
        };
        assert_ne!(
            base,
            fingerprint_of("h", "m", &[nsec, b.clone()]),
            "mtime nsec"
        );
        let present = PathStat {
            path: "b".into(),
            stat: Some((0, 0, 0)),
        };
        assert_ne!(
            base,
            fingerprint_of("h", "m", &[a, present]),
            "missing vs present"
        );
        assert_eq!(fingerprint_of("", "", &[]).len(), 64);
    }
}
