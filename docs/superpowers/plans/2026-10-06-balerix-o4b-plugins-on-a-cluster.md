# Balerix O4b: plugins on a cluster — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The operator runs plugins. A Plugin listed by a Daemon gets its pod, Service, Secrets and policy. The Daemon gets the plugin list, and managed fleets become Fleets. `e2e-k8s` passes with flow, web and `dev fake-plugin` on kind (Spec O §17, sub-project 4).

**Architecture:**
- §23.8's revision lands first, in `balerix-api`, the Daemon's `Declared` source and the SDK. Without it a rolled plugin can stick at `Starting`.
- The operator follows the 3a/3b split:
  - pure builders in `desired::plugin`, pinned by YAML snapshots;
  - a new Plugin controller in `controllers/plugin.rs`;
  - the Daemon controller's list, `PluginsReady` and managed-Fleet writer in `controllers/daemon_plugins.rs`;
  - all of these tested on envtest against a stub Daemon that grows the three plugin routes.
- kind-up builds three plugin images, and `e2e_k8s.rs` gains a second journey.

**Tech Stack:** Rust 1.99, kube 4.2.0 (`runtime`), k8s-openapi 0.28 (`v1_32`), axum 0.8, reqwest 0.13 (rustls, ring), rcgen 0.14, insta 1.49 (yaml), cargo-nextest, envtest 1.34.1, kind 0.33.0, kubectl 1.34.12, musl targets for the plugin binaries.

**Spec:** `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md`, §23.4–§23.6 and §23.8. Also read §5.5, §9, §20.2, §21.2, §21.4, §22.5 and §23.1–§23.3, which are context for what this plan changes.

## Global Constraints

- Branch `feat/kube-plugins-cluster`, cut from `docs/spec-o-4b`. The user's uncommitted `claude = "2.1.291"` line in `mise.toml` is committed on this branch in Task 9, and `mise run verify-claude` and `mise run verify-questions` are run there.
- Plugin port: `7644`. A plugin's url is `https://<plugin>.<namespace>.svc:7644`, and its Service is named `<plugin>`.
- Labels:
  - `balerix.ai/managed-by: <plugin>` on a managed Fleet;
  - `balerix.ai/plugin: <plugin>` on a plugin's objects;
  - component `plugin`.
- Pod mounts are under `/balerix/{token,tls,ca}`, scratch is at `/balerix/scratch` and `/tmp` is an `emptyDir`. The pod uses the agent pod's security context (`pod_security()`, `container_security()`).
- The revision is `desired::common::hash` over the spec, the grant, the resolved config, the referenced Secrets' values, the token and the serving certificate. It goes to the pod's `HASH_ANNOTATION` (`balerix.ai/input-hash`), to `BALERIX_PLUGIN_REVISION`, and to `DeclaredPlugin.revision`.
- Status texts, verbatim:
  - `hello.revision: required in kubernetes mode`
  - `hello.revision: this daemon holds <a>, the plugin is <b>; the list has not arrived yet` (409)
- Condition reasons:
  - Plugin `Deployed`: `NotListed`, `PluginListedTwice`, `WaitingForDaemon`, `InvalidSpec`, `SecretMissing`, `Available`, `DeploymentNotAvailable`.
  - Plugin `Ready`: the same blockers, plus `Ready`, `PluginRefused`, `PluginNotReady`, `DaemonUnavailable`.
  - Daemon `PluginsReady`: `NoPlugins`, `AllReady`, `PluginMissing`, `PluginListedTwice`, `PluginRefused`, `PluginNotReady`, and `Pending` while the Daemon has not answered. `PluginsUnsupported` is removed.
- Event reason: `FleetConflict`, a Warning on the Plugin.
- SDK hello retry: only on a 409, starting at 1 s, doubling to 30 s, with no limit.
- Not in scope: the chart and its RBAC, github and matrix in `e2e-k8s`, and hot reload of certificates.
- Every `desired` function is pure. Tokens, certificates and the time come in as inputs (§20.2).
- `mise run check`, `mise run operator` and `mise run plugins` stay green after every task. CI runs `e2e-k8s`.

## Review Focus

1. **A plugin that is listed, unlisted and listed again.** The objects are deleted on unlist and made again, with a new token, on relist. The Daemon's row comes back to `Ready` without an operator restart. (Task 5's `a_plugin_unlisted_owns_nothing_and_relisted_comes_back`.)
2. **A referenced Secret deleted while the plugin runs.** The Plugin reports `SecretMissing` naming `spec.secrets.<key>` and never a value. The Daemon controller does not send a list that pass. 4a's Daemon drops the managed requests of any plugin missing from a list (§23.7), so leaving the plugin out would delete its Fleets over a transient Secret. The Daemon keeps its last list, `PluginsReady` says why, and no Fleet is touched. Only a plugin whose token or serving Secret is not made yet, which is new and so has no requests, is left out of a pass (§23.4). (Task 5 `a_missing_secret_blocks_the_plugin_and_names_the_key`; Task 6 `a_plugin_whose_config_cannot_be_built_holds_the_list_back`, `a_plugin_whose_secrets_are_not_made_is_left_out_of_the_list`.)
3. **A managed request whose name is an existing Fleet the user wrote.** The user's Fleet is never touched, and the Plugin gets `FleetConflict`. (Task 6 `a_managed_request_never_overwrites_a_fleet_it_does_not_manage`.)
4. **The Daemon pod restarting.** The next reconcile sends the list again and the rows come back, with no new `hello`. This is already covered by 4a in two processes. Task 6's `the_list_is_sent_on_every_reconcile` pins that the operator re-sends it.
5. **The plugin pod starting before the list arrives.** It waits on 409 and is accepted once the list lands, without a pod restart. (Task 1 SDK test; Task 8 journey step "web's config changes".)

---

## File Structure

**Core workspace (Task 1)**
- `crates/balerix-api/src/plugin.rs`: `HelloRequest.revision`, `DeclaredPlugin.revision`.
- `crates/balerix-server/src/daemon.rs`: `DaemonError::ListPending`, and `revision: None` in one test.
- `crates/balerix-server/src/api.rs`: `ListPending` → 409.
- `crates/balerix-server/src/kube/declared.rs`: the revision check, and `replace` requiring it.
- `crates/balerix-plugin-sdk/src/lib.rs`: `Env.revision`.
- `crates/balerix-plugin-sdk/src/host.rs`: `hello` sends it.
- `crates/balerix-plugin-sdk/src/plugin.rs`: `serve_on` retries a 409.
- `crates/balerix-plugin-sdk/src/testing.rs`: `FakeHost::refuse_hellos`, and `revision: None` in `env`.
- `crates/balerix/tests/cli_kube_plugins.rs` and `crates/balerix-server/tests/kube_plugins_it.rs`: lists and hellos carry `revision`.

**Operator (Tasks 2–6)**
- `operator/src/daemon_client.rs`: `declare_plugins`, `plugins`, `managed_fleets`.
- `operator/tests/support/stub_daemon.rs`: the three routes.
- `operator/tests/client_it.rs`: against stubs and a real Daemon with `--tls-ca`.
- `operator/src/desired/names.rs`: plugin names and url.
- `operator/src/pki.rs`: `plugin_names`.
- `operator/src/desired/plugin.rs` (new):
  - the types `PluginInputs`, `PluginObjects` and `PluginState`;
  - the functions `grant`, `inject_secrets`, `revision`, `declared`, `plugin_objects`, `plugin_conditions`, `listing`, `plugins_ready`, `managed_fleet` and `retain_of`.
- `operator/src/desired/daemon.rs`:
  - the StatefulSet mounts the authority and passes `--tls-ca`;
  - `daemon_status`'s `PluginsReady` becomes `NoPlugins` or `Pending`.
- `operator/src/desired/fleet.rs`: `managed_by` from the label.
- `operator/src/api/plugin.rs`: the doc comment only.
- `operator/src/controllers/plugin.rs` (new): the Plugin controller, `read_config`, `read_token`, `read_serving`.
- `operator/src/controllers/daemon_plugins.rs` (new): `send_list`, `write_managed`.
- `operator/src/controllers/daemon.rs`: calls them, and requeues at `fleet_period` while plugins are listed.
- `operator/src/controllers/mod.rs`: registers the Plugin controller.
- `operator/tests/plugins_it.rs` (new): envtest tests for Tasks 5 and 6.
- `operator/tests/support/mod.rs`: `make_daemon_ready`.
- `operator/.config/nextest.toml`: `plugins_it` joins the envtest group; the `e2e-k8s` profile runs one test at a time.

**Images, e2e, CI (Tasks 7–8)**
- `scripts/kind-up.sh`: three plugin images.
- `scripts/operator.sh`: `BALERIX_K8S_PLUGIN_IMAGES`.
- `operator/tests/e2e_k8s.rs`: shared setup extracted, plus `the_plugin_journey_on_kind`.
- `.github/workflows/ci.yml`: the e2e-k8s job installs `musl-tools`, `jq` and the musl target.

**Docs (Task 9)**
- Spec §23.9 "Decided by the 4b plan", and §17 item 4 marked done.

---

### Task 1: A hello belongs to one revision (§23.8)

**Files:**
- Modify: `crates/balerix-api/src/plugin.rs:103-158` and its tests at `:337`, `:459`, `:473`.
- Modify: `crates/balerix-server/src/daemon.rs:85-100` and `:1577`.
- Modify: `crates/balerix-server/src/api.rs:68-80`.
- Modify: `crates/balerix-server/src/kube/declared.rs` (`replace` at `:111`, `hello` at `:253`, tests at `:402-430`).
- Modify: `crates/balerix-plugin-sdk/src/lib.rs:23-125`.
- Modify: `crates/balerix-plugin-sdk/src/host.rs:140-157`.
- Modify: `crates/balerix-plugin-sdk/src/plugin.rs:286-321`.
- Modify: `crates/balerix-plugin-sdk/src/testing.rs` (`Inner` at `:37`, `env` at `:114`, `hello` at `:347`).
- Modify: `crates/balerix/tests/cli_kube_plugins.rs:105-140`.
- Modify: `crates/balerix-server/tests/kube_plugins_it.rs:148-150` and every raw hello JSON in it (`:409` and any other `"protocol": 1` hello).

**Interfaces:**
- Produces:
  - `balerix_api::HelloRequest { …, revision: Option<String> }` (serde default, skipped when `None`);
  - `balerix_api::DeclaredPlugin { …, revision: String }` (required);
  - `balerix_server::DaemonError::ListPending(String)` → 409;
  - `balerix_plugin_sdk::Env { …, revision: Option<String> }`, read from `BALERIX_PLUGIN_REVISION`;
  - `FakeHost::refuse_hellos(&self, n: usize, status: u16, message: &str)`.
- Task 3's `declared()` fills `DeclaredPlugin.revision`, and Task 3's Deployment sets `BALERIX_PLUGIN_REVISION`.

- [ ] **Step 1: Cut the branch**

```bash
cd /workspace
git switch docs/spec-o-4b
git switch -c feat/kube-plugins-cluster
git status --short   # only ` M mise.toml` (the claude bump; committed in Task 9)
```

- [ ] **Step 2: Write the failing wire tests** in `crates/balerix-api/src/plugin.rs`'s test module

```rust
#[test]
fn a_hello_carries_the_revision_only_when_given() {
    let mut h: HelloRequest = serde_json::from_value(json!({
        "name": "flow", "version": "1.0.0", "protocol": 1, "listen": "0.0.0.0:7644"
    }))
    .unwrap();
    assert_eq!(h.revision, None);
    assert!(!serde_json::to_string(&h).unwrap().contains("revision"));
    h.revision = Some("r1".into());
    assert_eq!(serde_json::to_value(&h).unwrap()["revision"], json!("r1"));
}

#[test]
fn a_declared_plugin_requires_its_revision() {
    let entry = json!({
        "name": "flow", "grant": ["kv"], "token": "t0123456789abcdef0123456789abcdef",
        "url": "https://flow.ns.svc:7644"
    });
    let e = serde_json::from_value::<DeclaredPlugin>(entry.clone()).unwrap_err();
    assert!(e.to_string().contains("missing field `revision`"), "{e}");
    let mut with = entry;
    with["revision"] = json!("r1");
    assert_eq!(serde_json::from_value::<DeclaredPlugin>(with).unwrap().revision, "r1");
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo nextest run -p balerix-api a_hello_carries_the_revision a_declared_plugin_requires_its_revision`
Expected: compile error, `no field revision`.

- [ ] **Step 4: Add the fields**

In `HelloRequest`, after `manifest`:

```rust
    /// The revision of the list entry this plugin was built for (Spec O
    /// §23.8): `BALERIX_PLUGIN_REVISION`. Sent, like the manifest, only
    /// with an authority; a Daemon in Kubernetes mode refuses a hello
    /// without it and answers 409 to another one. Ignored on one machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
```

In `DeclaredPlugin`, after `url`:

```rust
    /// The operator's hash over everything the plugin's pod is built from
    /// (Spec O §23.8); the pod's `BALERIX_PLUGIN_REVISION` carries the same.
    pub revision: String,
```

Add `.field("revision", &self.revision)` to the hand-written `Debug`. Then fix every existing `HelloRequest { … }` and `DeclaredPlugin` literal the compiler points at:
- `revision: None` in `plugin.rs:338`, `plugin.rs:459` and `daemon.rs:1577`;
- `"revision": "r1"` in the JSON at `plugin.rs:477`.

- [ ] **Step 5: Run the wire tests**

Run: `cargo nextest run -p balerix-api`
Expected: PASS.

- [ ] **Step 6: Write the failing `Declared` tests** in `kube/declared.rs`

Update the helpers first:
- `entry` gains `revision: "r1".into()`;
- `hello` gains `revision: Some("r1".into())`.

Then add:

```rust
#[test]
fn another_revision_is_409_and_leaves_the_entry_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let reg = PluginRegistry::new();
    let d = DeclaredPlugins::new(dir.path().into(), reg.clone());
    d.replace(vec![entry("flow", &[Capability::Actions, Capability::Kv])]).unwrap();
    d.hello(&n("flow"), &hello("flow", "actions, kv")).unwrap();
    assert_eq!(d.list(|_| 0)[0].phase, AgentPhase::Ready);

    // a new pod built for r2 says hello before the list carrying r2 arrives
    let mut early = hello("flow", "actions, kv");
    early.revision = Some("r2".into());
    let err = d.hello(&n("flow"), &early).unwrap_err();
    assert_eq!(
        err,
        DaemonError::ListPending(
            "hello.revision: this daemon holds r1, the plugin is r2; the list has not arrived yet"
                .into()
        )
    );
    // the old pod still serves: nothing changed
    let row = &d.list(|_| 0)[0];
    assert_eq!((row.phase, row.message.as_str()), (AgentPhase::Ready, ""));
    assert!(reg.ready_addr(&n("flow")).is_some(), "still in the chain");

    // the list arrives: the entry waits, and the early pod is accepted
    let mut r2 = entry("flow", &[Capability::Actions, Capability::Kv]);
    r2.revision = "r2".into();
    d.replace(vec![r2]).unwrap();
    assert_eq!(d.list(|_| 0)[0].phase, AgentPhase::Starting);
    d.hello(&n("flow"), &early).unwrap();
    assert_eq!(d.list(|_| 0)[0].phase, AgentPhase::Ready);
}

#[test]
fn a_hello_without_a_revision_is_refused_and_kept_on_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let d = DeclaredPlugins::new(dir.path().into(), PluginRegistry::new());
    d.replace(vec![entry("flow", &[Capability::Kv])]).unwrap();
    let mut none = hello("flow", "kv");
    none.revision = None;
    let err = d.hello(&n("flow"), &none).unwrap_err().to_string();
    assert!(err.ends_with("hello.revision: required in kubernetes mode"), "{err}");
    let row = &d.list(|_| 0)[0];
    assert_eq!(
        (row.phase, row.message.as_str()),
        (AgentPhase::Failed, "hello.revision: required in kubernetes mode")
    );
}

#[test]
fn an_entry_without_a_revision_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let d = DeclaredPlugins::new(dir.path().into(), PluginRegistry::new());
    let mut e = entry("flow", &[]);
    e.revision = String::new();
    assert_eq!(
        d.replace(vec![e]).unwrap_err().to_string(),
        "plugins[0].revision: required"
    );
}
```

In `hello_is_checked_in_order_and_a_refusal_is_kept_on_the_row`, insert the revision case between the protocol and manifest cases:

```rust
let mut no_revision = hello("flow", "kv");
no_revision.revision = None;
// …
(no_revision, "hello.revision: required in kubernetes mode"),
```

- [ ] **Step 7: Run them to see them fail**

Run: `cargo nextest run -p balerix-server --lib kube::declared`
Expected: compile error, `no variant ListPending`.

- [ ] **Step 8: Implement**

In `daemon.rs`'s `DaemonError`:

```rust
    /// Spec O §23.8: a plugin's hello names a revision the Daemon's list
    /// does not hold yet. 409, and nothing changes; the SDK retries.
    #[error("{0}")]
    ListPending(String),
```

In `api.rs`'s mapping, add `DaemonError::ListPending(_) => StatusCode::CONFLICT,`.

In `DeclaredPlugins::replace`, beside the token-length check:

```rust
            if p.revision.is_empty() {
                return Err(err("revision", "required".into()));
            }
```

In `DeclaredPlugins::hello`, replace the `checked` expression:

```rust
        let checked = if req.protocol != PLUGIN_PROTOCOL {
            Err(format!(
                "hello.protocol: this daemon speaks protocol {PLUGIN_PROTOCOL}, got {}",
                req.protocol
            ))
        } else {
            match req.revision.as_deref() {
                None => Err("hello.revision: required in kubernetes mode".to_string()),
                // §23.8: a pod built for the next list; the entry and the
                // pod still serving under it are left alone
                Some(r) if r != e.plugin.revision => {
                    return Err(DaemonError::ListPending(format!(
                        "hello.revision: this daemon holds {}, the plugin is {r}; the list has not arrived yet",
                        e.plugin.revision
                    )));
                }
                Some(_) => match &req.manifest {
                    None => Err("hello.manifest: required in kubernetes mode".to_string()),
                    Some(m) => check_manifest(&e.plugin, m).map(|()| m.clone()),
                },
            }
        };
```

- [ ] **Step 9: Run the Daemon tests**

Run: `cargo nextest run -p balerix-server --lib kube::declared`
Expected: PASS.

- [ ] **Step 10: Write the failing SDK tests**

In `crates/balerix-plugin-sdk/src/lib.rs` tests:

```rust
#[test]
fn the_revision_is_read_and_optional() {
    let base = |k: &str| match k {
        "BALERIX_API_URL" => Some("http://127.0.0.1:1".into()),
        "BALERIX_PLUGIN_NAME" => Some("flow".into()),
        "BALERIX_PLUGIN_TOKEN" => Some("t".into()),
        "BALERIX_PLUGIN_SCRATCH" => Some("/s".into()),
        _ => None,
    };
    assert_eq!(Env::from_env(base).unwrap().revision, None);
    let with = |k: &str| match k {
        "BALERIX_PLUGIN_REVISION" => Some(" r1 ".into()),
        other => base(other),
    };
    assert_eq!(Env::from_env(with).unwrap().revision.as_deref(), Some("r1"));
}
```

In `crates/balerix-plugin-sdk/src/plugin.rs` tests:

```rust
/// Spec O §23.8: a pod that says hello before the list naming its
/// revision arrives is answered 409; it waits, keeps its server bound,
/// and is configured once a hello is accepted.
#[tokio::test]
async fn a_409_hello_is_retried_until_accepted() {
    let fake = crate::testing::FakeHost::start("tok", json!({ "k": 1 }), vec![]).await;
    fake.refuse_hellos(2, 409, "hello.revision: this daemon holds r1, the plugin is r2; the list has not arrived yet");
    let host = Host::new(fake.env("rec", std::path::Path::new("/s"))).unwrap();
    let (listener, listen) = bind().await.unwrap();
    let configured = Arc::new(std::sync::Mutex::new(None::<Value>));
    let plugin = Recording(configured.clone());
    let started = std::time::Instant::now();
    let serving = tokio::spawn(async move { serve_on(&host, "0.1.0", plugin, listener, listen).await });
    let got = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(c) = configured.lock().unwrap().clone() {
                return c;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("configured after the retries");
    assert_eq!(got, json!({ "k": 1 }));
    assert_eq!(fake.hellos().len(), 3, "two refused, one accepted");
    // 1 s, then 2 s
    assert!(started.elapsed() >= std::time::Duration::from_secs(3));
    serving.abort();
}

#[tokio::test]
async fn any_other_refused_hello_still_ends_serve() {
    let fake = crate::testing::FakeHost::start("tok", json!({}), vec![]).await;
    fake.refuse_hellos(1, 400, "hello.revision: required in kubernetes mode");
    let host = Host::new(fake.env("rec", std::path::Path::new("/s"))).unwrap();
    let (listener, listen) = bind().await.unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        serve_on(&host, "0.1.0", Silent, listener, listen),
    )
    .await
    .unwrap();
    assert!(matches!(&result, Err(SdkError::Status { status: 400, .. })), "{result:?}");
}
```

If the test module has no `Recording` plugin that keeps `configure`'s value, add one:

```rust
struct Recording(Arc<std::sync::Mutex<Option<Value>>>);
impl Plugin for Recording {
    async fn configure(&self, config: Value) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(config);
        Ok(())
    }
}
```

Match `configure`'s real signature in `Plugin`; the test at `:647` shows its shape.

- [ ] **Step 11: Run them to see them fail**

Run: `cargo nextest run -p balerix-plugin-sdk revision 409 other_refused`
Expected: compile error, `no field revision` and `no method refuse_hellos`.

- [ ] **Step 12: Implement the SDK side**

In `Env`, add the field with a doc comment:

```rust
    /// `BALERIX_PLUGIN_REVISION` (Spec O §23.8): the list entry this pod
    /// was built for; sent in `hello` only with an authority.
    pub revision: Option<String>,
```

In `from_env`, add `revision: opt("BALERIX_PLUGIN_REVISION"),`. Add `revision: None` in `FakeHost::env` and in any other `Env { … }` literal the compiler finds.

In `Host::hello`:

```rust
            manifest: manifest.cloned(),
            // like the manifest (§23.1): a 0.2.0 daemon refuses unknown fields
            revision: self.env.ca.as_ref().and(self.env.revision.clone()),
```

In `plugin.rs`, add above `serve_on`:

```rust
/// Spec O §23.8: the first wait after a 409 hello, doubled per refusal.
const HELLO_RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
const HELLO_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// `hello`, again after every 409: a pod that started before the Daemon
/// holds its revision waits for the list rather than exit and crash-loop.
async fn hello_until_listed(
    host: &Host,
    version: &str,
    listen: &str,
    manifest: Option<&PluginManifest>,
) -> Result<balerix_api::HelloResponse, SdkError> {
    let mut wait = HELLO_RETRY_FIRST;
    loop {
        match host.hello(version, listen, manifest).await {
            Err(SdkError::Status { status: 409, message }) => {
                tracing::info!("hello: {message}; again in {wait:?}");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(HELLO_RETRY_MAX);
            }
            other => return other,
        }
    }
}
```

In `serve_on`, replace `host.hello(version, &listen, manifest.as_ref()).await` with `hello_until_listed(host, version, &listen, manifest.as_ref()).await`. Use whatever type name `PluginManifest` has in this file's imports.

In `testing.rs`, `Inner` gains `hello_refusals: Mutex<Vec<(u16, String)>>`, defaulted empty in `start`, and `FakeHost` gains:

```rust
    /// The next `n` hellos are recorded and answered `status` with
    /// `{"error": message}`.
    pub fn refuse_hellos(&self, n: usize, status: u16, message: &str) {
        let mut r = self.inner.hello_refusals.lock().unwrap_or_else(|e| e.into_inner());
        r.extend(std::iter::repeat_n((status, message.to_string()), n));
    }
```

In the fake's `hello` handler, after pushing to `hellos`:

```rust
    let refusal = {
        let mut r = inner.hello_refusals.lock().unwrap_or_else(|e| e.into_inner());
        (!r.is_empty()).then(|| r.remove(0))
    };
    if let Some((status, message)) = refusal {
        return error(StatusCode::from_u16(status).unwrap_or(StatusCode::CONFLICT), message);
    }
```

Use the file's existing `error(status, text)` helper; it takes a `StatusCode` and a message.

- [ ] **Step 13: Run the SDK tests**

Run: `cargo nextest run -p balerix-plugin-sdk`
Expected: PASS.

- [ ] **Step 14: Carry `revision` through 4a's integration tests**

- In `crates/balerix/tests/cli_kube_plugins.rs`, `declare` adds `"revision": "r1"` to the entry, and the fake's spawn adds `.env("BALERIX_PLUGIN_REVISION", "r1")`.
- In `crates/balerix-server/tests/kube_plugins_it.rs`, `entry` adds `"revision": "r1"`, and every raw hello JSON adds `"revision": "r1"`. Find them with `grep -n '"protocol": 1' crates/balerix-server/tests/kube_plugins_it.rs`.

Add one test to `kube_plugins_it.rs` that pins the 409 on the route:

```rust
#[tokio::test]
async fn a_hello_for_another_revision_is_409_on_the_route() {
    // build exactly as `declare_fake` does, then send its hello with
    // "revision": "r2": the status is 409 and the body's error is
    // "hello.revision: this daemon holds r1, the plugin is r2; the list has not arrived yet",
    // and GET /v1/plugins still lists fake as starting
}
```

Write the body by copying `declare_fake`'s list PUT and hello call. Change only the revision, and assert status `409`, the exact error, and the row's phase `starting`.

- [ ] **Step 15: Run the core suites**

Run: `mise run check`
Expected: PASS. This includes `cli_kube_plugins` and `kube_plugins_it`.
Run: `mise run plugins`
Expected: PASS. The in-tree plugins compile against the new `Env` field.

- [ ] **Step 16: Commit**

```bash
git add crates/
git commit -m "feat(plugins): a hello belongs to one revision (Spec O §23.8)"
```

---

### Task 2: The Daemon client's plugin calls, and the stub's

**Files:**
- Modify: `operator/src/daemon_client.rs` (after `down`, `:263`).
- Modify: `operator/tests/support/stub_daemon.rs`.
- Modify: `operator/tests/client_it.rs` (the `serve` fn at `:273`, plus new tests).

**Interfaces:**
- Consumes: `balerix_api::{DeclaredPlugins, PluginStatus, ManagedFleet}` (Task 1).
- Produces:
  - `DaemonClient::declare_plugins(&self, list: &DeclaredPlugins) -> Result<(), ClientError>`;
  - `DaemonClient::plugins(&self) -> Result<Vec<PluginStatus>, ClientError>`;
  - `DaemonClient::managed_fleets(&self) -> Result<Vec<ManagedFleet>, ClientError>`;
  - `StubDaemon::lists() -> Vec<DeclaredPlugins>`, `set_plugin_rows(Vec<PluginStatus>)`, `set_managed(Vec<ManagedFleet>)`.

- [ ] **Step 1: Write the failing stub-answer test** in `client_it.rs`

```rust
#[tokio::test]
async fn the_plugin_calls_have_their_meaning() {
    let row = serde_json::json!([{ "name": "flow", "version": "0.1.1", "phase": "ready",
        "listen": "https://flow.ns.svc:7644", "routes": false, "message": "", "active_agents": 0 }]);
    let managed = serde_json::json!([{ "name": "m", "plugin": "fake", "file": { "crews": {} },
        "down": { "keep_repos": true, "keep_sessions": false, "purge": false, "force": false } }]);
    let base = stub(
        Router::new()
            .route(
                "/v1/plugins",
                get(move || { let row = row.clone(); async move { Json(row) } })
                    .put(|| async { refusal(StatusCode::BAD_REQUEST, "plugins[0].token: listed twice") }),
            )
            .route("/v1/managed-fleets", get(move || { let m = managed.clone(); async move { Json(m) } })),
    )
    .await;
    let c = client(&base);
    assert_eq!(
        c.declare_plugins(&balerix_api::DeclaredPlugins::default()).await.unwrap_err(),
        ClientError::Rejected("plugins[0].token: listed twice".into())
    );
    let rows = c.plugins().await.unwrap();
    assert_eq!((rows[0].name.as_str(), rows[0].phase), ("flow", balerix_api::AgentPhase::Ready));
    let m = c.managed_fleets().await.unwrap();
    assert_eq!((m[0].name.as_str(), m[0].down.unwrap().keep_repos), ("m", true));
}
```

If `PluginStatus`'s field names on the wire differ (for example camelCase), take them from `serde_json::to_value` of a real row. The real-Daemon test in Step 4 shows them.

- [ ] **Step 2: Run it to see it fail**

Run: `scripts/operator.sh check` (or `cd operator && cargo nextest run --test client_it the_plugin_calls`)
Expected: compile error, `no method declare_plugins`.

- [ ] **Step 3: Implement** in `daemon_client.rs`, following the existing methods' shape

```rust
    /// `PUT /v1/plugins` (§23.2, §23.4): the whole list, in interceptor
    /// order. Idempotent; re-sent on every reconcile.
    pub async fn declare_plugins(&self, list: &DeclaredPlugins) -> Result<(), ClientError> {
        let response = self
            .http
            .put(self.url("/v1/plugins"))
            .bearer_auth(&self.token)
            .json(list)
            .send()
            .await
            .map_err(unavailable)?;
        if !response.status().is_success() {
            return Err(Self::refused(response).await);
        }
        Ok(())
    }

    /// `GET /v1/plugins`: one row per declared plugin.
    pub async fn plugins(&self) -> Result<Vec<PluginStatus>, ClientError> {
        let response = self
            .http
            .get(self.url("/v1/plugins"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(unavailable)?;
        if !response.status().is_success() {
            return Err(Self::refused(response).await);
        }
        decoded(response).await
    }

    /// `GET /v1/managed-fleets` (§23.3): the plugins' fleet files.
    pub async fn managed_fleets(&self) -> Result<Vec<ManagedFleet>, ClientError> {
        let response = self
            .http
            .get(self.url("/v1/managed-fleets"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(unavailable)?;
        if !response.status().is_success() {
            return Err(Self::refused(response).await);
        }
        decoded(response).await
    }
```

Extend the `use balerix_api::{…}` line with `DeclaredPlugins, ManagedFleet, PluginStatus`, and the module doc with the three routes.

- [ ] **Step 4: A real Daemon with `--tls-ca`.** In `client_it.rs`, `serve` gains a `tls_ca: bool` parameter. When it is true, add `.arg("--tls-ca").arg(root.join("ca.crt"))` after writing `authority.cert_pem` to `ca.crt`. The existing caller passes `false`. Add:

```rust
#[tokio::test]
async fn a_real_kubernetes_mode_daemon_takes_the_list_and_lists_its_rows() {
    let Some(balerix) = support::balerix() else { return };
    let root = support::temp_root("client-real-plugins");
    let (_daemon, base, authority) = serve_with(&balerix, &root, true);
    let c = DaemonClient::new(&base, &authority.cert_pem, TOKEN, Duration::from_secs(10)).unwrap();
    wait_ready(&c, &root).await;
    let list: balerix_api::DeclaredPlugins = serde_json::from_value(serde_json::json!({ "plugins": [{
        "name": "flow", "grant": ["actions", "kv"], "config": {},
        "token": "flow-token-0123456789abcdef0123456789", "url": "https://flow.team-a.svc:7644",
        "revision": "r1" }] })).unwrap();
    c.declare_plugins(&list).await.unwrap();
    c.declare_plugins(&list).await.unwrap(); // idempotent
    let rows = c.plugins().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].name.as_str(), rows[0].phase), ("flow", balerix_api::AgentPhase::Starting));
    assert_eq!(rows[0].listen.as_deref(), Some("https://flow.team-a.svc:7644"));
    assert_eq!(c.managed_fleets().await.unwrap(), vec![]);
    // a list without revisions is refused whole
    let mut bad = list.clone();
    bad.plugins[0].revision = String::new();
    assert_eq!(
        c.declare_plugins(&bad).await.unwrap_err(),
        ClientError::Rejected("plugins[0].revision: required".into())
    );
}
```

Refactor the existing test's ready-wait loop into `async fn wait_ready(c: &DaemonClient, root: &Path)`, and keep `serve(balerix, root)` as `serve_with(balerix, root, false)`.

- [ ] **Step 5: Extend the stub Daemon**

In `stub_daemon.rs`, `Inner` gains `lists: Vec<DeclaredPlugins>`, `plugin_rows: Vec<PluginStatus>` and `managed: Vec<ManagedFleet>`. Then add the handlers and routes:

```rust
async fn put_plugins(State(s): State<Shared>, Json(list): Json<DeclaredPlugins>) -> StatusCode {
    s.0.lock().unwrap().lists.push(list);
    StatusCode::NO_CONTENT
}
async fn get_plugins(State(s): State<Shared>) -> Json<Vec<PluginStatus>> {
    Json(s.0.lock().unwrap().plugin_rows.clone())
}
async fn get_managed(State(s): State<Shared>) -> Json<Vec<ManagedFleet>> {
    Json(s.0.lock().unwrap().managed.clone())
}
// in start():
            .route("/v1/plugins", get(get_plugins).put(put_plugins))
            .route("/v1/managed-fleets", get(get_managed))
// on StubDaemon:
    pub fn lists(&self) -> Vec<DeclaredPlugins> { self.state.0.lock().unwrap().lists.clone() }
    pub fn set_plugin_rows(&self, rows: Vec<PluginStatus>) { self.state.0.lock().unwrap().plugin_rows = rows; }
    pub fn set_managed(&self, rows: Vec<ManagedFleet>) { self.state.0.lock().unwrap().managed = rows; }
```

Make `put_fleet` honour `request.managed_by`: `FleetRecord::with_owner(request.spec, Some(request.managed_by.clone().unwrap_or_else(|| "kubernetes".into())))`. Task 6's assertions read the owner.

- [ ] **Step 6: Run**

Run: `mise run operator`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add operator/src/daemon_client.rs operator/tests/
git commit -m "feat(operator): the Daemon client's plugin list, rows and managed fleets"
```

---

### Task 3: `desired::plugin` — a Plugin's objects, list entry and conditions

**Files:**
- Create: `operator/src/desired/plugin.rs` (with `#[cfg(test)] mod tests` and snapshots under `operator/src/desired/snapshots/`).
- Modify: `operator/src/desired/mod.rs` (`pub mod plugin;`).
- Modify: `operator/src/desired/names.rs`.
- Modify: `operator/src/desired/common.rs` (`DesiredError::Invalid`; tests of names).
- Modify: `operator/src/pki.rs` (`plugin_names`).
- Modify: `operator/src/api/plugin.rs` (doc comment: drop "until then … `PluginsUnsupported`").

**Interfaces:**
- Consumes: `Issued` (pki), `labels`, `pod_security`, `container_security`, `owner_of`, `claim`, `typed`, `hash`, `HASH_ANNOTATION`, `Cond`, and `names::{endpoint, daemon_label, authority}`.
- Produces:
  - in `names`: `plugin(p) -> String` (Deployment and NetworkPolicy name, `balerix-plugin-<p>` bounded), `plugin_token(p)`, `plugin_serving(p)`, `plugin_scratch(p)`, and `plugin_url(namespace, p) -> String`;
  - `pki::plugin_names(namespace, plugin) -> Vec<String>`;
  - in `desired::plugin`:
    - `pub const PLUGIN_PORT: i32 = 7644;`
    - `pub const PLUGIN_LABEL: &str = "balerix.ai/plugin";`
    - `pub const MANAGED_BY_LABEL: &str = "balerix.ai/managed-by";`
    - `pub struct PluginInputs { pub token: String, pub serving: Issued, pub config: Value }`
    - `pub fn grant(spec: &PluginSpec) -> Result<BTreeSet<Capability>, String>`
    - `pub fn inject_secrets(config: &Value, values: &BTreeMap<String, String>) -> Result<Value, String>`
    - `pub fn revision(plugin: &Plugin, inputs: &PluginInputs) -> Result<String, DesiredError>`
    - `pub fn declared(plugin: &Plugin, namespace: &str, inputs: &PluginInputs) -> Result<DeclaredPlugin, DesiredError>`
    - `pub struct PluginObjects { pub token: Secret, pub serving: Secret, pub claim: Option<PersistentVolumeClaim>, pub deployment: Deployment, pub service: Service, pub policy: NetworkPolicy }`
    - `pub fn plugin_objects(plugin: &Plugin, daemon: &str, inputs: &PluginInputs) -> Result<PluginObjects, DesiredError>`
    - `pub enum PluginState<'a> { NotListed, ListedTwice(Vec<String>), Blocked { reason: &'static str, message: String }, Running { available: bool, row: Result<Option<&'a PluginStatus>, String> } }`
    - `pub fn plugin_conditions(state: &PluginState<'_>) -> Vec<Cond>` (`Deployed`, `Ready`, in that order)
    - `pub enum Listing { None, One(String), Many(Vec<String>) }`
    - `pub fn listing(plugin: &str, daemons: &[Daemon]) -> Listing`

`managed_fleet`, `retain_of` and `plugins_ready` are Task 4's.

- [ ] **Step 1: Write the failing tests** in `desired/plugin.rs`

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use serde_json::json;

    use super::*;

    fn plugin(extra: Value) -> Plugin {
        let mut spec = json!({ "image": "balerix-plugin-web:e2e", "needs": ["fleets", "attach", "actions", "workspace"],
                               "config": { "enabled": true } });
        for (k, v) in extra.as_object().unwrap() {
            spec[k] = v.clone();
        }
        serde_json::from_value(json!({
            "apiVersion": "balerix.ai/v1alpha1", "kind": "Plugin",
            "metadata": { "name": "web", "namespace": "team-a", "uid": "plugin-uid", "generation": 2 },
            "spec": spec
        }))
        .unwrap()
    }

    fn inputs() -> PluginInputs {
        PluginInputs {
            token: "TOKEN".into(),
            serving: Issued { cert_pem: "TLS CERT".into(), key_pem: "TLS KEY".into(), not_after: 1_807_776_000 },
            config: json!({ "enabled": true }),
        }
    }

    #[test]
    fn names_and_url() {
        assert_eq!(names::plugin("web"), "balerix-plugin-web");
        assert_eq!(names::plugin_token("web"), "balerix-plugin-web-token");
        assert_eq!(names::plugin_serving("web"), "balerix-plugin-web-tls");
        assert_eq!(names::plugin_scratch("web"), "balerix-plugin-web-scratch");
        assert_eq!(names::plugin_url("team-a", "web"), "https://web.team-a.svc:7644");
        assert_eq!(
            crate::pki::plugin_names("team-a", "web"),
            ["web", "web.team-a", "web.team-a.svc", "web.team-a.svc.cluster.local"]
        );
    }

    #[test]
    fn the_grant_is_the_needs_and_an_unknown_one_is_named() {
        assert_eq!(grant(&plugin(json!({})).spec).unwrap().len(), 4);
        assert_eq!(
            grant(&plugin(json!({ "needs": ["kv", "telepathy"] })).spec).unwrap_err(),
            "spec.needs[1]: unknown capability \"telepathy\""
        );
    }

    #[test]
    fn secrets_go_in_at_the_top_and_never_over_a_config_key() {
        let values = BTreeMap::from([("password".to_string(), "hunter2".to_string())]);
        assert_eq!(
            inject_secrets(&json!({ "user": "u" }), &values).unwrap(),
            json!({ "user": "u", "password": "hunter2" })
        );
        assert_eq!(
            inject_secrets(&json!({ "password": "x" }), &values).unwrap_err(),
            "spec.secrets.password: collides with spec.config.password"
        );
        assert_eq!(inject_secrets(&json!([]), &values).unwrap_err(), "spec.config: not a mapping");
    }

    #[test]
    fn the_revision_moves_with_every_input_and_nothing_else() {
        let p = plugin(json!({}));
        let r = revision(&p, &inputs()).unwrap();
        assert_eq!(r.len(), 64);
        assert_eq!(r, revision(&p, &inputs()).unwrap(), "stable");
        let mut token = inputs();
        token.token = "OTHER".into();
        let mut cert = inputs();
        cert.serving.cert_pem = "RENEWED".into();
        let mut config = inputs();
        config.config = json!({ "enabled": false });
        for changed in [token, cert, config] {
            assert_ne!(revision(&p, &changed).unwrap(), r);
        }
        assert_ne!(revision(&plugin(json!({ "image": "other:1" })), &inputs()).unwrap(), r);
        // status and metadata other than the name do not count
        let mut later = p.clone();
        later.metadata.generation = Some(9);
        assert_eq!(revision(&later, &inputs()).unwrap(), r);
    }

    #[test]
    fn the_list_entry() {
        let d = declared(&plugin(json!({ "fleetDefaults": { "claude": { "model": "sonnet" } } })), "team-a", &inputs()).unwrap();
        assert_eq!(d.name, "web");
        assert_eq!(d.url, "https://web.team-a.svc:7644");
        assert_eq!(d.token, "TOKEN");
        assert_eq!(d.config, json!({ "enabled": true }));
        assert_eq!(d.fleet_defaults, json!({ "claude": { "model": "sonnet" } }));
        assert_eq!(d.grant.len(), 4);
        assert_eq!(d.revision, revision(&plugin(json!({ "fleetDefaults": { "claude": { "model": "sonnet" } } })), &inputs()).unwrap());
    }

    #[test]
    fn the_plugins_objects() {
        let o = plugin_objects(&plugin(json!({ "expose": { "port": 8080 }, "scratch": { "size": "1Gi" },
            "resources": { "requests": { "cpu": "50m" } } })), "default", &inputs()).unwrap();
        insta::assert_yaml_snapshot!("plugin_secret_token", o.token);
        insta::assert_yaml_snapshot!("plugin_secret_serving", o.serving);
        insta::assert_yaml_snapshot!("plugin_claim", o.claim);
        insta::assert_yaml_snapshot!("plugin_deployment", o.deployment);
        insta::assert_yaml_snapshot!("plugin_service", o.service);
        insta::assert_yaml_snapshot!("plugin_policy", o.policy);
    }

    #[test]
    fn without_scratch_or_expose_there_is_an_empty_dir_and_one_port() {
        let o = plugin_objects(&plugin(json!({})), "default", &inputs()).unwrap();
        assert!(o.claim.is_none());
        let pod = serde_json::to_value(&o.deployment).unwrap()["spec"]["template"]["spec"].clone();
        let scratch = pod["volumes"].as_array().unwrap().iter().find(|v| v["name"] == "scratch").unwrap();
        assert_eq!(scratch["emptyDir"], json!({}));
        assert_eq!(serde_json::to_value(&o.service).unwrap()["spec"]["ports"].as_array().unwrap().len(), 1);
        let policy = serde_json::to_value(&o.policy).unwrap();
        assert_eq!(policy["spec"]["ingress"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_pod_says_its_revision_twice_and_rolls_on_it() {
        let p = plugin(json!({}));
        let o = plugin_objects(&p, "default", &inputs()).unwrap();
        let v = serde_json::to_value(&o.deployment).unwrap();
        let r = revision(&p, &inputs()).unwrap();
        assert_eq!(v["spec"]["template"]["metadata"]["annotations"][HASH_ANNOTATION], json!(r));
        let env = v["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap();
        let get = |n: &str| env.iter().find(|e| e["name"] == n).map(|e| e["value"].clone());
        assert_eq!(get("BALERIX_PLUGIN_REVISION"), Some(json!(r)));
        assert_eq!(get("BALERIX_API_URL"), Some(json!("https://balerix-default.team-a.svc:7643")));
        assert_eq!(get("BALERIX_PLUGIN_LISTEN"), Some(json!("0.0.0.0:7644")));
        // the config is never on the pod (§23.1)
        assert!(!v.to_string().contains("\"enabled\""), "{v}");
    }

    #[test]
    fn listing_counts_the_daemons_that_name_the_plugin() {
        let daemon = |name: &str, plugins: &[&str]| -> Daemon {
            serde_json::from_value(json!({ "apiVersion": "balerix.ai/v1alpha1", "kind": "Daemon",
                "metadata": { "name": name, "namespace": "team-a" },
                "spec": { "storage": { "state": { "size": "1Gi" }, "shared": { "size": "1Gi" }, "agent": { "size": "1Gi" } },
                          "plugins": plugins } })).unwrap()
        };
        assert!(matches!(listing("web", &[daemon("a", &["flow"])]), Listing::None));
        assert!(matches!(listing("web", &[daemon("a", &["web"]), daemon("b", &[])]), Listing::One(d) if d == "a"));
        assert!(matches!(listing("web", &[daemon("a", &["web"]), daemon("b", &["web"])]),
            Listing::Many(ds) if ds == ["a", "b"]));
    }

    #[test]
    fn the_conditions() {
        let c = |s: &PluginState<'_>| {
            plugin_conditions(s).into_iter().map(|c| (c.type_, c.status, c.reason, c.message)).collect::<Vec<_>>()
        };
        assert_eq!(c(&PluginState::NotListed)[0], ("Deployed", Some(false), "NotListed".into(),
            "no Daemon in this namespace lists it in spec.plugins".into()));
        assert_eq!(c(&PluginState::ListedTwice(vec!["a".into(), "b".into()]))[1].2, "PluginListedTwice");
        let ready = PluginStatus { name: "web".into(), version: "0.2.1".into(), phase: AgentPhase::Ready,
            listen: None, routes: true, message: String::new(), active_agents: 0 };
        let refused = PluginStatus { phase: AgentPhase::Failed, message: "hello.manifest.needs: kv is not granted".into(), ..ready.clone() };
        assert_eq!(c(&PluginState::Running { available: true, row: Ok(Some(&ready)) }),
            vec![("Deployed", Some(true), "Available".into(), String::new()),
                 ("Ready", Some(true), "Ready".into(), String::new())]);
        assert_eq!(c(&PluginState::Running { available: true, row: Ok(Some(&refused)) })[1],
            ("Ready", Some(false), "PluginRefused".into(), "hello.manifest.needs: kv is not granted".into()));
        assert_eq!(c(&PluginState::Running { available: false, row: Ok(None) }),
            vec![("Deployed", Some(false), "DeploymentNotAvailable".into(), "the plugin's Deployment has no available replica".into()),
                 ("Ready", Some(false), "PluginNotReady".into(), "the Daemon has no row for it yet".into())]);
        assert_eq!(c(&PluginState::Running { available: true, row: Err("the Daemon is unavailable: x".into()) })[1].2,
            "DaemonUnavailable");
        assert_eq!(c(&PluginState::Blocked { reason: "SecretMissing", message: "spec.secrets.password: Secret s has no key k".into() })[1],
            ("Ready", Some(false), "SecretMissing".into(), "spec.secrets.password: Secret s has no key k".into()));
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cd operator && cargo nextest run --lib desired::plugin`
Expected: compile errors (module missing).

- [ ] **Step 3: Implement names and pki**

In `names.rs`:

```rust
/// A plugin's Deployment and NetworkPolicy (§23.4); its Service is the
/// plugin's own name, the host of its url.
pub fn plugin(plugin: &str) -> String {
    job_name(&format!("balerix-plugin-{plugin}"), "")
}
pub fn plugin_token(plugin: &str) -> String {
    job_name(&format!("balerix-plugin-{plugin}"), "-token")
}
pub fn plugin_serving(plugin: &str) -> String {
    job_name(&format!("balerix-plugin-{plugin}"), "-tls")
}
pub fn plugin_scratch(plugin: &str) -> String {
    job_name(&format!("balerix-plugin-{plugin}"), "-scratch")
}
pub fn plugin_url(namespace: &str, plugin: &str) -> String {
    format!("https://{plugin}.{namespace}.svc:{}", crate::desired::plugin::PLUGIN_PORT)
}
```

In `pki.rs`, beside `daemon_names`:

```rust
/// The names the Daemon calls a plugin by: its Service, `<plugin>`.
pub fn plugin_names(namespace: &str, plugin: &str) -> Vec<String> {
    vec![
        plugin.to_string(),
        format!("{plugin}.{namespace}"),
        format!("{plugin}.{namespace}.svc"),
        format!("{plugin}.{namespace}.svc.cluster.local"),
    ]
}
```

- [ ] **Step 4: Implement `desired/plugin.rs`**

```rust
//! A listed Plugin into what runs it (Spec O §5.5, §23.4): its token and
//! serving Secrets, the optional scratch claim, a Deployment of one
//! hardened pod, its Service and NetworkPolicy; and its entry in the
//! Daemon's `PUT /v1/plugins`. The token, the certificate and the
//! resolved config come in as `PluginInputs`; nothing random is made here.

use std::collections::{BTreeMap, BTreeSet};

use balerix_api::{AgentPhase, Capability, DeclaredPlugin, PluginStatus};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use serde_json::{Value, json};

use super::common::{
    Cond, DAEMON_PORT, DesiredError, HASH_ANNOTATION, claim, container_security, hash, labels,
    owner_of, pod_security, typed,
};
use super::names;
use crate::api::{ClaimSpec, Daemon, Plugin, PluginSpec};
use crate::pki::Issued;

pub const PLUGIN_PORT: i32 = 7644;
pub const PLUGIN_LABEL: &str = "balerix.ai/plugin";
/// On a Fleet the operator wrote for a plugin's managed request (§23.4).
pub const MANAGED_BY_LABEL: &str = "balerix.ai/managed-by";

/// What a listed Plugin's objects and list entry are built from.
pub struct PluginInputs {
    pub token: String,
    pub serving: Issued,
    /// `spec.config` with `spec.secrets` injected.
    pub config: Value,
}

fn name_of(plugin: &Plugin) -> Result<(&str, &str), DesiredError> {
    let name = plugin.metadata.name.as_deref().ok_or(DesiredError::Missing("the Plugin", "metadata.name"))?;
    let namespace = plugin
        .metadata
        .namespace
        .as_deref()
        .ok_or(DesiredError::Missing("the Plugin", "metadata.namespace"))?;
    Ok((name, namespace))
}

/// `spec.needs` as capabilities, the first unknown one named.
pub fn grant(spec: &PluginSpec) -> Result<BTreeSet<Capability>, String> {
    spec.needs
        .iter()
        .enumerate()
        .map(|(i, n)| {
            serde_json::from_value::<Capability>(json!(n))
                .map_err(|_| format!("spec.needs[{i}]: unknown capability {n:?}"))
        })
        .collect()
}

/// `plugins.yaml`'s `secrets` rule (plugins spec G-7): each key goes in at
/// the config's top level and may not replace a config key.
pub fn inject_secrets(config: &Value, values: &BTreeMap<String, String>) -> Result<Value, String> {
    let mut out = config.clone();
    let map = out.as_object_mut().ok_or("spec.config: not a mapping")?;
    for (key, value) in values {
        if map.contains_key(key) {
            return Err(format!("spec.secrets.{key}: collides with spec.config.{key}"));
        }
        map.insert(key.clone(), json!(value));
    }
    Ok(out)
}

/// §23.8: one hash over everything the pod and the list entry are built
/// from. The Deployment rolls on it and the pod's hello carries it.
pub fn revision(plugin: &Plugin, inputs: &PluginInputs) -> Result<String, DesiredError> {
    let (name, _) = name_of(plugin)?;
    Ok(hash(&json!({
        "name": name,
        "spec": serde_json::to_value(&plugin.spec)?,
        "config": inputs.config,
        "token": inputs.token,
        "certificate": inputs.serving.cert_pem,
    })))
}

pub fn declared(plugin: &Plugin, namespace: &str, inputs: &PluginInputs) -> Result<DeclaredPlugin, DesiredError> {
    let (name, _) = name_of(plugin)?;
    let grant = grant(&plugin.spec).map_err(DesiredError::Invalid)?;
    Ok(DeclaredPlugin {
        name: name.to_string(),
        grant,
        config: inputs.config.clone(),
        fleet_defaults: plugin.spec.fleet_defaults.clone(),
        token: inputs.token.clone(),
        url: names::plugin_url(namespace, name),
        revision: revision(plugin, inputs)?,
    })
}
```

`spec` already covers the grant, `fleetDefaults` and the unresolved config. `config` adds the injected secret values. `revision` is what `plugin_objects` puts on the pod.

Then `plugin_objects`:

```rust
pub struct PluginObjects {
    pub token: Secret,
    pub serving: Secret,
    pub claim: Option<PersistentVolumeClaim>,
    pub deployment: Deployment,
    pub service: Service,
    pub policy: NetworkPolicy,
}

pub fn plugin_objects(plugin: &Plugin, daemon: &str, inputs: &PluginInputs) -> Result<PluginObjects, DesiredError> {
    let (name, namespace) = name_of(plugin)?;
    let owner = owner_of(plugin)?;
    let labels = labels(daemon, "plugin", &[(PLUGIN_LABEL, name)]);
    let selector = json!({ PLUGIN_LABEL: name, "balerix.ai/component": "plugin" });
    let daemon_pod = json!({ "balerix.ai/daemon": names::daemon_label(daemon), "balerix.ai/component": "daemon" });
    let metadata = |object: String| json!({
        "name": object, "namespace": namespace, "labels": labels, "ownerReferences": [owner],
    });
    let revision = revision(plugin, inputs)?;
    let env = |n: &str, v: &str| json!({ "name": n, "value": v });
    let mut ports = vec![json!({ "name": "plugin", "containerPort": PLUGIN_PORT })];
    let mut service_ports = vec![json!({ "name": "plugin", "port": PLUGIN_PORT, "targetPort": "plugin" })];
    let mut ingress = vec![json!({
        "from": [{ "podSelector": { "matchLabels": daemon_pod } }],
        "ports": [{ "protocol": "TCP", "port": PLUGIN_PORT }],
    })];
    if let Some(expose) = &plugin.spec.expose {
        ports.push(json!({ "name": "expose", "containerPort": expose.port }));
        service_ports.push(json!({ "name": "expose", "port": expose.port, "targetPort": "expose" }));
        ingress.push(json!({ "ports": [{ "protocol": "TCP", "port": expose.port }] }));
    }
    let scratch_volume = match &plugin.spec.scratch {
        Some(_) => json!({ "name": "scratch", "persistentVolumeClaim": { "claimName": names::plugin_scratch(name) } }),
        None => json!({ "name": "scratch", "emptyDir": {} }),
    };
    let secret = |object: String, type_: &str, data: Value| -> Result<Secret, DesiredError> {
        typed(json!({ "apiVersion": "v1", "kind": "Secret", "metadata": metadata(object), "type": type_, "stringData": data }))
    };
    Ok(PluginObjects {
        token: secret(names::plugin_token(name), "Opaque", json!({ "token": inputs.token }))?,
        serving: {
            let mut s = secret(
                names::plugin_serving(name),
                "kubernetes.io/tls",
                json!({ "tls.crt": inputs.serving.cert_pem, "tls.key": inputs.serving.key_pem }),
            )?;
            s.metadata.annotations = Some(BTreeMap::from([(
                super::daemon::NOT_AFTER_ANNOTATION.to_string(),
                inputs.serving.not_after.to_string(),
            )]));
            s
        },
        claim: plugin
            .spec
            .scratch
            .as_ref()
            .map(|s| {
                let mut c = claim(namespace, &names::plugin_scratch(name), labels.clone(),
                    &ClaimSpec { storage_class_name: None, size: s.size.clone() }, "ReadWriteOnce")?;
                c.metadata.owner_references = Some(vec![serde_json::from_value(owner.clone())?]);
                Ok::<_, DesiredError>(c)
            })
            .transpose()?,
        deployment: typed(json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": metadata(names::plugin(name)),
            "spec": {
                "replicas": 1,
                "selector": { "matchLabels": selector },
                "template": {
                    "metadata": { "labels": labels, "annotations": { HASH_ANNOTATION: revision } },
                    "spec": {
                        "automountServiceAccountToken": false,
                        "enableServiceLinks": false,
                        "securityContext": pod_security(),
                        "containers": [{
                            "name": "plugin",
                            "image": plugin.spec.image,
                            "env": [
                                env("BALERIX_API_URL", &names::endpoint(namespace, daemon)),
                                env("BALERIX_PLUGIN_NAME", name),
                                env("BALERIX_PLUGIN_TOKEN_FILE", "/balerix/token/token"),
                                env("BALERIX_PLUGIN_SCRATCH", "/balerix/scratch"),
                                env("BALERIX_CA_FILE", "/balerix/ca/ca.crt"),
                                env("BALERIX_PLUGIN_TLS_CERT", "/balerix/tls/tls.crt"),
                                env("BALERIX_PLUGIN_TLS_KEY", "/balerix/tls/tls.key"),
                                env("BALERIX_PLUGIN_LISTEN", &format!("0.0.0.0:{PLUGIN_PORT}")),
                                env("BALERIX_PLUGIN_REVISION", &revision),
                                env("HOME", "/tmp"),
                            ],
                            "ports": ports,
                            "resources": plugin.spec.resources,
                            "securityContext": container_security(),
                            "volumeMounts": [
                                { "name": "token", "mountPath": "/balerix/token", "readOnly": true },
                                { "name": "tls", "mountPath": "/balerix/tls", "readOnly": true },
                                { "name": "ca", "mountPath": "/balerix/ca", "readOnly": true },
                                { "name": "scratch", "mountPath": "/balerix/scratch" },
                                { "name": "tmp", "mountPath": "/tmp" },
                            ],
                        }],
                        "volumes": [
                            { "name": "token", "secret": { "secretName": names::plugin_token(name), "defaultMode": 0o440 } },
                            { "name": "tls", "secret": { "secretName": names::plugin_serving(name), "defaultMode": 0o440 } },
                            { "name": "ca", "configMap": { "name": names::authority(daemon) } },
                            scratch_volume,
                            { "name": "tmp", "emptyDir": {} },
                        ],
                    },
                },
            },
        }))?,
        service: typed(json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": metadata(name.to_string()),
            "spec": { "selector": selector, "ports": service_ports },
        }))?,
        // §23.4: in from the Daemon's pod (and anyone to `expose`); out to
        // the Daemon, DNS and 443
        policy: typed(json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
            "metadata": metadata(names::plugin(name)),
            "spec": {
                "podSelector": { "matchLabels": selector },
                "policyTypes": ["Ingress", "Egress"],
                "ingress": ingress,
                "egress": [
                    { "to": [{ "podSelector": { "matchLabels": daemon_pod } }], "ports": [{ "protocol": "TCP", "port": DAEMON_PORT }] },
                    { "ports": [{ "protocol": "UDP", "port": 53 }, { "protocol": "TCP", "port": 53 }] },
                    { "ports": [{ "protocol": "TCP", "port": 443 }] },
                ],
            },
        }))?,
    })
}
```

`plugin.spec.resources` is checked as a `ResourceRequirements` by `typed` when the Deployment is built. A bad value is a `DesiredError::Shape`, which Task 5 reports as `InvalidSpec` on `Deployed`.

Then the status pieces:

```rust
pub enum Listing {
    None,
    One(String),
    Many(Vec<String>),
}

/// The Daemons in the Plugin's namespace whose `spec.plugins` name it.
pub fn listing(plugin: &str, daemons: &[Daemon]) -> Listing {
    let mut by: Vec<String> = daemons
        .iter()
        .filter(|d| d.spec.plugins.iter().any(|p| p == plugin))
        .filter_map(|d| d.metadata.name.clone())
        .collect();
    by.sort();
    match by.len() {
        0 => Listing::None,
        1 => Listing::One(by.remove(0)),
        _ => Listing::Many(by),
    }
}

pub enum PluginState<'a> {
    NotListed,
    ListedTwice(Vec<String>),
    /// Listed, but its objects cannot be built: `WaitingForDaemon`,
    /// `InvalidSpec`, `SecretMissing`.
    Blocked { reason: &'static str, message: String },
    /// Its objects applied. `row` is the Daemon's answer: its row (or none
    /// yet), or why the Daemon could not be asked.
    Running { available: bool, row: Result<Option<&'a PluginStatus>, String> },
}

/// `Deployed`, then `Ready` (§23.4).
pub fn plugin_conditions(state: &PluginState<'_>) -> Vec<Cond> {
    let both = |reason: &str, message: &str| {
        vec![Cond::no("Deployed", reason, message), Cond::no("Ready", reason, message)]
    };
    match state {
        PluginState::NotListed => both("NotListed", "no Daemon in this namespace lists it in spec.plugins"),
        PluginState::ListedTwice(ds) => both("PluginListedTwice", &format!("listed by Daemons {}", ds.join(", "))),
        PluginState::Blocked { reason, message } => both(reason, message),
        PluginState::Running { available, row } => {
            let deployed = if *available {
                Cond::yes("Deployed", "Available", "")
            } else {
                Cond::no("Deployed", "DeploymentNotAvailable", "the plugin's Deployment has no available replica")
            };
            let ready = match row {
                Err(e) => Cond::no("Ready", "DaemonUnavailable", e),
                Ok(None) => Cond::no("Ready", "PluginNotReady", "the Daemon has no row for it yet"),
                Ok(Some(r)) if r.phase == AgentPhase::Ready => Cond::yes("Ready", "Ready", ""),
                Ok(Some(r)) if r.phase == AgentPhase::Failed => Cond::no("Ready", "PluginRefused", &r.message),
                Ok(Some(r)) => Cond::no("Ready", "PluginNotReady", &format!("the Daemon lists it {}", wire(&r.phase))),
            };
            vec![deployed, ready]
        }
    }
}

/// An `AgentPhase` as the wire spells it.
fn wire(phase: &AgentPhase) -> String {
    serde_json::to_value(phase).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}
```

`declared` needs a variant in `DesiredError` (`common.rs`) for `grant`'s refusal:

```rust
    /// A value the schema admits and the operator refuses: `<path>: <why>`.
    #[error("{0}")]
    Invalid(String),
```

- [ ] **Step 5: Run the tests and accept the snapshots**

Run: `cd operator && cargo insta test --review --lib -- desired::plugin` (or `INSTA_UPDATE=always cargo nextest run --lib desired::plugin`, then read every `.snap`)
Expected: PASS once the snapshots are written. Read each one against §23.4:
- the Deployment's env and mounts;
- the Service's ports;
- the policy's two ingress and three egress rules;
- the owner references;
- the claim owned by the Plugin.

Also update `names_are_the_documented_ones` in `common.rs` with the four plugin names.

- [ ] **Step 6: Commit**

```bash
git add operator/src/desired/ operator/src/pki.rs operator/src/api/plugin.rs
git commit -m "feat(operator): desired::plugin — a listed Plugin's objects, list entry and conditions"
```

---

### Task 4: `desired` for the Daemon side — `--tls-ca`, `PluginsReady`, managed Fleets, `managed_by`

**Files:**
- Modify: `operator/src/desired/daemon.rs` (StatefulSet args and volumes at `:212-250`; `daemon_status` at `:300-370`; tests at `:560-580`).
- Modify: `operator/src/desired/plugin.rs` (`plugins_ready`, `managed_fleet`, `retain_of`).
- Modify: `operator/src/desired/fleet.rs:226-241` (`managed_by`).
- Modify: the snapshots `daemon_statefulset` and the fleet request snapshot, if any.

**Interfaces:**
- Consumes: Task 3's constants. `balerix_api::{ManagedFleet, DownQuery, PluginStatus}`. `balerix_config::merge`. `crate::api::{Fleet, FleetSpec, FleetCrew, Retain}`.
- Produces:
  - `pub fn plugins_ready(listed: &[String], missing: &[String], twice: &[String], rows: &[PluginStatus]) -> Cond`
  - `pub fn managed_fleet(row: &ManagedFleet, plugin: &Plugin, daemon: &str) -> Result<Fleet, DesiredError>`
  - `pub fn retain_of(down: &DownQuery) -> Retain`
  - `daemon_status` now gives `PluginsReady` as `True/NoPlugins` or `Unknown/Pending`, and no longer counts it toward `Ready`. Task 6 replaces it once the Daemon answers.

- [ ] **Step 1: Write the failing tests**

In `desired/daemon.rs`, replace `a_plugin_list_is_unsupported_until_sub_project_four` with:

```rust
/// §23.4: the plugins are judged once the Daemon answers; until then
/// `PluginsReady` is pending and does not hold `Ready` back.
#[test]
fn a_plugin_list_is_pending_until_the_daemon_answers() {
    let d = daemon(json!({ "plugins": ["flow", "web"] }));
    let s = status_of(&d, Some("Bound"), 1, JobOutcome::Succeeded(String::new()));
    assert_eq!(
        cond(&s, "PluginsReady"),
        ("Unknown", "Pending", "judged once the daemon answers")
    );
    assert_eq!(cond(&s, "Ready"), ("True", "Ready", ""));
}

#[test]
fn the_daemon_trusts_its_own_authority_for_its_plugins() {
    let o = daemon_objects(&daemon(json!({})), &cfg(), 1_807_776_000).unwrap();
    let pod = serde_json::to_value(&o.statefulset).unwrap()["spec"]["template"]["spec"].clone();
    let args = pod["containers"][0]["args"].as_array().unwrap();
    let at = args.iter().position(|a| a == "--tls-ca").expect("--tls-ca");
    assert_eq!(args[at + 1], json!("/balerix/ca/ca.crt"));
    assert!(pod["volumes"].as_array().unwrap().iter().any(|v| v["name"] == "ca"
        && v["configMap"]["name"] == json!("balerix-default-ca")));
}
```

In `desired/plugin.rs` tests:

```rust
fn row(name: &str, phase: AgentPhase, message: &str) -> PluginStatus {
    PluginStatus { name: name.into(), version: "1".into(), phase, listen: None, routes: false,
        message: message.into(), active_agents: 0 }
}

#[test]
fn plugins_ready_names_the_first_plugin_in_list_order_that_is_not() {
    let listed = vec!["flow".to_string(), "web".to_string()];
    let f = |missing: &[&str], twice: &[&str], rows: &[PluginStatus]| {
        let c = plugins_ready(&listed, &missing.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &twice.iter().map(|s| s.to_string()).collect::<Vec<_>>(), rows);
        (c.status, c.reason, c.message)
    };
    assert_eq!(plugins_ready(&[], &[], &[], &[]).reason, "NoPlugins");
    let ok = [row("flow", AgentPhase::Ready, ""), row("web", AgentPhase::Ready, "")];
    assert_eq!(f(&[], &[], &ok), (Some(true), "AllReady".into(), String::new()));
    assert_eq!(f(&["flow"], &[], &ok), (Some(false), "PluginMissing".into(), "Plugin flow does not exist".into()));
    assert_eq!(f(&[], &["web"], &ok), (Some(false), "PluginListedTwice".into(),
        "Plugin web is listed by another Daemon too".into()));
    assert_eq!(f(&[], &[], &[row("flow", AgentPhase::Failed, "hello.manifest.needs: kv is not granted"), ok[1].clone()]),
        (Some(false), "PluginRefused".into(), "flow: hello.manifest.needs: kv is not granted".into()));
    assert_eq!(f(&[], &[], &[ok[0].clone()]), (Some(false), "PluginNotReady".into(), "web: not declared yet".into()));
    assert_eq!(f(&[], &[], &[ok[0].clone(), row("web", AgentPhase::Starting, "")]),
        (Some(false), "PluginNotReady".into(), "web: starting".into()));
}

#[test]
fn a_managed_fleet_is_the_file_beneath_the_plugins_fleet_defaults() {
    let p = plugin(json!({ "fleetDefaults": { "claude": { "model": "sonnet" }, "sandbox": { "network": { "block": false } } } }));
    let row: ManagedFleet = serde_json::from_value(json!({ "name": "m", "plugin": "web", "file": {
        "apiVersion": "balerix/v1", "kind": "Fleet", "name": "m",
        "defaults": { "claude": { "model": "opus" } },
        "crews": { "c": { "repo": "git://git/repo.git", "ref": "main", "git": { "auth": "none" }, "agents": { "carol": {} } } } } }))
        .unwrap();
    let f = managed_fleet(&row, &p, "default").unwrap();
    insta::assert_yaml_snapshot!("managed_fleet", f);
    assert_eq!(f.metadata.labels.as_ref().unwrap()[MANAGED_BY_LABEL], "web");
    assert_eq!(f.spec.daemon, "default");
    assert_eq!(f.spec.defaults["claude"]["model"], json!("opus"), "the file wins");
    assert_eq!(f.spec.defaults["sandbox"]["network"]["block"], json!(false), "beneath it, the plugin's");
    assert_eq!(f.metadata.owner_references.as_ref().unwrap()[0].name, "web");
}

#[test]
fn keep_repos_is_branches_and_anything_else_is_none() {
    use crate::api::Retain;
    assert_eq!(retain_of(&DownQuery { keep_repos: true, ..Default::default() }), Retain::Branches);
    assert_eq!(retain_of(&DownQuery { purge: true, ..Default::default() }), Retain::None);
}
```

In `desired/fleet.rs` tests, beside the existing request test:

```rust
#[test]
fn a_managed_fleet_tells_the_daemon_which_plugin_it_acts_for() {
    let mut f = /* the fleet this module's tests already build */;
    f.metadata.labels = Some(BTreeMap::from([(crate::desired::plugin::MANAGED_BY_LABEL.to_string(), "fake".to_string())]));
    let plan = plan_fleet(&f, &daemon(), &tokens_for(&f), &Images::for_version("0.2.0")).unwrap();
    assert_eq!(plan.request.unwrap().managed_by.as_deref(), Some("fake"));
}
```

Use the module's existing fleet, daemon and tokens helpers. Read the top of its `mod tests` and pick the ones `a_fleet…request` uses.

- [ ] **Step 2: Run them to see them fail**

Run: `cd operator && cargo nextest run --lib desired`
Expected: compile errors and failing asserts.

- [ ] **Step 3: Implement**

In `daemon_objects`'s container `args`, append `"--tls-ca", "/balerix/ca/ca.crt",`. Add the mount `{ "name": "ca", "mountPath": "/balerix/ca", "readOnly": true }` and the volume `{ "name": "ca", "configMap": { "name": names::authority(name) } }`. Its plugins' serving certificates come from this same authority (§23.4).

In `daemon_status`, replace the `plugins` block and `failing`:

```rust
        // §23.4: the controller replaces this once the daemon answers
        let plugins = if daemon.spec.plugins.is_empty() {
            Cond::yes("PluginsReady", "NoPlugins", "")
        } else {
            Cond::unknown("PluginsReady", "Pending", "judged once the daemon answers")
        };
        // ...
        let failing = [&storage, &tools].into_iter().find(|c| c.status != Some(true));
```

Update the module comment on `daemon_status` to match: "`PluginsReady` … judged by the controller".

In `desired/plugin.rs`:

```rust
/// The Daemon's `PluginsReady` (§23.4): the first listed plugin, in list
/// order, that is not ready names why.
pub fn plugins_ready(listed: &[String], missing: &[String], twice: &[String], rows: &[PluginStatus]) -> Cond {
    if listed.is_empty() {
        return Cond::yes("PluginsReady", "NoPlugins", "");
    }
    for name in listed {
        if missing.contains(name) {
            return Cond::no("PluginsReady", "PluginMissing", &format!("Plugin {name} does not exist"));
        }
        if twice.contains(name) {
            return Cond::no("PluginsReady", "PluginListedTwice", &format!("Plugin {name} is listed by another Daemon too"));
        }
        match rows.iter().find(|r| &r.name == name) {
            None => return Cond::no("PluginsReady", "PluginNotReady", &format!("{name}: not declared yet")),
            Some(r) if r.phase == AgentPhase::Ready => {}
            Some(r) if r.phase == AgentPhase::Failed => {
                return Cond::no("PluginsReady", "PluginRefused", &format!("{name}: {}", r.message));
            }
            Some(r) => return Cond::no("PluginsReady", "PluginNotReady", &format!("{name}: {}", wire(&r.phase))),
        }
    }
    Cond::yes("PluginsReady", "AllReady", "")
}

/// `keep-repos` keeps the branches (O-16); any other down is plain.
pub fn retain_of(down: &DownQuery) -> Retain {
    if down.keep_repos { Retain::Branches } else { Retain::None }
}

/// A managed request as a Fleet (§23.4): the file's defaults merged over
/// the Plugin's `fleetDefaults`, its crews as they are, labelled for the
/// plugin and owned by its Plugin. The Daemon resolved the file when the
/// plugin sent it; the restricted surface is not checked again.
pub fn managed_fleet(row: &ManagedFleet, plugin: &Plugin, daemon: &str) -> Result<Fleet, DesiredError> {
    let (plugin_name, namespace) = name_of(plugin)?;
    let file_defaults = row.file.get("defaults").cloned().unwrap_or_else(|| json!({}));
    typed(json!({
        "apiVersion": "balerix.ai/v1alpha1",
        "kind": "Fleet",
        "metadata": {
            "name": row.name,
            "namespace": namespace,
            "labels": { MANAGED_BY_LABEL: plugin_name },
            "ownerReferences": [owner_of(plugin)?],
        },
        "spec": {
            "daemon": daemon,
            "retain": Retain::None,
            "defaults": balerix_config::merge(&plugin.spec.fleet_defaults, &file_defaults),
            "crews": row.file.get("crews").cloned().unwrap_or_else(|| json!({})),
        },
    }))
}
```

Check that the operator crate already depends on `balerix_config`; `desired/fleet.rs` uses it. Add `use balerix_api::{DownQuery, ManagedFleet};` and `use crate::api::{Fleet, Retain};`.

In `desired/fleet.rs`'s request builder:

```rust
    managed_by: fleet
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(crate::desired::plugin::MANAGED_BY_LABEL))
        .cloned(),
```

- [ ] **Step 4: Run, and review the snapshot diffs**

Run: `cd operator && cargo insta test --review --lib`
Expected: `daemon_statefulset` changes by exactly the arg pair, the mount and the volume. `managed_fleet` is new. Nothing else changes.

- [ ] **Step 5: Commit**

```bash
git add operator/src/desired/
git commit -m "feat(operator): the daemon trusts its authority for plugins; PluginsReady, managed Fleets, managed_by"
```

---

### Task 5: The Plugin controller

**Files:**
- Create: `operator/src/controllers/plugin.rs`.
- Modify: `operator/src/controllers/mod.rs` (`pub mod plugin;`, and `plugin::controller` in `set`).
- Modify: `operator/src/controllers/daemon.rs` (make `read_issued` `pub(crate)`).
- Create: `operator/tests/plugins_it.rs`.
- Modify: `operator/tests/support/mod.rs` (`make_daemon_ready`).
- Modify: `operator/.config/nextest.toml`.

**Interfaces:**
- Consumes: Task 3 (`plugin_objects`, `plugin_conditions`, `listing`, `grant`, `inject_secrets`, `PluginInputs`, `PluginState`, `Listing`); Task 2 (`DaemonClient::plugins`); `daemon::{authority_and_token, read_secret_string, read_issued}`; `pki::{issue_serving, verifies, needs_renewal, new_token, plugin_names}`.
- Produces (used by Task 6):
  - `pub async fn read_config(ctx: &Context, namespace: &str, plugin: &Plugin) -> Result<Result<Value, (&'static str, String)>, Error>`: the config with secrets injected, or a blocking `(reason, message)`.
  - `pub async fn read_token(ctx: &Context, namespace: &str, plugin: &str) -> Result<Option<String>, Error>`
  - `pub async fn read_serving(ctx: &Context, namespace: &str, plugin: &str) -> Result<Option<Issued>, Error>`
  - `pub async fn controller(ctx: Arc<Context>, watches: &kube::Client, namespace: Option<&str>)`
  - `pub async fn reconcile(plugin: Arc<Plugin>, ctx: Arc<Context>) -> Result<Action, Error>`
  - `support::make_daemon_ready(client: &kube::Client, ns: &str)`: finishes the pool Job, binds the shared claim and marks the StatefulSet ready (the inline block from `a_daemon_that_fails_readyz…`, waits included).

- [ ] **Step 1: Share the kubelet steps.** Move the "wait for StatefulSet and pool Job, finish the Job, bind the claim, mark the StatefulSet ready" block of `controllers_it.rs:627-660` into `support::make_daemon_ready`, and call it from that test. Run `mise run operator` and expect PASS. The behaviour is unchanged.

- [ ] **Step 2: Add `plugins_it` to the envtest group** in `operator/.config/nextest.toml`:

```toml
[[profile.default.overrides]]
filter = 'binary(controllers_it) | binary(plugins_it)'
test-group = 'envtest'
```

- [ ] **Step 3: Write the failing envtest tests** in `operator/tests/plugins_it.rs`

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Plugin controller and the Daemon controller's plugin half against a
//! real API server (Spec O §23.4, §23.6), the Daemon a stub.
mod support;

use std::time::Duration;

use balerix_api::{AgentPhase, PluginStatus};
use balerix_operator::api::{Daemon, DaemonSpec, Plugin, PluginSpec};
use balerix_operator::pki;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::Api;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use support::envtest::envtest;
use support::stub_daemon::StubDaemon;
use support::{TestClock, make_daemon_ready, namespace, spawn_operator, wait_for};

fn daemon_spec(plugins: &[&str]) -> DaemonSpec {
    serde_json::from_value(serde_json::json!({
        "storage": { "state": { "size": "1Gi" }, "shared": { "size": "10Gi" }, "agent": { "size": "2Gi" } },
        "plugins": plugins,
    }))
    .unwrap()
}

fn plugin_spec(extra: serde_json::Value) -> PluginSpec {
    let mut spec = serde_json::json!({ "image": "balerix-plugin-web:test", "needs": ["fleets", "actions"],
        "config": { "enabled": true } });
    for (k, v) in extra.as_object().unwrap() {
        spec[k] = v.clone();
    }
    serde_json::from_value(spec).unwrap()
}

fn ready_row(name: &str) -> PluginStatus {
    PluginStatus { name: name.into(), version: "1".into(), phase: AgentPhase::Ready, listen: None,
        routes: true, message: String::new(), active_agents: 0 }
}

fn condition(p: &Plugin, type_: &str) -> Option<(String, String, String)> {
    let c = p.status.as_ref()?.conditions.iter().find(|c| c.type_ == type_)?;
    Some((c.status.clone(), c.reason.clone(), c.message.clone()))
}

/// A Daemon listing `plugins`, its kubelet steps done, a stub, an operator.
async fn world(label: &str, plugins: &[&str]) -> (&'static support::envtest::EnvTest, String, StubDaemon, TestClock, tokio::task::AbortHandle) {
    let env = envtest().await.expect("envtest");
    let ns = namespace(&env.client, label).await;
    Api::<Daemon>::namespaced(env.client.clone(), &ns)
        .create(&PostParams::default(), &Daemon::new("default", daemon_spec(plugins)))
        .await
        .unwrap();
    let stub = StubDaemon::start().await;
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, Some(stub.url()), &clock);
    make_daemon_ready(&env.client, &ns).await;
    (env, ns, stub, clock, operator)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlisted_plugin_owns_nothing() {
    let (env, ns, _stub, _clock, operator) = world("unlisted", &[]).await;
    let plugins: Api<Plugin> = Api::namespaced(env.client.clone(), &ns);
    plugins.create(&PostParams::default(), &Plugin::new("web", plugin_spec(serde_json::json!({})))).await.unwrap();
    let got = wait_for("Deployed=False/NotListed", Duration::from_secs(30), || async {
        let p = plugins.get("web").await.unwrap();
        condition(&p, "Deployed").filter(|c| c.1 == "NotListed")
    })
    .await;
    assert_eq!(got.2, "no Daemon in this namespace lists it in spec.plugins");
    let deployments: Api<Deployment> = Api::namespaced(env.client.clone(), &ns);
    assert!(deployments.get_opt("balerix-plugin-web").await.unwrap().is_none());
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_plugin_gets_its_objects_and_a_changed_input_rolls_it() {
    let (env, ns, stub, _clock, operator) = world("listed", &["web"]).await;
    let c = env.client.clone();
    let plugins: Api<Plugin> = Api::namespaced(c.clone(), &ns);
    plugins.create(&PostParams::default(),
        &Plugin::new("web", plugin_spec(serde_json::json!({ "expose": { "port": 8080 }, "scratch": { "size": "1Gi" } })))).await.unwrap();
    let deployments: Api<Deployment> = Api::namespaced(c.clone(), &ns);
    let first = wait_for("the Deployment", Duration::from_secs(60), || async {
        deployments.get_opt("balerix-plugin-web").await.unwrap()
    }).await;
    for (kind, found) in [
        ("token", Api::<Secret>::namespaced(c.clone(), &ns).get_opt("balerix-plugin-web-token").await.unwrap().is_some()),
        ("serving", Api::<Secret>::namespaced(c.clone(), &ns).get_opt("balerix-plugin-web-tls").await.unwrap().is_some()),
        ("service", Api::<Service>::namespaced(c.clone(), &ns).get_opt("web").await.unwrap().is_some()),
        ("policy", Api::<NetworkPolicy>::namespaced(c.clone(), &ns).get_opt("balerix-plugin-web").await.unwrap().is_some()),
        ("claim", Api::<PersistentVolumeClaim>::namespaced(c.clone(), &ns).get_opt("balerix-plugin-web-scratch").await.unwrap().is_some()),
    ] {
        assert!(found, "{kind}");
    }
    // the serving certificate verifies under the Daemon's authority for the Service's name
    let ca = Api::<ConfigMap>::namespaced(c.clone(), &ns).get("balerix-default-ca").await.unwrap()
        .data.unwrap()["ca.crt"].clone();
    let serving = Api::<Secret>::namespaced(c.clone(), &ns).get("balerix-plugin-web-tls").await.unwrap();
    let cert = String::from_utf8(serving.data.unwrap()["tls.crt"].0.clone()).unwrap();
    assert!(pki::verifies(&ca, &cert, &format!("web.{ns}.svc"), support::now()));

    // the kubelet: one available replica; the Daemon: a ready row
    deployments.patch_status("balerix-plugin-web", &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "status": { "replicas": 1, "availableReplicas": 1 } }))).await.unwrap();
    stub.set_plugin_rows(vec![ready_row("web")]);
    wait_for("Deployed and Ready", Duration::from_secs(30), || async {
        let p = plugins.get("web").await.unwrap();
        (condition(&p, "Deployed")?.0 == "True" && condition(&p, "Ready")?.0 == "True").then_some(())
    }).await;

    // a config change is a new revision: the pod template's hash moves
    let hash = |d: &Deployment| d.spec.as_ref().unwrap().template.metadata.as_ref().unwrap()
        .annotations.as_ref().unwrap()["balerix.ai/input-hash"].clone();
    plugins.patch("web", &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "spec": { "config": { "enabled": false } } }))).await.unwrap();
    wait_for("a new pod-template hash", Duration::from_secs(30), || async {
        let d = deployments.get("balerix-plugin-web").await.unwrap();
        (hash(&d) != hash(&first)).then_some(())
    }).await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_authority_reissues_the_plugins_certificate() {
    let (env, ns, _stub, _clock, operator) = world("authority", &["web"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(&PostParams::default(), &Plugin::new("web", plugin_spec(serde_json::json!({})))).await.unwrap();
    let secrets: Api<Secret> = Api::namespaced(c.clone(), &ns);
    let cert = |s: Secret| String::from_utf8(s.data.unwrap()["tls.crt"].0.clone()).unwrap();
    let before = wait_for("the serving Secret", Duration::from_secs(60), || async {
        secrets.get_opt("balerix-plugin-web-tls").await.unwrap()
    }).await;
    // a new authority: delete it, and the Daemon controller mints another (§22.4)
    secrets.delete("balerix-default-ca", &DeleteParams::default()).await.unwrap();
    let after = wait_for("a reissued certificate", Duration::from_secs(60), || async {
        let now = secrets.get("balerix-plugin-web-tls").await.ok()?;
        (cert(now.clone()) != cert(before.clone())).then_some(now)
    }).await;
    let ca = Api::<ConfigMap>::namespaced(c.clone(), &ns).get("balerix-default-ca").await.unwrap()
        .data.unwrap()["ca.crt"].clone();
    assert!(pki::verifies(&ca, &cert(after), &format!("web.{ns}.svc"), support::now()));
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_secret_blocks_the_plugin_and_names_the_key() {
    let (env, ns, _stub, _clock, operator) = world("secret", &["web"]).await;
    let plugins: Api<Plugin> = Api::namespaced(env.client.clone(), &ns);
    plugins.create(&PostParams::default(), &Plugin::new("web", plugin_spec(serde_json::json!({
        "secrets": { "password": { "secretName": "creds", "key": "pw" } } })))).await.unwrap();
    let got = wait_for("SecretMissing", Duration::from_secs(30), || async {
        condition(&plugins.get("web").await.unwrap(), "Deployed").filter(|c| c.1 == "SecretMissing")
    }).await;
    assert_eq!(got.2, "spec.secrets.password: Secret creds does not exist");
    // the key absent from an existing Secret is named too
    Api::<Secret>::namespaced(env.client.clone(), &ns).create(&PostParams::default(), &serde_json::from_value(
        serde_json::json!({ "metadata": { "name": "creds" }, "stringData": { "other": "x" } })).unwrap()).await.unwrap();
    wait_for("the key named", Duration::from_secs(30), || async {
        condition(&plugins.get("web").await.unwrap(), "Deployed")
            .filter(|c| c.2 == "spec.secrets.password: Secret creds has no key pw")
    }).await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_unlisted_owns_nothing_and_relisted_comes_back() {
    let (env, ns, _stub, _clock, operator) = world("relist", &["web"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns)
        .create(&PostParams::default(), &Plugin::new("web", plugin_spec(serde_json::json!({})))).await.unwrap();
    let secrets: Api<Secret> = Api::namespaced(c.clone(), &ns);
    let token = |s: Secret| s.data.unwrap()["token"].0.clone();
    let first = wait_for("the token", Duration::from_secs(60), || async {
        secrets.get_opt("balerix-plugin-web-token").await.unwrap()
    }).await;
    let daemons: Api<Daemon> = Api::namespaced(c.clone(), &ns);
    daemons.patch("default", &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "spec": { "plugins": [] } }))).await.unwrap();
    let deployments: Api<Deployment> = Api::namespaced(c.clone(), &ns);
    wait_for("objects gone", Duration::from_secs(30), || async {
        (deployments.get_opt("balerix-plugin-web").await.unwrap().is_none()
            && secrets.get_opt("balerix-plugin-web-token").await.unwrap().is_none()).then_some(())
    }).await;
    daemons.patch("default", &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "spec": { "plugins": ["web"] } }))).await.unwrap();
    let again = wait_for("a new token", Duration::from_secs(60), || async {
        secrets.get_opt("balerix-plugin-web-token").await.unwrap()
    }).await;
    assert_ne!(token(again), token(first));
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_listed_by_two_daemons_is_neither_ones() {
    let (env, ns, _stub, _clock, operator) = world("twice", &["web"]).await;
    let c = env.client.clone();
    Api::<Daemon>::namespaced(c.clone(), &ns)
        .create(&PostParams::default(), &Daemon::new("other", daemon_spec(&["web"]))).await.unwrap();
    let plugins: Api<Plugin> = Api::namespaced(c.clone(), &ns);
    plugins.create(&PostParams::default(), &Plugin::new("web", plugin_spec(serde_json::json!({})))).await.unwrap();
    let got = wait_for("PluginListedTwice", Duration::from_secs(30), || async {
        condition(&plugins.get("web").await.unwrap(), "Deployed").filter(|c| c.1 == "PluginListedTwice")
    }).await;
    assert_eq!(got.2, "listed by Daemons default, other");
    operator.abort();
}
```

Add `pub fn now() -> i64` (the system clock, as `client_it.rs` has it) to `support/mod.rs`.

- [ ] **Step 4: Run them to see them fail**

Run: `mise run operator`
Expected: `plugins_it` fails, because no Plugin controller runs (the waits time out).

- [ ] **Step 5: Implement `controllers/plugin.rs`**

```rust
//! The Plugin controller (Spec O §5.5, §23.4): a Plugin named in one
//! Daemon's `spec.plugins` gets its Secrets, claim, Deployment, Service and
//! NetworkPolicy, each owned by it; unlisted, it owns nothing. Its `Ready`
//! is the Daemon's row for it, polled on the Fleet period.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, ResourceExt};
use serde_json::Value;

use super::daemon::{authority_and_token, read_issued, read_secret_string};
use super::{Context, Error, api_in, apply, bounded, error_policy, patch_status, reconciled, report};
use crate::api::{Daemon, Plugin, PluginStatus as PluginObjectStatus};
use crate::desired::common::conditions;
use crate::desired::names;
use crate::desired::plugin::{
    Listing, PluginInputs, PluginState, grant, inject_secrets, listing, plugin_conditions, plugin_objects,
};
use crate::pki::{self, Issued};

pub async fn controller(ctx: Arc<Context>, watches: &kube::Client, namespace: Option<&str>) {
    let client = watches;
    Controller::new(api_in::<Plugin>(client, namespace), watcher::Config::default())
        .owns(api_in::<Deployment>(client, namespace), watcher::Config::default())
        .owns(api_in::<Service>(client, namespace), watcher::Config::default())
        .owns(api_in::<Secret>(client, namespace), watcher::Config::default())
        .owns(api_in::<NetworkPolicy>(client, namespace), watcher::Config::default())
        .run(bounded(reconcile), error_policy, ctx.clone())
        .for_each(|r| async move { report("plugin", r) })
        .await;
}

/// `spec.config` with `spec.secrets` injected, or why it cannot be.
pub async fn read_config(
    ctx: &Context,
    namespace: &str,
    plugin: &Plugin,
) -> Result<Result<Value, (&'static str, String)>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let mut values = BTreeMap::new();
    for (key, r) in &plugin.spec.secrets {
        let Some(secret) = secrets.get_opt(&r.secret_name).await? else {
            return Ok(Err(("SecretMissing", format!("spec.secrets.{key}: Secret {} does not exist", r.secret_name))));
        };
        let Some(value) = read_secret_string(&secret, &r.key) else {
            return Ok(Err(("SecretMissing", format!("spec.secrets.{key}: Secret {} has no key {}", r.secret_name, r.key))));
        };
        values.insert(key.clone(), value);
    }
    Ok(inject_secrets(&plugin.spec.config, &values).map_err(|e| ("InvalidSpec", e)))
}

pub async fn read_token(ctx: &Context, namespace: &str, plugin: &str) -> Result<Option<String>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    Ok(secrets.get_opt(&names::plugin_token(plugin)).await?.as_ref().and_then(|s| read_secret_string(s, "token")))
}

pub async fn read_serving(ctx: &Context, namespace: &str, plugin: &str) -> Result<Option<Issued>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    Ok(read_issued(secrets.get_opt(&names::plugin_serving(plugin)).await?.as_ref(), "tls.crt", "tls.key"))
}

/// Unlisted: the objects a listing made are deleted (§23.4).
async fn remove_owned(ctx: &Context, namespace: &str, plugin: &str) -> Result<(), Error> {
    async fn gone<K>(api: Api<K>, name: &str) -> Result<(), Error>
    where
        K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    let c = &ctx.client;
    gone(Api::<Deployment>::namespaced(c.clone(), namespace), &names::plugin(plugin)).await?;
    gone(Api::<Service>::namespaced(c.clone(), namespace), plugin).await?;
    gone(Api::<NetworkPolicy>::namespaced(c.clone(), namespace), &names::plugin(plugin)).await?;
    gone(Api::<Secret>::namespaced(c.clone(), namespace), &names::plugin_token(plugin)).await?;
    gone(Api::<Secret>::namespaced(c.clone(), namespace), &names::plugin_serving(plugin)).await?;
    gone(Api::<PersistentVolumeClaim>::namespaced(c.clone(), namespace), &names::plugin_scratch(plugin)).await?;
    Ok(())
}

pub async fn reconcile(plugin: Arc<Plugin>, ctx: Arc<Context>) -> Result<Action, Error> {
    let namespace = plugin.namespace().unwrap_or_default();
    let name = plugin.name_any();
    let daemons = Api::<Daemon>::namespaced(ctx.client.clone(), &namespace).list(&ListParams::default()).await?.items;
    let rows;
    let state = match listing(&name, &daemons) {
        Listing::None => {
            remove_owned(&ctx, &namespace, &name).await?;
            PluginState::NotListed
        }
        Listing::Many(ds) => PluginState::ListedTwice(ds),
        Listing::One(daemon) => match listed(&ctx, &plugin, &namespace, &name, &daemon).await? {
            Err((reason, message)) => PluginState::Blocked { reason, message },
            Ok(available) => {
                rows = row_of(&ctx, &namespace, &daemon).await;
                PluginState::Running {
                    available,
                    row: rows.as_ref().map(|rs| rs.iter().find(|r| r.name == name)).map_err(Clone::clone),
                }
            }
        },
    };
    let old = plugin.status.as_ref().map_or(&[][..], |s| s.conditions.as_slice());
    let status = PluginObjectStatus {
        observed_generation: plugin.metadata.generation,
        conditions: conditions(old, &plugin_conditions(&state), plugin.metadata.generation, &ctx.k8s_now()),
    };
    patch_status(&ctx.client, plugin.as_ref(), &status).await?;
    reconciled(&ctx, plugin.as_ref());
    Ok(Action::requeue(ctx.run.fleet_period))
}

/// The objects of a Plugin `daemon` lists; `Ok(available)` once applied.
async fn listed(
    ctx: &Context,
    plugin: &Plugin,
    namespace: &str,
    name: &str,
    daemon: &str,
) -> Result<Result<bool, (&'static str, String)>, Error> {
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let Some(authority) = read_issued(secrets.get_opt(&names::authority(daemon)).await?.as_ref(), "ca.crt", "ca.key") else {
        return Ok(Err(("WaitingForDaemon", format!("Daemon {daemon} has no authority yet"))));
    };
    if let Err(e) = grant(&plugin.spec) {
        return Ok(Err(("InvalidSpec", e)));
    }
    let config = match read_config(ctx, namespace, plugin).await? {
        Ok(c) => c,
        Err(blocked) => return Ok(Err(blocked)),
    };
    let now = ctx.now();
    let plugin_names = pki::plugin_names(namespace, name);
    let token = match read_token(ctx, namespace, name).await? {
        Some(t) => t,
        None => pki::new_token(),
    };
    let serving = match read_serving(ctx, namespace, name).await? {
        Some(s) if pki::verifies(&authority.cert_pem, &s.cert_pem, &plugin_names[0], now)
            && !pki::needs_renewal(s.not_after, now) => s,
        _ => {
            tracing::info!(plugin = %name, "issuing the plugin's serving certificate");
            pki::issue_serving(&authority, namespace, daemon, &plugin_names, now)?
        }
    };
    let inputs = PluginInputs { token, serving, config };
    let objects = match plugin_objects(plugin, daemon, &inputs) {
        Ok(o) => o,
        Err(e) => return Ok(Err(("InvalidSpec", e.to_string()))),
    };
    apply(&ctx.client, &objects.token).await?;
    apply(&ctx.client, &objects.serving).await?;
    if let Some(claim) = &objects.claim {
        let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
        // a claim is immutable once bound: created when absent
        if claims.get_opt(&claim.name_any()).await?.is_none() {
            claims.create(&PostParams::default(), claim).await?;
        }
    }
    let deployment = apply(&ctx.client, &objects.deployment).await?;
    apply(&ctx.client, &objects.service).await?;
    apply(&ctx.client, &objects.policy).await?;
    let available = deployment.status.and_then(|s| s.available_replicas).unwrap_or(0) >= 1;
    Ok(Ok(available))
}

/// The Daemon's rows, or why it could not be asked.
async fn row_of(ctx: &Context, namespace: &str, daemon: &str) -> Result<Vec<balerix_api::PluginStatus>, String> {
    let (authority, token) = authority_and_token(ctx, namespace, daemon).await.map_err(|e| e.to_string())?;
    let client = ctx
        .daemon_client(namespace, daemon, &names::endpoint(namespace, daemon), &authority, &token)
        .map_err(|e| e.to_string())?;
    client.plugins().await.map_err(|e| e.to_string())
}
```

`apply` returns the server's object. If the apply response's status lags the Deployment status the test patched, `available` reads one reconcile late, and the 1 s test period absorbs that. If clippy or the borrow checker objects to `rows` declared outside the match, restructure so `row` holds owned data: change `PluginState::Running.row` to `Result<Option<PluginStatus>, String>` (owned) in Task 3. Do it in both places and keep the tests' shape.

In `controllers/mod.rs`, add `pub mod plugin;` and `Box::pin(plugin::controller(ctx.clone(), watches, ns)),` in `set`. Update the doc comment from "The four controllers" to "The five controllers".

In `controllers/daemon.rs`, change `fn read_issued` to `pub(crate) fn read_issued`.

- [ ] **Step 6: Run**

Run: `mise run operator`
Expected: PASS. The six new `plugins_it` tests pass, and every earlier test passes too. `a_plugin_list_is_pending_until_the_daemon_answers` was Task 4's.

- [ ] **Step 7: Commit**

```bash
git add operator/
git commit -m "feat(operator): the Plugin controller (Spec O §5.5, §23.4)"
```

---

### Task 6: The Daemon controller sends the list, judges `PluginsReady` and writes managed Fleets

**Files:**
- Create: `operator/src/controllers/daemon_plugins.rs`.
- Modify: `operator/src/controllers/daemon.rs` (`reconcile`, after the `/readyz` check).
- Modify: `operator/src/controllers/mod.rs` (`mod daemon_plugins;`).
- Modify: `operator/tests/plugins_it.rs` (new tests).

**Interfaces:**
- Consumes: Task 5's `read_config`, `read_token` and `read_serving`; Task 3's `declared` and `grant`; Task 4's `plugins_ready`, `managed_fleet`, `retain_of` and `MANAGED_BY_LABEL`; Task 2's client calls; `Context::warn`.
- Produces:
  - `pub async fn send_list(ctx: &Context, daemon: &Daemon, namespace: &str, client: &DaemonClient) -> Result<(Cond, Option<Vec<String>>), Error>`: `PluginsReady`, and the names sent, or `None` when a listed plugin held the list back and nothing was sent.
  - `pub async fn write_managed(ctx: &Context, daemon: &Daemon, namespace: &str, client: &DaemonClient, sent: &[String]) -> Result<(), Error>`

- [ ] **Step 1: Write the failing envtest tests** (append to `plugins_it.rs`)

```rust
fn made(stub: &StubDaemon) -> Vec<Vec<String>> {
    stub.lists().iter().map(|l| l.plugins.iter().map(|p| p.name.clone()).collect()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_list_is_sent_on_every_reconcile_in_spec_order() {
    let (env, ns, stub, _clock, operator) = world("list", &["web", "flow"]).await;
    let plugins: Api<Plugin> = Api::namespaced(env.client.clone(), &ns);
    for name in ["flow", "web"] {
        plugins.create(&PostParams::default(), &Plugin::new(name, plugin_spec(serde_json::json!({})))).await.unwrap();
    }
    wait_for("three lists naming web then flow", Duration::from_secs(60), || async {
        (made(&stub).iter().filter(|l| *l == &["web", "flow"]).count() >= 3).then_some(())
    }).await;
    let list = stub.lists().last().unwrap().clone();
    assert_eq!(list.plugins[0].url, format!("https://web.{ns}.svc:7644"));
    // the entry's revision is the pod's
    let d = Api::<Deployment>::namespaced(env.client.clone(), &ns).get("balerix-plugin-web").await.unwrap();
    let env_rev = d.spec.unwrap().template.spec.unwrap().containers[0].env.clone().unwrap()
        .into_iter().find(|e| e.name == "BALERIX_PLUGIN_REVISION").unwrap().value.unwrap();
    assert_eq!(list.plugins[0].revision, env_rev);
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn plugins_ready_follows_the_daemons_rows() {
    let (env, ns, stub, _clock, operator) = world("rows", &["web"]).await;
    let daemons: Api<Daemon> = Api::namespaced(env.client.clone(), &ns);
    let reason = || async {
        let d = daemons.get("default").await.unwrap();
        let s = d.status?;
        let c = s.conditions.iter().find(|c| c.type_ == "PluginsReady")?;
        Some((c.status.clone(), c.reason.clone(), c.message.clone()))
    };
    wait_for("PluginMissing", Duration::from_secs(60), || async {
        reason().await.filter(|r| r.1 == "PluginMissing" && r.2 == "Plugin web does not exist")
    }).await;
    Api::<Plugin>::namespaced(env.client.clone(), &ns)
        .create(&PostParams::default(), &Plugin::new("web", plugin_spec(serde_json::json!({})))).await.unwrap();
    stub.set_plugin_rows(vec![PluginStatus { phase: AgentPhase::Failed,
        message: "hello.manifest.needs: kv is not granted".into(), ..ready_row("web") }]);
    wait_for("PluginRefused", Duration::from_secs(60), || async {
        reason().await.filter(|r| r.1 == "PluginRefused" && r.2 == "web: hello.manifest.needs: kv is not granted")
    }).await;
    // Ready says why too
    let d = daemons.get("default").await.unwrap();
    assert!(d.status.unwrap().conditions.iter().any(|c| c.type_ == "Ready" && c.reason == "PluginRefused"));
    stub.set_plugin_rows(vec![ready_row("web")]);
    wait_for("AllReady and Ready", Duration::from_secs(60), || async {
        let d = daemons.get("default").await.unwrap();
        let s = d.status?;
        (s.conditions.iter().any(|c| c.type_ == "PluginsReady" && c.reason == "AllReady")
            && s.conditions.iter().any(|c| c.type_ == "Ready" && c.status == "True")).then_some(())
    }).await;
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_whose_config_cannot_be_built_holds_the_list_back() {
    let (env, ns, stub, _clock, operator) = world("heldback", &["flow", "web"]).await;
    let plugins: Api<Plugin> = Api::namespaced(env.client.clone(), &ns);
    plugins.create(&PostParams::default(), &Plugin::new("flow", plugin_spec(serde_json::json!({})))).await.unwrap();
    plugins.create(&PostParams::default(), &Plugin::new("web", plugin_spec(serde_json::json!({})))).await.unwrap();
    wait_for("a list of both", Duration::from_secs(60), || async {
        made(&stub).iter().any(|l| l == &["flow", "web"]).then_some(())
    }).await;
    // web's config now names a Secret that does not exist
    plugins.patch("web", &PatchParams::default(), &Patch::Merge(serde_json::json!({ "spec": {
        "secrets": { "password": { "secretName": "absent", "key": "pw" } } } }))).await.unwrap();
    let daemons: Api<Daemon> = Api::namespaced(env.client.clone(), &ns);
    wait_for("PluginsReady says why", Duration::from_secs(60), || async {
        let s = daemons.get("default").await.unwrap().status?;
        s.conditions.iter().find(|c| c.type_ == "PluginsReady"
            && c.reason == "SecretMissing"
            && c.message == "web: spec.secrets.password: Secret absent does not exist").cloned()
    }).await;
    // no list without web went out: the Daemon keeps the last one, and with it web's managed requests
    let sent = stub.lists().len();
    support::hold_for("a list sent while web is blocked", Duration::from_secs(3), || async {
        (stub.lists().len() > sent).then_some(())
    }).await;
    assert!(made(&stub).iter().all(|l| l == &["flow", "web"]), "{:?}", made(&stub));
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_whose_secrets_are_not_made_is_left_out_of_the_list() {
    // §23.4: a plugin that cannot be sent yet and has no managed requests
    // to lose is left out, and the rest go. The Plugin controller makes a
    // token and certificate at once, so envtest cannot hold one back; a
    // listed name with no Plugin object takes the same `continue`.
    let (env, ns, stub, _clock, operator) = world("leftout", &["flow", "ghost"]).await;
    Api::<Plugin>::namespaced(env.client.clone(), &ns)
        .create(&PostParams::default(), &Plugin::new("flow", plugin_spec(serde_json::json!({})))).await.unwrap();
    wait_for("a list with flow alone", Duration::from_secs(60), || async {
        made(&stub).iter().any(|l| l == &["flow"]).then_some(())
    }).await;
    operator.abort();
}

fn managed(name: &str, plugin: &str, down: Option<balerix_api::DownQuery>) -> balerix_api::ManagedFleet {
    serde_json::from_value(serde_json::json!({ "name": name, "plugin": plugin, "file": {
        "apiVersion": "balerix/v1", "kind": "Fleet", "name": name,
        "crews": { "c": { "repo": "acme/api", "git": { "auth": "none" }, "agents": { "carol": {} } } } },
        "down": down })).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_request_becomes_a_labelled_fleet_and_its_down_deletes_it() {
    let (env, ns, stub, _clock, operator) = world("managed", &["fake"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns).create(&PostParams::default(),
        &Plugin::new("fake", plugin_spec(serde_json::json!({ "needs": ["actions", "fleets", "kv", "manage"] })))).await.unwrap();
    stub.set_managed(vec![managed("m", "fake", None)]);
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    let f = wait_for("Fleet m", Duration::from_secs(60), || async { fleets.get_opt("m").await.unwrap() }).await;
    assert_eq!(f.labels()["balerix.ai/managed-by"], "fake");
    assert_eq!(f.spec.daemon, "default");
    // the Fleet controller applies it for the plugin
    wait_for("a PUT managed by fake", Duration::from_secs(60), || async {
        stub.puts().iter().any(|p| p.spec.name == "m" && p.managed_by.as_deref() == Some("fake")).then_some(())
    }).await;
    // the plugin downs it with keep-repos: Branches, then gone
    stub.set_managed(vec![managed("m", "fake", Some(balerix_api::DownQuery { keep_repos: true, ..Default::default() }))]);
    wait_for("Fleet m gone", Duration::from_secs(120), || async {
        fleets.get_opt("m").await.unwrap().is_none().then_some(())
    }).await;
    assert!(stub.deletes().contains(&"m".to_string()));
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_request_never_overwrites_a_fleet_it_does_not_manage() {
    let (env, ns, stub, _clock, operator) = world("conflict", &["fake"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns).create(&PostParams::default(),
        &Plugin::new("fake", plugin_spec(serde_json::json!({ "needs": ["fleets", "manage"] })))).await.unwrap();
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    let mine: balerix_operator::api::Fleet = serde_json::from_value(serde_json::json!({
        "apiVersion": "balerix.ai/v1alpha1", "kind": "Fleet", "metadata": { "name": "m" },
        "spec": { "daemon": "default", "crews": { "c": { "repo": "acme/mine", "git": { "auth": "none" }, "agents": { "me": {} } } } } })).unwrap();
    fleets.create(&PostParams::default(), &mine).await.unwrap();
    stub.set_managed(vec![managed("m", "fake", None)]);
    let events: Api<k8s_openapi::api::events::v1::Event> = Api::namespaced(c.clone(), &ns);
    wait_for("FleetConflict on the Plugin", Duration::from_secs(60), || async {
        events.list(&Default::default()).await.unwrap().items.into_iter()
            .find(|e| e.reason.as_deref() == Some("FleetConflict")
                && e.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some("fake"))
    }).await;
    let still = fleets.get("m").await.unwrap();
    assert_eq!(still.spec.crews["c"].repo, "acme/mine");
    assert!(still.labels().get("balerix.ai/managed-by").is_none());
    operator.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_plugin_deletes_its_fleets() {
    let (env, ns, stub, _clock, operator) = world("drop", &["fake"]).await;
    let c = env.client.clone();
    Api::<Plugin>::namespaced(c.clone(), &ns).create(&PostParams::default(),
        &Plugin::new("fake", plugin_spec(serde_json::json!({ "needs": ["fleets", "manage"] })))).await.unwrap();
    stub.set_managed(vec![managed("m", "fake", None)]);
    let fleets: Api<balerix_operator::api::Fleet> = Api::namespaced(c.clone(), &ns);
    wait_for("Fleet m", Duration::from_secs(60), || async { fleets.get_opt("m").await.unwrap() }).await;
    Api::<Daemon>::namespaced(c.clone(), &ns).patch("default", &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "spec": { "plugins": [] } }))).await.unwrap();
    wait_for("Fleet m gone", Duration::from_secs(120), || async {
        fleets.get_opt("m").await.unwrap().is_none().then_some(())
    }).await;
    operator.abort();
}
```

The Fleet's own finalizer runs its cleanup Jobs when the managed Fleet is deleted (`Retain::None` runs `remove_job`). The deletion waits on them, so in `dropping_the_plugin_deletes_its_fleets` the test plays the kubelet: wait for the Job `m-c-remove` and `finish_job(&c, &ns, "m-c-remove", true, None)`. The keep-repos test runs no Job (`Branches`). Check `cleanup_fleet` for the exact Job name (`names::remove_job("m", "c")`) and for whether a Fleet with no Agents left needs anything else.

- [ ] **Step 2: Run them to see them fail**

Run: `mise run operator`
Expected: the seven new tests time out.

- [ ] **Step 3: Implement `controllers/daemon_plugins.rs`**

```rust
//! The Daemon controller's plugin half (Spec O §23.4): the list it sends
//! on every reconcile, `PluginsReady` from the Daemon's rows, and the
//! managed requests written as Fleets.

use kube::api::{DeleteParams, ListParams};
use kube::{Api, ResourceExt};

use super::plugin::{read_config, read_serving, read_token};
use super::{Context, Error, apply};
use crate::api::{Daemon, Fleet, Plugin};
use crate::daemon_client::DaemonClient;
use crate::desired::common::Cond;
use crate::desired::plugin::{
    MANAGED_BY_LABEL, PluginInputs, declared, grant, managed_fleet, plugins_ready, retain_of,
};

/// Builds and sends `PUT /v1/plugins` from `spec.plugins` in order, then
/// judges `PluginsReady` from `GET /v1/plugins`. A plugin with no Plugin,
/// listed by another Daemon too, or whose token or serving Secret is not
/// made yet is left out of this pass: none of these has managed requests
/// to lose. A plugin that has both but whose grant or config cannot be
/// built holds the whole list back: the Daemon drops the requests of any
/// plugin a list leaves out (§23.7), so sending without it would delete
/// its Fleets over a deleted Secret. Returns the names sent, `None` when
/// nothing was.
pub async fn send_list(
    ctx: &Context,
    daemon: &Daemon,
    namespace: &str,
    client: &DaemonClient,
) -> Result<(Cond, Option<Vec<String>>), Error> {
    let name = daemon.name_any();
    let plugins: Api<Plugin> = Api::namespaced(ctx.client.clone(), namespace);
    let others: Vec<Daemon> = Api::<Daemon>::namespaced(ctx.client.clone(), namespace)
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|d| d.name_any() != name)
        .collect();
    let (mut missing, mut twice, mut entries) = (Vec::new(), Vec::new(), Vec::new());
    for p in &daemon.spec.plugins {
        if others.iter().any(|d| d.spec.plugins.contains(p)) {
            twice.push(p.clone());
            continue;
        }
        let Some(plugin) = plugins.get_opt(p).await? else {
            missing.push(p.clone());
            continue;
        };
        let (Some(token), Some(serving)) =
            (read_token(ctx, namespace, p).await?, read_serving(ctx, namespace, p).await?)
        else {
            continue;
        };
        let config = match (grant(&plugin.spec), read_config(ctx, namespace, &plugin).await?) {
            (Err(e), _) => Err(("InvalidSpec", e)),
            (Ok(_), c) => c,
        };
        let config = match config {
            Ok(c) => c,
            Err((reason, message)) => {
                return Ok((Cond::no("PluginsReady", reason, &format!("{p}: {message}")), None));
            }
        };
        entries.push(declared(&plugin, namespace, &PluginInputs { token, serving, config })?);
    }
    let sent: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    client.declare_plugins(&balerix_api::DeclaredPlugins { plugins: entries }).await?;
    let rows = client.plugins().await?;
    Ok((plugins_ready(&daemon.spec.plugins, &missing, &twice, &rows), Some(sent)))
}

/// Each live request of a plugin sent this pass becomes a Fleet labelled
/// for it. A down request, or a sent plugin's Fleet with no live request,
/// deletes the Fleet; so does a plugin dropped from `spec.plugins`. A
/// plugin still listed but left out of this pass (its Secrets not made)
/// keeps its Fleets. A Fleet without the label, or with another plugin's,
/// is never written: a `FleetConflict` Event instead.
pub async fn write_managed(
    ctx: &Context,
    daemon: &Daemon,
    namespace: &str,
    client: &DaemonClient,
    sent: &[String],
) -> Result<(), Error> {
    let name = daemon.name_any();
    let fleets: Api<Fleet> = Api::namespaced(ctx.client.clone(), namespace);
    let plugins: Api<Plugin> = Api::namespaced(ctx.client.clone(), namespace);
    let rows = client.managed_fleets().await?;
    for row in rows.iter().filter(|r| sent.contains(&r.plugin)) {
        let Some(plugin) = plugins.get_opt(&row.plugin).await? else { continue };
        let current = fleets.get_opt(&row.name).await?;
        if let Some(f) = &current
            && f.labels().get(MANAGED_BY_LABEL) != Some(&row.plugin)
        {
            let note = format!("Fleet {} exists and is not managed by plugin {}: left as it is", row.name, row.plugin);
            ctx.warn(&plugin, "FleetConflict", note).await;
            continue;
        }
        let mut desired = managed_fleet(row, &plugin, &name)?;
        match &row.down {
            None => {
                apply(&ctx.client, &desired).await?;
            }
            Some(down) if current.is_some() => {
                desired.spec.retain = retain_of(down);
                apply(&ctx.client, &desired).await?;
                delete_fleet(&fleets, &row.name).await?;
            }
            Some(_) => {}
        }
    }
    let ours = fleets.list(&ListParams::default().labels(MANAGED_BY_LABEL)).await?.items;
    for f in ours.iter().filter(|f| f.spec.daemon == name) {
        let plugin = f.labels().get(MANAGED_BY_LABEL).cloned().unwrap_or_default();
        let dropped = !daemon.spec.plugins.contains(&plugin);
        let live = rows.iter().any(|r| r.name == f.name_any() && r.plugin == plugin && r.down.is_none());
        if dropped || (sent.contains(&plugin) && !live) {
            delete_fleet(&fleets, &f.name_any()).await?;
        }
    }
    Ok(())
}

async fn delete_fleet(fleets: &Api<Fleet>, name: &str) -> Result<(), Error> {
    match fleets.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}
```

A Fleet already deleting keeps a `deletionTimestamp`. Re-applying it is harmless and deleting it again is a no-op.

In `controllers/daemon.rs`'s `reconcile`, replace the `/readyz` block's success path:

```rust
        match client.ready().await {
            Err(e) => {
                let not_ready = Cond::no("Ready", "DaemonNotReady", &e.to_string());
                with_condition(&mut status.conditions, &daemon, not_ready, &ctx.k8s_now());
            }
            Ok(()) => {
                // §23.4: on every reconcile; it is how the list returns
                // after a Daemon restart
                let (plugins, sent) = super::daemon_plugins::send_list(&ctx, &daemon, &namespace, &client).await?;
                // a held-back list wrote nothing new to the Daemon: leave the Fleets
                if let Some(sent) = sent {
                    super::daemon_plugins::write_managed(&ctx, &daemon, &namespace, &client, &sent).await?;
                }
                if plugins.status != Some(true) {
                    let ready = Cond::no("Ready", &plugins.reason, &plugins.message);
                    with_condition(&mut status.conditions, &daemon, ready, &ctx.k8s_now());
                }
                with_condition(&mut status.conditions, &daemon, plugins, &ctx.k8s_now());
            }
        }
```

Also requeue sooner while plugins are listed. The managed requests and the rows are polled (§23.4):

```rust
    let period = if daemon.spec.plugins.is_empty() { ctx.run.period } else { ctx.run.fleet_period };
    Ok(Action::requeue(again.min(period)))
```

`with_condition` replaces an entry already in `computed`. `PluginsReady` is always present, since Task 4's `daemon_status` puts it there.

- [ ] **Step 4: Run**

Run: `mise run operator`
Expected: PASS, all of `controllers_it` and `plugins_it`.

- [ ] **Step 5: Commit**

```bash
git add operator/
git commit -m "feat(operator): the Daemon sends its plugin list, judges PluginsReady and writes managed Fleets"
```

---

### Task 7: Plugin images on kind, and CI

**Files:**
- Modify: `scripts/kind-up.sh` (after the agent image, before `kind load`).
- Modify: `scripts/operator.sh` (`e2e)`).
- Modify: `operator/.config/nextest.toml` (`[profile.e2e-k8s]`).
- Modify: `.github/workflows/ci.yml:114-171` (the e2e-k8s job).
- Modify: `mise.toml` (the `kind-up` description).

**Interfaces:**
- Produces:
  - the images `balerix-plugin-flow:e2e`, `balerix-plugin-web:e2e` and `balerix-fake-plugin:e2e`, loaded into kind;
  - the env var `BALERIX_K8S_PLUGIN_IMAGES=<flow>,<web>,<fake>`, which Task 8's gate reads.

- [ ] **Step 1: Build and load the plugin images** in `scripts/kind-up.sh`, before `kind load docker-image`

```bash
# the plugin images (Spec O §23.5): flow and web as released, a static musl
# binary on distroless (docker/plugin/Dockerfile); and the fake plugin, the
# balerix image whose entrypoint is `balerix dev fake-plugin`
case "$(uname -m)" in
  x86_64 | amd64) musl=x86_64-unknown-linux-musl ;;
  aarch64 | arm64) musl=aarch64-unknown-linux-musl ;;
  *) echo "kind-up: no musl target for $(uname -m)" >&2; exit 2 ;;
esac
command -v musl-gcc >/dev/null || { echo "kind-up: musl-gcc is not on PATH (apt-get install musl-tools)" >&2; exit 2; }
rustup target add "$musl" >/dev/null
for unit in flow web; do
  out="$root/dist-$unit"
  scripts/release/build.sh "$unit" "$musl" "$out" >/dev/null
  context=$(scripts/release/image-context.sh "$unit" "$out" "$root/context-$unit" | sed -n 's/^context=//p')
  docker build -t "balerix-plugin-$unit:e2e" -f docker/plugin/Dockerfile "$context"
done
docker build -t balerix-fake-plugin:e2e - <<'EOF'
FROM balerix:e2e
ENTRYPOINT ["/usr/bin/tini", "--", "balerix", "dev", "fake-plugin"]
CMD []
EOF
```

Then change the load line to:

```bash
kind load docker-image --name "$name" balerix:e2e balerix-agent:e2e \
  balerix-plugin-flow:e2e balerix-plugin-web:e2e balerix-fake-plugin:e2e
```

Add `rustup` to the `for tool in …` check. Update the header comment and the `kind-up` description in `mise.toml` to say "and the flow, web and fake plugin images".

- [ ] **Step 2: Pass the images to the journey.** In `scripts/operator.sh`'s `e2e)` branch:

```bash
    export BALERIX_K8S_PLUGIN_IMAGES="${BALERIX_K8S_PLUGIN_IMAGES:-balerix-plugin-flow:e2e,balerix-plugin-web:e2e,balerix-fake-plugin:e2e}"
```

- [ ] **Step 3: One journey at a time.** In `operator/.config/nextest.toml`'s `[profile.e2e-k8s]`, add:

```toml
# two journeys on one three-node kind cluster: one at a time
test-threads = 1
```

Update the comment above it to say "the journeys (tests/e2e_k8s.rs)" and "45 periods … per journey".

- [ ] **Step 4: CI.** In the e2e-k8s job of `.github/workflows/ci.yml`, change the tool install and add musl-tools:

```yaml
      - if: steps.changes.outputs.run == 'true'
        run: |
          sudo apt-get update -q
          sudo apt-get install -yq --no-install-recommends musl-tools
          mise install rust jq cargo:cargo-nextest kind@0.33.0 kubectl@1.34.12
```

Add `plugins/` to the path filter's regex, after `operator/|agent/|crates/`. Add a `plugins -> target` line to the rust-cache workspaces only if `scripts/plugin.sh target-dir` puts the plugins' target under `plugins/<unit>/target`; check it and use the directory it prints. Extend the failure dump's `kubectl get` to `daemons,fleets,crews,agents,plugins,deployments,pods,jobs,pvc`.

- [ ] **Step 5: Lint**

Run: `shellcheck scripts/kind-up.sh scripts/operator.sh && mise run lint`
Expected: PASS. Hadolint does not see the heredoc Dockerfile; it is two lines.

This host has no docker (memory: "anything needing a cluster runs in CI only"), so `kind-up` cannot run here. Its first run is Task 8's CI run.

- [ ] **Step 6: Commit**

```bash
git add scripts/ operator/.config/nextest.toml .github/workflows/ci.yml mise.toml
git commit -m "build(e2e-k8s): flow, web and fake plugin images on kind (Spec O §23.5)"
```

---

### Task 8: The plugin journey in `e2e-k8s`

**Files:**
- Modify: `operator/tests/e2e_k8s.rs`.

**Interfaces:**
- Consumes: the images from Task 7; everything above.
- Produces: `the_plugin_journey_on_kind`. The shared setup moves into helpers and `the_phase_3_journey_on_kind` keeps its steps:
  - `fn plugin_gate() -> Option<(String, String, String)>`: `BALERIX_K8S_PLUGIN_IMAGES` as flow, web and fake, under the same skip/panic rule as `gate`.
  - `async fn namespace_for(client: &kube::Client, label: &str) -> String`: creates `e2e-<label>-<pid>`.
  - `fn spawn_operator(ns: &str, daemon_image: &str, agent_image: &str, forward_port: u16) -> Operator`
  - `async fn git_server(client: &kube::Client, ns: &str, agent_image: &str) -> String`: the repo URL. This is the existing code at `:163-199` moved.
  - `fn daemon_object(plugins: &[&str]) -> Daemon`: the spec at `:201-220`, with `plugins`.
  - `async fn wait_daemon_ready(client, ns) -> ()`, and `fn forward(ns: &str, port: u16) -> Forward`.
  - `fn admin_http(client: &kube::Client, ns: &str, forward_port: u16) -> (reqwest::Client, String /*base*/, String /*token*/)`: a reqwest client trusting the Daemon's authority and resolving `balerix-default.<ns>.svc` to the forward. The TLS setup copies `DaemonClient::new_resolving`'s.

- [ ] **Step 1: Extract the shared setup** into the helpers above, moving the code verbatim with only parameters added. Run `cd operator && cargo clippy --all-targets -- -D warnings`; expect clean. The journey itself runs in CI.

- [ ] **Step 2: Write the journey**

```rust
/// Spec O §17 sub-project 4 / §23.6: flow and web listed and Ready; a flow
/// rule acting on an agent's Stop; web's review page through the Daemon;
/// web's config changing with no restart by hand (§23.8); the fake plugin's
/// managed Fleet coming up and going with the plugin.
#[tokio::test(flavor = "multi_thread")]
async fn the_plugin_journey_on_kind() {
    let Some((daemon_image, agent_image)) = gate() else { return };
    let Some((flow_image, web_image, fake_image)) = plugin_gate() else { return };
    let client = balerix_operator::request_client::request_client(kube::Config::infer().await.unwrap()).unwrap();
    let ns = namespace_for(&client, "plugins").await;
    let forward_port = free_port();
    let _operator = spawn_operator(&ns, &daemon_image, &agent_image, forward_port);
    let repo = git_server(&client, &ns, &agent_image).await;

    // 1. Plugins flow and web, listed by the Daemon in that order
    let plugins: Api<Plugin> = Api::namespaced(client.clone(), &ns);
    let plugin = |image: &str, needs: &[&str], config: serde_json::Value| -> PluginSpec {
        serde_json::from_value(serde_json::json!({ "image": image, "needs": needs, "config": config })).unwrap()
    };
    plugins.create(&PostParams::default(), &Plugin::new("flow", plugin(&flow_image, &["actions", "kv"], serde_json::json!({})))).await.unwrap();
    plugins.create(&PostParams::default(), &Plugin::new("web", plugin(&web_image, &["fleets", "attach", "actions", "workspace"], serde_json::json!({})))).await.unwrap();
    Api::<Daemon>::namespaced(client.clone(), &ns)
        .create(&PostParams::default(), &daemon_object(&["flow", "web"])).await.unwrap();
    let _forward = forward(&ns, forward_port);
    wait_daemon_ready(&client, &ns).await; // Ready needs PluginsReady=True now
    for name in ["flow", "web"] {
        wait_for_condition(&plugins, name, "Ready", Duration::from_secs(300)).await;
    }

    // 2. a Fleet whose alice runs flow: Stop → send_text into her stdin
    let fleets: Api<Fleet> = Api::namespaced(client.clone(), &ns);
    let flow_block = serde_json::json!({ "flow": { "initial": "working", "states": {
        "working": { "on": [{ "event": "Stop", "goto": "review", "send": { "text": "flow says: run the tests" } }] },
        "review": { "on": [{ "event": "Stop", "goto": "done" }] },
        "done": {} } } });
    fleets.create(&PostParams::default(), &serde_json::from_value(serde_json::json!({
        "apiVersion": "balerix.ai/v1alpha1", "kind": "Fleet", "metadata": { "name": "f" },
        "spec": { "daemon": "default", "crews": { "c": { "repo": repo, "ref": "main",
            "git": { "push": false, "auth": "none" },
            "agents": { "alice": { "plugins": flow_block } } } } } })).unwrap()).await.unwrap();
    let alice_pod = wait_agent_ready(&client, &ns, "f-c-alice", Duration::from_secs(900)).await;
    let stdin = poll(Duration::from_secs(120), || {
        exec(&ns, &alice_pod, "agent", "cat /balerix/agent/home/fake-claude.stdin 2>/dev/null")
            .ok().filter(|s| s.contains("flow says: run the tests"))
    }).await;
    assert!(stdin.contains("flow says"), "{stdin}");

    // 3. web's review page, through the Daemon's mount over the port-forward
    let (http, base, token) = admin_http(&client, &ns, forward_port).await;
    let page = http.get(format!("{base}/v1/plugins/web/agents/f/c/alice/review"))
        .bearer_auth(&token).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let body = page.text().await.unwrap();
    assert!(body.contains("alice"), "{body}");

    // 4. §23.8: web's config changes; its pod rolls and says hello under the
    //    new revision with no restart by hand; flow stays Ready
    let before = web_pod_uid(&client, &ns).await;
    plugins.patch("web", &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "spec": { "config": { "enabled": true } } }))).await.unwrap();
    poll(Duration::from_secs(300), || async {
        let now = web_pod_uid(&client, &ns).await;
        (now != before).then_some(())
    }).await;
    wait_for_condition(&plugins, "web", "Ready", Duration::from_secs(300)).await;
    wait_for_condition(&plugins, "flow", "Ready", Duration::from_secs(60)).await;

    // 5. the fake plugin manages Fleet m; its agent becomes Ready
    plugins.create(&PostParams::default(), &Plugin::new("fake", plugin(&fake_image,
        &["actions", "fleets", "kv", "manage"],
        serde_json::json!({ "manage": { "fleet": "m", "file": { "crews": { "c": { "repo": repo, "ref": "main",
            "git": { "push": false, "auth": "none" }, "agents": { "carol": {} } } } } } })))).await.unwrap();
    Api::<Daemon>::namespaced(client.clone(), &ns).patch("default", &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "spec": { "plugins": ["flow", "web", "fake"] } }))).await.unwrap();
    let m = poll(Duration::from_secs(600), || async { fleets.get_opt("m").await.unwrap() }).await;
    assert_eq!(m.labels()["balerix.ai/managed-by"], "fake");
    wait_agent_ready(&client, &ns, "m-c-carol", Duration::from_secs(900)).await;

    // 6. dropping fake from spec.plugins deletes its Fleet
    Api::<Daemon>::namespaced(client.clone(), &ns).patch("default", &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "spec": { "plugins": ["flow", "web"] } }))).await.unwrap();
    poll(Duration::from_secs(600), || async { fleets.get_opt("m").await.unwrap().is_none().then_some(()) }).await;
}
```

Write the small helpers the test names, beside the existing ones:
- `free_port()`, the `TcpListener` trick already at `:131`;
- `poll(timeout, f)`, the async-closure equivalent of `support::wait_for`. Use `support::wait_for` itself if `e2e_k8s.rs` already has `mod support;`;
- `wait_for_condition(api, name, type_, timeout)`, a `True` condition;
- `wait_agent_ready(client, ns, agent, timeout) -> String`, the Agent `Ready`, returning its pod name. Step 3 of the phase-3 journey has this logic;
- `web_pod_uid(client, ns) -> Option<String>`, the uid of the pod labelled `balerix.ai/plugin=web` with a Running phase.

The Fleet's agent layer key for plugins is `plugins`. That is the same block as the one-machine journey's `flow_block` in `crates/balerix/tests/e2e.rs:1191`, but with no PreToolUse rule, because the pod's fake claude needs no `rm -rf` block for this check. If the Fleet CRD's agent layer refuses the `plugins` key, read §4.2: the layers are open objects validated by `balerix-config`.

In the fake plugin's `manage.file`, `name` is not needed: `apply_fleet(fleet, file)` names the fleet.

- [ ] **Step 3: Lint, push and run in CI**

```bash
cd /workspace/operator && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check
cd /workspace && git add operator/tests/e2e_k8s.rs
git commit -m "test(e2e-k8s): the plugin journey — flow, web, a config roll and a managed Fleet (Spec O §23.6)"
git push -u origin feat/kube-plugins-cluster
gh pr create --draft --title "feat: plugins on a cluster (Spec O 4b, §23.4–§23.8)" --body "<filled in Task 9>"
gh pr checks --watch
```

Expected: every job green, `e2e-k8s` included. If `e2e-k8s` fails, read the job's failure dump (Plugins, Deployments and pod logs). Use superpowers:systematic-debugging before changing code. The first run also exercises kind-up's musl builds.

---

### Task 9: Spec §23.9, the claude bump, and the PR

**Files:**
- Modify: `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` (§17 item 4; new §23.9).
- Modify: `mise.toml` (the user's `claude = "2.1.291"`, already in the working tree).

- [ ] **Step 1: Write §23.9 "Decided by the 4b plan"** after §23.8, as built. Each of these is one bullet:
  - plugin object names (`balerix-plugin-<p>`, `-token`, `-tls`, `-scratch`; Service `<p>`);
  - `DaemonError::ListPending`;
  - `PluginsReady=Unknown/Pending` until the Daemon answers, and no longer part of `daemon_status`'s `Ready`;
  - the Daemon requeues at the Fleet period while plugins are listed;
  - the revision's inputs (name, spec, resolved config, token, certificate);
  - `HOME=/tmp` in the plugin pod;
  - no readiness probe: `Deployed` is the Deployment's available replica, and `Ready` is the row;
  - an unlisted Plugin's scratch claim is deleted with its other objects;
  - a listed plugin whose grant or config cannot be built holds the whole list back for the pass (the Daemon keeps its last list and the plugin's managed requests); only a plugin whose token or serving Secret is not made yet is left out;
  - the fake plugin image is a heredoc stage in kind-up;
  - `e2e-k8s` runs its two journeys one at a time.

  Add any ruling made while building, in the same style as §23.7's "Rulings made while building it".

  Mark §17 item 4 done: "4a done 2026-10 (PR #143); 4b done 2026-10 (PR #<n>)". Write `<n>` once the PR exists.

- [ ] **Step 2: The claude bump's by-hand checks**

Run: `mise run verify-claude` and `mise run verify-questions`. Drive them from a tmux session yourself (memory: verify-claude is drivable from tmux).
Expected: both pass. Save each report's text for the PR body. If either fails, stop and report; the bump is the user's.

- [ ] **Step 3: Commit and update the PR**

```bash
git add docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md mise.toml
git commit -m "docs(spec): Spec O §23.9 — decided by the 4b plan; chore(mise): bump claude to 2.1.291"
git push
gh pr edit --body-file <a file holding: summary, the §23.9 list, test evidence, the two by-hand reports>
gh pr checks --watch
```

Expected: every check green. Merging and marking the PR ready are the user's.

---

## Self-review notes

- **§23.4 coverage:**
  - Plugin controller: Task 5. Its listed, unlisted and listed-twice cases are pure in Task 3 and tested on envtest in Task 5.
  - Objects and hash: Task 3, with snapshots, and Task 5.
  - Secrets resolved by the operator: Task 3 `inject_secrets`, Task 5 `read_config`.
  - List sent on every reconcile, and `PluginsReady`: Tasks 4 and 6.
  - Plugin conditions: Tasks 3 and 5.
  - Managed-Fleet writer: Tasks 4 and 6.
  - Fleet controller sends `managed_by`: Task 4.
- **§23.5:** Task 7. **§23.6:** Task 5 and 6 envtest, and Task 8 e2e. **§23.8:** Task 1, the Task 3 env var and entry, and Task 8 step 4.
- **Type consistency:** `PluginInputs`, `PluginState`, `Listing`, `read_config`, `read_token`, `read_serving`, `send_list` and `write_managed` use the same names in their defining and consuming tasks. If Task 5 makes `PluginState::Running.row` owned, Task 3's test changes with it in the same task.
