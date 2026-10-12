//! The sidecar link (Spec O §7.2): JSON text frames on one WebSocket from
//! a pod's sidecar to its Daemon. The Daemon sends requests, one per id;
//! the sidecar answers each with a reply carrying the same id, and sends a
//! `status` frame on every change of its agent's status. Terminal bytes
//! for `attach` never travel here: an `attach` request names a session,
//! and the sidecar opens a second socket for it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{AgentStatus, KeyStep, WorkspaceDiff, WorkspaceTree, WorkspaceVersion};

/// Bumped when a frame changes shape. The sidecar sends it as the
/// `balerix-link-protocol` header on connect; the Daemon refuses another
/// value with 400.
pub const LINK_PROTOCOL: u32 = 1;

/// The header carrying `LINK_PROTOCOL`.
pub const LINK_PROTOCOL_HEADER: &str = "balerix-link-protocol";

/// The largest message, and the largest frame, either end of the link
/// accepts (both are set on the Daemon's upgrade and the sidecar's
/// client; each frame is sent whole, never fragmented). A sidecar whose
/// reply would be larger answers that request `Failed` instead, so the
/// call fails and the link stays up.
///
/// The workspace caps keep every reply far below it. JSON escapes a
/// control character as `\u00XX`, six bytes, so a string serializes to
/// at most six times its length:
///
/// - a diff: its `files` are at most `WORKSPACE_DIFF_LIMIT` (8 MiB) as
///   serialized; the rest is the envelope (about 150 bytes), two object
///   ids, and the `base_ref` the Daemon sent (a ref of at most 4096
///   bytes, 24 KiB escaped): under 8 MiB + 25 KiB;
/// - a tree: `WORKSPACE_TREE_ENTRY_LIMIT` (5000) entries, each a name of
///   at most 255 bytes (1530 escaped) and 57 bytes of
///   `{"name":"","kind":"symlink","size":18446744073709551615},`, so
///   5000 × 1587 = 7,935,000 bytes, plus a path of at most 4096 bytes
///   (24 KiB escaped): under 7.6 MiB;
/// - a file: `WORKSPACE_FILE_LIMIT` (1 MiB) bytes as a JSON array of at
///   most four characters each (`255,`): 4 MiB;
/// - a version or a runner's answer: well under 1 KiB.
///
/// The largest, a full diff, is under 8.1 MiB; 16 MiB is twice that.
/// Daemon → sidecar requests are smaller still: a `send_text` is at most
/// `MAX_SEND_TEXT` (256 KiB, 1.5 MiB escaped).
pub const LINK_MAX_MESSAGE: usize = 16 << 20;

/// Daemon → sidecar: one request. The reply carries the same `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkRequest {
    pub id: u64,
    pub op: LinkOp,
}

/// What a Daemon asks of a sidecar (§7.2's table). `SendText`, `SendKeys`
/// and `Attach` are `AgentRunner`'s; `Stop` and `Restart` move the
/// sidecar's stopped set (plugins spec §16.4); the four `Workspace*` are
/// `WorkspaceReader`'s, run by the sidecar in the agent's clone.
///
/// `deny_unknown_fields` holds for the struct variants; for the unit ones
/// serde ignores extra keys (serde-rs/serde#1358, the reason `PluginAction`
/// hand-writes its `Deserialize`). Harmless here: an extra key on `stop`
/// changes nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LinkOp {
    SendText {
        text: String,
        submit: bool,
    },
    SendKeys {
        steps: Vec<KeyStep>,
        delay_ms: u64,
    },
    Stop,
    Restart,
    /// The sidecar opens `GET /v1/agents/{id}/link/attach/{session}` and
    /// answers `Ok` once that socket is up.
    Attach {
        session: String,
    },
    WorkspaceDiff {
        base_ref: String,
    },
    WorkspaceFile {
        path: String,
    },
    WorkspaceTree {
        path: String,
    },
    WorkspaceVersion {
        base_ref: String,
    },
}

/// Sidecar → Daemon: the answer to one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkReply {
    pub id: u64,
    pub result: LinkResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LinkResult {
    Ok,
    Diff {
        diff: WorkspaceDiff,
    },
    /// The file's bytes as a JSON array; at most `WORKSPACE_FILE_LIMIT`.
    File {
        bytes: Vec<u8>,
    },
    Tree {
        tree: WorkspaceTree,
    },
    Version {
        version: WorkspaceVersion,
    },
    /// A struct variant, not a newtype: the payload carries its own tag
    /// (`reason`), and an internally tagged newtype variant would merge
    /// the two objects into one.
    Failed {
        failure: LinkFailure,
    },
}

/// Why a request failed, in the sidecar's words. `reason` lets the Daemon
/// rebuild the port error (`WorkspaceError` or `RunnerError`) the plugin
/// route maps to a status; `message` is the sidecar's error text. No
/// `deny_unknown_fields`: serde does not combine it with `flatten`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkFailure {
    #[serde(flatten)]
    pub reason: FailureKind,
    pub message: String,
}

/// `WorkspaceError`'s variants, plus `Runner` for a tmux failure. Tagged
/// `reason` so it can be flattened next to `message`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum FailureKind {
    Missing,
    NoSuchPath,
    InvalidPath,
    NotAFile,
    NotADirectory,
    TooLarge { limit: u64 },
    Tool,
    Filter,
    Io,
    Runner,
}

/// The largest message, and the largest frame, either end of an attach
/// socket accepts (set on the Daemon's upgrade and the sidecar's client).
/// Terminal bytes travel in pieces no larger: the sidecar sends what one
/// PTY read gives (8 KiB at most), and the Daemon cuts the viewer's input
/// to this size.
pub const ATTACH_MAX_MESSAGE: usize = 64 << 10;

/// Sidecar → Daemon on every change: the agent's status as the sidecar's
/// own planner keeps it, the pane's pid as tmux reports it (opaque to the
/// Daemon, §6.3), and how many hooks the sidecar answered for a Daemon it
/// could not reach (§7.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkStatus {
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default)]
    pub hook_failures: u64,
}

/// Every text frame a sidecar sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum SidecarFrame {
    Status(LinkStatus),
    Reply(LinkReply),
}

/// `fleet/crew/agent` → token, the operator's `agent_tokens` map.
pub type AgentTokens = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentPhase;
    use crate::{
        EntryKind, FileDiff, FileStatus, TreeEntry, WORKSPACE_DIFF_LIMIT, WORKSPACE_FILE_LIMIT,
        WORKSPACE_TREE_ENTRY_LIMIT,
    };
    use serde_json::json;

    fn reply_len(result: LinkResult) -> usize {
        serde_json::to_string(&SidecarFrame::Reply(LinkReply {
            id: u64::MAX,
            result,
        }))
        .unwrap()
        .len()
    }

    /// `LINK_MAX_MESSAGE`'s arithmetic, on the worst reply each cap allows:
    /// every string made of control characters, which JSON escapes to six
    /// bytes each.
    #[test]
    fn the_largest_replies_the_workspace_caps_allow_fit_the_link() {
        let worst = |n: usize| "\u{1}".repeat(n);
        let tree = reply_len(LinkResult::Tree {
            tree: WorkspaceTree {
                path: worst(4096),
                entries: (0..WORKSPACE_TREE_ENTRY_LIMIT)
                    .map(|_| TreeEntry {
                        name: worst(255),
                        kind: EntryKind::Symlink,
                        size: Some(u64::MAX),
                    })
                    .collect(),
            },
        });
        assert!(tree < 7_960_000 + 24_600 + 100, "{tree}");
        let bytes = reply_len(LinkResult::File {
            bytes: vec![255; usize::try_from(WORKSPACE_FILE_LIMIT).unwrap()],
        });
        assert!(bytes <= (4 << 20) + 100, "{bytes}");
        // a diff's `files` fill `WORKSPACE_DIFF_LIMIT`; the envelope is the
        // rest
        let file = FileDiff {
            path: worst(4096),
            old_path: Some(worst(4096)),
            status: FileStatus::Typechange,
            uncommitted: true,
            binary: false,
            patch: worst(100_000),
            truncated: true,
        };
        let one = serde_json::to_string(&file).unwrap().len() + 1; // and a comma
        let files: Vec<FileDiff> = std::iter::repeat_n(file, WORKSPACE_DIFF_LIMIT / one).collect();
        let files_len = serde_json::to_string(&files).unwrap().len();
        assert!(files_len <= WORKSPACE_DIFF_LIMIT, "{files_len}");
        let diff = reply_len(LinkResult::Diff {
            diff: WorkspaceDiff {
                base_ref: worst(4096),
                merge_base: "f".repeat(64),
                head: "f".repeat(64),
                files,
                truncated: true,
            },
        });
        assert!(diff - files_len < 25 << 10, "{}", diff - files_len);
        for len in [tree, bytes, diff] {
            assert!(len * 2 <= LINK_MAX_MESSAGE, "{len}: under half the limit");
        }
    }

    #[test]
    fn requests_are_tagged_by_kind_under_op() {
        let r = LinkRequest {
            id: 7,
            op: LinkOp::SendText {
                text: "hi".into(),
                submit: true,
            },
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "id": 7, "op": { "kind": "send_text", "text": "hi", "submit": true } })
        );
        let back: LinkRequest = serde_json::from_value(json!({
            "id": 8, "op": { "kind": "workspace_file", "path": "src/main.rs" }
        }))
        .unwrap();
        assert_eq!(
            back.op,
            LinkOp::WorkspaceFile {
                path: "src/main.rs".into()
            }
        );
        let stop: LinkRequest =
            serde_json::from_value(json!({ "id": 1, "op": { "kind": "stop" } })).unwrap();
        assert_eq!(stop.op, LinkOp::Stop);
        assert!(
            serde_json::from_value::<LinkRequest>(json!({ "id": 1, "op": { "kind": "fly" } }))
                .is_err()
        );
    }

    #[test]
    fn replies_and_status_are_tagged_by_frame() {
        let reply = SidecarFrame::Reply(LinkReply {
            id: 7,
            result: LinkResult::Ok,
        });
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            json!({ "frame": "reply", "id": 7, "result": { "kind": "ok" } })
        );
        let failed = SidecarFrame::Reply(LinkReply {
            id: 9,
            result: LinkResult::Failed {
                failure: LinkFailure {
                    reason: FailureKind::TooLarge { limit: 1048576 },
                    message: "file is larger than 1 MiB".into(),
                },
            },
        });
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(v["result"]["kind"], "failed");
        assert_eq!(v["result"]["failure"]["reason"], "too_large");
        assert_eq!(v["result"]["failure"]["limit"], 1048576);
        assert_eq!(
            v["result"]["failure"]["message"],
            "file is larger than 1 MiB"
        );
        let back: SidecarFrame = serde_json::from_value(v).unwrap();
        assert_eq!(back, failed);
        let status = SidecarFrame::Status(LinkStatus {
            status: AgentStatus {
                phase: AgentPhase::Ready,
                ..AgentStatus::default()
            },
            pid: Some(56),
            hook_failures: 2,
        });
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["frame"], "status");
        assert_eq!(v["status"]["phase"], "ready");
        assert_eq!(v["pid"], 56);
        let back: SidecarFrame = serde_json::from_value(v).unwrap();
        assert!(
            matches!(back, SidecarFrame::Status(s) if s.pid == Some(56) && s.hook_failures == 2)
        );
    }

    #[test]
    fn file_bytes_travel_as_a_json_array() {
        let r = LinkResult::File {
            bytes: vec![0, 255, 10],
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "kind": "file", "bytes": [0, 255, 10] })
        );
    }

    #[test]
    fn the_protocol_is_one() {
        assert_eq!(LINK_PROTOCOL, 1);
    }
}
