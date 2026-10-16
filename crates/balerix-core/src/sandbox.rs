//! What a `sandbox` block may set: the key allowlist and the literal-path
//! rule shared by a plugin manifest's block (#2) and a fleet's (#187).
//! No filesystem access here: the check against balerix's own roots, which
//! needs the layout and resolves symlinks, is the runtime's
//! (`balerix_runtime::sandbox::{check_plugin_sandbox, check_fleet_sandbox}`),
//! which runs over the grants this check returns.
//!
//! Everything nono accepts outside the allowlist reaches past the
//! filesystem rules (nono 0.80.0's profile schema): `extends`, `groups`,
//! `packs`, `command_policies` (with its own `fs_read`), `workdir`, `hooks`
//! and `session_hooks` (run outside the sandbox), credential capture and
//! injection, raw Seatbelt rules, process inspection and capability
//! elevation. Widening a list means checking what the key lets nono's
//! supervisor do outside the sandbox.

use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

/// Whose block is checked: decides the allowlist and the wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// A plugin manifest's block (#2).
    Plugin,
    /// A fleet's, crew's or agent's block (#187): the plugin's keys and
    /// the `unix_socket*` grant lists, whose paths are root-checked too.
    Fleet,
}

impl SandboxPolicy {
    /// The message for a key outside the allowlist.
    pub fn refusal(self) -> &'static str {
        match self {
            Self::Plugin => "not accepted in a plugin's sandbox block",
            Self::Fleet => "not accepted in a fleet's sandbox block",
        }
    }

    fn access(self, key: &str) -> Option<Option<Access>> {
        if let Some(a) = grant_access(key) {
            return Some(Some(a));
        }
        if key == "deny" {
            return Some(None);
        }
        (self == Self::Fleet && UNIX_SOCKET_KEYS.contains(&key)).then_some(Some(Access::Socket))
    }
}

/// `platform_overrides` names nono knows.
pub const SANDBOX_OSES: &[&str] = &["linux", "macos", "windows"];

/// The six grant lists; `deny` (a literal path, granting nothing) is
/// accepted beside them. Not `bypass_protection`: on macOS it lifts
/// nono's `deny_credentials` for `~/.ssh` and `~/.aws`.
pub const GRANT_KEYS: &[&str] = &[
    "read",
    "read_file",
    "allow",
    "allow_file",
    "write",
    "write_file",
];

/// A fleet's block only: AF_UNIX socket grants. Still subject to the
/// daemon's `[sandbox] unix_sockets` policy, which is applied after the
/// merge.
pub const UNIX_SOCKET_KEYS: &[&str] = &[
    "unix_socket",
    "unix_socket_bind",
    "unix_socket_dir",
    "unix_socket_dir_bind",
    "unix_socket_subtree",
    "unix_socket_subtree_bind",
];

/// Ports, domains and proxies; never credentials (`credentials`,
/// `custom_credentials` make nono's supervisor read a secret — a
/// `file://` or `env://` one included — and hand it to an upstream the
/// block names) nor TLS interception.
pub const NETWORK_KEYS: &[&str] = &[
    "block",
    "allow_http2",
    "network_profile",
    "allow_domain",
    "deny_domain",
    "open_port",
    "open_port_range",
    "connect_port",
    "listen_port",
    "listen_port_range",
    "no_proxy",
    "upstream_proxy",
    "upstream_bypass",
];

/// `ipc_mode`, at its default value only (`IPC_MODE`). `signal_mode` is
/// balerix's (#118, refused with its own message); the rest of
/// `security` would widen the sandbox at runtime (`process_info_mode`
/// reads other processes, among them their tokens in their environment;
/// `capability_elevation` with `approval_backends` lets a webhook grant
/// paths) or degrade it (`wsl2_proxy_policy`).
pub const SECURITY_KEYS: &[&str] = &["ipc_mode"];

/// The one `security.ipc_mode` accepted: nono's default. `full` lifts
/// nono's IPC scoping.
pub const IPC_MODE: &str = "shared_memory_only";

/// Keys balerix sets itself, refused with these messages (the runtime's
/// `check_conflicts` refuses them again at render, in the same words).
const ENVIRONMENT_OWNED: &str = "balerix-owned; set variables through `env` instead";
const META_OWNED: &str = "balerix-owned";
const SIGNAL_MODE_OWNED: &str = "balerix-owned; agents stay signal-isolated";

/// Prefixes no entry may name: nono would hand the sandbox another
/// process's view (`/proc/<pid>/root`, `/proc/<pid>/fd`) through them.
pub const REFUSED_PREFIXES: &[&str] = &["/proc", "/dev/fd"];

/// What a grant list hands out, for the root check's exemptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// `read`, `read_file`.
    Read,
    /// `allow`, `allow_file`, `write`, `write_file`.
    Write,
    /// The `unix_socket*` lists (fleets only).
    Socket,
}

fn grant_access(key: &str) -> Option<Access> {
    match key {
        "read" | "read_file" => Some(Access::Read),
        "allow" | "allow_file" | "write" | "write_file" => Some(Access::Write),
        _ => None,
    }
}

/// One path a block grants, for the runtime's root check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockGrant {
    /// Its path in the block: `filesystem.read[0]`,
    /// `platform_overrides.linux.filesystem.allow[1]`.
    pub at: String,
    /// The entry as written.
    pub raw: String,
    pub path: PathBuf,
    pub access: Access,
}

/// A refused block. `at` is the path within the block (empty for the
/// block itself); `path_under` names it in a larger document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{at}: {message}")]
pub struct SandboxRefusal {
    pub at: String,
    pub message: String,
}

impl SandboxRefusal {
    fn new(at: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            at: at.into(),
            message: message.into(),
        }
    }

    /// The refused path under `prefix` (`sandbox`,
    /// `crews.c.agents.a.sandbox`).
    pub fn path_under(&self, prefix: &str) -> String {
        match (prefix.is_empty(), self.at.is_empty()) {
            (true, _) => self.at.clone(),
            (false, true) => prefix.to_string(),
            (false, false) => format!("{prefix}.{}", self.at),
        }
    }
}

/// Checks a `sandbox` block against `policy`: a mapping holding only the
/// allowlisted keys (top level and in `platform_overrides.<os>`), every
/// path entry a string or `{ path, when }` naming an absolute, literal path
/// (no `~`, `$`, `..` or glob: nono expands `*` and `**`, which would reach
/// what a literal comparison never saw; nothing under `/proc` or
/// `/dev/fd`), `security.ipc_mode` only at its default and
/// `network.allow_domain` plain strings. Returns every grant, for the
/// root check. `environment`, `meta` and `security.signal_mode` are
/// refused with the messages the runtime's `check_conflicts` gives them.
pub fn check_sandbox_block(
    policy: SandboxPolicy,
    block: &Value,
) -> Result<Vec<BlockGrant>, SandboxRefusal> {
    let Value::Object(top) = block else {
        return Err(SandboxRefusal::new("", "expected a mapping"));
    };
    let mut walk = Walk {
        policy,
        grants: Vec::new(),
    };
    walk.block(top, "", true)?;
    Ok(walk.grants)
}

struct Walk {
    policy: SandboxPolicy,
    grants: Vec<BlockGrant>,
}

fn object<'v>(v: &'v Value, at: &str) -> Result<&'v Map<String, Value>, SandboxRefusal> {
    v.as_object()
        .ok_or_else(|| SandboxRefusal::new(at, "expected an object"))
}

impl Walk {
    fn not_accepted(&self, at: String) -> SandboxRefusal {
        SandboxRefusal::new(at, self.policy.refusal())
    }

    fn keys_in(&self, value: &Value, at: &str, allowed: &[&str]) -> Result<(), SandboxRefusal> {
        match object(value, at)?
            .keys()
            .find(|k| !allowed.contains(&k.as_str()))
        {
            Some(k) => Err(self.not_accepted(format!("{at}.{k}"))),
            None => Ok(()),
        }
    }

    /// The top level (`top`) or one `platform_overrides.<os>` patch.
    fn block(
        &mut self,
        u: &Map<String, Value>,
        prefix: &str,
        top: bool,
    ) -> Result<(), SandboxRefusal> {
        for (key, value) in u {
            let at = format!("{prefix}{key}");
            match key.as_str() {
                "environment" if top => return Err(SandboxRefusal::new(at, ENVIRONMENT_OWNED)),
                "meta" if top => return Err(SandboxRefusal::new(at, META_OWNED)),
                "filesystem" => self.filesystem(object(value, &at)?, &at)?,
                "network" => self.network(value, &at)?,
                "security" => self.security(value, &at)?,
                "platform_overrides" if top => {
                    for (os, patch) in object(value, &at)? {
                        let os_at = format!("{at}.{os}");
                        if !SANDBOX_OSES.contains(&os.as_str()) {
                            return Err(self.not_accepted(os_at));
                        }
                        self.block(object(patch, &os_at)?, &format!("{os_at}."), false)?;
                    }
                }
                _ => return Err(self.not_accepted(at)),
            }
        }
        Ok(())
    }

    fn security(&self, value: &Value, at: &str) -> Result<(), SandboxRefusal> {
        let security = object(value, at)?;
        if security.contains_key("signal_mode") {
            return Err(SandboxRefusal::new(
                format!("{at}.signal_mode"),
                SIGNAL_MODE_OWNED,
            ));
        }
        self.keys_in(value, at, SECURITY_KEYS)?;
        match security.get("ipc_mode") {
            Some(mode) if mode.as_str() != Some(IPC_MODE) => Err(SandboxRefusal::new(
                format!("{at}.ipc_mode"),
                format!("only {IPC_MODE:?} is accepted"),
            )),
            _ => Ok(()),
        }
    }

    fn network(&self, value: &Value, at: &str) -> Result<(), SandboxRefusal> {
        self.keys_in(value, at, NETWORK_KEYS)?;
        let Some(domains) = object(value, at)?.get("allow_domain") else {
            return Ok(());
        };
        let list_at = format!("{at}.allow_domain");
        let Value::Array(items) = domains else {
            return Err(SandboxRefusal::new(list_at, "expected an array"));
        };
        match items.iter().position(|d| !d.is_string()) {
            Some(i) => Err(SandboxRefusal::new(
                format!("{list_at}[{i}]"),
                "expected a domain string; endpoint rules are not accepted",
            )),
            None => Ok(()),
        }
    }

    fn filesystem(&mut self, fs: &Map<String, Value>, at: &str) -> Result<(), SandboxRefusal> {
        for (key, entries) in fs {
            let list_at = format!("{at}.{key}");
            let Some(access) = self.policy.access(key) else {
                return Err(self.not_accepted(list_at));
            };
            let Value::Array(items) = entries else {
                return Err(SandboxRefusal::new(list_at, "expected an array"));
            };
            for (i, item) in items.iter().enumerate() {
                let entry_at = format!("{list_at}[{i}]");
                let raw = entry_text(item).ok_or_else(|| {
                    SandboxRefusal::new(&entry_at, "expected a path string or { path, when }")
                })?;
                let path = literal_path(raw).map_err(|m| SandboxRefusal::new(&entry_at, m))?;
                if let Some(access) = access {
                    self.grants.push(BlockGrant {
                        at: entry_at,
                        raw: raw.to_string(),
                        path,
                        access,
                    });
                }
            }
        }
        Ok(())
    }
}

/// The path of a nono `ConditionalPath`: a string, or an object with a
/// string `path` and at most a `when` beside it. Anything else is refused
/// here rather than left to nono's validation.
fn entry_text(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        Value::Object(m) if m.keys().all(|k| k == "path" || k == "when") => {
            m.get("path").and_then(Value::as_str)
        }
        _ => None,
    }
}

/// An entry nono will not expand: absolute, no `~`, `$`, `..` or glob,
/// and not under `REFUSED_PREFIXES`.
fn literal_path(raw: &str) -> Result<PathBuf, String> {
    if raw.starts_with('~') {
        return Err(format!("{raw}: `~` is not expanded; name an absolute path"));
    }
    if raw.contains('$') {
        return Err(format!(
            "{raw}: variables are not expanded; name an absolute path"
        ));
    }
    if raw.contains(['*', '?', '[', '{']) {
        return Err(format!(
            "{raw}: glob patterns (`*`, `?`, `[`, `{{`) are not accepted; name a literal path"
        ));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(format!("{raw}: must be an absolute path"));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(format!("{raw}: must not contain `..`"));
    }
    if REFUSED_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Err(format!(
            "{raw}: paths under /proc and /dev/fd are not accepted"
        ));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn refused(policy: SandboxPolicy, block: Value) -> String {
        check_sandbox_block(policy, &block).unwrap_err().to_string()
    }

    /// Every key outside the allowlist is refused by its path, with the
    /// policy's wording; the fleet's list adds the socket grants only.
    #[test]
    fn keys_outside_the_allowlist_are_refused_by_path() {
        let refused_both = [
            (json!({ "extends": "default" }), "extends"),
            (json!({ "groups": { "include": ["x"] } }), "groups"),
            (json!({ "packs": ["a/b"] }), "packs"),
            (json!({ "workdir": { "access": "readwrite" } }), "workdir"),
            (json!({ "hooks": {} }), "hooks"),
            (json!({ "session_hooks": {} }), "session_hooks"),
            (json!({ "credential_capture": {} }), "credential_capture"),
            (
                json!({ "credential_providers": {} }),
                "credential_providers",
            ),
            (json!({ "credential_routes": {} }), "credential_routes"),
            (json!({ "env_credentials": {} }), "env_credentials"),
            (
                json!({ "unsafe_macos_seatbelt_rules": [] }),
                "unsafe_macos_seatbelt_rules",
            ),
            (json!({ "command_policies": {} }), "command_policies"),
            (json!({ "commands": {} }), "commands"),
            (json!({ "binary": "/bin/sh" }), "binary"),
            (json!({ "linux": { "af_unix_mediation": "off" } }), "linux"),
            (
                json!({ "filesystem": { "bypass_protection": ["/x"] } }),
                "filesystem.bypass_protection",
            ),
            (
                json!({ "filesystem": { "suppress_save_prompt": ["/x"] } }),
                "filesystem.suppress_save_prompt",
            ),
            (
                json!({ "network": { "custom_credentials": {} } }),
                "network.custom_credentials",
            ),
            (
                json!({ "network": { "credentials": ["github"] } }),
                "network.credentials",
            ),
            (
                json!({ "network": { "tls_intercept": {} } }),
                "network.tls_intercept",
            ),
            (
                json!({ "security": { "process_info_mode": "allow_all" } }),
                "security.process_info_mode",
            ),
            (
                json!({ "security": { "capability_elevation": true } }),
                "security.capability_elevation",
            ),
            (
                json!({ "security": { "approval_backends": {} } }),
                "security.approval_backends",
            ),
            (
                json!({ "platform_overrides": { "linux": { "hooks": {} } } }),
                "platform_overrides.linux.hooks",
            ),
            (
                json!({ "platform_overrides": { "linux": { "extends": "x" } } }),
                "platform_overrides.linux.extends",
            ),
            (
                json!({ "platform_overrides": { "macos": { "command_policies": {} } } }),
                "platform_overrides.macos.command_policies",
            ),
            (
                json!({ "platform_overrides": { "linux": { "network": { "credentials": [] } } } }),
                "platform_overrides.linux.network.credentials",
            ),
            (
                json!({ "platform_overrides": { "linux": { "security": { "process_info_mode": "x" } } } }),
                "platform_overrides.linux.security.process_info_mode",
            ),
            (
                json!({ "platform_overrides": { "linux": { "filesystem": { "bypass_protection": [] } } } }),
                "platform_overrides.linux.filesystem.bypass_protection",
            ),
            (
                json!({ "platform_overrides": { "linux": { "environment": {} } } }),
                "platform_overrides.linux.environment",
            ),
            (
                json!({ "platform_overrides": { "linux": { "meta": {} } } }),
                "platform_overrides.linux.meta",
            ),
            (
                json!({ "platform_overrides": { "linux": { "platform_overrides": {} } } }),
                "platform_overrides.linux.platform_overrides",
            ),
            (
                json!({ "platform_overrides": { "beos": {} } }),
                "platform_overrides.beos",
            ),
        ];
        for policy in [SandboxPolicy::Plugin, SandboxPolicy::Fleet] {
            for (block, at) in &refused_both {
                assert_eq!(
                    refused(policy, block.clone()),
                    format!("{at}: {}", policy.refusal())
                );
            }
        }
        for key in UNIX_SOCKET_KEYS {
            assert_eq!(
                refused(
                    SandboxPolicy::Plugin,
                    json!({ "filesystem": { *key: ["/run/x"] } })
                ),
                format!("filesystem.{key}: not accepted in a plugin's sandbox block")
            );
        }
        assert_eq!(
            SandboxPolicy::Fleet.refusal(),
            "not accepted in a fleet's sandbox block"
        );
    }

    #[test]
    fn the_block_and_its_sections_must_be_mappings() {
        for policy in [SandboxPolicy::Plugin, SandboxPolicy::Fleet] {
            assert_eq!(refused(policy, json!([1])), ": expected a mapping");
            assert_eq!(refused(policy, Value::Null), ": expected a mapping");
            assert_eq!(
                refused(policy, json!({ "network": [] })),
                "network: expected an object"
            );
            assert_eq!(
                refused(policy, json!({ "filesystem": { "read": "/x" } })),
                "filesystem.read: expected an array"
            );
            assert_eq!(
                refused(policy, json!({ "platform_overrides": { "linux": 1 } })),
                "platform_overrides.linux: expected an object"
            );
        }
    }

    #[test]
    fn paths_must_be_literal_and_absolute_in_every_list() {
        for (entry, why) in [
            ("~/.local/state/balerix", "`~` is not expanded"),
            ("$HOME/x", "variables are not expanded"),
            ("/opt/${X}", "variables are not expanded"),
            ("relative/dir", "must be an absolute path"),
            ("/opt/../etc", "must not contain `..`"),
            ("/home/*/.local/state/balerix/**", "glob patterns"),
            ("/opt/data?", "glob patterns"),
            ("/opt/[ab]", "glob patterns"),
            ("/opt/{a,b}", "glob patterns"),
        ] {
            for key in ["read", "allow_file", "deny", "unix_socket_subtree_bind"] {
                let e = refused(
                    SandboxPolicy::Fleet,
                    json!({ "filesystem": { key: ["/ok", entry] } }),
                );
                assert!(e.starts_with(&format!("filesystem.{key}[1]: ")), "{e}");
                assert!(e.contains(why), "{key} {entry}: {e}");
            }
        }
        let e = refused(
            SandboxPolicy::Fleet,
            json!({ "platform_overrides": { "linux": { "filesystem": { "read": ["/st*/**"] } } } }),
        );
        assert!(
            e.starts_with("platform_overrides.linux.filesystem.read[0]: "),
            "{e}"
        );
    }

    #[test]
    fn a_path_entry_of_another_shape_is_refused() {
        for entry in [
            json!(42),
            json!(null),
            json!(["/x"]),
            json!({}),
            json!({ "path": 1 }),
            json!({ "path": "/x", "mode": "rw" }),
        ] {
            assert_eq!(
                refused(
                    SandboxPolicy::Fleet,
                    json!({ "filesystem": { "read": ["/ok", entry] } })
                ),
                "filesystem.read[1]: expected a path string or { path, when }"
            );
        }
    }

    #[test]
    fn every_accepted_key_passes_and_the_grants_come_back() {
        let fs = json!({
            "read": ["/opt/lib", { "path": "/opt/data", "when": "linux" }],
            "read_file": ["/etc/hosts"],
            "allow": ["/var/tmp/a"],
            "allow_file": ["/var/tmp/x.db"],
            "write": ["/var/tmp/out"],
            "write_file": ["/var/tmp/out.log"],
            "deny": ["/srv/secret"],
        });
        let network = json!({
            "block": false, "allow_http2": true, "network_profile": "dev",
            "allow_domain": ["example.com"], "deny_domain": ["evil.example"],
            "open_port": [8080], "open_port_range": [[9000, 9010]], "connect_port": [443],
            "listen_port": [8081], "listen_port_range": [[9100, 9110]],
            "no_proxy": [".internal"], "upstream_proxy": "proxy:3128", "upstream_bypass": ["*.corp"],
        });
        let block = json!({
            "filesystem": fs, "network": network, "security": { "ipc_mode": "shared_memory_only" },
            "platform_overrides": {
                "linux": { "filesystem": { "unix_socket": ["/run/s.sock"] }, "security": { "ipc_mode": "shared_memory_only" } },
                "macos": { "filesystem": { "read": ["/Library/X"] } },
                "windows": {},
            }
        });
        let grants = check_sandbox_block(SandboxPolicy::Fleet, &block).unwrap();
        let seen: Vec<(&str, &str, Access)> = grants
            .iter()
            .map(|g| (g.at.as_str(), g.raw.as_str(), g.access))
            .collect();
        assert_eq!(
            seen,
            [
                ("filesystem.allow[0]", "/var/tmp/a", Access::Write),
                ("filesystem.allow_file[0]", "/var/tmp/x.db", Access::Write),
                ("filesystem.read[0]", "/opt/lib", Access::Read),
                ("filesystem.read[1]", "/opt/data", Access::Read),
                ("filesystem.read_file[0]", "/etc/hosts", Access::Read),
                ("filesystem.write[0]", "/var/tmp/out", Access::Write),
                (
                    "filesystem.write_file[0]",
                    "/var/tmp/out.log",
                    Access::Write
                ),
                (
                    "platform_overrides.linux.filesystem.unix_socket[0]",
                    "/run/s.sock",
                    Access::Socket
                ),
                (
                    "platform_overrides.macos.filesystem.read[0]",
                    "/Library/X",
                    Access::Read
                ),
            ]
        );
        // the plugin's list lacks only the socket grants
        let mut plugin = block.clone();
        plugin["platform_overrides"]["linux"]["filesystem"] = json!({});
        check_sandbox_block(SandboxPolicy::Plugin, &plugin).unwrap();
        check_sandbox_block(SandboxPolicy::Fleet, &json!({})).unwrap();
    }

    /// #187 review: the keys balerix owns are refused here too, at apply
    /// time, with the messages the runtime's `check_conflicts` gives at
    /// render (which stays, as the second layer).
    #[test]
    fn balerix_owned_keys_are_refused_with_their_own_messages() {
        for policy in [SandboxPolicy::Plugin, SandboxPolicy::Fleet] {
            for (block, expected) in [
                (
                    json!({ "environment": {} }),
                    "environment: balerix-owned; set variables through `env` instead",
                ),
                (json!({ "meta": { "name": "x" } }), "meta: balerix-owned"),
                (
                    json!({ "security": { "signal_mode": "allow_all" } }),
                    "security.signal_mode: balerix-owned; agents stay signal-isolated",
                ),
                (
                    json!({ "platform_overrides": { "linux": { "security": { "signal_mode": null } } } }),
                    "platform_overrides.linux.security.signal_mode: balerix-owned; agents stay \
                     signal-isolated",
                ),
            ] {
                assert_eq!(refused(policy, block), expected);
            }
        }
    }

    /// `ipc_mode: full` lifts nono's IPC scoping; only the default is
    /// accepted, whoever's block it is.
    #[test]
    fn ipc_mode_is_accepted_only_at_its_default() {
        for policy in [SandboxPolicy::Plugin, SandboxPolicy::Fleet] {
            for value in [json!("full"), json!("other"), json!(null), json!(1)] {
                assert_eq!(
                    refused(policy, json!({ "security": { "ipc_mode": value } })),
                    "security.ipc_mode: only \"shared_memory_only\" is accepted"
                );
            }
            assert_eq!(
                refused(
                    policy,
                    json!({ "platform_overrides": { "macos": { "security": { "ipc_mode": "full" } } } })
                ),
                "platform_overrides.macos.security.ipc_mode: only \"shared_memory_only\" is accepted"
            );
            check_sandbox_block(
                policy,
                &json!({ "security": { "ipc_mode": "shared_memory_only" } }),
            )
            .unwrap();
        }
    }

    /// An `allow_domain` object with `endpoints` turns on nono's TLS
    /// interception, which `tls_intercept` is refused for.
    #[test]
    fn allow_domain_takes_strings_only() {
        for policy in [SandboxPolicy::Plugin, SandboxPolicy::Fleet] {
            assert_eq!(
                refused(
                    policy,
                    json!({ "network": { "allow_domain": ["a.example", { "domain": "b.example", "endpoints": [] }] } })
                ),
                "network.allow_domain[1]: expected a domain string; endpoint rules are not accepted"
            );
            assert_eq!(
                refused(
                    policy,
                    json!({ "network": { "allow_domain": "a.example" } })
                ),
                "network.allow_domain: expected an array"
            );
            assert!(
                refused(
                    policy,
                    json!({ "platform_overrides": { "linux": { "network": { "allow_domain": [{}] } } } })
                )
                .starts_with("platform_overrides.linux.network.allow_domain[0]: ")
            );
        }
    }

    #[test]
    fn proc_and_dev_fd_are_refused() {
        for entry in [
            "/proc",
            "/proc/1/root",
            "/proc/self/fd/3",
            "/dev/fd",
            "/dev/fd/0",
        ] {
            for key in ["read", "deny", "unix_socket"] {
                let e = refused(
                    SandboxPolicy::Fleet,
                    json!({ "filesystem": { key: [entry] } }),
                );
                assert_eq!(
                    e,
                    format!(
                        "filesystem.{key}[0]: {entry}: paths under /proc and /dev/fd are not accepted"
                    )
                );
            }
        }
        check_sandbox_block(
            SandboxPolicy::Fleet,
            &json!({ "filesystem": { "read": ["/procfs", "/dev/null", "/dev/fdx"] } }),
        )
        .unwrap();
    }

    #[test]
    fn a_refusal_is_named_under_its_documents_path() {
        let r = SandboxRefusal::new("filesystem.allow[0]", "x");
        assert_eq!(
            r.path_under("crews.c.agents.a.sandbox"),
            "crews.c.agents.a.sandbox.filesystem.allow[0]"
        );
        assert_eq!(r.path_under(""), "filesystem.allow[0]");
        assert_eq!(
            SandboxRefusal::new("", "x").path_under("sandbox"),
            "sandbox"
        );
    }
}
