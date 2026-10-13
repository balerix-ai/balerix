//! Workspace wire types (Spec C §2.2) and the path rule every reader
//! applies before any I/O (Spec C §2.2 "Paths"). The rule is here, not in
//! `balerix-core`, because the web plugin validates comment paths with it
//! and may depend on this crate only.

use serde::{Deserialize, Serialize};

/// `GET …/workspace/file` refuses a file larger than this (413).
pub const WORKSPACE_FILE_LIMIT: u64 = 1 << 20;
/// One file's `patch` is cut at a line boundary beyond this.
pub const WORKSPACE_PATCH_LIMIT: usize = 256 << 10;
/// A diff lists at most this many files, in path order.
pub const WORKSPACE_FILE_COUNT_LIMIT: usize = 500;
/// A diff's `files`, serialized as JSON, take at most this many bytes.
/// Past it, the file that crosses the limit has its patch cut at a line
/// boundary (`truncated` on the file), and the list stops at the first
/// file that does not fit even without a patch (`truncated` on the diff).
pub const WORKSPACE_DIFF_LIMIT: usize = 8 << 20;
/// A tree lists at most this many entries, the first by name; past it,
/// `truncated` on the tree.
pub const WORKSPACE_TREE_ENTRY_LIMIT: usize = 5000;

/// The rule for `path` on the `file` and `tree` routes and for comment
/// paths: relative, `/`-separated, at most 4096 bytes, no empty, `.` or
/// `..` segment, no `\` or NUL, and no segment named `.git` (any case).
/// The empty path is the worktree root. `Err` is the reason, for
/// `workspace: invalid path: <reason>`.
pub fn check_path(path: &str) -> Result<(), String> {
    if path.len() > 4096 {
        return Err("longer than 4096 bytes".into());
    }
    if path.contains('\0') {
        return Err("contains NUL".into());
    }
    if path.contains('\\') {
        return Err("contains a backslash".into());
    }
    if path.starts_with('/') {
        return Err("absolute".into());
    }
    if path.is_empty() {
        return Ok(());
    }
    for segment in path.split('/') {
        match segment {
            "" => return Err("empty segment".into()),
            "." | ".." => return Err(format!("{segment:?} segment")),
            s if s.eq_ignore_ascii_case(".git") => return Err(".git segment".into()),
            _ => {}
        }
    }
    Ok(())
}

/// Exhaustive on purpose: these are git's statuses for a diff against a
/// merge-base, and the reader folds any other letter git prints into
/// `Modified`, so no new variant is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Typechange,
}

/// One file of a `WorkspaceDiff` (Spec C §2.2).
///
/// `#[non_exhaustive]`: a later field is not a breaking change. Outside
/// this crate, build one with [`FileDiff::new`] and the `with_*` methods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FileDiff {
    pub path: String,
    /// Set for `renamed` and `copied`; serialized as `null` otherwise.
    #[serde(default)]
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// The worktree differs from `HEAD` for this path (an untracked file
    /// is `added` and uncommitted).
    pub uncommitted: bool,
    /// `patch` is empty for a binary file.
    pub binary: bool,
    /// Unified diff for this one file, three lines of context, the
    /// `diff --git` header included.
    pub patch: String,
    /// The patch was cut at `WORKSPACE_PATCH_LIMIT`, or to keep the diff
    /// within `WORKSPACE_DIFF_LIMIT`.
    #[serde(default)]
    pub truncated: bool,
}

/// `GET …/workspace/diff`: the worktree against the merge-base with
/// `base_ref`.
///
/// `#[non_exhaustive]`: build one with [`WorkspaceDiff::new`] and
/// [`WorkspaceDiff::with_truncated`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceDiff {
    /// `origin/<crew ref>`.
    pub base_ref: String,
    pub merge_base: String,
    pub head: String,
    pub files: Vec<FileDiff>,
    /// The file list stopped at `WORKSPACE_FILE_COUNT_LIMIT`, or at
    /// `WORKSPACE_DIFF_LIMIT`.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    /// Anything else, including a kind a later daemon adds on the wire.
    #[serde(other)]
    Other,
}

/// One entry of a `WorkspaceTree`.
///
/// `#[non_exhaustive]`: build one with [`TreeEntry::new`] and
/// [`TreeEntry::with_size`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Files only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// `GET …/workspace/tree`: one directory, never recursive, `.git` never
/// listed, sorted by name.
///
/// `#[non_exhaustive]`: build one with [`WorkspaceTree::new`] and
/// [`WorkspaceTree::with_truncated`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceTree {
    pub path: String,
    pub entries: Vec<TreeEntry>,
    /// The directory holds more than `WORKSPACE_TREE_ENTRY_LIMIT` entries;
    /// `entries` are the first by name. Absent (an older daemon) reads as
    /// `false`.
    #[serde(default)]
    pub truncated: bool,
}

/// `GET …/workspace/version` (Spec D §2.1): `head` is the full sha;
/// `fingerprint` is 64 hex chars over `HEAD`, the merge-base and the size
/// and mtime of every changed or untracked path. Compared for equality,
/// never interpreted.
///
/// `#[non_exhaustive]`: build one with [`WorkspaceVersion::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkspaceVersion {
    pub head: String,
    pub fingerprint: String,
}

impl FileDiff {
    /// A file with no `old_path`, committed, not binary, an empty patch
    /// and not truncated.
    pub fn new(path: impl Into<String>, status: FileStatus) -> Self {
        Self {
            path: path.into(),
            old_path: None,
            status,
            uncommitted: false,
            binary: false,
            patch: String::new(),
            truncated: false,
        }
    }

    /// The path before a rename or copy.
    #[must_use]
    pub fn with_old_path(mut self, old_path: Option<String>) -> Self {
        self.old_path = old_path;
        self
    }

    #[must_use]
    pub fn with_uncommitted(mut self, uncommitted: bool) -> Self {
        self.uncommitted = uncommitted;
        self
    }

    #[must_use]
    pub fn with_binary(mut self, binary: bool) -> Self {
        self.binary = binary;
        self
    }

    #[must_use]
    pub fn with_patch(mut self, patch: impl Into<String>) -> Self {
        self.patch = patch.into();
        self
    }

    #[must_use]
    pub fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }
}

impl WorkspaceDiff {
    /// A diff whose file list is whole (`truncated` false).
    pub fn new(
        base_ref: impl Into<String>,
        merge_base: impl Into<String>,
        head: impl Into<String>,
        files: Vec<FileDiff>,
    ) -> Self {
        Self {
            base_ref: base_ref.into(),
            merge_base: merge_base.into(),
            head: head.into(),
            files,
            truncated: false,
        }
    }

    #[must_use]
    pub fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }
}

impl TreeEntry {
    /// An entry with no `size` (set one for a file with
    /// [`TreeEntry::with_size`]).
    pub fn new(name: impl Into<String>, kind: EntryKind) -> Self {
        Self {
            name: name.into(),
            kind,
            size: None,
        }
    }

    #[must_use]
    pub fn with_size(mut self, size: Option<u64>) -> Self {
        self.size = size;
        self
    }
}

impl WorkspaceTree {
    /// A listing that holds every entry (`truncated` false).
    pub fn new(path: impl Into<String>, entries: Vec<TreeEntry>) -> Self {
        Self {
            path: path.into(),
            entries,
            truncated: false,
        }
    }

    #[must_use]
    pub fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }
}

impl WorkspaceVersion {
    pub fn new(head: impl Into<String>, fingerprint: impl Into<String>) -> Self {
        Self {
            head: head.into(),
            fingerprint: fingerprint.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_path_rule_accepts_relative_paths_and_names_the_reason_otherwise() {
        for ok in [
            "",
            "src",
            "src/lib.rs",
            "a-b_c.d/e~f",
            "dir/.gitignore",
            "gitx/.gith",
        ] {
            assert_eq!(check_path(ok), Ok(()), "{ok:?}");
        }
        for (bad, reason) in [
            ("/etc/passwd", "absolute"),
            ("../x", "\"..\" segment"),
            ("a/../b", "\"..\" segment"),
            ("./a", "\".\" segment"),
            ("a//b", "empty segment"),
            ("a/", "empty segment"),
            (".git", ".git segment"),
            ("a/.git/config", ".git segment"),
            ("a/.GIT", ".git segment"),
            ("a\\b", "contains a backslash"),
            ("a\0b", "contains NUL"),
        ] {
            assert_eq!(check_path(bad), Err(reason.to_string()), "{bad:?}");
        }
        let long = "x".repeat(4097);
        assert_eq!(check_path(&long), Err("longer than 4096 bytes".to_string()));
    }

    #[test]
    fn the_diff_and_tree_round_trip_with_lowercase_enums() {
        let d = WorkspaceDiff::new(
            "origin/main",
            "a".repeat(40),
            "b".repeat(40),
            vec![
                FileDiff::new("COPYING", FileStatus::Renamed)
                    .with_old_path(Some("LICENSE".into()))
                    .with_patch("diff --git a/LICENSE b/COPYING\n"),
            ],
        );
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["files"][0]["status"], "renamed");
        assert_eq!(v["files"][0]["old_path"], "LICENSE");
        let back: WorkspaceDiff = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
        let plain: FileDiff = serde_json::from_value(json!({
            "path": "x", "status": "added", "uncommitted": true, "binary": false, "patch": ""
        }))
        .unwrap();
        assert_eq!(plain.old_path, None);
        assert!(!plain.truncated, "defaults");
        assert_eq!(
            serde_json::to_value(&plain).unwrap()["old_path"],
            serde_json::Value::Null,
            "always present, null when absent"
        );
        let t = WorkspaceTree::new(
            "src",
            vec![
                TreeEntry::new("lib.rs", EntryKind::File).with_size(Some(13)),
                TreeEntry::new("sub", EntryKind::Dir),
            ],
        );
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["truncated"], false, "always present");
        assert_eq!(v["entries"][0]["kind"], "file");
        assert_eq!(v["entries"][0]["size"], 13);
        assert!(v["entries"][1].get("size").is_none(), "size only for files");
        assert_eq!(serde_json::from_value::<WorkspaceTree>(v).unwrap(), t);
        // an older daemon's tree has no `truncated`
        let old: WorkspaceTree =
            serde_json::from_value(json!({ "path": "", "entries": [] })).unwrap();
        assert_eq!(old, WorkspaceTree::new("", vec![]));
        let cut =
            WorkspaceTree::new("", vec![TreeEntry::new("a", EntryKind::Dir)]).with_truncated(true);
        let v = serde_json::to_value(&cut).unwrap();
        assert_eq!(
            v,
            json!({ "path": "", "entries": [{ "name": "a", "kind": "dir" }], "truncated": true })
        );
        assert_eq!(serde_json::from_value::<WorkspaceTree>(v).unwrap(), cut);
        // a kind a later daemon adds reads as `other`
        let later: TreeEntry =
            serde_json::from_value(json!({ "name": "s", "kind": "socket" })).unwrap();
        assert_eq!(later, TreeEntry::new("s", EntryKind::Other));
        assert_eq!(WORKSPACE_FILE_LIMIT, 1 << 20);
        assert_eq!(WORKSPACE_PATCH_LIMIT, 256 << 10);
        assert_eq!(WORKSPACE_FILE_COUNT_LIMIT, 500);
        assert_eq!(WORKSPACE_DIFF_LIMIT, 8 << 20);
        assert_eq!(WORKSPACE_TREE_ENTRY_LIMIT, 5000);
        let v = WorkspaceVersion::new("h".repeat(40), "f".repeat(64));
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(
            j,
            json!({ "head": "h".repeat(40), "fingerprint": "f".repeat(64) })
        );
        assert_eq!(serde_json::from_value::<WorkspaceVersion>(j).unwrap(), v);
    }
}
