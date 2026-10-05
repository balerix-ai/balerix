# Plugins Without a Cluster (Spec O 4a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Daemon in Kubernetes mode host plugins it does not launch, and make the plugin SDK able to run in a pod. Concretely:
- `PUT /v1/plugins` declares the list.
- `hello` carries the manifest and is checked against a grant.
- Both directions speak TLS under the Daemon's authority.
- Readiness survives a Daemon restart.
- Managed fleets are stored for the operator at `GET /v1/managed-fleets`.

All of it is proved without a cluster.

**Architecture:**
- **`balerix-api`** gains the wire types: the manifest in `hello`, the plugin list, managed-fleet rows, and `managed_by`.
- **`balerix-plugin-sdk`** gains file-backed inputs and TLS on both its client and its server. TLS is rustls on ring, trusting one authority file.
- **`balerix-server`:**
  - `PluginHost` becomes one of two adapters behind a closed `PluginSource` enum. The other is `kube::DeclaredPlugins`, which holds the list, checks `hello`, persists accepted hellos and drives readiness from the health poll.
  - `kube::ManagedStore` keeps the plugins' managed-fleet requests on the state directory.
  - The plugin client and the route proxy learn `https://` with the Daemon's authority.
- **`balerix`** gains `serve --tls-ca`, and `dev fake-plugin` moves onto the SDK's `serve`.
- Two-process tests run the real `balerix serve --mode kubernetes` against the real `balerix dev fake-plugin` over TLS.

**Tech Stack:**
- Rust 2024.
- TLS: rustls 0.23.45 (ring); axum-server 0.8.0 (`tls-rustls-no-provider`); reqwest 0.13.4 (`rustls-no-provider`, `use_preconfigured_tls`); hyper-rustls 0.27.10 (`http1`, `tls12`, `logging`, `ring`); tokio-tungstenite 0.30.0 (`rustls-tls-webpki-roots`, for `Connector::Rustls`).
- Tests: rcgen 0.14.10 for throwaway authorities.
- All of these were probed together on 2026-10-05: one authority, an axum-server TLS listener, and a 200 through reqwest, a 200 through hyper-util's legacy client on hyper-rustls, and a 101 through tungstenite.

**Spec:** `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` §23 (with §9, §7.4, §10.3). Read §23 before any task.

**Decisions this plan makes beyond §23.** Task 9 records them as §23.7.
- **`Plugin::manifest()` returns `Option<&'static str>` and defaults to `None`.** It is not a required method, so the SDK's own test plugins and third-party plugins keep compiling. Kubernetes mode refuses a `hello` without a manifest anyway.
- **The SDK sends the manifest only when it was given an authority (`BALERIX_CA_FILE`).** `HelloRequest` is `deny_unknown_fields`, so a released one-machine Daemon (0.2.0) would answer 400 to a `hello` carrying it. On one machine the manifest is ignored, so nothing is lost.
- **`PluginSource` is a closed enum (`Packages`, `Declared`), not a trait object.** There are two adapters and their methods are async; the enum needs no boxing.
- **`PUT /v1/plugins` registers every plugin at once,** with a placeholder manifest (no needs, no hooks, not ready). A fleet that names the plugin then applies with its pairs `pending`, as on one machine, where `sync` registers before any `hello`. Without this, an apply answers `no plugin "x" is installed` until the plugin's `hello`.
- **`serve --tls-ca <file>` is optional in Kubernetes mode.** The Daemon pods 3b ships do not mount it until 4b. Without it, `PUT /v1/plugins` is 409 `this daemon was started without --tls-ca; it cannot call plugins`.
- **`PluginAddr.listen` may hold a URL.** `PluginAddr::base()` gives `https://…` as-is and prefixes a bare `host:port` with `http://`. The client and the proxy build their URLs from it.
- **The proxy uses hyper-rustls's `https_or_http` connector.** reqwest's rustls feature already brings it into the lock. Loopback `http://` keeps working through the same client.
- **In Kubernetes mode a plugin's `DELETE fleets/{name}` still downs the fleet** through `down_as` (the mirror sends `stop`) and also marks the stored request down. A down request is kept until the plugin applies that name again, the plugin is dropped, or the record is purged.
- **`Caller::Kubernetes` becomes `Caller::Kubernetes { managed_by: Option<AgentName> }`.**
- **`dev fake-plugin` uses `serve`.** `configure` writes the hello file and spawns the manage step, so the fake gets TLS and the manifest with no code of its own.

## Global Constraints

- **Branch:** `feat/kube-plugins-offline`, cut from `docs/spec-o-4-plugins`. Make one commit per task with a conventional message (`feat(api): …`, `feat(sdk): …`, `feat(server): …`, `test(balerix): …`, `docs(spec-o): …`).
- **Protocol:** `PLUGIN_PROTOCOL` stays `1`.
- **Plugin port in a pod:** `7644`. `BALERIX_PLUGIN_LISTEN` defaults to `127.0.0.1:0`.
- **New SDK variables:** `BALERIX_PLUGIN_TOKEN_FILE`, `BALERIX_CA_FILE`, `BALERIX_PLUGIN_TLS_CERT`, `BALERIX_PLUGIN_TLS_KEY`, `BALERIX_PLUGIN_LISTEN`.
- **Tokens** are at least 32 characters, for list entries as for `agent_tokens`.
- **Health:** the poll stays `HEALTH_INTERVAL = 10 s`. A declared plugin leaves ready after `MISSED_POLLS = 3` consecutive failures.
- **Persisted files**, written atomically (write a sibling `.tmp`, then rename):
  - `<state>/plugins/<name>/hello.json` is `{"entry": "<sha256 hex of the entry>", "manifest": {…}}`.
  - `<state>/managed/<name>.json` is `{"name", "plugin", "file", "down"?}`.
- **Exact error texts:**
  - `hello.manifest: required in kubernetes mode`
  - `hello.manifest.name: "<m>" does not match the plugin "<p>"`
  - `hello.manifest.needs: <cap> is not granted`
  - `this daemon reads plugins.yaml` (409, tmux mode `PUT /v1/plugins`)
  - `this daemon is in kubernetes mode; change its plugins through the Daemon's spec.plugins` (409, `POST /v1/plugins/sync` and `DELETE /v1/plugins/{name}`)
  - `this daemon was started without --tls-ca; it cannot call plugins` (409)
  - `plugins[<i>].url: must be https://`
  - `plugins[<i>].token: a token is at least 32 characters`
  - `plugins[<i>].name: <reason>`
  - `plugins[<i>].name: listed twice`
- **TLS:**
  - The ring provider is installed with `let _ = rustls::crypto::ring::default_provider().install_default();` before any config.
  - Client configs use `builder_with_provider(ring)`, `with_safe_default_protocol_versions()`, a `RootCertStore` holding only the authority file's certificates, and `with_no_client_auth()`.
  - Nothing uses webpki or native roots, even where a feature pulls them in.
- **`scripts/check-core-deps.sh`:** core reqwest features become exactly `__rustls,__tls,json,rustls-no-provider`, compared under `LC_ALL=C`.
- **Code rules:**
  - Non-test code has no `unwrap`/`expect`, and clippy runs with `-D warnings`.
  - `#![forbid(unsafe_code)]` holds.
  - Library errors are `thiserror`.
  - Comments are sparse and say *why*, citing `§23.x` in the surrounding style.
  - Secrets (tokens, config) never appear in `Debug` output.
- **Gates:**
  - Before every commit, run `mise run check`: fmt, clippy, `check-core-deps`, the core nextest suite, `package-plugins flow web`.
  - A task that touches `plugins/*` also runs `scripts/plugin.sh check <name>` for each plugin it touches.
  - A task that touches `operator/` also runs `mise run operator`.
- **Focused runs** (from `/workspace`): `cargo nextest run -p <crate> -E 'test(<name>)'`. For integration tests: `cargo nextest run -p balerix-server --test <file> -E 'test(<name>)'`.
- **One machine is unchanged:** `plugins_it`, `events_it`, `manage_it`, `protocol_it`, and the e2e `plugin_manage_journey` stay green with no edits beyond added struct fields.

## Review Focus

1. **A plugin built on this SDK, run by a released 0.2.0 one-machine Daemon, must still get through `hello`.** The body may carry no `manifest` key when no authority is configured. Pinned in Task 3: a test serialises the `hello` body without a CA and asserts the key is absent.
2. **The web terminal's WebSocket through the Daemon's proxy must upgrade over TLS.** The proxy copies upgraded bytes from a hyper-rustls stream; a 101 that never completes would hang the browser. Pinned in Task 4: a proxied `wss` echo through a TLS stub plugin.
3. **One missed health poll (a plugin pod's brief GC pause, a slow node) must not take a plugin out of the interceptor chain.** It takes three in a row. A plugin that comes back gets its pairs activated again. Pinned in Task 5's readiness test.
4. **After a Daemon restart, an entry the operator changed while the Daemon was down** (a new grant, config or token) must not be treated as hello'd. Only an unchanged entry's hello is restored. Pinned in Task 5's restore test.
5. **A Daemon running without `--tls-ca` must refuse `PUT /v1/plugins`** at once with the 409 text above. It must not accept the list and then fail every call with an opaque TLS error. Pinned in Task 6.

---

## File structure

| File | Change | Responsibility |
|---|---|---|
| `crates/balerix-api/src/plugin.rs` | modify | `HelloRequest.manifest`, `DeclaredPlugin`, `DeclaredPlugins`, `ManagedFleet` |
| `crates/balerix-api/src/request.rs` | modify | `FleetRequest.managed_by` |
| `crates/balerix-core/src/plugin.rs` | modify | `reserved_plugin_reason` |
| `Cargo.toml` (workspace) | modify | TLS features on reqwest / tokio-tungstenite, `hyper-rustls` |
| `scripts/check-core-deps.sh` | modify | the new exact reqwest feature set |
| `crates/balerix-plugin-sdk/src/lib.rs` | modify | `Env`'s new inputs |
| `crates/balerix-plugin-sdk/src/tls.rs` | create | the authority-only client config and the serving config |
| `crates/balerix-plugin-sdk/src/host.rs` | modify | TLS client, `wss://`, manifest in `hello` |
| `crates/balerix-plugin-sdk/src/plugin.rs` | modify | `Plugin::manifest`, bind to `Env.listen`, TLS serving |
| `plugins/{flow,web,matrix,github}/src/*` | modify | `fn manifest()` via `include_str!` |
| `crates/balerix-server/src/plugins/client.rs` | modify | `PluginClient::new(ca)`, URLs from `PluginAddr::base` |
| `crates/balerix-server/src/plugins/registry.rs` | modify | `PluginAddr::base`, `declare`, `accept_hello` |
| `crates/balerix-server/src/proxy.rs` | modify | hyper-rustls connector, URL from `base` |
| `crates/balerix-server/src/kube/declared.rs` | create | `DeclaredPlugins`: list, hello checks, persistence, readiness |
| `crates/balerix-server/src/kube/managed.rs` | create | `ManagedStore`: plugins' managed-fleet requests |
| `crates/balerix-server/src/plugins/source.rs` | create | `PluginSource` enum and `PluginSetup` |
| `crates/balerix-server/src/daemon.rs` | modify | dispatch through `PluginSource`, `Caller::Kubernetes { managed_by }`, managed requests |
| `crates/balerix-server/src/api.rs` | modify | `PUT /v1/plugins`, `GET /v1/managed-fleets`, the 409s, `managed_by` |
| `crates/balerix-server/src/testing.rs` | modify | `PluginSetup` in the harness, TLS stub plugin |
| `crates/balerix/src/cli.rs`, `commands/serve.rs` | modify | `--tls-ca` |
| `crates/balerix/src/commands/dev.rs` | modify | fake plugin on `serve`, its manifest |
| `crates/balerix/tests/cli_kube_plugins.rs` | create | the two-process tests |
| `operator/src/desired/fleet.rs`, `operator/tests/client_it.rs` | modify | `managed_by: None` in literals |
| `docs/plugin-protocol.md`, `docs/THREAT-MODEL.md`, spec §23.7 | modify | docs |

---

### Task 1: Wire types and the reserved plugin name

**Files:**
- Modify: `crates/balerix-api/src/plugin.rs` (`HelloRequest` at ~line 106; add the new types after `HelloResponse`)
- Modify: `crates/balerix-api/src/request.rs:12-36` (`FleetRequest`)
- Modify: `crates/balerix-api/src/lib.rs` (re-exports)
- Modify: `crates/balerix-core/src/plugin.rs` (after `reserved_fleet_reason`)
- Modify: `crates/balerix-server/src/plugins/host.rs` (`PluginHost::resolve`, refuse a reserved plugin name)
- Modify: every `FleetRequest { … }` literal: `crates/balerix/src/commands/fleet.rs`; `crates/balerix-server/tests/{streams_it,workspace_it,manage_it,events_it,plugins_it,api_it}.rs`; `operator/src/desired/fleet.rs`; `operator/tests/client_it.rs`
- Modify: every `HelloRequest { … }` literal: `crates/balerix-plugin-sdk/src/host.rs:130`, `crates/balerix-api/src/plugin.rs:272`, `crates/balerix-server/src/daemon.rs:1382`

**Interfaces:**
- Produces:
  - `balerix_api::HelloRequest { name, version, protocol, listen, manifest: Option<PluginManifest> }`, which is no longer `Eq`.
  - `balerix_api::DeclaredPlugin { name: String, grant: BTreeSet<Capability>, config: Value, fleet_defaults: Value, token: String, url: String }`.
  - `balerix_api::DeclaredPlugins { plugins: Vec<DeclaredPlugin> }`.
  - `balerix_api::ManagedFleet { name: String, plugin: String, file: Value, down: Option<DownQuery> }`.
  - `FleetRequest.managed_by: Option<String>`.
  - `balerix_core::reserved_plugin_reason(name: &str) -> Option<&'static str>`.

- [ ] **Step 1: Cut the branch**

```bash
cd /workspace && git switch docs/spec-o-4-plugins && git switch -c feat/kube-plugins-offline
```

- [ ] **Step 2: Write the failing tests.** Append to the `#[cfg(test)] mod tests` in `crates/balerix-api/src/plugin.rs`:

```rust
#[test]
fn a_hello_without_a_manifest_still_parses_and_serialises_without_the_key() {
    let old = r#"{"name":"web","version":"0.2.1","protocol":1,"listen":"127.0.0.1:9"}"#;
    let h: HelloRequest = serde_json::from_str(old).unwrap();
    assert_eq!(h.manifest, None);
    let back = serde_json::to_value(&h).unwrap();
    assert!(back.get("manifest").is_none(), "{back}");
}

#[test]
fn a_hello_carries_the_manifest_when_given() {
    let m: PluginManifest = serde_norway::from_str(
        "apiVersion: balerix/v1\nkind: Plugin\nname: flow\nversion: 0.1.1\nprotocol: 1\nstart: serve\nneeds: [actions, kv]\n",
    )
    .unwrap();
    let h = HelloRequest {
        name: "flow".into(),
        version: "0.1.1".into(),
        protocol: PLUGIN_PROTOCOL,
        listen: "0.0.0.0:7644".into(),
        manifest: Some(m.clone()),
    };
    let v = serde_json::to_value(&h).unwrap();
    assert_eq!(v["manifest"]["needs"], serde_json::json!(["actions", "kv"]));
    let back: HelloRequest = serde_json::from_value(v).unwrap();
    assert_eq!(back.manifest, Some(m));
}

#[test]
fn a_declared_plugin_parses_redacts_and_refuses_an_unknown_capability() {
    let body = serde_json::json!({ "plugins": [{
        "name": "flow", "grant": ["actions", "kv"], "config": { "secret": "s3cret" },
        "fleetDefaults": { "claude": { "binary": "fake-claude" } },
        "token": "t0123456789abcdef0123456789abcdef", "url": "https://flow.ns.svc:7644"
    }]});
    let d: DeclaredPlugins = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(d.plugins[0].grant, BTreeSet::from([Capability::Actions, Capability::Kv]));
    let dbg = format!("{:?}", d.plugins[0]);
    assert!(!dbg.contains("s3cret") && !dbg.contains("t0123456789"), "{dbg}");
    let mut bad = body;
    bad["plugins"][0]["grant"] = serde_json::json!(["root"]);
    let err = serde_json::from_value::<DeclaredPlugins>(bad).unwrap_err().to_string();
    assert!(err.contains("unknown variant `root`"), "{err}");
}

#[test]
fn a_managed_fleet_row_omits_down_while_live() {
    let row = ManagedFleet {
        name: "gh-1".into(),
        plugin: "github".into(),
        file: serde_json::json!({ "crews": {} }),
        down: None,
    };
    let v = serde_json::to_value(&row).unwrap();
    assert!(v.get("down").is_none(), "{v}");
}
```

Append this to `crates/balerix-core/src/plugin.rs` tests:

```rust
#[test]
fn kubernetes_is_not_a_plugin_name() {
    assert!(reserved_plugin_reason("kubernetes").is_some());
    assert_eq!(reserved_plugin_reason("flow"), None);
}
```

- [ ] **Step 3: Run them to see them fail.**

Run: `cargo nextest run -p balerix-api -p balerix-core -E 'test(manifest) | test(declared) | test(managed_fleet_row) | test(kubernetes_is_not)'`
Expected: FAIL to compile (`manifest`, `DeclaredPlugins`, `ManagedFleet`, `reserved_plugin_reason` not found).

- [ ] **Step 4: Implement.** In `crates/balerix-api/src/plugin.rs`:

```rust
/// Body of `POST /v1/plugin-host/hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloRequest {
    pub name: String,
    pub version: String,
    pub protocol: u32,
    /// `127.0.0.1:<port>` the plugin listens on. Ignored in Kubernetes
    /// mode, where the Daemon calls the address the operator gave (Spec O
    /// §23.1).
    pub listen: String,
    /// The plugin's own `balerix-plugin.yaml` (Spec O §23.1). Required by a
    /// Daemon in Kubernetes mode, which checks its `needs` against the
    /// grant; ignored on one machine, which reads the package. Sent only
    /// when the plugin has an authority to trust: a 0.2.0 daemon refuses
    /// unknown fields here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<PluginManifest>,
}

/// One entry of `PUT /v1/plugins` (Spec O §23.2): what the operator knows
/// of a plugin it runs. Order in the list is interceptor order.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredPlugin {
    pub name: String,
    /// The capabilities the manifest's `needs` must stay within.
    pub grant: BTreeSet<Capability>,
    /// Daemon-level config with secrets injected, handed back in `hello`.
    #[serde(default = "empty_object")]
    pub config: Value,
    #[serde(
        default = "empty_object",
        rename = "fleetDefaults",
        skip_serializing_if = "is_empty_object"
    )]
    pub fleet_defaults: Value,
    /// The plugin's token: its bearer to the Daemon and the Daemon's to it.
    pub token: String,
    /// `https://<plugin>.<namespace>.svc:7644`.
    pub url: String,
}

/// Hand-written: `token` and `config` hold secrets.
impl std::fmt::Debug for DeclaredPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclaredPlugin")
            .field("name", &self.name)
            .field("grant", &self.grant)
            .field("config", &"<redacted>")
            .field("fleet_defaults", &self.fleet_defaults)
            .field("token", &"<redacted>")
            .field("url", &self.url)
            .finish()
    }
}

/// Body of `PUT /v1/plugins`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredPlugins {
    pub plugins: Vec<DeclaredPlugin>,
}

/// One row of `GET /v1/managed-fleets` (Spec O §23.3): a plugin's
/// unresolved fleet file, for the operator to write as a Fleet. `down` is
/// the plugin's down query once it downed the fleet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedFleet {
    pub name: String,
    pub plugin: String,
    pub file: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down: Option<crate::DownQuery>,
}
```

Add `use std::collections::BTreeSet;` if it is not already imported. Re-export `DeclaredPlugin`, `DeclaredPlugins` and `ManagedFleet` from `lib.rs` next to `HelloRequest`.

In `request.rs`, add this to `FleetRequest` after `agent_tokens`, and add `.field("managed_by", &self.managed_by)` to its `Debug`:

```rust
    /// Spec O §23.3: the plugin a managed Fleet belongs to. Only with
    /// `agent_tokens`; the record keeps that plugin as its owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
```

In `crates/balerix-core/src/plugin.rs`, export it from the crate root as the neighbours are:

```rust
/// Why a plugin may not take `name`, if it may not (Spec O §23.2):
/// `kubernetes` is the owner of the operator's fleets, so a plugin of that
/// name would own them.
pub fn reserved_plugin_reason(name: &str) -> Option<&'static str> {
    match name {
        "kubernetes" => Some("reserved: it is the owner name of the operator's fleets"),
        _ => None,
    }
}
```

In `PluginHost::resolve`, right after the `manifest.name != entry.name` check:

```rust
            if let Some(reason) = balerix_core::reserved_plugin_reason(&entry.name) {
                return Err(entry_error(i, "name", reason.to_string()));
            }
```

Add `manifest: None` to every `HelloRequest` literal and `managed_by: None` to every `FleetRequest` literal listed under **Files**. Find any missed with `grep -rn "agent_tokens: None\|agent_tokens: Some\|HelloRequest {" --include=*.rs crates agent operator plugins`.

- [ ] **Step 5: Run the focused tests, then the gates.**

Run: `cargo nextest run -p balerix-api -p balerix-core -E 'test(manifest) | test(declared) | test(managed_fleet_row) | test(kubernetes_is_not)'`
Expected: PASS.
Run: `mise run check && mise run operator`
Expected: both green.

- [ ] **Step 6: Commit**

```bash
git add -A crates operator
git commit -m "feat(api): the manifest in hello, the plugin list, managed-fleet rows and managed_by (Spec O §23.1–§23.3)"
```

---

### Task 2: TLS in the core dependency set and the SDK's client

**Files:**
- Modify: `Cargo.toml` (workspace dependencies)
- Modify: `scripts/check-core-deps.sh`
- Modify: `crates/balerix-plugin-sdk/Cargo.toml`
- Create: `crates/balerix-plugin-sdk/src/tls.rs`
- Modify: `crates/balerix-plugin-sdk/src/lib.rs` (`Env`, `pub mod tls`)
- Modify: `crates/balerix-plugin-sdk/src/host.rs` (`Host::new`, `ws_url`, `connect`)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `Env` gains `ca: Option<PathBuf>`, `tls: Option<(PathBuf, PathBuf)>` (cert, key) and `listen: String`. `token` is now read from `BALERIX_PLUGIN_TOKEN_FILE` when that is set.
  - `balerix_plugin_sdk::tls::client_config(ca: &Path) -> Result<Arc<rustls::ClientConfig>, SdkError>`.
  - `Host::new(env)` trusts `env.ca` alone when set and then requires `env.api_url` to start with `https://`.

- [ ] **Step 1: Write the failing tests.** In `crates/balerix-plugin-sdk/src/lib.rs` tests:

```rust
#[test]
fn env_reads_the_file_inputs_and_holds_their_rules() {
    let dir = tempfile::tempdir().unwrap();
    let tok = dir.path().join("token");
    std::fs::write(&tok, "tok-from-file\n").unwrap();
    let ca = dir.path().join("ca.crt");
    let mut vars: Vec<(&str, String)> = vec![
        ("BALERIX_API_URL", "https://balerix.ns.svc:7643".into()),
        ("BALERIX_PLUGIN_NAME", "flow".into()),
        ("BALERIX_PLUGIN_TOKEN_FILE", tok.display().to_string()),
        ("BALERIX_PLUGIN_SCRATCH", "/scratch".into()),
        ("BALERIX_CA_FILE", ca.display().to_string()),
        ("BALERIX_PLUGIN_TLS_CERT", "/tls/tls.crt".into()),
        ("BALERIX_PLUGIN_TLS_KEY", "/tls/tls.key".into()),
        ("BALERIX_PLUGIN_LISTEN", "0.0.0.0:7644".into()),
    ];
    let get = |vars: &Vec<(&str, String)>| {
        let vars = vars.clone();
        move |k: &str| vars.iter().find(|(kk, _)| *kk == k).map(|(_, v)| v.clone())
    };
    let e = Env::from_env(get(&vars)).unwrap();
    assert_eq!(e.token, "tok-from-file", "file wins, trimmed");
    assert_eq!(e.ca.as_deref(), Some(ca.as_path()));
    assert_eq!(e.listen, "0.0.0.0:7644");
    assert!(e.tls.is_some());
    // a CA with a plain-http daemon url is refused
    vars[0].1 = "http://balerix:7643".into();
    assert_eq!(
        Env::from_env(get(&vars)).unwrap_err().to_string(),
        "environment: BALERIX_CA_FILE is set, so BALERIX_API_URL must be https://"
    );
    vars[0].1 = "https://balerix.ns.svc:7643".into();
    // a certificate without its key is refused
    vars.retain(|(k, _)| *k != "BALERIX_PLUGIN_TLS_KEY");
    assert_eq!(
        Env::from_env(get(&vars)).unwrap_err().to_string(),
        "environment: BALERIX_PLUGIN_TLS_CERT and BALERIX_PLUGIN_TLS_KEY go together"
    );
}

#[test]
fn env_without_the_new_variables_is_todays_env() {
    let e = Env::from_env(env_of(FULL)).unwrap();
    assert_eq!((e.ca, e.tls, e.listen.as_str()), (None, None, "127.0.0.1:0"));
}
```

In `host.rs` tests (create the module if there is none), add a TLS round trip against a throwaway authority:

```rust
#[cfg(test)]
mod tls_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use axum::Router;
    use axum::routing::post;

    /// (ca.crt, tls.crt, tls.key) for 127.0.0.1 in `dir`.
    pub(crate) fn authority(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca, ca_key);
        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
            .unwrap()
            .signed_by(&key, &issuer)
            .unwrap();
        let paths = (dir.join("ca.crt"), dir.join("tls.crt"), dir.join("tls.key"));
        std::fs::write(&paths.0, ca_cert.pem()).unwrap();
        std::fs::write(&paths.1, leaf.pem()).unwrap();
        std::fs::write(&paths.2, key.serialize_pem()).unwrap();
        paths
    }

    #[tokio::test]
    async fn the_host_client_trusts_the_given_authority_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let (ca, cert, key) = authority(dir.path());
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key).await.unwrap();
        let handle: axum_server::Handle<std::net::SocketAddr> = axum_server::Handle::new();
        let app = Router::new().route(
            "/v1/plugin-host/hello",
            post(|| async { axum::Json(serde_json::json!({ "config": { "ok": true } })) }),
        );
        let h = handle.clone();
        tokio::spawn(async move {
            axum_server::bind_rustls("127.0.0.1:0".parse().unwrap(), config)
                .handle(h)
                .serve(app.into_make_service())
                .await
        });
        let addr = handle.listening().await.unwrap();
        let env = |ca: Option<std::path::PathBuf>| crate::Env {
            api_url: format!("https://{addr}"),
            name: "flow".into(),
            token: "t".into(),
            scratch: dir.path().into(),
            ca,
            tls: None,
            listen: "127.0.0.1:0".into(),
        };
        let host = Host::new(env(Some(ca))).unwrap();
        assert_eq!(host.hello("0.1.0", "x").await.unwrap().config["ok"], true);
        // another authority's file: the handshake fails
        let other = tempfile::tempdir().unwrap();
        let (other_ca, _, _) = authority(other.path());
        let host = Host::new(env(Some(other_ca))).unwrap();
        assert!(matches!(host.hello("0.1.0", "x").await, Err(SdkError::Transport(_))));
    }
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo nextest run -p balerix-plugin-sdk -E 'test(env_) | test(trusts_the_given_authority)'`
Expected: FAIL to compile (no field `ca`, no `rcgen`/`axum_server` in the SDK).

- [ ] **Step 3: Update the dependency set.** In the workspace `Cargo.toml`, rewrite the `reqwest` and `tokio-tungstenite` entries and add `hyper-rustls`. Keep the comment style and say why:

```toml
# Async HTTP for the daemon → plugin calls and the plugin SDK (plugins spec
# §16.1). Spec O §23.1: TLS, trusting one authority, for a plugin in a pod
# and the Daemon that calls it — rustls with no provider of its own (ring
# below, as axum-server). No HTTP/2, no proxy discovery, no root store.
reqwest = { version = "0.13.4", default-features = false, features = ["json", "rustls-no-provider"] }
```

```toml
# The WebSocket client for the SDK and the tests. `rustls-tls-webpki-roots`
# is the feature that exposes `Connector::Rustls` (Spec O §23.1); the
# bundled roots go unused, the authority file is the only trust, as in agent/.
tokio-tungstenite = { version = "0.30.0", features = ["rustls-tls-webpki-roots"] }
# The plugin-route proxy's TLS connector (Spec O §23.1), already in the
# lock through reqwest's rustls feature; `https_or_http` keeps loopback.
hyper-rustls = { version = "0.27.10", default-features = false, features = ["http1", "tls12", "logging", "ring"] }
```

Add these to `crates/balerix-plugin-sdk/Cargo.toml`: under `[dependencies]`, `rustls`, `rustls-pki-types` and `axum-server` (all `{ workspace = true }`); under `[dev-dependencies]`, `rcgen = { workspace = true }`.

In `scripts/check-core-deps.sh`, change the comment's "`json` alone" to the new set and the comparison to:

```bash
have=$(sed -n 's/^[^A-Za-z]*reqwest feature "\([^"]*\)".*/\1/p' <<<"$tree" | LC_ALL=C sort -u | paste -sd, -)
want="__rustls,__tls,json,rustls-no-provider"
if [[ $have != "$want" ]]; then
  echo "core reqwest features are '$have', expected '$want' (Spec O §23.1; a plugin leaked into the core resolution)" >&2
  exit 1
fi
echo "core workspace clean: members match Spec H §3, reqwest features are '$want'"
```

Run: `scripts/check-core-deps.sh`
Expected: the "clean" line. If the set differs, stop and report it to the controller rather than widening it: the probe gave exactly these four.

- [ ] **Step 4: Implement `tls.rs`.** Create `crates/balerix-plugin-sdk/src/tls.rs`:

```rust
//! Trust for a plugin in a pod (Spec O §23.1): one authority, the mounted
//! file, and nothing else; rustls on the ring provider, shared by the host
//! client (reqwest) and the streams (tokio-tungstenite).

use std::path::Path;
use std::sync::Arc;

use rustls_pki_types::pem::PemObject;

use crate::SdkError;

fn tls_error(path: &Path, e: impl std::fmt::Display) -> SdkError {
    SdkError::Transport(format!("{}: {e}", path.display()))
}

pub fn client_config(ca: &Path) -> Result<Arc<rustls::ClientConfig>, SdkError> {
    // one process-wide provider; a second install is a harmless `Err`
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pki_types::CertificateDer::pem_file_iter(ca).map_err(|e| tls_error(ca, e))? {
        roots
            .add(cert.map_err(|e| tls_error(ca, e))?)
            .map_err(|e| tls_error(ca, e))?;
    }
    if roots.is_empty() {
        return Err(tls_error(ca, "no certificate in the authority file"));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| tls_error(ca, e))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}
```

Add `pub mod tls;` to `lib.rs`.

- [ ] **Step 5: Implement `Env`.** In `lib.rs`, add the three fields with doc comments citing §23.1 (`ca`, `tls`, `listen`) and extend `from_env`:

```rust
impl Env {
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, SdkError> {
        let opt = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let var = |k: &'static str| opt(k).ok_or(SdkError::MissingEnv(k));
        // Spec O §23.1: a pod mounts the token; the file wins
        let token = match opt("BALERIX_PLUGIN_TOKEN_FILE") {
            Some(path) => std::fs::read_to_string(&path)
                .map_err(|e| SdkError::Transport(format!("{path}: {e}")))?
                .trim()
                .to_string(),
            None => var("BALERIX_PLUGIN_TOKEN")?,
        };
        let api_url = var("BALERIX_API_URL")?.trim_end_matches('/').to_string();
        let ca = opt("BALERIX_CA_FILE").map(PathBuf::from);
        if ca.is_some() && !api_url.starts_with("https://") {
            return Err(SdkError::Env(
                "BALERIX_CA_FILE is set, so BALERIX_API_URL must be https://",
            ));
        }
        let tls = match (opt("BALERIX_PLUGIN_TLS_CERT"), opt("BALERIX_PLUGIN_TLS_KEY")) {
            (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
            (None, None) => None,
            _ => {
                return Err(SdkError::Env(
                    "BALERIX_PLUGIN_TLS_CERT and BALERIX_PLUGIN_TLS_KEY go together",
                ));
            }
        };
        Ok(Self {
            api_url,
            name: var("BALERIX_PLUGIN_NAME")?,
            token,
            scratch: PathBuf::from(var("BALERIX_PLUGIN_SCRATCH")?),
            ca,
            tls,
            listen: opt("BALERIX_PLUGIN_LISTEN").unwrap_or_else(|| "127.0.0.1:0".into()),
        })
    }
```

Add `#[error("environment: {0}")] Env(&'static str),` to `SdkError`. Add `ca`, `tls` and `listen` to `Env`'s `Debug`; they are paths, not secrets. The existing test's `MissingEnv("BALERIX_PLUGIN_TOKEN")` case still holds, because with no `_FILE` the plain variable is required. Every `Env { … }` literal in the SDK's `testing.rs` (`FakeHost::env`) and its tests gains `ca: None, tls: None, listen: "127.0.0.1:0".into()`.

- [ ] **Step 6: Implement the client.** In `host.rs`, `Host` keeps the TLS config for the streams:

```rust
#[derive(Clone)]
pub struct Host {
    env: Env,
    http: reqwest::Client,
    /// Spec O §23.1: the authority-only config, for `wss://` too.
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl Host {
    pub fn new(env: Env) -> Result<Self, SdkError> {
        let tls = env.ca.as_deref().map(crate::tls::client_config).transpose()?;
        let mut builder = reqwest::Client::builder().no_proxy().timeout(TIMEOUT);
        if let Some(tls) = &tls {
            builder = builder.use_preconfigured_tls((**tls).clone());
        }
        let http = builder
            .build()
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        Ok(Self { env, http, tls })
    }
```

`ws_url` maps `https://` to `wss://` as well as `http://` to `ws://`:

```rust
    fn ws_url(&self, path: &str) -> String {
        let url = self.url(path);
        if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            url.replacen("http://", "ws://", 1)
        }
    }
```

In `connect`, replace `connect_async(req)` with:

```rust
        let connector = self.tls.clone().map(tokio_tungstenite::Connector::Rustls);
        match tokio_tungstenite::connect_async_tls_with_config(req, None, false, connector).await {
```

Add `use std::sync::Arc;` and drop the now-unused `connect_async` import.

- [ ] **Step 7: Run the tests and the gate.**

Run: `cargo nextest run -p balerix-plugin-sdk`
Expected: PASS, including the two `env_` tests and the TLS round trip.
Run: `mise run check`
Expected: green; `check-core-deps` prints the new set.

- [ ] **Step 8: Commit**

```bash
git add -A Cargo.toml Cargo.lock scripts/check-core-deps.sh crates/balerix-plugin-sdk
git commit -m "feat(sdk): TLS to the daemon from a mounted authority; token, CA and listen from the environment (Spec O §23.1)"
```

---

### Task 3: The SDK serves TLS, binds `BALERIX_PLUGIN_LISTEN`, and says hello with its manifest

**Files:**
- Modify: `crates/balerix-plugin-sdk/src/plugin.rs` (`Plugin::manifest`, `bind`, `run`, `serve`, `serve_on`)
- Modify: `crates/balerix-plugin-sdk/src/host.rs` (`hello` gains the manifest)
- Modify: `crates/balerix-plugin-sdk/src/lib.rs` (re-exports)
- Modify: `plugins/flow/src/plugin.rs`, `plugins/web/src/plugin.rs`, `plugins/matrix/src/plugin.rs`, `plugins/github/src/plugin.rs` (or wherever each `impl Plugin for` lives; find with `grep -rn "impl balerix_plugin_sdk::Plugin for\|impl Plugin for" plugins/*/src`), and each plugin's `Cargo.lock`
- Modify: `crates/balerix/src/commands/dev.rs` (fake plugin on `serve`)

**Interfaces:**
- Consumes: `Env.{ca, tls, listen}` and `tls::client_config` (Task 2).
- Produces:
  - `Plugin::manifest(&self) -> Option<&'static str>`, defaulting to `None`.
  - `bind_to(addr: &str) -> Result<(TcpListener, String), SdkError>`; `bind()` = `bind_to("127.0.0.1:0")`.
  - `run(listener, plugin, token, tls: Option<&(PathBuf, PathBuf)>)`.
  - `Host::hello(&self, version: &str, listen: &str, manifest: Option<&PluginManifest>)`.
  - `serve` binds `env.listen`, serves TLS when `env.tls` is set, and sends the parsed manifest when `env.ca` is set.

- [ ] **Step 1: Write the failing tests.** Append to `plugin.rs` tests:

```rust
struct WithManifest;
impl Plugin for WithManifest {
    fn manifest(&self) -> Option<&'static str> {
        Some("apiVersion: balerix/v1\nkind: Plugin\nname: t\nversion: 0.1.0\nprotocol: 1\nstart: serve\nneeds: [kv]\n")
    }
}

#[tokio::test]
async fn hello_carries_the_manifest_only_with_an_authority() {
    // the SDK's fake host over plain http: the CA-less case
    let fake = crate::testing::FakeHost::start("tok", serde_json::json!({}), vec![]).await;
    let host = Host::new(fake.env("t", std::path::Path::new("scratch"))).unwrap();
    let (listener, listen) = bind().await.unwrap();
    let h = host.clone();
    let server = tokio::spawn(async move { serve_on(&h, "0.1.0", WithManifest, listener, listen).await });
    let start = std::time::Instant::now();
    while fake.hellos().is_empty() {
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "no hello");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // `manifest` is skipped when `None`, so `None` here is the key absent
    // on the wire: what a 0.2.0 daemon's deny_unknown_fields needs
    assert_eq!(fake.hellos()[0].manifest, None);
    server.abort();
}

#[test]
fn the_manifest_a_plugin_returns_is_parsed_before_hello() {
    let m = parse_manifest(&WithManifest).unwrap().unwrap();
    assert_eq!(m.name, "t");
    struct Broken;
    impl Plugin for Broken {
        fn manifest(&self) -> Option<&'static str> {
            Some("not: [a manifest")
        }
    }
    let err = parse_manifest(&Broken).unwrap_err().to_string();
    assert!(err.starts_with("configure: balerix-plugin.yaml:"), "{err}");
}
```

`FakeHost` is the SDK's own fake in `testing.rs`; `hellos()` already records each `HelloRequest`.

Then a TLS serving test, using `tls_tests::authority` from Task 2 (make that helper `pub(crate)` in a `#[cfg(test)] pub(crate) mod test_tls` in `lib.rs` so both files can use it):

```rust
#[tokio::test]
async fn run_serves_tls_with_the_given_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let (ca, cert, key) = crate::test_tls::authority(dir.path());
    let (listener, listen) = bind().await.unwrap();
    let tls = (cert, key);
    let server = tokio::spawn(async move { run(listener, Arc::new(Silent), "tok", Some(&tls)).await });
    let client = reqwest::Client::builder()
        .use_preconfigured_tls((*crate::tls::client_config(&ca).unwrap()).clone())
        .build()
        .unwrap();
    let r = client
        .get(format!("https://{listen}/v1/health"))
        .bearer_auth("tok")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    server.abort();
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo nextest run -p balerix-plugin-sdk -E 'test(manifest) | test(serves_tls)'`
Expected: FAIL to compile (`manifest`, `parse_manifest`, and `run`'s fourth argument do not exist).

- [ ] **Step 3: Implement.** In `plugin.rs`:

```rust
    /// The plugin's own `balerix-plugin.yaml`, usually
    /// `include_str!("../package/balerix-plugin.yaml")` (Spec O §23.1). A
    /// Daemon in Kubernetes mode refuses a `hello` without it and checks its
    /// `needs` against the plugin's grant; one machine reads the package.
    fn manifest(&self) -> Option<&'static str> {
        None
    }
```

```rust
/// The manifest `plugin` returns, parsed; `Err` when it does not parse,
/// which `serve` reports before saying hello.
pub fn parse_manifest<P: Plugin>(plugin: &P) -> Result<Option<PluginManifest>, SdkError> {
    plugin
        .manifest()
        .map(|text| {
            serde_norway::from_str(text)
                .map_err(|e| SdkError::Configure(format!("balerix-plugin.yaml: {e}")))
        })
        .transpose()
}

/// A listener on `addr` (`127.0.0.1:0` on one machine, `0.0.0.0:7644` in
/// a pod) and its `host:port`.
pub async fn bind_to(addr: &str) -> Result<(tokio::net::TcpListener, String), SdkError> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| SdkError::Bind(format!("{addr}: {e}")))?;
    let listen = listener
        .local_addr()
        .map_err(|e| SdkError::Bind(e.to_string()))?
        .to_string();
    Ok((listener, listen))
}

pub async fn bind() -> Result<(tokio::net::TcpListener, String), SdkError> {
    bind_to("127.0.0.1:0").await
}

/// Serves the router until the future is dropped: plain HTTP, or TLS with
/// `tls = (certificate, key)` (Spec O §23.1). A renewed certificate is a
/// restart's, never reloaded.
pub async fn run<P: Plugin>(
    listener: tokio::net::TcpListener,
    plugin: Arc<P>,
    token: &str,
    tls: Option<&(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<(), SdkError> {
    let app = router(plugin, token);
    let Some((cert, key)) = tls else {
        return axum::serve(listener, app)
            .into_future()
            .await
            .map_err(|e| SdkError::Bind(e.to_string()));
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
        .await
        .map_err(|e| SdkError::Bind(format!("{}: {e}", cert.display())))?;
    let std = listener
        .into_std()
        .map_err(|e| SdkError::Bind(e.to_string()))?;
    axum_server::from_tcp_rustls(std, config)
        .map_err(|e| SdkError::Bind(e.to_string()))?
        .serve(app.into_make_service())
        .await
        .map_err(|e| SdkError::Bind(e.to_string()))
}
```

> axum-server 0.8's `from_tcp_rustls(std::net::TcpListener, RustlsConfig) -> io::Result<Server<…>>` (checked in the 0.8.0 source); tokio's `into_std` keeps the socket non-blocking, which axum-server expects.

`serve` and `serve_on`:

```rust
pub async fn serve<P: Plugin>(host: &Host, version: &str, plugin: P) -> Result<(), SdkError> {
    let (listener, listen) = bind_to(&host.env().listen).await?;
    serve_on(host, version, plugin, listener, listen).await
}

async fn serve_on<P: Plugin>(
    host: &Host,
    version: &str,
    plugin: P,
    listener: tokio::net::TcpListener,
    listen: String,
) -> Result<(), SdkError> {
    // Spec O §23.1: only with an authority, so a released one-machine
    // daemon (which refuses unknown fields in hello) never sees it
    let manifest = match host.env().ca {
        Some(_) => parse_manifest(&plugin)?,
        None => None,
    };
    let plugin = Arc::new(plugin);
    let token = host.env().token.clone();
    let tls = host.env().tls.clone();
    let listener_plugin = plugin.clone();
    let server =
        tokio::spawn(async move { run(listener, listener_plugin, &token, tls.as_ref()).await });
    // … the rest unchanged, except:
    let reply = match host.hello(version, &listen, manifest.as_ref()).await {
```

In `host.rs`:

```rust
    pub async fn hello(
        &self,
        version: &str,
        listen: &str,
        manifest: Option<&PluginManifest>,
    ) -> Result<HelloResponse, SdkError> {
        let req = HelloRequest {
            name: self.env.name.clone(),
            version: version.to_string(),
            protocol: PLUGIN_PROTOCOL,
            listen: listen.to_string(),
            manifest: manifest.cloned(),
        };
```

Every other `host.hello(v, l)` call in the SDK and its tests becomes `host.hello(v, l, None)`. Add `serde_norway = { workspace = true }` to the SDK's dependencies. Re-export `bind_to` and `parse_manifest` from `lib.rs`.

- [ ] **Step 4: The in-tree plugins say which manifest they are.** For each of flow, web, matrix and github, add to its `impl Plugin for …`:

```rust
    fn manifest(&self) -> Option<&'static str> {
        Some(include_str!("../package/balerix-plugin.yaml"))
    }
```

Adjust the relative path to where the `impl` file sits. Then refresh each plugin's lock and check it:

```bash
for p in flow web matrix github; do (cd plugins/$p && cargo update -w) && scripts/plugin.sh check $p; done
```

Expected: each `check` is green. `cargo update -w` adds only the SDK's new dependencies (rustls, axum-server, hyper-rustls, webpki-roots and their trees) and bumps nothing else. Confirm with `git diff --stat plugins/*/Cargo.lock` and a skim of each diff for `version =` changes on existing packages. If anything existing moved, revert that lock and use `cargo update -p balerix-plugin-sdk` instead.

- [ ] **Step 5: The fake plugin moves onto `serve`.** In `crates/balerix/src/commands/dev.rs`, replace the body of `fake_plugin_command`'s `block_on`:

```rust
    rt.block_on(async move {
        let host = Host::new(env)?;
        let plugin = FakePlugin {
            scratch: scratch.clone(),
            host: host.clone(),
        };
        match balerix_plugin_sdk::serve(&host, env!("CARGO_PKG_VERSION"), plugin).await {
            Ok(()) => Ok(String::new()),
            Err(SdkError::Bind(e)) => {
                std::fs::write(scratch.join("fake-plugin.bind-failed"), &e)?;
                bail!("fake-plugin: cannot bind its listener: {e}");
            }
            Err(e) => Err(e.into()),
        }
    })
```

`FakePlugin` gains `host: Host` and these two methods:

```rust
    fn manifest(&self) -> Option<&'static str> {
        Some(FAKE_PLUGIN_MANIFEST)
    }

    /// The `hello` reply: written to scratch, then `config.manage` applied
    /// off the serving path (an apply may take the SDK's 120 s).
    async fn configure(&self, config: Value) -> Result<(), String> {
        std::fs::write(
            self.scratch.join("fake-plugin.hello"),
            serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        eprintln!("fake-plugin: hello acknowledged");
        let (host, scratch) = (self.host.clone(), self.scratch.clone());
        tokio::spawn(async move {
            if let Err(e) = manage_from_config(&host, &config, &scratch).await {
                eprintln!("fake-plugin: manage: {e}");
            }
        });
        Ok(())
    }
```

```rust
/// What the fake would ship as `balerix-plugin.yaml`: every hook, and
/// every capability the e2e and the Kubernetes tests exercise.
const FAKE_PLUGIN_MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: fake\nversion: 0.0.0\nprotocol: 1\nstart: serve\nhooks:\n  observe: [SessionStart, Stop, PreToolUse, PostToolUse, UserPromptSubmit, Notification, SubagentStop, PreCompact, SessionEnd]\n  intercept: [PreToolUse, Stop]\nneeds: [actions, fleets, kv, manage]\n";
```

Compare the hook names with the package manifest the e2e builds for the fake (`grep -rn "fake-plugin" crates/balerix/tests/e2e.rs`) and copy its `hooks` and `needs` exactly. The name `fake` must equal what the tests declare in Task 8. The existing unit test `the_fake_plugin_applies_the_fleet_its_config_names` calls `manage_from_config` directly and stays as it is.

- [ ] **Step 6: Run the tests and the gates.**

Run: `cargo nextest run -p balerix-plugin-sdk && cargo nextest run -p balerix -E 'test(fake_plugin)'`
Expected: PASS.
Run: `mise run check`
Expected: green.

- [ ] **Step 7: Commit**

```bash
git add -A crates plugins
git commit -m "feat(sdk): serve TLS on BALERIX_PLUGIN_LISTEN and send the manifest in hello; the in-tree plugins and the fake declare theirs (Spec O §23.1)"
```

---

### Task 4: The Daemon calls plugins over TLS, and `serve --tls-ca`

**Files:**
- Modify: `crates/balerix-server/Cargo.toml` (`hyper-rustls`, `rustls-pki-types`; dev-dependencies `rcgen`, `axum-server`)
- Modify: `crates/balerix-server/src/plugins/registry.rs` (`PluginAddr::base`)
- Modify: `crates/balerix-server/src/plugins/client.rs` (`PluginClient::new(ca)`, `url`)
- Modify: `crates/balerix-server/src/proxy.rs` (`HttpClient`, `client(ca)`, `upstream_uri(base, …)`)
- Modify: `crates/balerix-server/src/kube/tls.rs` (`client_config`)
- Modify: `crates/balerix-server/src/daemon.rs` (`Daemon::start` takes the proxy client's authority, see below)
- Modify: `crates/balerix-server/src/testing.rs` (`StubScript.tls`)
- Modify: `crates/balerix/src/cli.rs` (`--tls-ca`), `crates/balerix/src/commands/serve.rs`

**Interfaces:**
- Consumes: nothing from Tasks 1–3 but the workspace dependencies.
- Produces:
  - `PluginAddr::base(&self) -> String`.
  - `balerix_server::kube::tls::client_config(ca: &Path) -> std::io::Result<Arc<rustls::ClientConfig>>`.
  - `PluginClient::new(tls: Option<Arc<rustls::ClientConfig>>) -> Result<PluginClient, PluginError>` and `PluginClient::trusts(&self) -> bool`.
  - `proxy::client(tls: Option<Arc<rustls::ClientConfig>>) -> HttpClient`.
  - `Daemon::start` takes `client: PluginClient` as today. The proxy client is built from `client.tls()`, so no new parameter is needed.
  - `ServeArgs.tls_ca: Option<PathBuf>`.
  - `StubScript.tls: Option<(PathBuf, PathBuf)>`.

- [ ] **Step 1: Write the failing tests.** In `client.rs` tests:

```rust
#[test]
fn a_url_is_used_as_given_and_a_bare_address_is_loopback_http() {
    let bare = PluginAddr { listen: "127.0.0.1:9".into(), token: "t".into() };
    assert_eq!(bare.base(), "http://127.0.0.1:9");
    let url = PluginAddr { listen: "https://flow.ns.svc:7644".into(), token: "t".into() };
    assert_eq!(url.base(), "https://flow.ns.svc:7644");
    assert_eq!(PluginClient::url(&url, "/v1/health"), "https://flow.ns.svc:7644/v1/health");
}

#[tokio::test]
async fn the_client_calls_a_tls_plugin_under_the_given_authority() {
    let dir = tempfile::tempdir().unwrap();
    let (ca, cert, key) = crate::testing::test_authority(dir.path());
    let stub = crate::testing::stub_plugin(crate::testing::StubScript {
        health_ok: true,
        tls: Some((cert, key)),
        ..Default::default()
    })
    .await;
    assert!(stub.listen.starts_with("https://127.0.0.1:"), "{}", stub.listen);
    let tls = crate::kube::tls::client_config(&ca).unwrap();
    let client = PluginClient::new(Some(tls)).unwrap();
    let addr = PluginAddr { listen: stub.listen.clone(), token: "t".into() };
    client.health(&addr).await.unwrap();
    // without the authority the handshake fails as a connect failure
    let plain = PluginClient::new(None).unwrap();
    assert_eq!(plain.health(&addr).await.unwrap_err(), CallFailure::Connect);
}
```

In `proxy.rs` tests, add the upgrade through TLS (Review Focus 2). Use the stub's `/v1/routes/ws` echo route, which Step 3 adds to `stub_plugin`:

```rust
#[tokio::test]
async fn a_websocket_upgrades_through_the_proxy_to_a_tls_plugin() {
    use futures_util::{SinkExt, StreamExt};
    let dir = tempfile::tempdir().unwrap();
    let (ca, cert, key) = crate::testing::test_authority(dir.path());
    let stub = crate::testing::stub_plugin(crate::testing::StubScript {
        tls: Some((cert, key)),
        ..Default::default()
    })
    .await;
    let tls = crate::kube::tls::client_config(&ca).unwrap();
    let base = stub.listen.clone();
    let proxy = client(Some(tls));
    // a one-route front door that forwards /ws through `forward`
    let app = axum::Router::new().route(
        "/ws",
        axum::routing::any(move |req: axum::extract::Request| {
            let (proxy, base) = (proxy.clone(), base.clone());
            async move {
                let addr = crate::plugins::PluginAddr { listen: base, token: "t".into() };
                forward(&proxy, &"stub".parse().unwrap(), &addr, "ws", req).await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let front = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{front}/ws")).await.unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::text("ping")).await.unwrap();
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(echoed.into_text().unwrap().as_str(), "ping");
}
```

> `forward` is the proxy's existing per-request function (around `proxy.rs:196`). Match its real name and argument order; if it takes the request's parts differently, adapt the closure, not `forward`. The assertion is the point.

In `cli_serve.rs`, add to the existing kube-mode flag test that `--tls-ca` alone in tmux mode is refused like the other TLS flags: `--tls-cert, --tls-key, --tls-ca and --admin-token-file are for --mode kubernetes`. Update that test's existing expected string the same way.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo nextest run -p balerix-server -E 'test(url_is_used) | test(tls_plugin) | test(upgrades_through_the_proxy)'`
Expected: FAIL to compile.

- [ ] **Step 3: Implement.**
  - **`kube/tls.rs`:** add `client_config`. Its body is Task 2's `tls::client_config` with `std::io::Error::other` for errors, because this crate does not depend on the SDK. Re-export it from `kube/mod.rs`.
  - **`testing.rs`:**
    - `pub fn test_authority(dir: &Path) -> (PathBuf, PathBuf, PathBuf)`, the same body as the SDK's `authority`. This is a lib module, so put `rcgen` under `[dependencies]` of `balerix-server` with a comment saying it is the test harness's (as `tempfile` is) rather than under dev-dependencies.
    - `stub_plugin` serves through `axum_server::from_tcp_rustls` when `script.tls` is set, and sets `listen` to `https://127.0.0.1:<port>` in that case (bare `127.0.0.1:<port>` otherwise, as today).
    - Add a `/v1/routes/ws` route that upgrades and echoes text frames.
  - **`registry.rs`:**

```rust
impl PluginAddr {
    /// Where to call the plugin: a URL as given (Kubernetes mode, Spec O
    /// §23.2), or `http://` + a loopback `host:port` from `hello`.
    pub fn base(&self) -> String {
        if self.listen.starts_with("https://") || self.listen.starts_with("http://") {
            self.listen.trim_end_matches('/').to_string()
        } else {
            format!("http://{}", self.listen)
        }
    }
}
```

  - **`client.rs`:**

```rust
#[derive(Clone)]
pub struct PluginClient {
    http: reqwest::Client,
    /// Spec O §23.1: the Daemon's authority, trusted alone; `None` on one
    /// machine, where every plugin is loopback HTTP.
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl PluginClient {
    pub fn new(tls: Option<Arc<rustls::ClientConfig>>) -> Result<Self, PluginError> {
        let mut builder = reqwest::Client::builder().no_proxy().timeout(CALL_TIMEOUT);
        if let Some(tls) = &tls {
            builder = builder.use_preconfigured_tls((**tls).clone());
        }
        let http = builder
            .build()
            .map_err(|e| PluginError::Internal(format!("http client: {e}")))?;
        Ok(Self { http, tls })
    }

    /// Whether this client can call an `https://` plugin.
    pub fn trusts(&self) -> bool {
        self.tls.is_some()
    }

    pub fn tls(&self) -> Option<Arc<rustls::ClientConfig>> {
        self.tls.clone()
    }

    fn url(addr: &PluginAddr, path: &str) -> String {
        format!("{}{path}", addr.base())
    }
```

  `From<reqwest::Error>` already maps a handshake failure (`is_connect()`) to `Connect`. Leave it.

  - **`proxy.rs`:**

```rust
pub type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

pub fn client(tls: Option<Arc<rustls::ClientConfig>>) -> HttpClient {
    let mut http = HttpConnector::new();
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    // https_or_http: loopback plugins stay plain; an https plugin is
    // verified against the Daemon's authority alone (Spec O §23.1)
    http.enforce_http(false);
    let tls = tls.map_or_else(no_roots, |t| (*t).clone());
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new()).build(connector)
}

/// A config that trusts nothing: an `https://` plugin on a daemon given
/// no authority fails its handshake rather than trusting a default store.
fn no_roots() -> rustls::ClientConfig {
    let _ = rustls::crypto::ring::default_provider().install_default();
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map(|b| b.with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth())
        .unwrap_or_else(|_| unreachable!("ring supports the safe default versions"))
}
```

  > `unreachable!` sits inside `unwrap_or_else` because the workspace lints forbid `expect`. If clippy objects, return `rustls::ClientConfig` from a `OnceLock`-free helper that maps the error with `tracing::error!` and builds the config with `builder()` instead. The behaviour (no roots) is what matters.

  `upstream_uri(listen, …)` becomes `upstream_uri(base, …)` and builds `format!("{base}{path}{query}")`. Its caller passes `addr.base()`. Update the existing `upstream_uri` unit tests to pass `"http://127.0.0.1:9"`.
  - **`daemon.rs`:** `proxy_client: crate::proxy::client(client.tls())`.
  - **Callers of `PluginClient::new()`:** the one-machine `serve` path and the tests pass `None`. Find them with `grep -rn "PluginClient::new()" crates`.
  - **`cli.rs`:**

```rust
    /// The Daemon's authority, PEM (--mode kubernetes, Spec O §23.1): the
    /// one root it trusts when calling plugins. Without it the Daemon
    /// refuses a plugin list.
    #[arg(long)]
    pub tls_ca: Option<PathBuf>,
```

  - **`serve.rs`:**
    - Tmux mode refuses `--tls-ca` together with the other kube flags; update the shared message.
    - `run_kubernetes` takes `ca: Option<&Path>` and builds `let tls = ca.map(kube::tls::client_config).transpose().with_context(|| "--tls-ca")?;`.
    - It then passes `PluginClient::new(tls)`.

- [ ] **Step 4: Run the tests and the gate.**

Run: `cargo nextest run -p balerix-server -E 'test(url_is_used) | test(tls_plugin) | test(upgrades_through_the_proxy) | test(upstream_uri)' && cargo nextest run -p balerix --test cli_serve`
Expected: PASS.
Run: `mise run check`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add -A crates
git commit -m "feat(server): call plugins and proxy their routes over TLS under the Daemon's authority; serve --tls-ca (Spec O §23.1)"
```

---

### Task 5: `DeclaredPlugins`, the Kubernetes-mode plugin source

**Files:**
- Create: `crates/balerix-server/src/kube/declared.rs`
- Modify: `crates/balerix-server/src/kube/mod.rs` (`pub mod declared; pub use declared::DeclaredPlugins;`)
- Modify: `crates/balerix-server/src/plugins/registry.rs` (`declare`, `accept_hello`)

**Interfaces:**
- Consumes: `DeclaredPlugin`, `HelloRequest.manifest` (Task 1); `PluginAddr::base`, `PluginClient` (Task 4).
- Produces:

```rust
pub struct DeclaredPlugins { /* state_dir, registry, entries: Mutex<Entries> */ }
impl DeclaredPlugins {
    pub fn new(state_dir: PathBuf, registry: Arc<PluginRegistry>) -> Arc<Self>;
    /// `PUT /v1/plugins`: validates, replaces the list, re-registers.
    /// Returns the names that left the list.
    pub fn replace(&self, list: Vec<DeclaredPlugin>) -> Result<Vec<AgentName>, DeclareError>;
    pub fn plugin_for_token(&self, token: &str) -> Option<AgentName>;
    pub fn config(&self, name: &AgentName) -> Option<Value>;
    /// Every check of §23.2; on success the hello is accepted, persisted,
    /// and the plugin is ready. The refusal is kept for `list`.
    pub fn hello(&self, name: &AgentName, req: &HelloRequest) -> Result<HelloResponse, DaemonError>;
    /// One health result; `true` when the plugin just became ready again
    /// (the caller re-sends its activations).
    pub fn health(&self, name: &AgentName, ok: Result<(), String>) -> bool;
    /// What to poll: every listed plugin with an accepted (or restored) hello.
    pub fn pollable(&self) -> Vec<(AgentName, PluginAddr)>;
    pub fn list(&self, active: impl Fn(&AgentName) -> u32) -> Vec<PluginStatus>;
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("plugins[{index}].{field}: {message}")]
pub struct DeclareError { pub index: usize, pub field: &'static str, pub message: String }
pub const MISSED_POLLS: u32 = 3;
```

  `PluginRegistry` gains:

```rust
/// Spec O §23.2: the operator's list. Every entry is registered at once
/// with a placeholder manifest (no needs, no hooks) and not ready, so a
/// fleet naming it applies with its pairs pending; `keep` names the
/// entries whose accepted hello still holds (their manifest and readiness
/// are kept). Rows of plugins no longer listed are dropped.
pub fn declare(&self, entries: &[(AgentName, Value /* fleetDefaults */)], keep: &[AgentName]);
/// An accepted hello: the real manifest, the url and token, ready.
pub fn accept_hello(&self, name: &AgentName, manifest: PluginManifest, url: String, token: String);
```

- [ ] **Step 1: Write the failing tests.** In `declared.rs`, use a `#[cfg(test)] mod tests` with these helpers:

```rust
fn entry(name: &str, grant: &[Capability]) -> DeclaredPlugin {
    DeclaredPlugin {
        name: name.into(),
        grant: grant.iter().copied().collect(),
        config: json!({ "k": name }),
        fleet_defaults: json!({}),
        token: format!("{name}-token-0123456789abcdef0123456789"),
        url: format!("https://{name}.ns.svc:7644"),
    }
}

fn hello(name: &str, needs: &str) -> HelloRequest {
    HelloRequest {
        name: name.into(),
        version: "1.0.0".into(),
        protocol: PLUGIN_PROTOCOL,
        listen: "0.0.0.0:7644".into(),
        manifest: Some(
            serde_norway::from_str(&format!(
                "apiVersion: balerix/v1\nkind: Plugin\nname: {name}\nversion: 1.0.0\nprotocol: 1\nstart: serve\nneeds: [{needs}]\nhooks: {{ intercept: [Stop] }}\n"
            ))
            .unwrap(),
        ),
    }
}

fn n(s: &str) -> AgentName { s.parse().unwrap() }
```

Then the tests:

```rust
#[test]
fn the_list_is_validated_whole() {
    let dir = tempfile::tempdir().unwrap();
    let d = DeclaredPlugins::new(dir.path().into(), PluginRegistry::new());
    let mut short = entry("flow", &[]);
    short.token = "short".into();
    let mut plain = entry("web", &[]);
    plain.url = "http://web:7644".into();
    let cases = [
        (vec![entry("flow", &[]), entry("flow", &[])], "plugins[1].name: listed twice"),
        (vec![entry("kubernetes", &[])], "plugins[0].name: reserved: it is the owner name of the operator's fleets"),
        (vec![entry("Bad Name", &[])], "plugins[0].name:"),
        (vec![short], "plugins[0].token: a token is at least 32 characters"),
        (vec![plain], "plugins[0].url: must be https://"),
    ];
    for (list, want) in cases {
        let err = d.replace(list).unwrap_err().to_string();
        assert!(err.starts_with(want), "{err} / {want}");
    }
}

#[test]
fn hello_is_checked_in_order_and_a_refusal_is_kept_on_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let reg = PluginRegistry::new();
    let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
    d.replace(vec![entry("flow", &[Capability::Actions, Capability::Kv])]).unwrap();
    assert!(reg.is_installed("flow"), "registered before hello");
    assert!(!reg.has(&n("flow"), Capability::Kv), "placeholder: no needs yet");

    let mut no_manifest = hello("flow", "kv");
    no_manifest.manifest = None;
    let mut wrong_name = hello("flow", "kv");
    if let Some(m) = wrong_name.manifest.as_mut() { m.name = "web".into(); }
    let mut old_protocol = hello("flow", "kv");
    old_protocol.protocol = 2;
    for (req, want) in [
        (old_protocol, "hello.protocol: this daemon speaks protocol 1, got 2"),
        (no_manifest, "hello.manifest: required in kubernetes mode"),
        (wrong_name, "hello.manifest.name: \"web\" does not match the plugin \"flow\""),
        (hello("flow", "actions, workspace, kv"), "hello.manifest.needs: workspace is not granted"),
    ] {
        let err = d.hello(&n("flow"), &req).unwrap_err().to_string();
        assert!(err.ends_with(want), "{err} / {want}");
        let row = &d.list(|_| 0)[0];
        assert_eq!((row.phase, row.message.as_str()), (AgentPhase::Failed, want));
    }

    let reply = d.hello(&n("flow"), &hello("flow", "actions, kv")).unwrap();
    assert_eq!(reply.config, json!({ "k": "flow" }));
    let row = &d.list(|_| 0)[0];
    assert_eq!((row.phase, row.listen.as_deref()), (AgentPhase::Ready, Some("https://flow.ns.svc:7644")));
    assert!(reg.has(&n("flow"), Capability::Kv));
    assert_eq!(reg.ready_addr(&n("flow")).unwrap().base(), "https://flow.ns.svc:7644");
}

#[test]
fn a_token_names_its_plugin_and_nothing_else_does() {
    let dir = tempfile::tempdir().unwrap();
    let d = DeclaredPlugins::new(dir.path().into(), PluginRegistry::new());
    d.replace(vec![entry("flow", &[]), entry("web", &[])]).unwrap();
    assert_eq!(d.plugin_for_token(&entry("web", &[]).token), Some(n("web")));
    assert_eq!(d.plugin_for_token("nope-0123456789abcdef0123456789abcdef"), None);
}

#[test]
fn three_missed_polls_take_a_plugin_out_and_a_good_one_brings_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let reg = PluginRegistry::new();
    let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
    d.replace(vec![entry("flow", &[Capability::Kv])]).unwrap();
    d.hello(&n("flow"), &hello("flow", "kv")).unwrap();
    for _ in 0..2 {
        assert!(!d.health(&n("flow"), Err("timeout".into())));
        assert!(reg.ready_addr(&n("flow")).is_some(), "one or two misses keep it in the chain");
    }
    assert!(!d.health(&n("flow"), Err("timeout".into())));
    assert!(reg.ready_addr(&n("flow")).is_none());
    let row = &d.list(|_| 0)[0];
    assert_eq!((row.phase, row.message.as_str()), (AgentPhase::Failed, "health: timeout"));
    assert!(d.health(&n("flow"), Ok(())), "back: the caller re-activates");
    assert!(reg.ready_addr(&n("flow")).is_some());
    assert!(!d.health(&n("flow"), Ok(())), "already ready: nothing to re-send");
}

#[test]
fn a_restart_restores_an_unchanged_entrys_hello_and_not_a_changed_one() {
    let dir = tempfile::tempdir().unwrap();
    {
        let d = DeclaredPlugins::new(dir.path().into(), PluginRegistry::new());
        d.replace(vec![entry("flow", &[Capability::Kv]), entry("web", &[Capability::Kv])]).unwrap();
        d.hello(&n("flow"), &hello("flow", "kv")).unwrap();
        d.hello(&n("web"), &hello("web", "kv")).unwrap();
    }
    let reg = PluginRegistry::new();
    let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
    let mut changed = entry("web", &[Capability::Kv]);
    changed.config = json!({ "k": "new" });
    d.replace(vec![entry("flow", &[Capability::Kv]), changed]).unwrap();
    let pollable: Vec<String> = d.pollable().into_iter().map(|(n, _)| n.to_string()).collect();
    assert_eq!(pollable, vec!["flow"], "only the unchanged entry's hello is restored");
    assert!(reg.ready_addr(&n("flow")).is_none(), "restored, not ready until a poll answers");
    assert!(reg.has(&n("flow"), Capability::Kv), "the restored manifest is registered");
    assert!(d.health(&n("flow"), Ok(())), "first good poll: ready, re-activate");
    assert_eq!(d.list(|_| 0)[1].phase, AgentPhase::Starting, "web waits for a new hello");
}

#[test]
fn a_dropped_plugin_leaves_the_registry_and_its_hello_file() {
    let dir = tempfile::tempdir().unwrap();
    let reg = PluginRegistry::new();
    let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
    d.replace(vec![entry("flow", &[Capability::Kv])]).unwrap();
    d.hello(&n("flow"), &hello("flow", "kv")).unwrap();
    assert!(dir.path().join("flow/hello.json").exists());
    assert_eq!(d.replace(vec![]).unwrap(), vec![n("flow")]);
    assert!(!reg.is_installed("flow"));
    assert!(!dir.path().join("flow/hello.json").exists());
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo nextest run -p balerix-server -E 'test(declared::)'`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the registry methods** in `registry.rs`:

```rust
    pub fn declare(&self, entries: &[(AgentName, Value)], keep: &[AgentName]) {
        let mut w = self.write();
        let old = std::mem::take(&mut w.plugins);
        w.order = entries.iter().map(|(n, _)| n.clone()).collect();
        for (name, fleet_defaults) in entries {
            let prev = old.get(name).filter(|_| keep.contains(name));
            let info = match prev {
                Some(p) => PluginInfo { fleet_defaults: fleet_defaults.clone(), ..p.clone() },
                None => PluginInfo {
                    manifest: placeholder_manifest(name),
                    listen: None,
                    token: None,
                    ready: false,
                    degraded: None,
                    fleet_defaults: fleet_defaults.clone(),
                },
            };
            w.plugins.insert(name.clone(), info);
        }
        let names: Vec<AgentName> = w.plugins.keys().cloned().collect();
        w.rows.retain(|(_, p), _| names.contains(p));
    }

    pub fn accept_hello(&self, name: &AgentName, manifest: PluginManifest, url: String, token: String) {
        if let Some(p) = self.write().plugins.get_mut(name) {
            p.manifest = manifest;
            p.listen = Some(url);
            p.token = Some(token);
            p.ready = true;
            p.degraded = None;
        }
    }
```

```rust
/// Spec O §23.2: what a declared plugin is before its first hello: it
/// subscribes to nothing and may call nothing.
fn placeholder_manifest(name: &AgentName) -> PluginManifest {
    PluginManifest {
        api_version: "balerix/v1".into(),
        kind: balerix_api::PLUGIN_KIND.into(),
        name: name.to_string(),
        version: String::new(),
        protocol: balerix_api::PLUGIN_PROTOCOL,
        start: String::new(),
        hooks: Default::default(),
        needs: Default::default(),
        routes: false,
        sandbox: Value::Object(Default::default()),
    }
}
```

- [ ] **Step 4: Implement `declared.rs`.** Lay it out as follows:
  - **`Entry`** holds `{ plugin: DeclaredPlugin, hash: String, hello: HelloState, misses: u32 }`, with `enum HelloState { Waiting, Refused(String), Accepted { manifest: PluginManifest, version: String, ready: bool, failed: Option<String> } }`. A restored hello is `Accepted { ready: false, failed: None }`.
  - **`hash(&DeclaredPlugin)`** is the lowercase-hex sha256 of `serde_json::to_vec(plugin)`. `DeclaredPlugin` serialises its `BTreeSet` and `Value` maps deterministically, so the bytes are stable.
  - **`replace`:**
    - Validates every entry first and changes nothing on error. Each entry needs: a name that parses as `AgentName`, isn't reserved (`reserved_plugin_reason`) and isn't a duplicate; a token of at least 32 characters; an `https://` url.
    - Builds the new map. An entry whose name and hash are unchanged keeps its `HelloState`. A new or changed entry starts `Waiting`, unless `<state_dir>/<name>/hello.json` exists with `"entry"` equal to the new hash and its manifest still passes the grant check; that entry starts `Accepted { ready: false }`.
    - Deletes `hello.json` of every dropped name.
    - Calls `registry.declare(…)`, with `keep` = the names whose state is `Accepted`.
    - For each restored entry, calls `registry.accept_hello(…)` followed by `registry.set_ready(name, false)`, so its manifest is registered but it is not called until a poll answers.
    - Returns the dropped names.
  - **`hello`** runs the checks in the order the test pins:
    1. protocol, with the same text as `PluginHost::hello`;
    2. the manifest is present;
    3. its name equals the plugin's;
    4. its needs are within the grant, naming the first missing one in `Capability` order via `crate::plugins::wire_label`.

    A refusal sets `Refused(text)` and returns `DaemonError::Invalid(text)`. On success it writes `hello.json` atomically, sets `Accepted { ready: true }` with misses 0, calls `registry.accept_hello(name, manifest, url, token)` and returns `HelloResponse { config }`.
  - **`health`:**
    - `Ok` resets misses. If the state is `Accepted` and not ready, it marks ready, calls `registry.set_ready(true)` and `registry.set_degraded(None)`, and returns `true`.
    - `Err(e)` increments misses. At `MISSED_POLLS` and while ready, it marks not ready with `failed = Some(format!("health: {e}"))`, calls `registry.set_ready(false)` and `registry.set_degraded(Some(e))`, and returns `false`.
    - In every other case it returns `false`.
  - **`list`** maps each entry in list order to a `PluginStatus`:
    - `Waiting` → `Starting`.
    - `Refused(m)` → `Failed` with `m`.
    - `Accepted { ready: true }` → `Ready`.
    - `Accepted { failed: Some(m) }` → `Failed` with `m`.
    - `Accepted` restored → `Starting`.

    `listen` is `Some(url)`, `routes` comes from the manifest when accepted (else `false`), `version` from the accepted manifest (else `""`), and `active_agents` from the closure.
  - **`plugin_for_token`** compares each entry's token with `crate::auth::constant_time_eq` (the helper `daemon.rs` uses).
  - **Writes:** `std::fs::write(tmp)` then `std::fs::rename(tmp, path)`, creating `<state_dir>/<name>/` first. A write failure is logged with `tracing::warn!` and does not fail the hello; persistence only saves a re-hello.

- [ ] **Step 5: Run the tests.**

Run: `cargo nextest run -p balerix-server -E 'test(declared::) | test(registry::)'`
Expected: PASS.

- [ ] **Step 6: Gate and commit.**

Run: `mise run check`
Expected: green.

```bash
git add -A crates/balerix-server
git commit -m "feat(server): DeclaredPlugins — the operator's plugin list, hello against a grant, persisted hellos, readiness from the health poll (Spec O §23.2)"
```

---

### Task 6: The Daemon hosts declared plugins: `PluginSource`, `PUT /v1/plugins`, the 409s

**Files:**
- Create: `crates/balerix-server/src/plugins/source.rs`
- Modify: `crates/balerix-server/src/plugins/mod.rs` (`pub mod source; pub use source::{PluginSetup, PluginSource};`)
- Modify: `crates/balerix-server/src/daemon.rs` (`start`, `plugin_hello`, `plugin_for_token`, `get`, `snapshots`, `poll_health`, new `declare_plugins`, `list_plugins`, extract `reactivate`)
- Modify: `crates/balerix-server/src/api.rs` (`PUT /v1/plugins`, `list_plugins`, `sync_plugins`, `purge_plugin`)
- Modify: `crates/balerix-server/src/testing.rs` (`Harness` passes `PluginSetup::Packages`; new `Harness::daemon_declared`)
- Modify: `crates/balerix/src/commands/serve.rs` (both modes)
- Test: `crates/balerix-server/tests/kube_plugins_it.rs` (new)

**Interfaces:**
- Consumes: `DeclaredPlugins` (Task 5); `PluginClient::trusts` (Task 4).
- Produces:

```rust
pub enum PluginSetup {
    /// One machine: `plugins.yaml` and the reserved fleet (today).
    Packages(PluginHostConfig),
    /// Kubernetes mode (Spec O §23.2): the operator's list; hellos persist
    /// under `state_dir` (`<state>/plugins`).
    Declared { state_dir: PathBuf },
}
pub enum PluginSource {
    Packages(Arc<PluginHost>),
    Declared(Arc<DeclaredPlugins>),
}
// Daemon:
pub fn is_kubernetes(&self) -> bool;               // ports.kube.is_some()
pub async fn declare_plugins(&self, list: DeclaredPlugins) -> Result<Vec<AgentName>, DaemonError>;
pub async fn list_plugins(&self) -> Vec<PluginStatus>;
async fn reactivate(&self, name: &AgentName);      // the row re-send, from plugin_hello
```

  `Daemon::start`'s `plugin_config: PluginHostConfig` parameter becomes `plugins: PluginSetup`.

- [ ] **Step 1: Write the failing tests.** Create `crates/balerix-server/tests/kube_plugins_it.rs`, modelled on `kube_api_it.rs`'s `World` and `call` (copy them; they are small). Add `Harness::daemon_declared(handler, dir, client: PluginClient)`, which builds the daemon with `PluginSetup::Declared { state_dir: dir.join("plugins") }` and the given client.

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §23.2: the plugin list, hello against the grant, the 409s.

const ADMIN: &str = "0123456789abcdef0123456789abcdef";

fn entry(name: &str, url: &str, grant: &[&str]) -> Value {
    json!({ "name": name, "grant": grant, "config": { "greeting": "hi" },
            "token": format!("{name}-tok-0123456789abcdef0123456789ab"), "url": url })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_without_an_authority_refuses_the_list() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world_with(&h, PluginClient::new(None).unwrap()).await;
    let (s, v) = w.call("PUT", "/v1/plugins", Some(ADMIN),
        Some(json!({ "plugins": [entry("flow", "https://127.0.0.1:1", &["kv"])] }))).await;
    assert_eq!((s, v["error"].as_str()), (409, Some("this daemon was started without --tls-ca; it cannot call plugins")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_list_is_declared_hello_is_checked_and_the_row_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let (ca, cert, key) = balerix_server::testing::test_authority(dir.path());
    let stub = stub_plugin(StubScript { health_ok: true, tls: Some((cert, key)), ..Default::default() }).await;
    let h = Harness::kube(Duration::from_secs(3600));
    let tls = balerix_server::kube::tls::client_config(&ca).unwrap();
    let w = world_with(&h, PluginClient::new(Some(tls)).unwrap()).await;

    // a plain-http url is refused whole (Review Focus 5's sibling)
    let (s, v) = w.call("PUT", "/v1/plugins", Some(ADMIN),
        Some(json!({ "plugins": [entry("flow", "http://x:1", &["kv"])] }))).await;
    assert_eq!((s, v["error"].as_str()), (400, Some("plugins[0].url: must be https://")));

    let (s, _) = w.call("PUT", "/v1/plugins", Some(ADMIN),
        Some(json!({ "plugins": [entry("flow", &stub.listen, &["kv"])] }))).await;
    assert_eq!(s, 204);
    let (_, rows) = w.call("GET", "/v1/plugins", Some(ADMIN), None).await;
    assert_eq!(rows[0]["phase"], "starting");

    let token = entry("flow", "", &[])["token"].as_str().unwrap().to_string();
    let manifest = |needs: &str| json!({ "apiVersion": "balerix/v1", "kind": "Plugin", "name": "flow",
        "version": "1.0.0", "protocol": 1, "start": "serve", "needs": needs.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>() });
    let hello = |needs: &str| json!({ "name": "flow", "version": "1.0.0", "protocol": 1,
        "listen": "0.0.0.0:7644", "manifest": manifest(needs) });
    let (s, v) = w.call("POST", "/v1/plugin-host/hello", Some(&token), Some(hello("kv,workspace"))).await;
    assert_eq!((s, v["error"].as_str()), (400, Some("hello.manifest.needs: workspace is not granted")));
    let (_, rows) = w.call("GET", "/v1/plugins", Some(ADMIN), None).await;
    assert_eq!((rows[0]["phase"].as_str(), rows[0]["message"].as_str()),
        (Some("failed"), Some("hello.manifest.needs: workspace is not granted")));

    let (s, v) = w.call("POST", "/v1/plugin-host/hello", Some(&token), Some(hello("kv"))).await;
    assert_eq!((s, v["config"]["greeting"].as_str()), (200, Some("hi")));
    let (_, rows) = w.call("GET", "/v1/plugins", Some(ADMIN), None).await;
    assert_eq!(rows[0]["phase"], "ready");
    // the token works on the host routes its manifest allows
    let (s, _) = w.call("GET", "/v1/plugin-host/kv", Some(&token), None).await;
    assert_eq!(s, 200);
    let (s, _) = w.call("GET", "/v1/plugin-host/fleets", Some(&token), None).await;
    assert_eq!(s, 403, "fleets is neither needed nor granted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kubernetes_mode_refuses_sync_and_purge_and_tmux_mode_refuses_the_list() {
    let h = Harness::kube(Duration::from_secs(3600));
    let w = world_with(&h, PluginClient::new(None).unwrap()).await;
    let msg = "this daemon is in kubernetes mode; change its plugins through the Daemon's spec.plugins";
    let (s, v) = w.call("POST", "/v1/plugins/sync", Some(ADMIN), None).await;
    assert_eq!((s, v["error"].as_str()), (409, Some(msg)));
    let (s, v) = w.call("DELETE", "/v1/plugins/flow", Some(ADMIN), None).await;
    assert_eq!((s, v["error"].as_str()), (409, Some(msg)));
    let tmux = Harness::new(Duration::from_secs(3600));
    let w = tmux_world(&tmux).await;
    let (s, v) = w.call("PUT", "/v1/plugins", Some(ADMIN), Some(json!({ "plugins": [] }))).await;
    assert_eq!((s, v["error"].as_str()), (409, Some("this daemon reads plugins.yaml")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fleet_naming_a_declared_plugin_applies_with_its_pair_pending() {
    let dir = tempfile::tempdir().unwrap();
    let (ca, _, _) = balerix_server::testing::test_authority(dir.path());
    let h = Harness::kube(Duration::from_secs(3600));
    let tls = balerix_server::kube::tls::client_config(&ca).unwrap();
    let w = world_with(&h, PluginClient::new(Some(tls)).unwrap()).await;
    let (s, _) = w.call("PUT", "/v1/plugins", Some(ADMIN),
        Some(json!({ "plugins": [entry("flow", "https://127.0.0.1:1", &["kv"])] }))).await;
    assert_eq!(s, 204);
    // no hello yet: the operator's fleet names the plugin all the same
    let mut req = request(Some(json!({ "f/c/a": "a".repeat(32) })));
    req["spec"]["crews"]["c"]["agents"]["a"]["plugins"] = json!({ "flow": {} });
    let (s, rec) = w.call("PUT", "/v1/fleets/f", Some(ADMIN), Some(req)).await;
    assert_eq!(s, 200, "not `no plugin \"flow\" is installed`: {rec}");
    assert_eq!(rec["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"], "pending");
}
```

Write the last test's body to match its comment. Use `kube_api_it.rs`'s `request(Some(tokens))` shape, and put `"plugins": { "flow": {} }` in agent `a`'s settings. Check that the record's activation overlay says `pending`; `kube_api_it.rs` and `events_it.rs` show how the overlay reads.

**Status codes and token handling this task decides:**
- `PUT /v1/plugins` answers 204 on success.
- A capability the manifest lacks answers 403. That is today's `PluginError::Capability` mapping; check `ApiError::from(PluginError)` and use whatever status it gives.
- `hello` and the host routes take the plugin's token as their bearer, as on one machine.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo nextest run -p balerix-server --test kube_plugins_it`
Expected: FAIL to compile (no `daemon_declared`, no route).

- [ ] **Step 3: Implement `source.rs`** with the two enums above. The `PluginSource` helpers the daemon calls are:

```rust
impl PluginSource {
    pub fn packages(&self) -> Option<&Arc<PluginHost>> {
        match self { Self::Packages(h) => Some(h), Self::Declared(_) => None }
    }
    pub fn declared(&self) -> Option<&Arc<DeclaredPlugins>> {
        match self { Self::Declared(d) => Some(d), Self::Packages(_) => None }
    }
}
```

- [ ] **Step 4: Wire the Daemon.**
  - **`start`:** for `PluginSetup::Packages(cfg)`, do as today. For `Declared { state_dir }`, build `DeclaredPlugins::new(state_dir, registry.clone())` and spawn no plugin actor. The field `plugins: Arc<PluginHost>` becomes `plugins: PluginSource`. `pub fn plugins(&self) -> &Arc<PluginHost>` becomes `pub fn plugin_host(&self) -> Option<&Arc<PluginHost>>`; fix its callers (the purge and sync routes, tests).
  - **`plugin_hello`:**

```rust
        let response = match &self.plugins {
            PluginSource::Packages(host) => {
                if !self.verify_secret(&plugin_id(name), token).await {
                    return Err(DaemonError::Unauthorized);
                }
                host.hello(name, req, token).await?
            }
            PluginSource::Declared(d) => {
                if d.plugin_for_token(token).as_ref() != Some(name) {
                    return Err(DaemonError::Unauthorized);
                }
                d.hello(name, &req)?
            }
        };
        self.handler.on_hello(name);
        self.reactivate(name).await;
        self.bump();
        Ok(response)
```

  `reactivate` is the existing `if let Some(addr) = self.registry.ready_addr(name) { … }` block, moved verbatim into its own method.
  - **`plugin_for_token`:** `Declared` → `d.plugin_for_token(token)`; `Packages` → today's body.
  - **`get`:** for a reserved fleet name, `Packages` → `Some(host.record())` and `Declared` → `None`. **`snapshots`:** pushes the plugin record only for `Packages`.
  - **`poll_health`:**

```rust
    pub async fn poll_health(&self) {
        if let Some(d) = self.plugins.declared() {
            for (name, addr) in d.pollable() {
                let result = self.client.health(&addr).await.map_err(|e| e.to_string());
                if let Err(e) = &result {
                    tracing::warn!(plugin = %name, "health check failed: {e}");
                }
                if d.health(&name, result) {
                    self.reactivate(&name).await;
                    self.bump();
                }
            }
            return;
        }
        // … today's loop, unchanged
    }
```

  - **`declare_plugins`:**

```rust
    /// `PUT /v1/plugins` (Spec O §23.2). The rows of a dropped plugin go
    /// with it; its stored managed requests too (Task 7).
    pub async fn declare_plugins(&self, list: DeclaredPlugins) -> Result<Vec<AgentName>, DaemonError> {
        let Some(d) = self.plugins.declared() else {
            return Err(DaemonError::Managed("this daemon reads plugins.yaml".into()));
        };
        if !self.client.trusts() {
            return Err(DaemonError::Managed(
                "this daemon was started without --tls-ca; it cannot call plugins".into(),
            ));
        }
        let dropped = d.replace(list.plugins).map_err(|e| DaemonError::Invalid(e.to_string()))?;
        self.bump();
        Ok(dropped)
    }
```

  `DaemonError::Managed` already answers 409. Check `impl From<DaemonError> for ApiError`; if `Managed` maps to anything else, add a `Conflict(String)` variant mapped to 409 rather than overloading.
  - **`list_plugins`:** `Packages` → `host.list().await`; `Declared` → `d.list(|n| self.registry.active_agents(n))`.
  - **`sync_plugins`, `purge`:** in Kubernetes mode, answer `DaemonError::Managed` with the kubernetes-mode text. Do this in the daemon methods, so the CLI paths and the routes agree.
- [ ] **Step 5: Routes.** In `api.rs`:

```rust
        .route("/v1/plugins", get(list_plugins).put(declare_plugins))
```

```rust
async fn declare_plugins(
    State(state): State<AppState>,
    b: Result<Json<balerix_api::DeclaredPlugins>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(list) = b.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    state.daemon.declare_plugins(list).await?;
    Ok(StatusCode::NO_CONTENT)
}
```

  `list_plugins` calls `state.daemon.list_plugins()`.
- [ ] **Step 6: The two `serve` paths.**
  - The tmux path passes `PluginSetup::Packages(PluginHostConfig { … })` as today.
  - `run_kubernetes` passes `PluginSetup::Declared { state_dir: layout.plugins_state_dir() }`. Drop the `plugins_file` there and update the function's doc comment: it reads no `plugins.yaml` and takes the list from `PUT /v1/plugins`.
  - The `Harness` constructors pass `Packages(plugin_config_in(dir))`.
- [ ] **Step 7: Run the tests and the gate.**

Run: `cargo nextest run -p balerix-server`
Expected: PASS, the whole crate (one machine unchanged).
Run: `mise run check`
Expected: green.

- [ ] **Step 8: Commit**

```bash
git add -A crates
git commit -m "feat(server): Kubernetes mode hosts the operator's plugin list — PUT /v1/plugins, hello against the grant, readiness from health (Spec O §23.2)"
```

---

### Task 7: Managed fleets in Kubernetes mode

**Files:**
- Create: `crates/balerix-server/src/kube/managed.rs`
- Modify: `crates/balerix-server/src/kube/mod.rs`
- Modify: `crates/balerix-server/src/daemon.rs` (`Caller::Kubernetes { managed_by }`, `check_owner`, `apply_kube`, `manage_fleet`, `down_as` caller path, `declare_plugins`, `forget_purged`, `start`)
- Modify: `crates/balerix-server/src/plugin_api.rs` (`delete_fleet`)
- Modify: `crates/balerix-server/src/api.rs` (`GET /v1/managed-fleets`, `update_fleet` passes `managed_by`)
- Test: `crates/balerix-server/tests/kube_plugins_it.rs`, plus unit tests in `managed.rs`

**Interfaces:**
- Consumes: `ManagedFleet`, `FleetRequest.managed_by` (Task 1); `PluginSource::Declared` (Task 6).
- Produces:

```rust
pub struct ManagedStore { dir: PathBuf, rows: Mutex<BTreeMap<String, ManagedFleet>> }
impl ManagedStore {
    pub fn load(dir: PathBuf) -> Arc<Self>;           // <state>/managed; a bad file is logged and skipped
    pub fn put(&self, row: ManagedFleet);             // live (down = None), persisted
    pub fn mark_down(&self, name: &str, down: DownQuery);
    pub fn forget(&self, name: &str);
    pub fn forget_plugin(&self, plugin: &str) -> Vec<String>;
    pub fn list(&self) -> Vec<ManagedFleet>;           // by name
}
// Caller:
Kubernetes { managed_by: Option<AgentName> }
// Daemon::apply_kube gains `managed_by: Option<AgentName>`.
```

  `PluginSetup::Declared` gains `managed_dir: PathBuf` (`<state>/managed`, from a new `StateLayout::managed_dir()` beside `fleets_dir()`).

- [ ] **Step 1: Write the failing tests.** Unit tests in `managed.rs`: a `put` survives a reload; `mark_down` keeps the row with its query; `forget_plugin` removes and returns only that plugin's names; a corrupt file is skipped with the others loaded.

In `kube_plugins_it.rs`, add a test named `a_managed_put_is_stored_listed_and_applied_by_the_operator_as_the_plugins`. Reuse the TLS stub world from Task 6. Declare `fake` with grant `["fleets", "manage"]`, say hello with those needs, then:

```rust
    // the plugin applies a fleet file: answered now, with the record
    let file = json!({ "crews": { "c": { "repo": "acme/api", "agents": { "a": {} } } } });
    let (s, rec) = w.call("PUT", "/v1/plugin-host/fleets/gh-1", Some(&token), Some(json!({ "file": file }))).await;
    assert_eq!((s, rec["owner"].as_str()), (200, Some("fake")));
    // the restricted surface is still checked synchronously
    let (s, v) = w.call("PUT", "/v1/plugin-host/fleets/gh-2", Some(&token),
        Some(json!({ "file": { "defaults": { "env": { "X": "1" } }, "crews": {} } }))).await;
    assert_eq!(s, 400, "{v}");
    // the operator sees it
    let (s, rows) = w.call("GET", "/v1/managed-fleets", Some(ADMIN), None).await;
    assert_eq!((s, rows.as_array().unwrap().len()), (200, 1));
    assert_eq!((rows[0]["name"].as_str(), rows[0]["plugin"].as_str()), (Some("gh-1"), Some("fake")));
    assert!(rows[0].get("down").is_none());
    // the operator applies it on the plugin's behalf; the owner stays the plugin
    let mut req = request_for("gh-1", Some(json!({ "gh-1/c/a": "a".repeat(32) })));
    req["managed_by"] = json!("fake");
    let (s, rec) = w.call("PUT", "/v1/fleets/gh-1", Some(ADMIN), Some(req.clone())).await;
    assert_eq!((s, rec["owner"].as_str()), (200, Some("fake")));
    // the operator without managed_by, or for another plugin, is refused
    req["managed_by"] = json!("other");
    let (s, _) = w.call("PUT", "/v1/fleets/gh-1", Some(ADMIN), Some(req.clone())).await;
    assert_eq!(s, 409);
    req.as_object_mut().unwrap().remove("managed_by");
    let (s, v) = w.call("PUT", "/v1/fleets/gh-1", Some(ADMIN), Some(req)).await;
    assert_eq!((s, v["error"].as_str()), (409, Some("fleet gh-1 is managed by plugin fake")));
    // the CLI is refused as on one machine
    let (s, _) = w.call("PUT", "/v1/fleets/gh-1", Some(ADMIN), Some(json!({ "spec": rec["spec"] }))).await;
    assert_eq!(s, 409);
    // the plugin downs it: the row says so
    let (s, _) = w.call("DELETE", "/v1/plugin-host/fleets/gh-1?keep_repos=true&keep_sessions=false&purge=false&force=false", Some(&token), None).await;
    assert_eq!(s, 200);
    let (_, rows) = w.call("GET", "/v1/managed-fleets", Some(ADMIN), None).await;
    assert_eq!(rows[0]["down"]["keep_repos"], true);
    // dropping the plugin drops its requests
    let (s, _) = w.call("PUT", "/v1/plugins", Some(ADMIN), Some(json!({ "plugins": [] }))).await;
    assert_eq!(s, 204);
    let (_, rows) = w.call("GET", "/v1/managed-fleets", Some(ADMIN), None).await;
    assert_eq!(rows, json!([]));
```

`request_for(name, tokens)` is `kube_api_it.rs`'s `request` generalised to a fleet name; its spec has crew `c` and agent `a`. A restart test: build a second daemon on the same state dir, and check that `GET /v1/managed-fleets` still lists the live request.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo nextest run -p balerix-server -E 'test(managed)'`
Expected: FAIL to compile.

- [ ] **Step 3: Implement.**
  - **`managed.rs`** stores one file per row, `<dir>/<name>.json`, written atomically. A row's name is a `FleetName`, so it is safe as a file name. `load` reads every `*.json` and skips (and logs) a file that does not parse.
  - **`Caller`:** `Kubernetes { managed_by: Option<AgentName> }`. `owner()` gives `managed_by` when set, else `KUBERNETES_OWNER`. `check_owner` gains these arms ahead of the existing Kubernetes arms:

```rust
            (Caller::Kubernetes { managed_by: Some(me) }, Some(p)) if p != me.as_str() => Err(managed(p)),
            (Caller::Kubernetes { managed_by: Some(_) }, _) => Ok(()),
```

  The two existing `Caller::Kubernetes` arms become `Caller::Kubernetes { managed_by: None }`.
  - **`apply_kube`:** gains `managed_by: Option<AgentName>` and passes `Caller::Kubernetes { managed_by }`. In `update_fleet`, parse `req.managed_by` as an `AgentName` (400 `managed_by: <e>` on a bad name). A `managed_by` without `agent_tokens` is 400 `managed_by is sent only with agent_tokens`.
  - **`manage_fleet`:** after a successful `apply_as`, when `self.plugins.declared()` is `Some`, call `managed.put(ManagedFleet { name, plugin, file, down: None })`. Keep a clone of `file` taken before it moves into the resolver.
  - **`plugin_api::delete_fleet`:** after a successful `down_as` with `Caller::Plugin`, call `daemon.managed_down(&name, q)`, a new small daemon method that is a no-op without a store.
  - **`declare_plugins`:** for each dropped name, call `managed.forget_plugin(name)`.
  - **`forget_purged`:** `managed.forget(name)`.
  - **Store ownership:** hold the store as `managed: Option<Arc<ManagedStore>>` on `Daemon`, set from `PluginSetup::Declared { managed_dir, .. }`.
  - **Route:** `GET /v1/managed-fleets` (admin) → `Json(daemon.managed_fleets())`. That is the list, or 409 with the tmux-mode text `this daemon reads plugins.yaml` when there is no store.
  - **`run_kubernetes`:** passes `managed_dir: layout.managed_dir()`.
- [ ] **Step 4: Run the tests and the gate.**

Run: `cargo nextest run -p balerix-server`
Expected: PASS (the `manage_it` one-machine suite unchanged).
Run: `mise run check && mise run operator`
Expected: green. The operator only consumes `FleetRequest`, which already has the field from Task 1.

- [ ] **Step 5: Commit**

```bash
git add -A crates
git commit -m "feat(server): managed fleets in Kubernetes mode — stored, listed for the operator, applied on the plugin's behalf (Spec O §23.3)"
```

---

### Task 8: Two-process tests: the real Daemon and the real fake plugin over TLS

**Files:**
- Create: `crates/balerix/tests/cli_kube_plugins.rs`
- Modify: `crates/balerix/Cargo.toml` (dev-dependencies, if `rustls`/`rustls-pki-types`/`rcgen` are not already there; `cli_serve.rs` uses them, so they likely are)

**Interfaces:**
- Consumes: everything above. The binaries are `balerix serve --mode kubernetes --tls-ca` and `balerix dev fake-plugin`.
- Produces: nothing for other tasks.

This test file carries §23.6's two-process cases. It lifts `fake_tools`, `balerix`, `wait_for_file`, `tls_files`, `tls_get` and `Kill` from `cli_serve.rs`. Move those into `crates/balerix/tests/support/mod.rs` and `mod support;` from both files, rather than copying them. Add a `tls_call(addr, ca, method, path, token, body: Option<&Value>) -> (u16, Value)` beside `tls_get`, on the same raw rustls stream, sending `Content-Type: application/json` and `Content-Length`.

- [ ] **Step 1: Write the tests.**

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Spec O §23.6 (4a): `balerix serve --mode kubernetes` and `balerix dev
//! fake-plugin` as two processes, TLS both ways under one authority.

mod support;
use support::*;

const ADMIN: &str = "0123456789abcdef0123456789abcdef";
const FAKE_TOKEN: &str = "fake-token-0123456789abcdef01234567";

struct Kube {
    _home: tempfile::TempDir,
    addr: String,
    ca: std::path::PathBuf,
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
    _daemon: Kill,
}

/// The Daemon in Kubernetes mode with `--tls-ca`, on a state dir that a
/// second start can reuse (`home`).
fn kube_daemon(home: tempfile::TempDir, port: Option<u16>) -> Kube {
    let tools = home.path().join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    fake_tools(&tools);
    let tls_dir = home.path().join("tls");
    std::fs::create_dir_all(&tls_dir).unwrap();
    // one authority across restarts: reuse the files when they exist
    let (ca, cert, key) = if tls_dir.join("ca.crt").exists() {
        (tls_dir.join("ca.crt"), tls_dir.join("tls.crt"), tls_dir.join("tls.key"))
    } else {
        tls_files(&tls_dir)
    };
    let token_file = home.path().join("admin-token");
    std::fs::write(&token_file, format!("{ADMIN}\n")).unwrap();
    let endpoint = home.path().join(".local/state/balerix/server/endpoint");
    let _ = std::fs::remove_file(&endpoint);
    let child = balerix(home.path(), &tools)
        .args(["serve", "--mode", "kubernetes", "--tmux-socket", "unused", "--bind"])
        .arg(format!("127.0.0.1:{}", port.unwrap_or(0)))
        .arg("--tls-cert").arg(&cert)
        .arg("--tls-key").arg(&key)
        .arg("--tls-ca").arg(&ca)
        .arg("--admin-token-file").arg(&token_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let url = wait_for_file(&endpoint);
    Kube {
        addr: url.trim_start_matches("https://").to_string(),
        _home: home,
        ca,
        cert,
        key,
        _daemon: Kill(child),
    }
}

/// A free loopback port: bound then released. The fake binds it again.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// `balerix dev fake-plugin` as a pod would run it: every input a file or
/// a variable, TLS both ways.
fn fake_plugin(k: &Kube, port: u16, scratch: &std::path::Path) -> Kill {
    let token = scratch.join("token");
    std::fs::write(&token, FAKE_TOKEN).unwrap();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_balerix"))
        .args(["dev", "fake-plugin"])
        .env_clear()
        .env("BALERIX_API_URL", format!("https://{}", k.addr))
        .env("BALERIX_PLUGIN_NAME", "fake")
        .env("BALERIX_PLUGIN_TOKEN_FILE", &token)
        .env("BALERIX_PLUGIN_SCRATCH", scratch)
        .env("BALERIX_CA_FILE", &k.ca)
        .env("BALERIX_PLUGIN_TLS_CERT", &k.cert)
        .env("BALERIX_PLUGIN_TLS_KEY", &k.key)
        .env("BALERIX_PLUGIN_LISTEN", format!("127.0.0.1:{port}"))
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    Kill(child)
}

fn declare(k: &Kube, port: u16, grant: &[&str], config: serde_json::Value) {
    let (s, v) = tls_call(&k.addr, &k.ca, "PUT", "/v1/plugins", Some(ADMIN), Some(&serde_json::json!({
        "plugins": [{ "name": "fake", "grant": grant, "config": config,
                      "token": FAKE_TOKEN, "url": format!("https://127.0.0.1:{port}") }]
    })));
    assert_eq!(s, 204, "{v}");
}

fn wait_for_phase(k: &Kube, phase: &str) -> serde_json::Value {
    let start = std::time::Instant::now();
    loop {
        let (_, rows) = tls_call(&k.addr, &k.ca, "GET", "/v1/plugins", Some(ADMIN), None);
        if rows[0]["phase"] == phase {
            return rows[0].clone();
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(15), "phase {phase} never came: {rows}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn a_fake_plugin_in_its_own_process_says_hello_over_tls_and_intercepts() {
    let k = kube_daemon(tempfile::tempdir().unwrap(), None);
    let port = free_port();
    let scratch = tempfile::tempdir().unwrap();
    declare(&k, port, &["actions", "fleets", "kv", "manage"], serde_json::json!({ "greeting": "hi" }));
    let _fake = fake_plugin(&k, port, scratch.path());
    wait_for_phase(&k, "ready");
    assert!(wait_for_file(&scratch.path().join("fake-plugin.hello")).contains("\"greeting\": \"hi\""));
    // the operator's fleet with the fake enabled for its agent; a hook
    // event from the agent's sidecar is intercepted over TLS
    let tokens = serde_json::json!({ "f/c/a": "a".repeat(32) });
    let (s, v) = tls_call(&k.addr, &k.ca, "PUT", "/v1/fleets/f", Some(ADMIN), Some(&fleet_request("f", "fake", tokens)));
    assert_eq!(s, 200, "{v}");
    // activation is async: wait until the pair is active
    wait_until(|| {
        let (_, rec) = tls_call(&k.addr, &k.ca, "GET", "/v1/fleets/f", Some(ADMIN), None);
        rec["status"]["agents"]["f/c/a"]["plugins"]["fake"]["state"] == "active"
    });
    let (s, verdict) = tls_call(&k.addr, &k.ca, "POST", "/v1/agents/f/c/a/events", Some(&"a".repeat(32)),
        Some(&serde_json::json!({ "hook_event_name": "PreToolUse", "session_id": "s1",
                                   "tool_name": "Bash", "tool_input": { "command": "rm -rf /" } })));
    assert_eq!((s, verdict["decision"].as_str()), (200, Some("block")), "{verdict}");
}

#[test]
fn a_grant_short_of_the_manifest_refuses_the_hello_and_the_row_says_which() {
    let k = kube_daemon(tempfile::tempdir().unwrap(), None);
    let port = free_port();
    let scratch = tempfile::tempdir().unwrap();
    declare(&k, port, &["actions", "fleets", "kv"], serde_json::json!({}));
    let _fake = fake_plugin(&k, port, scratch.path());
    let row = wait_for_phase(&k, "failed");
    assert_eq!(row["message"], "hello.manifest.needs: manage is not granted");
}

#[test]
fn a_managed_fleet_from_the_plugin_is_listed_for_the_operator() {
    let k = kube_daemon(tempfile::tempdir().unwrap(), None);
    let port = free_port();
    let scratch = tempfile::tempdir().unwrap();
    let file = serde_json::json!({ "crews": { "c": { "repo": "acme/api", "agents": { "a": {} } } } });
    declare(&k, port, &["actions", "fleets", "kv", "manage"],
        serde_json::json!({ "manage": { "fleet": "gh-1", "file": file } }));
    let _fake = fake_plugin(&k, port, scratch.path());
    let outcome = wait_for_file(&scratch.path().join("fake-plugin.manage"));
    assert!(outcome.contains("\"owner\": \"fake\""), "{outcome}");
    let (_, rows) = tls_call(&k.addr, &k.ca, "GET", "/v1/managed-fleets", Some(ADMIN), None);
    assert_eq!(rows[0]["name"], "gh-1");
    assert_eq!(rows[0]["plugin"], "fake");
}

#[test]
fn a_daemon_restart_keeps_a_running_plugin_ready_without_a_new_hello() {
    let k = kube_daemon(tempfile::tempdir().unwrap(), None);
    let port = free_port();
    let scratch = tempfile::tempdir().unwrap();
    declare(&k, port, &["actions", "fleets", "kv", "manage"], serde_json::json!({}));
    let _fake = fake_plugin(&k, port, scratch.path());
    wait_for_phase(&k, "ready");
    let hello_at = std::fs::metadata(scratch.path().join("fake-plugin.hello")).unwrap().modified().unwrap();
    // restart the Daemon on the same state (the fake keeps running), then
    // re-send the list as the operator does after a restart
    let k = restart(k);
    declare(&k, port, &["actions", "fleets", "kv", "manage"], serde_json::json!({}));
    wait_for_phase(&k, "ready"); // the first health poll, ≤ 10 s
    let again = std::fs::metadata(scratch.path().join("fake-plugin.hello")).unwrap().modified().unwrap();
    assert_eq!(hello_at, again, "no second hello");
}
```

**Helpers to write:**
- `fleet_request(name, plugin, tokens)` returns `{ "spec": <spec with crew c, agent a, plugins.<plugin>: {}>, "agent_tokens": tokens }`.
- `wait_until(f)` polls every 100 ms for up to 15 s.
- `restart(k: Kube) -> Kube` takes the port from `k.addr`, moves `k._home` out, and drops the rest (the `Kill` stops the Daemon). It then calls `kube_daemon(home, Some(port))`, so the fake's `BALERIX_API_URL` stays valid and the same authority files are reused. Make `Kube`'s fields destructurable (drop the leading underscores, or destructure with `let Kube { _home, addr, .. } = k;`) so the `TempDir` survives the restart.

**Two limits of these tests:**
- **Restart timing.** The restart test waits up to 15 s, because readiness after a restart comes from the 10 s health poll. It is the slowest test in the crate. Mark it `#[ignore]` only if it pushes the suite past its CI budget; if you do, say so in the PR body.
- **Kubernetes mode needs no fake tools.** It never runs tmux, git or nono, so the fake binary needs no tools on `PATH`. Keep `fake_tools` anyway, because `serve` discovers them at start.

- [ ] **Step 2: Run them.**

Run: `cargo nextest run -p balerix --test cli_kube_plugins`
Expected: PASS. If the intercept test sees a 200 without `block`, check the activation state first: the verdict comes only from an active pair.

- [ ] **Step 3: Gate and commit.**

Run: `mise run check`
Expected: green.

```bash
git add -A crates/balerix/tests
git commit -m "test(balerix): the Daemon in Kubernetes mode and the fake plugin as two processes over TLS (Spec O §23.6)"
```

---

### Task 9: Docs, the spec's plan decisions, push and draft PR

**Files:**
- Modify: `docs/plugin-protocol.md`: `hello.manifest`; the five new variables; TLS in a pod; `listen` ignored in Kubernetes mode; `PUT /v1/plugins`, `GET /v1/managed-fleets` and the 409s on the admin side
- Modify: `docs/THREAT-MODEL.md`: the "Plugin ↔ daemon" boundary in a pod (TLS under the Daemon's authority, the token as a mounted file, no nono, the pod as the boundary, `needs` within the operator's grant); the "compromised plugin" actor gets "in Kubernetes mode: its pod, its token, and nothing it was not granted"
- Modify: `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md`: add `### 23.7 Decided by the plan`, listing the plan's "Decisions this plan makes beyond §23" bullets and any ruling made during execution
- Modify: `AGENTS.md`, only if the task list or gotchas name `check-core-deps`'s feature set or the plugin `hello` shape

- [ ] **Step 1: Write the docs.** Keep each doc's voice:
  - `plugin-protocol.md` is a reference. Add rows and short paragraphs; no narrative.
  - THREAT-MODEL adds to the existing boundary bullet and actor; it does not open a new section.
  - Run `grep -n "reqwest\|json alone\|hello" AGENTS.md` and fix only what is now wrong.
- [ ] **Step 2: Run the full gates.**

Run: `mise run check && mise run operator && for p in flow web matrix github; do scripts/plugin.sh check $p; done`
Expected: all green.

- [ ] **Step 3: Commit, push, and open a draft PR.** The plan authorises the push and the draft. Merging is the user's call.

```bash
git add -A docs AGENTS.md
git commit -m "docs: plugins in Kubernetes mode — protocol, threat model, and the plan's decisions (Spec O §23.7)"
git push -u origin feat/kube-plugins-offline
gh pr create --draft --base main --title "feat: plugins without a cluster (Spec O 4a, §23.1–§23.3)" --body-file - <<'EOF'
Spec O sub-project 4a: the Daemon in Kubernetes mode hosts the operator's plugin list; the SDK runs in a pod.

- `hello` carries the manifest; `needs` is checked against the operator's grant.
- TLS both ways under the Daemon's authority (rustls + ring, authority-only); `serve --tls-ca`.
- `PUT /v1/plugins`, readiness from the health poll, hellos persisted across a Daemon restart.
- Managed fleets stored and listed at `GET /v1/managed-fleets`; the operator applies them with `managed_by`.
- Two-process tests: `balerix serve --mode kubernetes` + `balerix dev fake-plugin` over TLS.

One machine is unchanged. 4b (the Plugin controller, images, e2e-k8s) is next.

Spec: §23 (decisions by the plan: §23.7).
EOF
```

Expected: a draft PR URL. Wait for CI, and report any red job with its log rather than re-running it blind.

---

## Self-review notes (for the reviewer of this plan)

- **Spec coverage:**
  - §23.1 → Tasks 1–4.
  - §23.2 → Tasks 1, 5 and 6. The `kubernetes` reserved name is Task 1.
  - §23.3 → Tasks 1 and 7.
  - §23.6 (4a) → Tasks 5–8.
  - §23.4/§23.5 and 4b's half of §23.6 are the next plan.
- **Out of scope here, deliberately:**
  - The Daemon pod's `--tls-ca` mount and the operator sending the list: both are 4b.
  - github and matrix in a pod. They compile with `manifest()`; nothing here runs them.
