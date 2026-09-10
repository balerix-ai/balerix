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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The patch was cut at `WORKSPACE_PATCH_LIMIT`.
    #[serde(default)]
    pub truncated: bool,
}

/// `GET …/workspace/diff`: the worktree against the merge-base with
/// `base_ref`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDiff {
    /// `origin/<crew ref>`.
    pub base_ref: String,
    pub merge_base: String,
    pub head: String,
    pub files: Vec<FileDiff>,
    /// The file list stopped at `WORKSPACE_FILE_COUNT_LIMIT`.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Files only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// `GET …/workspace/tree`: one directory, never recursive, `.git` never
/// listed, sorted by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTree {
    pub path: String,
    pub entries: Vec<TreeEntry>,
}

/// `GET …/workspace/version` (Spec D §2.1): `head` is the full sha;
/// `fingerprint` is 64 hex chars over `HEAD`, the merge-base and the size
/// and mtime of every changed or untracked path. Compared for equality,
/// never interpreted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVersion {
    pub head: String,
    pub fingerprint: String,
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
        let d = WorkspaceDiff {
            base_ref: "origin/main".into(),
            merge_base: "a".repeat(40),
            head: "b".repeat(40),
            files: vec![FileDiff {
                path: "COPYING".into(),
                old_path: Some("LICENSE".into()),
                status: FileStatus::Renamed,
                uncommitted: false,
                binary: false,
                patch: "diff --git a/LICENSE b/COPYING\n".into(),
                truncated: false,
            }],
            truncated: false,
        };
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
        let t = WorkspaceTree {
            path: "src".into(),
            entries: vec![
                TreeEntry {
                    name: "lib.rs".into(),
                    kind: EntryKind::File,
                    size: Some(13),
                },
                TreeEntry {
                    name: "sub".into(),
                    kind: EntryKind::Dir,
                    size: None,
                },
            ],
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["entries"][0]["kind"], "file");
        assert_eq!(v["entries"][0]["size"], 13);
        assert!(v["entries"][1].get("size").is_none(), "size only for files");
        assert_eq!(serde_json::from_value::<WorkspaceTree>(v).unwrap(), t);
        assert_eq!(WORKSPACE_FILE_LIMIT, 1 << 20);
        assert_eq!(WORKSPACE_PATCH_LIMIT, 256 << 10);
        assert_eq!(WORKSPACE_FILE_COUNT_LIMIT, 500);
        let v = WorkspaceVersion {
            head: "h".repeat(40),
            fingerprint: "f".repeat(64),
        };
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(
            j,
            json!({ "head": "h".repeat(40), "fingerprint": "f".repeat(64) })
        );
        assert_eq!(serde_json::from_value::<WorkspaceVersion>(j).unwrap(), v);
    }
}
