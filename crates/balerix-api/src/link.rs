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
    use serde_json::json;

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
