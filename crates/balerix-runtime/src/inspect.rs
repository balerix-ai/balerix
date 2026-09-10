//! `WorkspaceReader` over real git (Spec C §3.2): the agent's worktree
//! against the crew's base, one file, one listing. Reads only, with the
//! repository's config escape hatches closed — an agent can write the
//! shared `.git/config` and `.gitattributes`, and this code runs as the
//! daemon.

use std::collections::BTreeSet;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use sha2::{Digest, Sha256};

use balerix_api::{
    EntryKind, FileDiff, FileStatus, TreeEntry, WORKSPACE_FILE_COUNT_LIMIT, WORKSPACE_FILE_LIMIT,
    WORKSPACE_PATCH_LIMIT, WorkspaceDiff, WorkspaceTree, WorkspaceVersion, check_path,
};
use balerix_core::{AgentId, WorkspaceError, WorkspaceReader};

use crate::layout::{AgentPaths, CrewPaths};
use crate::materializer::Runtime;
use crate::tools::Cmd;

/// Command-line config beats every config file: whatever an agent wrote
/// into the shared `.git/config`, no program runs from it here.
const CONFIG: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.quotePath=true",
    "-c",
    "diff.noprefix=false",
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
const FILTER_KEYS: &str = r"^(filter\..*\.(clean|smudge|process)|extensions\.worktreeconfig)$";

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

    /// One git call in the worktree, logged to the crew's `git.log` with
    /// the same `GIT_*` scrub as `Workspace::git`, no optional locks,
    /// fsmonitor off and hooks pointed at an empty directory. The log
    /// keeps the argv, stderr and the exit status but not the stdout:
    /// a review page's `diff.json` runs one `diff -U3` per file, up to
    /// 500 of them at 256 KiB each, and `Workspace::git`'s full-output
    /// logging would grow `git.log` by the whole diff on every fetch.
    ///
    /// `GIT_CEILING_DIRECTORIES` is the agent's own root, the parent of
    /// `workspace/`: the agent owns the worktree and can delete its `.git`
    /// file, and repository discovery would then walk up and run every
    /// command here in whatever repository contains the state root. git
    /// only honours a ceiling that matches the resolved path, so it is
    /// canonical.
    fn inspect_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        paths: &AgentPaths,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<String, WorkspaceError> {
        let no_hooks = crew.root.join("no-hooks");
        let _ = std::fs::create_dir_all(&no_hooks);
        let mut cmd =
            Cmd::new(&self.tools.git).log_argv_only(&crew.root.join("logs").join("git.log"));
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_PREFIX",
            "GIT_COMMON_DIR",
        ] {
            cmd = cmd.env_remove(var);
        }
        let ceiling = paths
            .root
            .canonicalize()
            .unwrap_or_else(|_| paths.root.clone());
        cmd = cmd
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CEILING_DIRECTORIES", ceiling.display().to_string())
            .args(CONFIG.iter().copied())
            .args([
                "-c".to_string(),
                format!("core.hooksPath={}", no_hooks.display()),
            ])
            .args(["-C".to_string(), paths.workspace.display().to_string()])
            .args(args.iter().copied());
        cmd.run_with_exit_codes(accepted)
            .map(|o| o.stdout)
            .map_err(|f| WorkspaceError::Tool {
                id: id.to_string(),
                subcommand: f.subcommand,
                args: f.args,
                stderr: f.stderr,
            })
    }
}

fn split_z(s: &str) -> BTreeSet<String> {
    s.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
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
    git: &dyn Fn(&[&str], &[i32]) -> Result<String, WorkspaceError>,
) -> Result<(), WorkspaceError> {
    let filters = git(
        &[
            "config",
            "--local",
            "--includes",
            "--name-only",
            "--get-regexp",
            FILTER_KEYS,
        ],
        &[0, 1],
    )?;
    match filters
        .lines()
        .next()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        Some(key) => Err(WorkspaceError::Filter {
            key: key.to_string(),
        }),
        None => Ok(()),
    }
}

impl WorkspaceReader for Runtime {
    fn diff(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceDiff, WorkspaceError> {
        let id = agent.to_string();
        let (paths, crew) = self.workspace_of(agent)?;
        let git = |args: &[&str], ok: &[i32]| self.inspect_git(&id, &crew, &paths, args, ok);
        refuse_filters(&git)?;
        let head = git(&["rev-parse", "HEAD"], &[0])?.trim().to_string();
        let merge_base = git(&["merge-base", base_ref, "HEAD"], &[0])?
            .trim()
            .to_string();
        let mut name_status_args: Vec<&str> = vec!["diff"];
        name_status_args.extend(DIFF_FLAGS);
        name_status_args.extend(["--name-status", "-z", "--find-renames", &merge_base]);
        let mut files = parse_name_status(&git(&name_status_args, &[0])?);
        let mut name_only_args: Vec<&str> = vec!["diff"];
        name_only_args.extend(DIFF_FLAGS);
        name_only_args.extend(["--name-only", "-z", "HEAD"]);
        let dirty = split_z(&git(&name_only_args, &[0])?);
        let untracked = split_z(&git(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            &[0],
        )?);
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
        for f in &mut files {
            f.uncommitted = f.uncommitted
                || dirty.contains(&f.path)
                || f.old_path.as_deref().is_some_and(|o| dirty.contains(o));
            let mut args: Vec<&str> = vec!["diff"];
            args.extend(DIFF_FLAGS);
            let raw = if untracked.contains(&f.path) {
                args.extend(["--no-index", "-U3", "--", "/dev/null", f.path.as_str()]);
                git(&args, &[0, 1])?
            } else {
                args.extend(["-U3", "--find-renames", merge_base.as_str(), "--"]);
                if let Some(old) = &f.old_path {
                    args.push(old);
                }
                args.push(&f.path);
                git(&args, &[0])?
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
    fn version(&self, agent: &AgentId, base_ref: &str) -> Result<WorkspaceVersion, WorkspaceError> {
        let id = agent.to_string();
        let (paths, crew) = self.workspace_of(agent)?;
        let git = |args: &[&str], ok: &[i32]| self.inspect_git(&id, &crew, &paths, args, ok);
        refuse_filters(&git)?;
        let head = git(&["rev-parse", "HEAD"], &[0])?.trim().to_string();
        let merge_base = git(&["merge-base", base_ref, "HEAD"], &[0])?
            .trim()
            .to_string();
        let mut against_base: Vec<&str> = vec!["diff"];
        against_base.extend(DIFF_FLAGS);
        against_base.extend(["--name-only", "-z", merge_base.as_str()]);
        let mut against_head: Vec<&str> = vec!["diff"];
        against_head.extend(DIFF_FLAGS);
        against_head.extend(["--name-only", "-z", "HEAD"]);
        let mut paths_seen = split_z(&git(&against_base, &[0])?);
        paths_seen.extend(split_z(&git(&against_head, &[0])?));
        paths_seen.extend(split_z(&git(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            &[0],
        )?));
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
