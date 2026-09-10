# Balerix Spec B / Phase 3 — Proxy, Attach, Watch and Web Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish Spec B: the reverse-proxied plugin mount with browser sessions, the daemon presenting each plugin's own token on every call, `AgentRunner::attach` on tmux with the daemon's `attach` and `fleets/watch` WebSocket streams, the SDK's stream clients and `routes`, and `balerix-plugin-web`, a browser terminal on a vendored xterm.js. Ends with the e2e `web_journey`: a login code becomes a cookie, the index lists alice from a watch-fed cache, a WebSocket through the proxy shows fake-claude's pane and types into it, and `down` empties the index.

**Architecture:** Ten tasks, each mergeable. The daemon first learns to authenticate *to* plugins (the plugin's own token as a bearer on every daemon → plugin call; the SDK router enforces it), then gains browser sessions (one-time login codes exchanged for an in-memory cookie) and the proxy (a plain reverse proxy on hyper's legacy client, upgrades passed through byte for byte). The `PtyStream` port lands in `balerix-core` with an echo fake, and tmux implements it as a throwaway grouped session in a `portable-pty` PTY. The daemon then serves `attach` and `fleets/watch` over axum's `ws` feature, the SDK gets `Host::attach`, `Host::watch_fleets`, `Plugin::routes` and matching `FakeHost` streams, and `balerix-plugin-web` is built on those: a per-agent `enabled` flag, a cache fed by `fleets/watch`, an index, a terminal page and a bridge to the daemon's attach socket. The e2e and the docs close it.

**Tech Stack:** Rust 1.98.1 (edition 2024); tokio 1.53.1, axum 0.8.9 (**now with `ws`**), reqwest 0.13.4 (no TLS), prometheus 0.14.0; **new exact dependencies:** tokio-tungstenite 0.30.0 (no TLS feature), portable-pty 0.9.0, and, promoted from transitive to direct, hyper 1.11.1, hyper-util 0.1.20, http-body-util 0.1.5, futures-util 0.3.34 (`sink`, `std` only); serde_path_to_error (web config); vendored `@xterm/xterm` 6.0.0 and `@xterm/addon-fit` 0.11.0 (minified builds only, digests in `VENDOR.md`); real `tmux 3.7c`, `nono 0.75.0`, `mise` for the integration and e2e tests.

**Spec:** `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` (the *plugins spec*), §18 (the phase 3 decisions) on top of §4.1, §4.2, §6, §7, §9, §11 and §11.1; `docs/plugin-protocol.md` for the wire contract. Read §18 whole before any task; §18.3 before Task 1; §18.2 before Tasks 2–3; §18.4 before Tasks 4–6; §18.5 before Tasks 7–8; §18.6 before Tasks 9–10. Where this plan refines §18 (recorded again in Task 10 as §18.7):

- **The plugin's token is kept in the registry from `hello`.** §18.3 says the daemon presents "that plugin's token"; the one place both the address and the token are known is `hello`, so `PluginRegistry::set_listen` takes the token too, and a `PluginAddr { listen, token }` (hand-written `Debug`, token redacted) replaces the bare `listen: String` everywhere the daemon calls a plugin.
- **The root of a mount forwards to `/v1/routes`, no trailing slash.** axum's `nest("/v1/routes", …)` answers the nested router's `/` at `/v1/routes` and 404s `/v1/routes/` (verified with axum 0.8.9 during planning), so `/v1/plugins/<name>/` forwards to `http://<listen>/v1/routes` and `/v1/plugins/<name>/<rest>` to `/v1/routes/<rest>`. `/v1/plugins/<name>` without the slash stays the purge route.
- **`destroy-unattached` is set after the client is attached.** tmux 3.7c destroys a detached session the moment the option is set on it, so the attach is one command sequence in the PTY: `new-session -t =<crew> -s balerix-attach-<hex> ; select-window -t =<agent> ; set-option destroy-unattached on ; set-option status off` (verified during planning: the grouped session shows `attached=1`, `current=<agent>`, the crew session's own current window is untouched, and closing the PTY destroys the grouped session while the crew's windows stay).
- **`FleetWatch::next` never ends.** §18.4's `Option<Vec<FleetRecord>>` had no `None` case worth building: the watch reconnects forever and a plugin drops it to stop, so `next()` returns `Vec<FleetRecord>`.
- **`Host` is `Clone`** (a `FleetWatch` owns one), `Plugin::routes` defaults to `None`, and the login `to` path is restricted to `[A-Za-z0-9/._~-]` under `/v1/plugins/` so it needs no encoding in the login URL.
- **The watch property test of §18.6 collapses to equality by construction** (every frame is the whole list) and is not written; `streams_it` and the cache's unit test cover it (recorded in §18.7).
- **Fixtures with a `transport: "websocket"` field** describe one frame, not a request/response; the HTTP replay skips them and the SDK's stream test asserts them. There are eighteen fixtures after this phase.
- **Verify at implementation time** (§11.1 rows, verdicts recorded in Task 10): the `hyper-util` legacy client's upgrade handoff (the spike compiled; the WebSocket echo through the proxy in Task 3 is the verdict), and `portable-pty` driving `tmux attach-session` (the `test-it` case in Task 4 is the verdict). The `ttyd` row is closed as *not used*.

## Global Constraints

Copied from the specs and the earlier plans; every task's requirements include these.

- Rust **1.98.1**, `edition = "2024"`, `rust-version = "1.98"`; every tool in `mise.toml` is an exact version. Run cargo as `mise x -- cargo …` or through `mise run <task>`.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass at every commit. `unsafe_code = "forbid"` (which is why the PTY comes from `portable-pty`, never a hand-rolled `pre_exec`). No `unwrap`/`expect` outside tests (test modules and `tests/*.rs` carry `#![allow(clippy::unwrap_used, clippy::expect_used)]`). `std::env::set_var` is unsafe in edition 2024 — inject environment through parameters.
- Library crates return `thiserror` errors whose `Display` starts with the config path; only binaries use `anyhow`. The web binary's failures print `web: <error>` to stderr and exit 1.
- Dependency direction: `api` leaf → `core` → `config` / `runtime` / `server` / `plugin-sdk` → binary; plugin crates depend on `balerix-plugin-sdk` and `balerix-api` only (plus third-party crates), never on `core`, `server`, `runtime` or `config`. `balerix-server` never imports `balerix-runtime`.
- New Cargo dependencies go in `[workspace.dependencies]` with an exact version and a reason in the commit message. This plan adds exactly: `tokio-tungstenite = "0.30.0"`, `portable-pty = "0.9.0"`, `hyper = { version = "1.11.1", features = ["http1", "client"] }`, `hyper-util = { version = "0.1.20", features = ["client-legacy", "http1", "tokio"] }`, `http-body-util = "0.1.5"`, `futures-util = { version = "0.3.34", default-features = false, features = ["sink", "std"] }`, and enables `axum`'s `ws` feature. No `futures` umbrella, no `async-trait`, no `tokio-util`, no TLS feature anywhere (P3-1).
- Plain HTTP on `127.0.0.1` only. Every `reqwest::Client` is built with `.no_proxy()`; the proxy client connects only to a `listen` the daemon validated at `hello`.
- Secrets never appear in `Debug` output, logs, argv or the environment: the plugin token in `PluginAddr`/`PluginInfo` and the session ids print `<redacted>`; the daemon logs plugin names, agent ids and error messages only.
- Untrusted input: hook payloads, plugin bodies, browser requests and plugin configs are validated before use; proxied request bodies are capped at 1 MiB; the login `to` path is validated; a browser's `Origin`/`Sec-Fetch-Site` is checked on every cookie request.
- Integration and e2e tests skip with a printed reason when a tool or Landlock is missing; `BALERIX_REQUIRE_TOOLS=1` (CI) panics instead. Temp roots live under `target/tmp`, never `/tmp`.
- Commit messages: imperative subject, body explains why, and end with the trailer line `Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q`.
- The pre-commit hook runs `mise run precommit` (gitleaks + `check`, e2e included). Every commit below goes through it; the branch is `spec-b3-proxy-attach-web`, already holding the §18 spec commit.

## File structure

```
Cargo.toml                                        axum ws; + hyper, hyper-util, http-body-util, futures-util, tokio-tungstenite, portable-pty, balerix-plugin-web path
mise.toml                                         (unchanged; package-plugins loops over the crates)
scripts/package-plugins.sh                        `for name in flow web`
scripts/vendor-xterm.sh                           new: fetch + verify the xterm.js build
scripts/verify-claude.sh                          + the web plugin and `plugin open web`
crates/balerix-api/src/
  protocol.rs                                     + ResizeFrame, Resize
  request.rs                                      + SessionRequest, SessionResponse
  lib.rs                                          re-exports
crates/balerix-core/src/
  ports.rs                                        + PtyStream; AgentRunner::attach
  fakes.rs                                        + FakePty (echo), FakeRunner::{attach, close_attach, resizes}
  lib.rs                                          + PtyStream
crates/balerix-runtime/
  Cargo.toml                                      + portable-pty
  src/tmux.rs                                     + TmuxAttach, attach(), stop_crew kills the group, sessions_in_group
  tests/tmux_it.rs                                + attach test
crates/balerix-plugin-sdk/
  Cargo.toml                                      + tokio-tungstenite, futures-util
  src/auth.rs                                     new: bearer(), constant_time_eq()
  src/plugin.rs                                   router(plugin, token): bearer check, routes nesting; Plugin::routes
  src/host.rs                                     Host: Clone; attach(), watch_fleets(); Attach, AttachRead, AttachWrite, FleetWatch
  src/testing.rs                                  Harness token + route helpers; FakeHost set_fleets/resizes/attaches + the two WS routes
  src/lib.rs                                      re-exports
  tests/conformance.rs                            headers on daemon-to-plugin fixtures; websocket fixtures; 18 fixtures
crates/balerix-server/
  Cargo.toml                                      + hyper, hyper-util, http-body-util; dev: tokio-tungstenite, futures-util
  src/plugins/registry.rs                         PluginAddr; PluginInfo.token; set_listen(name, listen, token); ready_addr
  src/plugins/client.rs                           every call takes &PluginAddr and sends the bearer
  src/plugins/chain.rs, host.rs                   adapted
  src/daemon.rs                                   + sessions, origin(), proxy_client(), runner(), changes()/bump(), forwarders
  src/sessions.rs                                 new: Sessions, cookie_value, set_cookie, same_origin, login_target
  src/proxy.rs                                    new: HttpClient, forwarded_headers, upstream_uri, forward
  src/attach.rs                                   new: bridge(WebSocket, Box<dyn PtyStream>)
  src/watch.rs                                    new: serve_watch(WebSocket, Arc<Daemon>)
  src/api.rs                                      + /v1/sessions, /v1/login/{code}, the proxy mount
  src/plugin_api.rs                               + fleets/watch, agents/{id}/attach
  src/metrics.rs                                  + balerix_plugin_proxy_requests_total
  src/testing.rs                                  StubScript.expect_token
  src/lib.rs                                      re-exports
  tests/support/mod.rs                            web package: routes + attach; Api::raw
  tests/browser_it.rs                             new: sessions + proxy
  tests/streams_it.rs                             new: attach + watch
  tests/protocol_it.rs, events_it.rs              adapted
crates/balerix/
  Cargo.toml                                      dev: tokio-tungstenite, futures-util
  src/cli.rs                                      + PluginCommand::Open
  src/client.rs                                   + create_session
  src/commands/plugin.rs                          + open_command
  src/commands/dev.rs                             fake-plugin passes the token to run()
  src/main.rs                                     dispatch
  tests/e2e.rs                                    + web_journey
crates/balerix-plugin-web/                        new crate
  Cargo.toml, src/{lib,config,state,routes,plugin,main}.rs
  assets/{xterm.js,xterm.css,addon-fit.js,LICENSE.xterm,VENDOR.md}
  package/{mise.toml,balerix-plugin.yaml}
  tests/plugin_it.rs
docs/plugin-protocol.md, docs/plugin-protocol/*.json  bearer, routes, streams; 4 new fixtures
docs/THREAT-MODEL.md, ARCHITECTURE.md, AGENTS.md, README.md, examples/payments.yaml, spec §18.7
```

---

### Task 1: The daemon presents the plugin's token on every call

**Files:**
- Modify: `crates/balerix-server/src/plugins/registry.rs`
- Modify: `crates/balerix-server/src/plugins/client.rs`
- Modify: `crates/balerix-server/src/plugins/chain.rs:101-107,165,185,207` and its tests (`set_listen` calls at 390, 446–448, 489, 569)
- Modify: `crates/balerix-server/src/plugins/host.rs:278-300` (`hello`)
- Modify: `crates/balerix-server/src/plugins/mod.rs`, `crates/balerix-server/src/lib.rs` (re-export `PluginAddr`)
- Modify: `crates/balerix-server/src/daemon.rs:172-184,236-267,285-319,426-462` and the test at 1374
- Modify: `crates/balerix-server/src/api.rs:177-184`
- Modify: `crates/balerix-server/src/testing.rs` (`StubScript.expect_token`)
- Modify: `crates/balerix-server/tests/protocol_it.rs`
- Create: `crates/balerix-plugin-sdk/src/auth.rs`
- Modify: `crates/balerix-plugin-sdk/src/plugin.rs`, `src/lib.rs`, `src/testing.rs`, `tests/conformance.rs`
- Modify: `crates/balerix-server/tests/events_it.rs:113-125`, `crates/balerix/src/commands/dev.rs:297-330`
- Modify: `docs/plugin-protocol/{activate,activate-rejected,events,intercept,health,metrics}.json`; Create: `docs/plugin-protocol/activate-bad-token.json`
- Modify: `docs/plugin-protocol.md` §2, §4, §6

**Interfaces:**
- Consumes: `PluginRegistry`, `PluginClient`, `Daemon::plugin_hello(name, token, req)`, the SDK `router`/`run`/`serve`.
- Produces:
  ```rust
  // balerix_server::plugins::registry (re-exported at crate root)
  pub struct PluginAddr { pub listen: String, pub token: String }   // Debug redacts token
  impl PluginRegistry {
      pub fn set_listen(&self, name: &AgentName, listen: String, token: String);
      pub fn ready_addr(&self, name: &AgentName) -> Option<PluginAddr>;   // replaces ready_listen
      pub fn interceptors(&self, agent: &AgentId, event: &str) -> Vec<(AgentName, PluginAddr)>;
      pub fn observers(&self, agent: &AgentId, event: &str) -> Vec<(AgentName, PluginAddr)>;
  }
  pub struct PluginInfo { pub manifest, pub listen: Option<String>, pub token: Option<String>, pub ready, pub degraded }  // Debug redacts token
  // balerix_server::plugins::client — every `listen: &str` becomes `addr: &PluginAddr`
  impl PluginClient { activate(&self, addr, req); deactivate(&self, addr, req); events(&self, addr, batch);
                      intercept(&self, addr, req, timeout); health(&self, addr); metrics(&self, addr, timeout) }
  // balerix_server::plugins::host
  impl PluginHost { pub async fn hello(&self, name: &AgentName, req: HelloRequest, token: &str) -> Result<HelloResponse, DaemonError> }
  // balerix_server::testing
  pub struct StubScript { …, pub expect_token: Option<String> }   // 401 { "error": "bad daemon token" } on mismatch
  // balerix_plugin_sdk
  pub mod auth { pub fn bearer(headers: &HeaderMap) -> Option<&str>; pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool }
  pub fn router<P: Plugin>(plugin: Arc<P>, token: &str) -> Router;          // 401 without `Bearer <token>`
  pub async fn run<P: Plugin>(listener, plugin: Arc<P>, token: &str) -> Result<(), SdkError>;
  // testing::Harness
  pub fn token(&self) -> &str;                                            // every Harness call sends it
  ```

- [ ] **Step 1: Write the failing registry test**

In `crates/balerix-server/src/plugins/registry.rs`, add to the `tests` module:

```rust
    #[test]
    fn hello_records_the_token_the_daemon_presents_and_never_prints_it() {
        let r = PluginRegistry::new();
        r.replace_plugins(&[plugin("flow", &["Stop"], &[], &[])], &[]);
        assert_eq!(r.ready_addr(&name("flow")), None);
        r.set_listen(&name("flow"), "127.0.0.1:4000".into(), "s3cret-token".into());
        let addr = r.ready_addr(&name("flow")).unwrap();
        assert_eq!(
            (addr.listen.as_str(), addr.token.as_str()),
            ("127.0.0.1:4000", "s3cret-token")
        );
        let dbg = format!("{addr:?}");
        assert!(
            dbg.contains("127.0.0.1:4000") && !dbg.contains("s3cret") && dbg.contains("<redacted>"),
            "{dbg}"
        );
        let dbg = format!("{:?}", r.plugin(&name("flow")).unwrap());
        assert!(!dbg.contains("s3cret") && dbg.contains("<redacted>"), "{dbg}");
        r.set_ready(&name("flow"), false);
        assert_eq!(r.ready_addr(&name("flow")), None, "not ready: no address");
        // a re-sync that keeps the plugin keeps its token with the address
        r.set_ready(&name("flow"), true);
        r.replace_plugins(&[plugin("flow", &["Stop"], &[], &[])], &["flow".into()]);
        assert_eq!(
            r.ready_addr(&name("flow")).unwrap().token,
            "s3cret-token"
        );
        let a = id("f/c/a");
        r.set_row(
            &a,
            &name("flow"),
            ActivationRow {
                config: json!({}),
                activation: PluginActivation::active(),
            },
        );
        assert_eq!(
            r.interceptors(&a, "Stop"),
            vec![(
                name("flow"),
                PluginAddr {
                    listen: "127.0.0.1:4000".into(),
                    token: "s3cret-token".into()
                }
            )]
        );
    }
```

Update the two existing registry tests: every `r.set_listen(&name(..), "…".into())` gains a third argument `"tok".into()`, `ready_listen(&x)` becomes `ready_addr(&x).map(|a| a.listen)` where a `String` is compared (e.g. `assert_eq!(r.ready_addr(&name("flow")).map(|a| a.listen).as_deref(), Some("127.0.0.1:4000"))`), and the `interceptors`/`observers` assertions compare against `PluginAddr { listen: "127.0.0.1:1".into(), token: "tok".into() }` instead of the bare string.

- [ ] **Step 2: Run the registry tests to see them fail**

Run: `mise x -- cargo test -p balerix-server registry`
Expected: compile errors (`set_listen` takes 2 arguments, no `ready_addr`, no `PluginAddr`).

- [ ] **Step 3: Implement `PluginAddr` and the token in the registry**

In `crates/balerix-server/src/plugins/registry.rs`, add `use std::fmt;` and replace the `PluginInfo` definition with:

```rust
/// Where a ready plugin listens and the bearer the daemon presents to it
/// (plugins spec §18.3): the plugin's own token, learned at `hello`.
#[derive(Clone, PartialEq, Eq)]
pub struct PluginAddr {
    pub listen: String,
    pub token: String,
}

impl fmt::Debug for PluginAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginAddr")
            .field("listen", &self.listen)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    /// From the last `hello`; kept while the plugin restarts.
    pub listen: Option<String>,
    /// The token the plugin presented at that `hello`: the bearer on every
    /// daemon → plugin call (§18.3). Kept with `listen`.
    pub token: Option<String>,
    /// `Ready` per the plugin fleet's record. Only ready plugins are called.
    pub ready: bool,
    /// The health poller's verdict; `hello` clears it.
    pub degraded: Option<String>,
}

impl fmt::Debug for PluginInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginInfo")
            .field("manifest", &self.manifest)
            .field("listen", &self.listen)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("ready", &self.ready)
            .field("degraded", &self.degraded)
            .finish()
    }
}
```

In `replace_plugins`, carry the token like the address: after `listen: prev.and_then(|i| i.listen.clone()),` add `token: prev.and_then(|i| i.token.clone()),`.

Replace `set_listen` and `ready_listen`:

```rust
    pub fn set_listen(&self, name: &AgentName, listen: String, token: String) {
        if let Some(p) = self.write().plugins.get_mut(name) {
            p.listen = Some(listen);
            p.token = Some(token);
            p.ready = true;
            p.degraded = None;
        }
    }

    /// `Some` only while the plugin is ready: where to call it and what
    /// bearer to present.
    pub fn ready_addr(&self, name: &AgentName) -> Option<PluginAddr> {
        let r = self.read();
        let p = r.plugins.get(name)?;
        if !p.ready {
            return None;
        }
        Some(PluginAddr {
            listen: p.listen.clone()?,
            token: p.token.clone()?,
        })
    }
```

In `subscribed`, change the return type to `Vec<(AgentName, PluginAddr)>` and the body's address line to:

```rust
                let listen = p.listen.clone().filter(|_| p.ready)?;
                let token = p.token.clone()?;
                let active = r
                    .rows
                    .get(&(agent.clone(), name.clone()))
                    .is_some_and(|row| row.activation.state == ActivationState::Active);
                (active && pick(&p.manifest)).then(|| (name.clone(), PluginAddr { listen, token }))
```

and change the signatures of `interceptors` and `observers` to return `Vec<(AgentName, PluginAddr)>`. In `crates/balerix-server/src/plugins/mod.rs` change the re-export line to `pub use registry::{ActivationRow, PluginAddr, PluginInfo, PluginRegistry};` and in `crates/balerix-server/src/lib.rs` add `PluginAddr` to the `pub use plugins::{…}` list.

- [ ] **Step 4: Make the client take a `PluginAddr` and send the bearer**

In `crates/balerix-server/src/plugins/client.rs`, add `use super::registry::PluginAddr;` and change `url` and every method:

```rust
    fn url(addr: &PluginAddr, path: &str) -> String {
        format!("http://{}{path}", addr.listen)
    }
```

`post` becomes `async fn post<T: Serialize + ?Sized>(&self, addr: &PluginAddr, path: &str, body: &T, timeout: Duration)` with the request built as `self.http.post(Self::url(addr, path)).bearer_auth(&addr.token).timeout(timeout).json(body)`. `activate`, `deactivate`, `events`, `intercept` take `addr: &PluginAddr` and pass it to `post`. `health` and `metrics` take `addr: &PluginAddr` and add `.bearer_auth(&addr.token)` after `.get(Self::url(addr, …))`.

Add above the struct doc: `//! Every call carries the plugin's own token as the bearer (§18.3): the plugin's listener is a loopback port any local process can reach, and this is how it tells the daemon from the rest.`

In the client's tests, replace `let listen = stub().await;` with

```rust
        let addr = PluginAddr {
            listen: stub().await,
            token: "t".into(),
        };
```

and every `&listen` with `&addr`; the connect-failure case becomes `c.health(&PluginAddr { listen: "127.0.0.1:1".into(), token: "t".into() })`; the oversized test builds its `addr` the same way from `oversized_stub().await`.

- [ ] **Step 5: Adapt the chain, the host, the daemon and the metrics scrape**

`crates/balerix-server/src/plugins/chain.rs`: line 101 `let Some(listen) = registry.ready_listen(&self.plugin)` → `let Some(addr) = registry.ready_addr(&self.plugin)`, and `client.events(&listen, …)` → `client.events(&addr, …)`; line 165 `for (name, listen) in …` → `for (name, addr) in …` and `self.client.intercept(&listen, …)` → `self.client.intercept(&addr, …)`. In the tests, every `r.set_listen(&…, l)` / `set_listen(&…, listen)` / `set_listen(&…, "127.0.0.1:1".into())` gains a trailing `"t".into()` argument (lines 390, 446–448, 489, 569).

`crates/balerix-server/src/plugins/host.rs`: `hello` gains a `token: &str` parameter after `req` and calls `self.registry.set_listen(name, req.listen.clone(), token.to_string());`.

`crates/balerix-server/src/daemon.rs`: `plugin_hello` calls `self.plugins.hello(name, req, token).await?`; every `ready_listen` becomes `ready_addr` and the bound name `addr` (lines 174, 250, 286, 303, 427); every `self.client.<call>(&listen, …)` becomes `(&addr, …)`. In the test at line 1374: `w.daemon.registry().set_listen(&flow, good.listen.clone(), "t".into());`.

`crates/balerix-server/src/api.rs` `metrics`: `let Some(addr) = registry.ready_addr(&name) else { continue; };` and `client.metrics(&addr, SCRAPE_TIMEOUT)`.

- [ ] **Step 6: Teach the stub plugin to demand the token**

In `crates/balerix-server/src/testing.rs`, add `pub expect_token: Option<String>,` to `StubScript` (after `metrics_body`), and in `stub_plugin` insert, after `.with_state(S { … })`, a token check as a route layer. Put this before `let app = Router::new()`:

```rust
    async fn check_token(
        State(s): State<S>,
        req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let ok = match &s.script.expect_token {
            None => true,
            Some(want) => crate::auth::bearer(req.headers())
                .is_some_and(|got| crate::auth::constant_time_eq(got.as_bytes(), want.as_bytes())),
        };
        if ok {
            next.run(req).await
        } else {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "bad daemon token" })),
            )
                .into_response()
        }
    }
```

and build the router as `Router::new().route(…)….route_layer(axum::middleware::from_fn_with_state(state.clone(), check_token)).with_state(state)` where `let state = S { script, calls: calls.clone() };` is bound before the router (the `record` closure keeps working on `&S`). The `S` struct must be `Clone` (it is).

- [ ] **Step 7: Assert the token on the wire in `protocol_it.rs`**

In `crates/balerix-server/tests/protocol_it.rs`, add `expect_token: Some("tok".into()),` to the `StubScript`, add `use balerix_server::PluginAddr;`, and replace `&stub.listen` everywhere with `&addr` where

```rust
    let addr = PluginAddr {
        listen: stub.listen.clone(),
        token: "tok".into(),
    };
```

Append to the test, after the metrics assertion:

```rust
    // §18.3: a call without the plugin's own token is refused by the plugin
    let f = fixture("activate-bad-token");
    let wrong = PluginAddr {
        listen: stub.listen.clone(),
        token: f["headers"]["authorization"]
            .as_str()
            .unwrap()
            .trim_start_matches("Bearer ")
            .to_string(),
    };
    let req: ActivateRequest = serde_json::from_value(f["request"].clone()).unwrap();
    let e = c.activate(&wrong, &req).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        format!(
            "HTTP {}: {}",
            f["status"].as_u64().unwrap(),
            f["response"]["error"].as_str().unwrap()
        )
    );
```

- [ ] **Step 8: Run the server tests**

Run: `mise x -- cargo test -p balerix-server`
Expected: PASS except `protocol_it` (the fixture `activate-bad-token.json` does not exist yet).

- [ ] **Step 9: Add the fixtures and the protocol doc lines**

Create `docs/plugin-protocol/activate-bad-token.json`:

```json
{
  "route": "POST /v1/activate",
  "direction": "daemon-to-plugin",
  "headers": { "authorization": "Bearer nope" },
  "request": { "agent": "payments/backend/bob", "config": { "initial": "working" } },
  "status": 401,
  "response": { "error": "bad daemon token" }
}
```

Add `"headers": { "authorization": "Bearer tok" },` as the line after `"direction"` in `activate.json`, `activate-rejected.json`, `events.json`, `intercept.json`, `health.json` and `metrics.json`.

In `docs/plugin-protocol.md`:
- §2, the `BALERIX_PLUGIN_TOKEN` row: append `It is also the bearer the daemon presents on every daemon → plugin call (§4); a plugin must check it and answer 401 \`{ "error": "bad daemon token" }\` to anything else (\`activate-bad-token.json\`), since its listener is a loopback port any local process can reach.`
- §4, after the first paragraph: `Every request carries \`Authorization: Bearer <BALERIX_PLUGIN_TOKEN>\` — the plugin's own token (§2). The fixtures' \`headers\` object is what the daemon sends; \`activate-bad-token.json\` records the refusal a plugin must answer.` Add a table row `| \`POST /v1/activate\`, wrong or missing bearer | same | \`{ error }\` | 401 | \`activate-bad-token.json\` |` after the `activate`, rejected row.
- §6: "fourteen fixtures" → "fifteen fixtures", and after the `{ route, direction, request, status, response }` description add: `daemon-to-plugin fixtures also carry \`headers\`, the request headers the daemon sends.`

Run: `mise x -- cargo test -p balerix-server --test protocol_it`
Expected: PASS.

- [ ] **Step 10: Write the failing SDK router test**

In `crates/balerix-plugin-sdk/src/plugin.rs` tests, change `defaults_accept_everything_and_pass_the_response_through` to start the server as `tokio::spawn(run(listener, Arc::new(Silent), "tok"));`, give `post` a token parameter — `async fn post(url: &str, token: &str, body: Value) -> (u16, Value)` sending `.bearer_auth(token)` — and call it with `"tok"`. Give the raw `reqwest` GETs `.bearer_auth("tok")`. Then add:

```rust
    #[tokio::test]
    async fn every_route_refuses_a_call_without_the_daemon_bearer() {
        let (listener, listen) = bind().await.unwrap();
        tokio::spawn(run(listener, Arc::new(Silent), "tok"));
        let base = format!("http://{listen}");
        for token in ["", "nope"] {
            let (s, v) = post(
                &format!("{base}/v1/activate"),
                token,
                json!({ "agent": "f/c/a", "config": {} }),
            )
            .await;
            assert_eq!((s, v), (401, json!({ "error": "bad daemon token" })), "{token:?}");
        }
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            c.get(format!("{base}/v1/health"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            401
        );
        assert_eq!(
            c.get(format!("{base}/v1/metrics"))
                .bearer_auth("tok")
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
    }
```

- [ ] **Step 11: Run it to see it fail**

Run: `mise x -- cargo test -p balerix-plugin-sdk plugin::tests`
Expected: compile error (`run` takes two arguments).

- [ ] **Step 12: Add `auth.rs` and the bearer check to the SDK router**

Create `crates/balerix-plugin-sdk/src/auth.rs`:

```rust
//! The bearer check every SDK route runs (plugins spec §18.3): the daemon
//! presents the plugin's own token. Small twins of the daemon's helpers —
//! the SDK depends on `balerix-api` only.

use axum::http::HeaderMap;

/// Length-then-bytes comparison with no early exit on the bytes.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b) {
        acc |= x ^ y;
    }
    acc == 0
}

/// The token of an `Authorization: Bearer <token>` header, trimmed.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get("authorization")?.to_str().ok()?;
    let (scheme, rest) = v.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let tok = rest.trim();
    (!tok.is_empty()).then_some(tok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn bearer_and_constant_time_eq_behave() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert("authorization", HeaderValue::from_static("Bearer  tok-1 "));
        assert_eq!(bearer(&h), Some("tok-1"));
        h.insert("authorization", HeaderValue::from_static("Basic x"));
        assert_eq!(bearer(&h), None);
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
```

In `crates/balerix-plugin-sdk/src/lib.rs` add `pub mod auth;` after `pub mod host;`.

In `crates/balerix-plugin-sdk/src/plugin.rs`, add imports `use axum::extract::Request; use axum::middleware::{self, Next};` and `use crate::auth::{bearer, constant_time_eq};`, then replace `router`:

```rust
/// The §4.2 router for `plugin`. Every route, the plugin's own under
/// `/v1/routes` included, needs `Authorization: Bearer <token>` — the
/// daemon presents the plugin's own token (plugins spec §18.3), because
/// the listener is a loopback port any local process can reach.
pub fn router<P: Plugin>(plugin: Arc<P>, token: &str) -> Router {
    let token: Arc<str> = Arc::from(token);
    Router::new()
        .route("/v1/activate", post(activate::<P>))
        .route("/v1/deactivate", post(deactivate::<P>))
        .route("/v1/events", post(events::<P>))
        .route("/v1/intercept", post(intercept::<P>))
        .route("/v1/health", get(health::<P>))
        .route("/v1/metrics", get(metrics::<P>))
        .with_state(plugin)
        .layer(middleware::from_fn_with_state(token, require_daemon_bearer))
        // daemon → plugin request bodies are capped at 1 MiB (plugin-protocol
        // §1), matching the daemon's own `plugin_api::router` layer
        // (`balerix-server/src/api.rs`); axum's default (2 MiB) is otherwise
        // silently more permissive than the spec promises.
        .layer(DefaultBodyLimit::max(1 << 20))
}

async fn require_daemon_bearer(
    State(token): State<Arc<str>>,
    req: Request,
    next: Next,
) -> Response {
    match bearer(req.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => error(StatusCode::UNAUTHORIZED, "bad daemon token"),
    }
}
```

`run` becomes `pub async fn run<P: Plugin>(listener: tokio::net::TcpListener, plugin: Arc<P>, token: &str) -> Result<(), SdkError>` serving `router(plugin, token)`; `serve_on` spawns `run(listener, plugin, &token)` with `let token = host.env().token.clone();` bound first (the spawned future needs an owned `String`: pass `token.clone()` into an `async move` block that calls `run(listener, plugin, &token).await`).

- [ ] **Step 13: Adapt the SDK harness, the conformance test, `events_it` and `fake-plugin`**

`crates/balerix-plugin-sdk/src/testing.rs`: `Harness` gains a `token: String` field set from `env.token.clone()` in `start`; `spawn` takes `token: &str` and calls `run(listener, Arc::new(plugin), token)` (bind the argument as `let token = token.to_string();` and spawn `async move { run(listener, plugin, &token).await }`); `restart` passes `&self.token`. Add `pub fn token(&self) -> &str { &self.token }`. `post`, `health` and `metrics` add `.bearer_auth(&self.token)` to their requests. The `the_harness_drives_every_route_through_the_wire` test is unchanged.

`crates/balerix-plugin-sdk/tests/conformance.rs`: the fixture count assertion becomes `15`; `tokio::spawn(run(listener, Arc::new(Reference::new()), "tok"));`; in the router replay, after `let req = match method {…};` add

```rust
        let mut req = req;
        if let Some(headers) = f["headers"].as_object() {
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str().unwrap());
            }
        }
```

The `activate-bad-token` fixture is then replayed like any other and its 401 body compared.

`crates/balerix-server/tests/events_it.rs` `start_flow`: build `env` first, then `tokio::spawn({ let plugin = plugin.clone(); let token = env.token.clone(); async move { run(listener, plugin, &token).await } });`, then `Host::new(env)` (which moves `env`). The `Good`/`bad` plugins in `plugin_metrics_are_re_exported_under_the_prefix_rule`: `Good` runs as `run(l1, Arc::new(Good(good)), &token(&w, "flow").await)` — bind the token in a `let` before the spawn; the raw `bad` router is served without a check and stays as it is.

`crates/balerix/src/commands/dev.rs` `fake_plugin_command`: bind `let token = env.token.clone();` before `Host::new(env)?` and spawn `run(listener, plugin, &token)` inside an `async move` block (`let server = tokio::spawn(async move { run(listener, plugin, &token).await });`).

- [ ] **Step 14: Run the whole check**

Run: `mise run check`
Expected: PASS (the e2e's `plugin_hello_journey`, `plugin_protocol_journey` and `flow_journey` still pass: every in-tree plugin runs on the SDK, which now checks the bearer the daemon now sends).

- [ ] **Step 15: Commit**

```bash
git add -A
git commit -m "Present the plugin's own token on every daemon -> plugin call

A plugin's listener is a loopback port any local process can reach, and
nothing authenticated the daemon to it: any process could activate,
intercept or, once routes exist, open a terminal. The daemon now sends
the plugin's own BALERIX_PLUGIN_TOKEN as the bearer on every call and
the SDK router refuses anything else with 401 (plugins spec §18.3). The
registry learns the token at hello, next to the listen address, and a
PluginAddr carries both with the token redacted from Debug.

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

---

### Task 2: Browser sessions — login codes, the cookie, the same-origin rule

**Files:**
- Create: `crates/balerix-server/src/sessions.rs`
- Modify: `crates/balerix-api/src/request.rs`, `crates/balerix-api/src/lib.rs`
- Modify: `crates/balerix-server/src/daemon.rs` (field, `sessions()`, `origin()`)
- Modify: `crates/balerix-server/src/api.rs` (two routes)
- Modify: `crates/balerix-server/src/lib.rs`
- Modify: `crates/balerix-server/tests/support/mod.rs` (`Api::raw`)
- Create: `crates/balerix-server/tests/browser_it.rs`

**Interfaces:**
- Consumes: `crate::vault::random_hex`, `crate::auth::constant_time_eq`, `Daemon` (`ports.hook_url` is the daemon's own origin).
- Produces:
  ```rust
  // balerix_api::request
  pub struct SessionRequest { pub to: Option<String> }        // deny_unknown_fields, default
  pub struct SessionResponse { pub login_url: String }
  // balerix_server::sessions
  pub const CODE_TTL: Duration;      // 60 s
  pub const SESSION_TTL: Duration;   // 12 h
  pub const COOKIE: &str;            // "balerix_session"
  pub const MOUNT_PREFIX: &str;      // "/v1/plugins/"
  pub struct Sessions;
  impl Sessions { pub fn new() -> Self; pub fn issue_code(&self) -> String; pub fn issue_code_at(&self, now: Instant) -> String;
                  pub fn redeem(&self, code: &str) -> Option<String>; pub fn redeem_at(&self, code: &str, now: Instant) -> Option<String>;
                  pub fn is_valid(&self, id: &str) -> bool; pub fn is_valid_at(&self, id: &str, now: Instant) -> bool }
  pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String>;
  pub fn set_cookie(id: &str) -> String;                     // "balerix_session=<id>; HttpOnly; SameSite=Strict; Path=/v1/plugins/"
  pub fn same_origin(headers: &HeaderMap, origin: &str) -> bool;
  pub fn login_target(to: Option<&str>) -> Option<String>;   // a path under MOUNT_PREFIX in [A-Za-z0-9/._~-]
  // balerix_server::Daemon
  pub fn sessions(&self) -> &Sessions;
  pub fn origin(&self) -> &str;                              // http://127.0.0.1:<port>
  // routes: POST /v1/sessions (admin) -> SessionResponse; GET /v1/login/{code}?to= -> 303 + Set-Cookie | 404 text | 400 text
  // tests/support: Api::raw(method, path, headers, body) -> (u16, Vec<(String, String)>, String)   (no redirects followed)
  ```

- [ ] **Step 1: Write the failing unit tests for `sessions.rs`**

Create `crates/balerix-server/src/sessions.rs` with the module doc and the test module only:

```rust
//! Browser sessions for the proxied plugin routes (plugins spec §18.2): a
//! one-time login code minted for the admin, exchanged by the browser for
//! an in-memory session cookie. The admin token never enters the browser;
//! nothing here is persisted, so sessions die with the daemon.

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::time::Duration;

    #[test]
    fn a_code_is_single_use_and_expires() {
        let s = Sessions::new();
        let t0 = Instant::now();
        let code = s.issue_code_at(t0);
        assert_eq!(code.len(), 64);
        assert_eq!(s.redeem_at("nope", t0), None);
        let id = s.redeem_at(&code, t0 + Duration::from_secs(30)).unwrap();
        assert_eq!(id.len(), 64);
        assert_ne!(id, code, "the session id is not the code");
        assert_eq!(s.redeem_at(&code, t0), None, "single use");
        let late = s.issue_code_at(t0);
        assert_eq!(
            s.redeem_at(&late, t0 + CODE_TTL + Duration::from_secs(1)),
            None,
            "expired"
        );
        assert!(s.is_valid_at(&id, t0 + Duration::from_secs(3600)));
        assert!(!s.is_valid_at("nope", t0));
        assert!(
            !s.is_valid_at(&id, t0 + SESSION_TTL + Duration::from_secs(1)),
            "sessions expire"
        );
    }

    #[test]
    fn the_cookie_is_found_among_others_and_rendered_with_its_flags() {
        let mut h = HeaderMap::new();
        assert_eq!(cookie_value(&h, COOKIE), None);
        h.insert(
            "cookie",
            HeaderValue::from_static("a=1; balerix_session=abc; b=2"),
        );
        assert_eq!(cookie_value(&h, COOKIE).as_deref(), Some("abc"));
        h.insert("cookie", HeaderValue::from_static("balerix_session_x=zzz"));
        assert_eq!(cookie_value(&h, COOKIE), None, "prefix is not a match");
        assert_eq!(
            set_cookie("abc"),
            "balerix_session=abc; HttpOnly; SameSite=Strict; Path=/v1/plugins/"
        );
    }

    #[test]
    fn same_origin_prefers_sec_fetch_site_then_origin() {
        let origin = "http://127.0.0.1:7643";
        let with = |pairs: &[(&str, &str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, HeaderValue::from_str(v).unwrap());
            }
            h
        };
        assert!(same_origin(&with(&[]), origin), "a navigation sends neither");
        assert!(same_origin(&with(&[("sec-fetch-site", "same-origin")]), origin));
        assert!(same_origin(&with(&[("sec-fetch-site", "none")]), origin));
        assert!(!same_origin(&with(&[("sec-fetch-site", "cross-site")]), origin));
        assert!(!same_origin(&with(&[("sec-fetch-site", "same-site")]), origin));
        assert!(same_origin(&with(&[("origin", origin)]), origin));
        assert!(same_origin(&with(&[("origin", "http://127.0.0.1:7643/")]), origin));
        assert!(!same_origin(&with(&[("origin", "http://localhost:7643")]), origin));
        assert!(!same_origin(&with(&[("origin", "http://evil.example")]), origin));
        assert!(
            !same_origin(
                &with(&[("sec-fetch-site", "cross-site"), ("origin", origin)]),
                origin
            ),
            "sec-fetch-site wins when present"
        );
    }

    #[test]
    fn a_login_may_only_land_under_the_mount() {
        assert_eq!(login_target(None).as_deref(), Some("/v1/plugins/"));
        assert_eq!(
            login_target(Some("/v1/plugins/web/")).as_deref(),
            Some("/v1/plugins/web/")
        );
        assert_eq!(
            login_target(Some("/v1/plugins/web/agents/f/c/a")).as_deref(),
            Some("/v1/plugins/web/agents/f/c/a")
        );
        for bad in [
            "/v1/fleets",
            "http://evil.example/v1/plugins/",
            "//evil.example/v1/plugins/",
            "/v1/plugins/web/?x=1",
            "/v1/plugins/web/\r\nSet-Cookie: x",
            "/v1/plugins",
            "",
        ] {
            assert_eq!(login_target(Some(bad)), None, "{bad:?}");
        }
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Add `pub mod sessions;` to `crates/balerix-server/src/lib.rs` (alphabetically, after `plugins`), then run: `mise x -- cargo test -p balerix-server sessions`
Expected: compile errors (nothing is defined).

- [ ] **Step 3: Implement `sessions.rs`**

Above the test module in `crates/balerix-server/src/sessions.rs`:

```rust
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

use crate::auth::constant_time_eq;
use crate::vault::random_hex;

/// How long a login code may sit unused.
pub const CODE_TTL: Duration = Duration::from_secs(60);
/// How long a browser session lives.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
pub const COOKIE: &str = "balerix_session";
/// Every proxied path starts here; a login may only redirect below it, and
/// the cookie is scoped to it.
pub const MOUNT_PREFIX: &str = "/v1/plugins/";

#[derive(Default)]
struct Inner {
    /// code → expiry
    codes: HashMap<String, Instant>,
    /// session id → expiry
    sessions: HashMap<String, Instant>,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<Inner>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn prune(inner: &mut Inner, now: Instant) {
        inner.codes.retain(|_, expiry| *expiry > now);
        inner.sessions.retain(|_, expiry| *expiry > now);
    }

    /// A fresh single-use code, 32 random bytes as hex, good for `CODE_TTL`.
    pub fn issue_code(&self) -> String {
        self.issue_code_at(Instant::now())
    }

    pub fn issue_code_at(&self, now: Instant) -> String {
        let mut inner = self.lock();
        Self::prune(&mut inner, now);
        let code = random_hex(32);
        inner.codes.insert(code.clone(), now + CODE_TTL);
        code
    }

    /// Exchanges a live code for a session id; the code is gone either way.
    pub fn redeem(&self, code: &str) -> Option<String> {
        self.redeem_at(code, Instant::now())
    }

    pub fn redeem_at(&self, code: &str, now: Instant) -> Option<String> {
        let mut inner = self.lock();
        Self::prune(&mut inner, now);
        // Few codes, compared in constant time each: a lookup by key would
        // leak through timing what a guesser is after.
        let key = inner
            .codes
            .keys()
            .find(|k| constant_time_eq(k.as_bytes(), code.as_bytes()))
            .cloned()?;
        inner.codes.remove(&key);
        let id = random_hex(32);
        inner.sessions.insert(id.clone(), now + SESSION_TTL);
        Some(id)
    }

    pub fn is_valid(&self, id: &str) -> bool {
        self.is_valid_at(id, Instant::now())
    }

    pub fn is_valid_at(&self, id: &str, now: Instant) -> bool {
        let mut inner = self.lock();
        Self::prune(&mut inner, now);
        inner
            .sessions
            .keys()
            .any(|k| constant_time_eq(k.as_bytes(), id.as_bytes()))
    }
}

/// The value of cookie `name` across every `Cookie` header, if any.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k.trim() == name).then(|| v.trim().to_string())
        })
}

/// The `Set-Cookie` value: HttpOnly, never cross-site, scoped to the mount.
pub fn set_cookie(id: &str) -> String {
    format!("{COOKIE}={id}; HttpOnly; SameSite=Strict; Path={MOUNT_PREFIX}")
}

/// Whether a cookie-authenticated request came from the daemon's own
/// origin (§18.2): `Sec-Fetch-Site` when the browser sends it (every
/// current one does), else `Origin` — absent on a plain navigation, and
/// exactly `http://127.0.0.1:<port>` on a fetch or WebSocket from a page
/// the daemon served (`localhost` is another origin to a browser).
pub fn same_origin(headers: &HeaderMap, origin: &str) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return matches!(site.trim(), "same-origin" | "none");
    }
    match headers.get("origin").and_then(|v| v.to_str().ok()) {
        None => true,
        Some(o) => o.trim().trim_end_matches('/') == origin,
    }
}

/// Where a login may send the browser: a path under the mount made of
/// `[A-Za-z0-9/._~-]`, so it needs no encoding in a URL and cannot smuggle
/// a query, a fragment, a header or another host. `None` for anything else.
pub fn login_target(to: Option<&str>) -> Option<String> {
    let to = to.unwrap_or(MOUNT_PREFIX);
    let charset_ok = to
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'~' | b'-'));
    (to.starts_with(MOUNT_PREFIX) && charset_ok).then(|| to.to_string())
}
```

Run: `mise x -- cargo test -p balerix-server sessions`
Expected: PASS (4 tests).

- [ ] **Step 4: The wire types and the daemon accessors**

In `crates/balerix-api/src/request.rs` append:

```rust
/// Body of `POST /v1/sessions` (plugins spec §18.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionRequest {
    /// Where the login redirects: a path under `/v1/plugins/`; the mount
    /// root when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResponse {
    /// `http://127.0.0.1:<port>/v1/login/<code>?to=<path>`, single use.
    pub login_url: String,
}
```

(with a round-trip test in that file's `tests` module: `SessionRequest { to: None }` serializes to `{}` and `{ "to": "/v1/plugins/web/" }` parses; an unknown field is rejected). In `crates/balerix-api/src/lib.rs` extend the `request` re-export to `pub use request::{DownQuery, ErrorBody, FleetRequest, SessionRequest, SessionResponse};`.

In `crates/balerix-server/src/daemon.rs`: add `use crate::sessions::Sessions;`, the field `sessions: Sessions,` (after `kv`), initialise it in `start` with `sessions: Sessions::new(),`, and the accessors after `kv()`:

```rust
    pub fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// The daemon's own origin, `http://127.0.0.1:<port>`: the login URL's
    /// host and the only `Origin` a cookie request may carry (§18.2).
    pub fn origin(&self) -> &str {
        &self.ports.hook_url
    }
```

- [ ] **Step 5: The two routes**

In `crates/balerix-server/src/api.rs`: add `use axum::http::header::{LOCATION, SET_COOKIE};`, `use balerix_api::{SessionRequest, SessionResponse};` (extend the existing `balerix_api` import), `use serde::Deserialize;`, and `use crate::sessions::{MOUNT_PREFIX, login_target, set_cookie};`. In `router`, add `.route("/v1/sessions", post(create_session))` to the `admin` router (before `route_layer`) and `.route("/v1/login/{code}", get(login))` to the top-level `Router::new()` chain after `/metrics`. Then the handlers:

```rust
/// `POST /v1/sessions`: a single-use login URL for the admin's browser
/// (plugins spec §18.2). The admin token itself never enters the browser.
async fn create_session(
    State(state): State<AppState>,
    b: Result<Json<SessionRequest>, JsonRejection>,
) -> Result<Json<SessionResponse>, ApiError> {
    let req = b
        .map(|Json(r)| r)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    let to = login_target(req.to.as_deref()).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("to: must be a path under {MOUNT_PREFIX}"),
        )
    })?;
    let code = state.daemon.sessions().issue_code();
    Ok(Json(SessionResponse {
        login_url: format!("{}/v1/login/{code}?to={to}", state.daemon.origin()),
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LoginQuery {
    to: Option<String>,
}

/// `GET /v1/login/{code}`: the browser's end. A live code becomes a session
/// cookie and a 303 to `to`; anything else is a plain-text 404 — the page
/// is for a human who pasted a stale URL, not for a client parsing JSON.
async fn login(
    State(state): State<AppState>,
    code: Result<Path<String>, PathRejection>,
    q: Result<Query<LoginQuery>, QueryRejection>,
) -> Response {
    let to = match q {
        Ok(Query(q)) => match login_target(q.to.as_deref()) {
            Some(to) => to,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("to: must be a path under {MOUNT_PREFIX}"),
                )
                    .into_response();
            }
        },
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    let Ok(Path(code)) = code else {
        return (StatusCode::NOT_FOUND, "unknown or expired login code").into_response();
    };
    match state.daemon.sessions().redeem(&code) {
        Some(id) => (
            StatusCode::SEE_OTHER,
            [(LOCATION, to), (SET_COOKIE, set_cookie(&id))],
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "unknown or expired login code").into_response(),
    }
}
```

- [ ] **Step 6: `Api::raw` in the test support and the failing integration test**

In `crates/balerix-server/tests/support/mod.rs`, add to `impl Api` (the agent must not follow redirects for this; build a second agent):

```rust
    /// A request with explicit headers, redirects not followed, answered
    /// as status, response headers and body text — for the login and
    /// proxy paths, where the headers are the point.
    pub fn raw(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> (u16, Vec<(String, String)>, String) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        let url = format!("{}{path}", self.base);
        let mut req = match method {
            "GET" => agent.get(&url).force_send_body(),
            "POST" => agent.post(&url),
            _ => unreachable!(),
        };
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut resp = match body {
            Some(b) => req.send(b).unwrap(),
            None => req.send_empty().unwrap(),
        };
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let text = resp.body_mut().read_to_string().unwrap();
        (status, headers, text)
    }

    pub fn token(&self) -> &str {
        &self.token
    }
```

Create `crates/balerix-server/tests/browser_it.rs`:

```rust
//! Plugins spec §18.2 through a real listener: login codes, the session
//! cookie, the same-origin rule, and (Task 3) the proxied mount.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use support::world;

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_code_becomes_a_cookie_once() {
    let w = world().await;
    let (s, v) = w.api.admin("POST", "/v1/sessions", Some(&json!({ "to": "/v1/plugins/web/" })));
    assert_eq!(s, 200, "{v}");
    let url = v["login_url"].as_str().unwrap().to_string();
    assert!(
        url.starts_with(&format!("{}/v1/login/", w.daemon.origin())),
        "{url}"
    );
    assert!(url.ends_with("?to=/v1/plugins/web/"), "{url}");
    let path = url.trim_start_matches(w.daemon.origin()).to_string();
    let (s, _) = w.api.call("POST", "/v1/sessions", None, Some(&json!({})));
    assert_eq!(s, 401, "admin only");
    let (s, v) = w.api.admin("POST", "/v1/sessions", Some(&json!({ "to": "/v1/fleets" })));
    assert_eq!((s, v["error"].as_str().unwrap()), (400, "to: must be a path under /v1/plugins/"));

    let (s, headers, _) = w.api.raw("GET", &path, &[], None);
    assert_eq!(s, 303);
    assert_eq!(header(&headers, "location"), Some("/v1/plugins/web/"));
    let cookie = header(&headers, "set-cookie").unwrap().to_string();
    assert!(
        cookie.starts_with("balerix_session=")
            && cookie.ends_with("; HttpOnly; SameSite=Strict; Path=/v1/plugins/"),
        "{cookie}"
    );
    let id = cookie.split(';').next().unwrap().trim_start_matches("balerix_session=");
    assert_eq!(id.len(), 64);
    assert!(w.daemon.sessions().is_valid(id));

    let (s, _, text) = w.api.raw("GET", &path, &[], None);
    assert_eq!((s, text.as_str()), (404, "unknown or expired login code"), "single use");
    let (s, _, _) = w.api.raw("GET", "/v1/login/nope", &[], None);
    assert_eq!(s, 404);
    let (s, _, text) = w.api.raw("GET", "/v1/login/nope?to=/v1/fleets", &[], None);
    assert_eq!((s, text.as_str()), (400, "to: must be a path under /v1/plugins/"));
}
```

- [ ] **Step 7: Run it**

Run: `mise x -- cargo test -p balerix-server --test browser_it`
Expected: PASS.

- [ ] **Step 8: Run the check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A
git commit -m "Mint single-use login codes that a browser exchanges for a session cookie

A browser cannot send the admin bearer on a navigation, and the phase 3
web plugin is a browser page (plugins spec §18.2). POST /v1/sessions
hands the admin a 60-second single-use code; GET /v1/login/<code> turns
it into an in-memory 12-hour session and an HttpOnly, SameSite=Strict
cookie scoped to /v1/plugins/. The admin token never reaches the browser
and nothing is persisted. The proxy that accepts the cookie is next.

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

---

### Task 3: The proxy mount and `balerix plugin open`

**Files:**
- Modify: `Cargo.toml` (axum `ws`; hyper, hyper-util, http-body-util, futures-util, tokio-tungstenite)
- Modify: `crates/balerix-server/Cargo.toml`
- Create: `crates/balerix-server/src/proxy.rs`
- Modify: `crates/balerix-server/src/daemon.rs` (`proxy_client`), `src/api.rs` (the mount), `src/metrics.rs`, `src/lib.rs`
- Modify: `crates/balerix-server/tests/support/mod.rs` (web package `routes: true`)
- Modify: `crates/balerix-server/tests/browser_it.rs`
- Modify: `crates/balerix/src/cli.rs`, `src/client.rs`, `src/commands/plugin.rs`, `src/main.rs`
- Modify: `docs/plugin-protocol.md` (new §4.1 "Routes")

**Interfaces:**
- Consumes: `PluginAddr`, `PluginRegistry::{plugin, ready_addr}`, `Sessions`, `bearer`, `constant_time_eq`.
- Produces:
  ```rust
  // balerix_server::proxy
  pub type HttpClient = hyper_util::client::legacy::Client<HttpConnector, axum::body::Body>;
  pub const MAX_BODY: usize;                       // 1 << 20
  pub const FORWARDED_PREFIX: &str;                // "x-balerix-forwarded-prefix"
  pub fn client() -> HttpClient;
  pub fn forwarded_headers(src: &HeaderMap) -> HeaderMap;             // request side
  pub fn response_headers(src: &HeaderMap, upgraded: bool) -> HeaderMap;
  pub fn upstream_uri(listen: &str, rest: &str, query: Option<&str>) -> Result<Uri, String>;
  pub async fn forward(client: &HttpClient, addr: &PluginAddr, name: &str, rest: &str, req: Request) -> Response;
  // balerix_server::Daemon
  pub fn proxy_client(&self) -> &HttpClient;
  // balerix_server::Metrics
  pub fn proxy_request(&self, plugin: &str, status: u16);            // balerix_plugin_proxy_requests_total{plugin,status}
  // routes: ANY /v1/plugins/{name}/ and ANY /v1/plugins/{name}/{*rest}
  // balerix CLI: `balerix plugin open <name> [--api-url]` prints the login URL; Client::create_session(&self, to: &str) -> Result<String>
  ```

- [ ] **Step 1: The dependencies**

In `Cargo.toml` `[workspace.dependencies]`: change `axum = "0.8.9"` to `axum = { version = "0.8.9", features = ["ws"] }` and add, after `reqwest`:

```toml
# Phase 3 (plugins spec §18.1). hyper, hyper-util, http-body-util and
# futures-util were already in the lock through axum and reqwest and are
# direct, exact dependencies now: the proxy is a plain reverse proxy on
# hyper's legacy client (upgrades passed through as bytes), and the SDK's
# stream clients need Sink/Stream adaptors. tokio-tungstenite (no TLS
# feature, P3-1) is the WebSocket client for the SDK and the tests.
hyper = { version = "1.11.1", features = ["http1", "client"] }
hyper-util = { version = "0.1.20", features = ["client-legacy", "http1", "tokio"] }
http-body-util = "0.1.5"
futures-util = { version = "0.3.34", default-features = false, features = ["sink", "std"] }
tokio-tungstenite = "0.30.0"
```

In `crates/balerix-server/Cargo.toml` add to `[dependencies]`: `hyper = { workspace = true }`, `hyper-util = { workspace = true }`, `http-body-util = { workspace = true }`; and to `[dev-dependencies]`: `tokio-tungstenite = { workspace = true }`, `futures-util = { workspace = true }`.

- [ ] **Step 2: Write the failing unit tests for `proxy.rs`**

Create `crates/balerix-server/src/proxy.rs` with the doc comment and tests:

```rust
//! The reverse-proxied plugin mount (plugins spec §6, §18.1, §18.2):
//! `/v1/plugins/<name>/…` → `http://<listen>/v1/routes/…` on hyper's
//! legacy client. Bodies in are capped, bodies out stream, the daemon's
//! own credentials and the hop-by-hop headers never cross, and a 101 is
//! upgraded on both sides and copied byte for byte — the proxy never
//! parses a WebSocket frame.

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use proptest::prelude::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn credentials_host_and_hop_by_hop_headers_never_cross() {
        let src = headers(&[
            ("authorization", "Bearer admin"),
            ("cookie", "balerix_session=x"),
            ("host", "127.0.0.1:7643"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "x"),
            ("te", "trailers"),
            ("trailer", "x"),
            ("transfer-encoding", "chunked"),
            ("connection", "keep-alive"),
            ("upgrade", "h2c"),
            ("accept", "text/html"),
            ("x-custom", "1"),
        ]);
        let out = forwarded_headers(&headers(
            &src.iter()
                .filter(|(k, _)| k.as_str() != "upgrade")
                .map(|(k, v)| (k.as_str(), v.to_str().unwrap()))
                .collect::<Vec<_>>(),
        ));
        let names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["accept", "x-custom"], "no upgrade: connection dropped too");
        let out = forwarded_headers(&src);
        let mut names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["accept", "connection", "upgrade", "x-custom"],
            "an upgrade keeps its two hop-by-hop headers"
        );
        let resp = response_headers(&headers(&[("set-cookie", "a=1"), ("transfer-encoding", "chunked"), ("connection", "close"), ("content-type", "text/html")]), false);
        let mut names: Vec<&str> = resp.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["content-type", "set-cookie"]);
        let resp = response_headers(&headers(&[("connection", "Upgrade"), ("upgrade", "websocket"), ("sec-websocket-accept", "x")]), true);
        assert_eq!(resp.len(), 3, "a 101 keeps its upgrade headers");
    }

    #[test]
    fn the_upstream_uri_lands_under_v1_routes() {
        assert_eq!(
            upstream_uri("127.0.0.1:4000", "", None).unwrap().to_string(),
            "http://127.0.0.1:4000/v1/routes"
        );
        assert_eq!(
            upstream_uri("127.0.0.1:4000", "agents/f/c/a/ws", Some("cols=80"))
                .unwrap()
                .to_string(),
            "http://127.0.0.1:4000/v1/routes/agents/f/c/a/ws?cols=80"
        );
        assert!(upstream_uri("127.0.0.1:4000", "a b", None).is_err());
    }

    proptest! {
        /// Whatever comes in, the daemon's credentials and the hop-by-hop
        /// set never go out, and connection/upgrade go out only together
        /// with an upgrade.
        #[test]
        fn forwarded_headers_never_leak(
            names in proptest::collection::vec("[a-z-]{1,12}", 0..12),
            upgrade in proptest::bool::ANY,
        ) {
            let mut pairs: Vec<(String, String)> = names.iter().map(|n| (n.clone(), "v".to_string())).collect();
            for n in ["authorization", "cookie", "host", "connection", "te", "transfer-encoding"] {
                pairs.push((n.to_string(), "v".to_string()));
            }
            if upgrade {
                pairs.push(("upgrade".to_string(), "websocket".to_string()));
            }
            let src = headers(&pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>());
            let out = forwarded_headers(&src);
            for k in out.keys() {
                prop_assert!(!DROPPED.contains(&k.as_str()), "{k}");
            }
            prop_assert_eq!(out.contains_key("connection"), upgrade);
            prop_assert_eq!(out.contains_key("upgrade"), upgrade);
        }
    }
}
```

- [ ] **Step 3: Run them to see them fail**

Add `pub mod proxy;` to `crates/balerix-server/src/lib.rs`, then run: `mise x -- cargo test -p balerix-server proxy`
Expected: compile errors.

- [ ] **Step 4: Implement `proxy.rs`**

Above the tests:

```rust
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::api::ApiError;
use crate::plugins::PluginAddr;

pub type HttpClient = Client<HttpConnector, Body>;

/// Proxied request bodies are capped like every other body.
pub const MAX_BODY: usize = 1 << 20;
/// Tells a plugin where it is mounted, so it can build links.
pub const FORWARDED_PREFIX: &str = "x-balerix-forwarded-prefix";

/// Never forwarded on a request: the daemon's own credentials, the host
/// (hyper sets it from the upstream URI), and the hop-by-hop set of RFC
/// 9110 §7.6.1 — `connection` and `upgrade` excepted on an upgrade
/// request, which is exactly what they are for.
pub(crate) const DROPPED: [&str; 9] = [
    "authorization",
    "cookie",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Never forwarded on a response; `connection`/`upgrade` stay on a 101.
const DROPPED_RESPONSE: [&str; 6] = [
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

pub fn client() -> HttpClient {
    Client::builder(TokioExecutor::new()).build_http()
}

pub fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
}

fn filtered(src: &HeaderMap, dropped: &[&str], keep_upgrade: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src {
        let name = k.as_str();
        if dropped.contains(&name) {
            continue;
        }
        if !keep_upgrade && (name == "connection" || name == "upgrade") {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    out
}

pub fn forwarded_headers(src: &HeaderMap) -> HeaderMap {
    filtered(src, &DROPPED, is_upgrade(src))
}

pub fn response_headers(src: &HeaderMap, upgraded: bool) -> HeaderMap {
    filtered(src, &DROPPED_RESPONSE, upgraded)
}

/// `http://<listen>/v1/routes` for an empty `rest` (axum answers a nested
/// router's `/` there, not at `/v1/routes/`), else `/v1/routes/<rest>`,
/// with the query string as it came.
pub fn upstream_uri(listen: &str, rest: &str, query: Option<&str>) -> Result<Uri, String> {
    let path = if rest.is_empty() {
        "/v1/routes".to_string()
    } else {
        format!("/v1/routes/{rest}")
    };
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    format!("http://{listen}{path}{query}")
        .parse()
        .map_err(|e: axum::http::uri::InvalidUri| e.to_string())
}

/// Forwards one request to the plugin and answers with its response. On a
/// 101 both sides are upgraded and copied until either closes; the copy
/// runs on its own task, the 101 goes back to the client at once.
pub async fn forward(
    client: &HttpClient,
    addr: &PluginAddr,
    name: &str,
    rest: &str,
    mut req: Request,
) -> Response {
    // Taken before the request is consumed: hyper stores the client
    // side's upgrade handle in the request's extensions.
    let downstream = hyper::upgrade::on(&mut req);
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "body exceeds 1 MiB").into_response();
        }
    };
    let uri = match upstream_uri(&addr.listen, rest, parts.uri.query()) {
        Ok(u) => u,
        Err(e) => return ApiError::new(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let mut headers = forwarded_headers(&parts.headers);
    let prefix = format!("/v1/plugins/{name}");
    let (Ok(prefix), Ok(bearer)) = (
        HeaderValue::from_str(&prefix),
        HeaderValue::from_str(&format!("Bearer {}", addr.token)),
    ) else {
        return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "proxy: bad header value").into_response();
    };
    headers.insert(HeaderName::from_static(FORWARDED_PREFIX), prefix);
    headers.insert(header::AUTHORIZATION, bearer);
    let mut upstream = match Request::builder()
        .method(parts.method)
        .uri(uri)
        .body(Body::from(bytes))
    {
        Ok(r) => r,
        Err(e) => return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("proxy: {e}")).into_response(),
    };
    *upstream.headers_mut() = headers;
    let mut resp = match client.request(upstream).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(plugin = %name, "proxy: {e}");
            return ApiError::new(StatusCode::BAD_GATEWAY, format!("plugin {name:?}: {e}")).into_response();
        }
    };
    let upgraded = resp.status() == StatusCode::SWITCHING_PROTOCOLS;
    if upgraded {
        let upstream = hyper::upgrade::on(&mut resp);
        let plugin = name.to_string();
        tokio::spawn(async move {
            match (downstream.await, upstream.await) {
                (Ok(a), Ok(b)) => {
                    let (mut a, mut b) = (TokioIo::new(a), TokioIo::new(b));
                    if let Err(e) = tokio::io::copy_bidirectional(&mut a, &mut b).await {
                        tracing::debug!(plugin = %plugin, "proxied stream ended: {e}");
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    tracing::debug!(plugin = %plugin, "upgrade failed: {e}");
                }
            }
        });
    }
    let (mut parts, body) = resp.into_parts();
    parts.headers = response_headers(&parts.headers, upgraded);
    Response::from_parts(parts, Body::new(body))
}
```

`ApiError::new` is `pub` already; `ApiError` needs `pub use api::ApiError` (it is re-exported). Run: `mise x -- cargo test -p balerix-server proxy`
Expected: PASS (3 tests).

- [ ] **Step 5: The metric, the daemon's client, and the mount**

`crates/balerix-server/src/metrics.rs`: add the field `proxy_requests: IntCounterVec,` to `Inner`, register it in `new` as

```rust
        let proxy_requests = IntCounterVec::new(
            Opts::new(
                "balerix_plugin_proxy_requests_total",
                "Requests proxied to plugin routes, by response status",
            ),
            &["plugin", "status"],
        )?;
```

(added to the `for c in […]` registration list and the `Inner { … }` literal), with the method

```rust
    pub fn proxy_request(&self, plugin: &str, status: u16) {
        self.inner
            .proxy_requests
            .with_label_values(&[plugin, &status.to_string()])
            .inc();
    }
```

and in the metrics test add `m.proxy_request("web", 200);` plus the assertion `text.contains("balerix_plugin_proxy_requests_total{plugin=\"web\",status=\"200\"} 1")` and `"balerix_plugin_proxy_requests_total"` in the `# TYPE` list.

`crates/balerix-server/src/daemon.rs`: field `proxy_client: crate::proxy::HttpClient,` initialised with `proxy_client: crate::proxy::client(),` and

```rust
    pub fn proxy_client(&self) -> &crate::proxy::HttpClient {
        &self.proxy_client
    }
```

`crates/balerix-server/src/api.rs`: imports `use axum::routing::any;`, `use balerix_core::AgentName;` (already), `use crate::proxy;`, `use crate::sessions::{COOKIE, cookie_value, same_origin};`. In `router`, add a fourth sub-router merged like the others:

```rust
    // The plugin mount authenticates itself (bearer or session cookie),
    // so it sits outside the admin middleware. `/v1/plugins/{name}` with
    // no slash stays the purge route.
    let mount = Router::new()
        .route("/v1/plugins/{name}/", any(proxy_root))
        .route("/v1/plugins/{name}/{*rest}", any(proxy_rest));
```

and `.merge(mount)` after `.merge(plugin_host)`. The handlers:

```rust
async fn proxy_root(
    State(state): State<AppState>,
    Path(name): Path<String>,
    req: Request,
) -> Response {
    proxied(&state, &name, "", req).await
}

async fn proxy_rest(
    State(state): State<AppState>,
    Path((name, rest)): Path<(String, String)>,
    req: Request,
) -> Response {
    proxied(&state, &name, &rest, req).await
}

/// The mount (plugins spec §6, §18.2): authenticate, resolve the plugin,
/// forward, count.
async fn proxied(state: &AppState, name: &str, rest: &str, req: Request) -> Response {
    let resp = proxy_inner(state, name, rest, req)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    // an unparseable name is one label, not one per guess
    let label = name.parse::<AgentName>().map_or("unknown".to_string(), |n| n.to_string());
    state.daemon.metrics().proxy_request(&label, resp.status().as_u16());
    resp
}

async fn proxy_inner(
    state: &AppState,
    name: &str,
    rest: &str,
    req: Request,
) -> Result<Response, ApiError> {
    authenticate_browser_or_admin(state, req.headers())?;
    let no_routes = || ApiError::new(StatusCode::NOT_FOUND, format!("plugin {name:?} has no routes"));
    let plugin: AgentName = name.parse().map_err(|_: NameError| no_routes())?;
    let registry = state.daemon.registry();
    if !registry.plugin(&plugin).is_some_and(|p| p.manifest.routes) {
        return Err(no_routes());
    }
    let addr = registry.ready_addr(&plugin).ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, format!("plugin {name:?} is not ready"))
    })?;
    Ok(proxy::forward(state.daemon.proxy_client(), &addr, name, rest, req).await)
}

/// The admin bearer, or a live session cookie on a same-origin request
/// (§18.2). A bearer that is present but wrong is refused outright; the
/// cookie is never consulted then.
fn authenticate_browser_or_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid admin token");
    if let Some(t) = bearer(headers) {
        return if constant_time_eq(t.as_bytes(), state.daemon.token().as_bytes()) {
            Ok(())
        } else {
            Err(unauthorized())
        };
    }
    match cookie_value(headers, COOKIE) {
        Some(id) if state.daemon.sessions().is_valid(&id) => {
            if same_origin(headers, state.daemon.origin()) {
                Ok(())
            } else {
                Err(ApiError::new(StatusCode::FORBIDDEN, "cross-origin request refused"))
            }
        }
        _ => Err(unauthorized()),
    }
}
```

- [ ] **Step 6: The failing integration test through a plugin with routes**

In `crates/balerix-server/tests/support/mod.rs` change the `web` package's manifest extra to `"hooks: { observe: [SessionStart] }\nneeds: [fleets, attach]\nroutes: true\n"` and update the doc comment.

Append to `crates/balerix-server/tests/browser_it.rs` (add the imports `use std::collections::BTreeMap; use std::sync::Arc; use axum::extract::ws::{Message, WebSocketUpgrade}; use axum::http::HeaderMap; use axum::response::IntoResponse; use axum::routing::{get, post}; use axum::{Router, body::Bytes}; use futures_util::{SinkExt, StreamExt}; use balerix_core::plugin_id; use balerix_plugin_sdk::{Env, Host}; use tokio_tungstenite::connect_async; use tokio_tungstenite::tungstenite::client::IntoClientRequest;` and `use support::World;`):

```rust
async fn token(w: &World, plugin: &str) -> String {
    w.daemon
        .hook_secret(&plugin_id(&plugin.parse().unwrap()))
        .await
        .unwrap()
}

/// A plugin with routes, outside the SDK (the SDK's `routes` is Task 6):
/// its root echoes the prefix and the bearer it was given, `headers`
/// dumps what arrived, `post` answers the body length, `echo` is a
/// WebSocket echo. Every route demands `expect` as the bearer.
async fn routes_plugin(expect: String) -> String {
    let expect = Arc::new(expect);
    let check = {
        let expect = expect.clone();
        move |headers: &HeaderMap| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == format!("Bearer {expect}"))
        }
    };
    let c1 = check.clone();
    let c2 = check.clone();
    let c3 = check.clone();
    let app = Router::new()
        .route(
            "/v1/routes",
            get(move |headers: HeaderMap| async move {
                if !c1(&headers) {
                    return (axum::http::StatusCode::UNAUTHORIZED, String::new());
                }
                let prefix = headers
                    .get("x-balerix-forwarded-prefix")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-");
                (axum::http::StatusCode::OK, format!("root prefix={prefix}"))
            }),
        )
        .route(
            "/v1/routes/headers",
            get(move |headers: HeaderMap| async move {
                let map: BTreeMap<String, String> = headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                serde_json::to_string(&map).unwrap()
            }),
        )
        .route(
            "/v1/routes/post",
            post(move |headers: HeaderMap, body: Bytes| async move {
                if !c2(&headers) {
                    return (axum::http::StatusCode::UNAUTHORIZED, String::new());
                }
                (axum::http::StatusCode::OK, body.len().to_string())
            }),
        )
        .route(
            "/v1/routes/echo",
            get(move |headers: HeaderMap, ws: WebSocketUpgrade| async move {
                if !c3(&headers) {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(msg)) = socket.recv().await {
                        if let Message::Binary(b) = msg {
                            let _ = socket.send(Message::Binary(b)).await;
                        }
                    }
                })
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listen = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    listen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_proxies_plain_requests_and_websockets_and_filters_headers() {
    let w = world().await;
    let admin = [("Authorization", format!("Bearer {}", w.api.token()))];
    let admin: Vec<(&str, &str)> = admin.iter().map(|(k, v)| (*k, v.as_str())).collect();

    // before hello: 503; unknown or routeless: 404; no auth: 401
    let (s, _, text) = w.api.raw("GET", "/v1/plugins/web/", &admin, None);
    assert_eq!((s, text.as_str()), (503, "{\"error\":\"plugin \\\"web\\\" is not ready\"}"));
    let (s, _, text) = w.api.raw("GET", "/v1/plugins/flow/", &admin, None);
    assert_eq!((s, text.as_str()), (404, "{\"error\":\"plugin \\\"flow\\\" has no routes\"}"));
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/nope/x", &admin, None);
    assert_eq!(s, 404);
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/web/", &[], None);
    assert_eq!(s, 401);
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/web/", &[("Authorization", "Bearer wrong")], None);
    assert_eq!(s, 401);

    let web = token(&w, "web").await;
    let listen = routes_plugin(web.clone()).await;
    Host::new(Env {
        api_url: w.api.base.clone(),
        name: "web".into(),
        token: web.clone(),
        scratch: w.dir.path().join("s"),
    })
    .unwrap()
    .hello("0.1.0", &listen)
    .await
    .unwrap();

    let (s, _, text) = w.api.raw("GET", "/v1/plugins/web/", &admin, None);
    assert_eq!((s, text.as_str()), (200, "root prefix=/v1/plugins/web"), "the root maps to /v1/routes and the bearer is the plugin's own");
    let mut with_junk = admin.clone();
    with_junk.push(("Cookie", "balerix_session=stolen"));
    with_junk.push(("X-Custom", "1"));
    with_junk.push(("Connection", "keep-alive"));
    let (s, _, text) = w.api.raw("GET", "/v1/plugins/web/headers?q=1", &with_junk, None);
    assert_eq!(s, 200);
    let seen: BTreeMap<String, String> = serde_json::from_str(&text).unwrap();
    assert_eq!(seen.get("x-custom").map(String::as_str), Some("1"));
    assert_eq!(seen.get("x-balerix-forwarded-prefix").map(String::as_str), Some("/v1/plugins/web"));
    assert_eq!(seen.get("authorization").map(String::as_str), Some(format!("Bearer {web}").as_str()), "the plugin's token, not the admin's");
    assert!(!seen.contains_key("cookie"), "{seen:?}");
    assert!(!seen.contains_key("connection"), "{seen:?}");
    assert_eq!(seen.get("host").map(|h| h == &listen), Some(true), "host is the upstream's");

    let (s, _, text) = w.api.raw("POST", "/v1/plugins/web/post", &admin, Some(&[b'x'; 100][..]));
    assert_eq!((s, text.as_str()), (200, "100"));
    let big = vec![b'x'; (1 << 20) + 1];
    let (s, _, _) = w.api.raw("POST", "/v1/plugins/web/post", &admin, Some(&big[..]));
    assert_eq!(s, 413);

    // a WebSocket through the mount: upgraded on both sides, bytes copied
    let url = format!("{}/v1/plugins/web/echo", w.api.base.replacen("http://", "ws://", 1));
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {}", w.api.token()).parse().unwrap());
    let (mut ws, _) = connect_async(req).await.expect("spec §11.1: the upgrade passthrough on hyper-util's legacy client");
    ws.send(tokio_tungstenite::tungstenite::Message::Binary(b"ping".to_vec().into())).await.unwrap();
    let echo = ws.next().await.unwrap().unwrap();
    assert_eq!(echo.into_data().as_ref(), b"ping");
    ws.close(None).await.unwrap();
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert("authorization", "Bearer wrong".parse().unwrap());
    let e = connect_async(req).await.unwrap_err();
    assert!(matches!(e, tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 401), "{e}");

    // a session cookie works like the bearer, from the daemon's origin only
    let (_, v) = w.api.admin("POST", "/v1/sessions", Some(&json!({ "to": "/v1/plugins/web/" })));
    let path = v["login_url"].as_str().unwrap().trim_start_matches(w.daemon.origin()).to_string();
    let (_, headers, _) = w.api.raw("GET", &path, &[], None);
    let cookie = header(&headers, "set-cookie").unwrap().split(';').next().unwrap().to_string();
    let (s, _, text) = w.api.raw("GET", "/v1/plugins/web/", &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")], None);
    assert_eq!((s, text.as_str()), (200, "root prefix=/v1/plugins/web"));
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/web/", &[("Cookie", &cookie)], None);
    assert_eq!(s, 200, "a navigation sends no origin");
    let origin = w.daemon.origin().to_string();
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/web/", &[("Cookie", &cookie), ("Origin", &origin)], None);
    assert_eq!(s, 200);
    let (s, _, text) = w.api.raw("GET", "/v1/plugins/web/", &[("Cookie", &cookie), ("Origin", "http://evil.example")], None);
    assert_eq!((s, text.as_str()), (403, "{\"error\":\"cross-origin request refused\"}"));
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/web/", &[("Cookie", &cookie), ("Sec-Fetch-Site", "cross-site")], None);
    assert_eq!(s, 403);
    let (s, _, _) = w.api.raw("GET", "/v1/plugins/web/", &[("Cookie", "balerix_session=forged")], None);
    assert_eq!(s, 401);
    let (s, _, _) = w.api.raw("GET", "/v1/fleets", &[("Cookie", &cookie)], None);
    assert_eq!(s, 401, "the cookie opens the mount and nothing else");

    let (_, m) = w.api.call("GET", "/metrics", None, None);
    let m = m.as_str().unwrap();
    assert!(m.contains("balerix_plugin_proxy_requests_total{plugin=\"web\",status=\"200\"}"), "{m}");
    assert!(m.contains("balerix_plugin_proxy_requests_total{plugin=\"web\",status=\"413\"} 1"), "{m}");
    assert!(m.contains("balerix_plugin_proxy_requests_total{plugin=\"unknown\",status=\"404\"}") || m.contains("balerix_plugin_proxy_requests_total{plugin=\"nope\",status=\"404\"} 1"), "{m}");
}
```

(`nope` parses as an `AgentName`, so the last assertion's second branch is the one that holds; keep the first for a name that does not parse if you add one.)

- [ ] **Step 7: Run it**

Run: `mise x -- cargo test -p balerix-server --test browser_it`
Expected: PASS. If the WebSocket step fails with the passthrough in place, record the failure text and fall back to frame copying per spec §11.1 (accept the client socket with `WebSocketUpgrade`, connect to the plugin with `tokio_tungstenite::connect_async`, copy `Message`s both ways) — but only after checking that `hyper::upgrade::on(&mut req)` ran before `into_parts` and that the request reached `forward` with its `Connection: Upgrade` header intact.

- [ ] **Step 8: `balerix plugin open`**

`crates/balerix/src/cli.rs`: add the variant `/// Print a single-use login URL for the plugin's browser routes (valid 60 s).\n    Open(PluginOpenArgs),` to `PluginCommand` and

```rust
#[derive(Debug, Args)]
pub struct PluginOpenArgs {
    pub name: String,
    #[arg(long)]
    pub api_url: Option<String>,
}
```

`crates/balerix/src/client.rs`: import `SessionRequest, SessionResponse` from `balerix_api` and add

```rust
    /// `POST /v1/sessions`: a single-use login URL landing on `to`.
    pub fn create_session(&self, to: &str) -> Result<String> {
        let resp: SessionResponse = Self::must(
            self.request(
                "POST",
                "/v1/sessions",
                Some(&serde_json::to_value(SessionRequest {
                    to: Some(to.to_string()),
                })?),
            ),
            "session",
        )?;
        Ok(resp.login_url)
    }
```

with a test next to `plugin_calls_hit_the_plugin_routes`:

```rust
    #[test]
    fn create_session_posts_the_target_and_returns_the_url() {
        let (url, rx) = crate::testutil::stub_server(
            "200 OK",
            r#"{"login_url":"http://127.0.0.1:1/v1/login/abc?to=/v1/plugins/web/"}"#,
        );
        let c = Client::new(url, "t".into());
        assert_eq!(
            c.create_session("/v1/plugins/web/").unwrap(),
            "http://127.0.0.1:1/v1/login/abc?to=/v1/plugins/web/"
        );
        let raw = rx.recv().unwrap();
        assert!(raw.starts_with("POST /v1/sessions HTTP/1.1"), "{raw}");
        assert!(raw.ends_with(r#"{"to":"/v1/plugins/web/"}"#), "{raw}");
    }
```

`crates/balerix/src/commands/plugin.rs`:

```rust
/// `balerix plugin open <name>` (plugins spec §18.2): prints the login URL;
/// opening it in a browser sets the session cookie and lands on the
/// plugin's mount. Nothing is launched.
pub fn open_command(args: &PluginOpenArgs) -> Result<String> {
    let client = Client::connect(args.api_url.as_deref())?;
    let name: balerix_core::AgentName = args
        .name
        .parse()
        .map_err(|e: balerix_core::NameError| anyhow!("plugin name: {e}"))?;
    Ok(format!(
        "{}\n",
        client.create_session(&format!("/v1/plugins/{name}/"))?
    ))
}
```

(import `PluginOpenArgs` from `crate::cli`), and in `crates/balerix/src/main.rs` the arm `Command::Plugin { command: PluginCommand::Open(args) } => commands::plugin::open_command(&args),`.

- [ ] **Step 9: Protocol doc**

In `docs/plugin-protocol.md`, after §4's table paragraphs add:

```markdown
### 4.1 Routes

A manifest with `routes: true` mounts the plugin's own HTTP surface at
`/v1/plugins/<name>/…` on the daemon's listener, authenticated by the
admin bearer or a browser session cookie (plugins spec §18.2). The daemon
forwards `/v1/plugins/<name>/` to `GET|POST|… http://<listen>/v1/routes`
and `/v1/plugins/<name>/<rest>?<query>` to `/v1/routes/<rest>?<query>`,
with the method, the body (1 MiB cap, 413 beyond), and the request
headers minus `Authorization`, `Cookie`, `Host` and the hop-by-hop set
(`Connection` and `Upgrade` are kept on an upgrade request). Two headers
are added: `Authorization: Bearer <BALERIX_PLUGIN_TOKEN>` (§2) and
`X-Balerix-Forwarded-Prefix: /v1/plugins/<name>`, the mount to build links
from. The response streams back with its hop-by-hop headers removed; a
101 is upgraded on both sides and the two byte streams copied until either
closes, so a WebSocket route works unchanged behind the mount. 404
`plugin "x" has no routes` without `routes: true`, 503 `plugin "x" is not
ready` before `hello`. The Rust SDK nests `Plugin::routes` under
`/v1/routes` behind the same bearer check as every other route
(`routes.json`, Task 6).
```

- [ ] **Step 10: Run the check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A
git commit -m "Mount plugin routes under /v1/plugins/<name>/ through a byte-level proxy

The mount (plugins spec §6, §18.1, §18.2) is a plain reverse proxy on
hyper's legacy client: request bodies capped at 1 MiB, responses
streamed, the daemon's credentials and the hop-by-hop headers stripped,
the plugin's own token and the forwarded prefix added, and a 101
upgraded on both sides and copied until either closes, so a WebSocket
route needs no frame parsing in the daemon (§11.1 verdict: the
passthrough holds). It authenticates itself: the admin bearer, or the
session cookie of Task 2 on a same-origin request. `balerix plugin
open <name>` prints the login URL.

New exact dependencies: hyper, hyper-util, http-body-util and
futures-util (already in the lock, now direct), tokio-tungstenite (no
TLS feature) for the tests and the SDK to come, and axum's ws feature.

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

---

### Task 4: `PtyStream` — the port, the echo fake, and tmux attach

**Files:**
- Modify: `crates/balerix-core/src/ports.rs`, `src/fakes.rs`, `src/lib.rs`
- Modify: `Cargo.toml` (`portable-pty`), `crates/balerix-runtime/Cargo.toml`
- Modify: `crates/balerix-runtime/src/tmux.rs`, `src/lib.rs`
- Modify: `crates/balerix-runtime/tests/tmux_it.rs`

**Interfaces:**
- Consumes: `portable_pty::{native_pty_system, CommandBuilder, PtySize, MasterPty, Child}`, `crate::tools::Cmd`.
- Produces:
  ```rust
  // balerix_core (ports.rs, re-exported)
  pub trait PtyStream: Send {
      fn reader(&self) -> io::Result<Box<dyn Read + Send>>;
      fn writer(&self) -> io::Result<Box<dyn Write + Send>>;   // taken once
      fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
  }
  pub trait AgentRunner { …; fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError>; }
  // balerix_core::fakes
  pub struct FakePty;                                   // echo; Drop closes
  impl FakeRunner { pub fn close_attach(&self, agent: &AgentId) -> bool; pub fn resizes(&self) -> Vec<(String, u16, u16)> }
  // balerix_runtime::tmux
  pub const ATTACH_SESSION_PREFIX: &str = "balerix-attach-";
  pub struct TmuxAttach;                                // PtyStream; Drop kills the client and the grouped session
  pub(crate) fn sessions_in_group(text: &str, crew: &str) -> Vec<String>;
  ```

- [ ] **Step 1: Write the failing fake test**

In `crates/balerix-core/src/fakes.rs` tests add:

```rust
    #[test]
    fn the_fake_pty_echoes_records_resizes_and_closes() {
        use std::io::{Read, Write};
        let r = FakeRunner::default();
        let pty = r.attach(&id("f/c/a")).unwrap();
        let mut reader = pty.reader().unwrap();
        let mut writer = pty.writer().unwrap();
        writer.write_all(b"hi").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"hi");
        pty.resize(120, 40).unwrap();
        assert_eq!(r.resizes(), vec![("f/c/a".to_string(), 120, 40)]);
        assert!(r.close_attach(&id("f/c/a")), "an open attach was closed");
        assert!(!r.close_attach(&id("f/c/a")), "and only once");
        assert_eq!(reader.read(&mut buf).unwrap(), 0, "EOF after close");
        assert_eq!(
            writer.write_all(b"x").unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert!(r.calls().contains(&"attach f/c/a".to_string()));
        // dropping the stream closes it too
        let pty = r.attach(&id("f/c/b")).unwrap();
        let mut reader = pty.reader().unwrap();
        drop(pty);
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
        r.fail_next("attach", "f/c/c", "no window");
        assert!(r.attach(&id("f/c/c")).is_err());
    }
```

Run: `mise x -- cargo test -p balerix-core fakes`
Expected: compile error (no `attach`).

- [ ] **Step 2: The port**

In `crates/balerix-core/src/ports.rs` add `use std::io::{self, Read, Write};` and, before `pub trait Clock`:

```rust
/// A live terminal on one agent's window (plugins spec §18.4). Sync like
/// every port; the server bridges it to a WebSocket with one blocking
/// reader task. Dropping the stream ends the session.
pub trait PtyStream: Send {
    /// A reader of the terminal's output. Owned by the caller so a
    /// blocking reader thread can outlive the borrow.
    fn reader(&self) -> io::Result<Box<dyn Read + Send>>;
    /// The writer of keystrokes. Taken once: a second call fails.
    fn writer(&self) -> io::Result<Box<dyn Write + Send>>;
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
}
```

and to `AgentRunner`:

```rust
    /// A terminal on the agent's window (plugins spec §18.4). The runner
    /// decides how; nothing about tmux crosses this port.
    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError>;
```

In `crates/balerix-core/src/lib.rs` add `PtyStream` to the `ports` re-export list.

- [ ] **Step 3: The fake**

In `crates/balerix-core/src/fakes.rs`, add the imports `use std::collections::VecDeque; use std::io::{self, Read, Write}; use std::sync::{Arc, Condvar};` and `PtyStream` to the `crate::ports` import. Before `#[derive(Default)] pub struct FakeRunner`:

```rust
#[derive(Default)]
struct PtyBuf {
    data: VecDeque<u8>,
    closed: bool,
}

/// One fake terminal's shared end: what the writer wrote, until the
/// reader takes it; `closed` ends the reader with EOF and the writer with
/// `BrokenPipe`.
#[derive(Default)]
struct PtyShared {
    buf: Mutex<PtyBuf>,
    cv: Condvar,
}

impl PtyShared {
    fn close(&self) {
        lock(&self.buf).closed = true;
        self.cv.notify_all();
    }
}

struct FakePtyReader(Arc<PtyShared>);

impl Read for FakePtyReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut b = lock(&self.0.buf);
        while b.data.is_empty() && !b.closed {
            b = self.0.cv.wait(b).unwrap_or_else(|e| e.into_inner());
        }
        if b.data.is_empty() {
            return Ok(0);
        }
        let n = out.len().min(b.data.len());
        for (slot, byte) in out.iter_mut().zip(b.data.drain(..n)) {
            *slot = byte;
        }
        Ok(n)
    }
}

struct FakePtyWriter(Arc<PtyShared>);

impl Write for FakePtyWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut b = lock(&self.0.buf);
        if b.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pty closed"));
        }
        b.data.extend(bytes);
        self.0.cv.notify_all();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The fake's terminal: an echo. Bytes written come back on the reader,
/// resizes are recorded on the runner, `FakeRunner::close_attach` (or a
/// drop) ends it.
pub struct FakePty {
    shared: Arc<PtyShared>,
    agent: String,
    resizes: Arc<Mutex<Vec<(String, u16, u16)>>>,
}

impl PtyStream for FakePty {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(FakePtyReader(self.shared.clone())))
    }
    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(FakePtyWriter(self.shared.clone())))
    }
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        lock(&self.resizes).push((self.agent.clone(), cols, rows));
        Ok(())
    }
}

impl Drop for FakePty {
    fn drop(&mut self) {
        self.shared.close();
    }
}
```

Add to `FakeRunner` the fields `attached: Mutex<BTreeMap<String, Arc<PtyShared>>>,` and `resizes: Arc<Mutex<Vec<(String, u16, u16)>>>,` (both `Default`), the methods

```rust
    /// Ends the reader of the latest attach for `agent`; `false` if none.
    pub fn close_attach(&self, agent: &AgentId) -> bool {
        match lock(&self.attached).remove(&agent.to_string()) {
            Some(shared) => {
                shared.close();
                true
            }
            None => false,
        }
    }
    pub fn resizes(&self) -> Vec<(String, u16, u16)> {
        lock(&self.resizes).clone()
    }
```

and to `impl AgentRunner for FakeRunner`:

```rust
    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError> {
        let id = agent.to_string();
        self.check("attach", &id)?;
        let shared = Arc::new(PtyShared::default());
        lock(&self.attached).insert(id.clone(), shared.clone());
        Ok(Box::new(FakePty {
            shared,
            agent: id,
            resizes: self.resizes.clone(),
        }))
    }
```

Run: `mise x -- cargo test -p balerix-core`
Expected: PASS, and the reconcile model-based tests are untouched.

- [ ] **Step 4: Write the failing `sessions_in_group` test and the tmux integration test**

In `crates/balerix-runtime/src/tmux.rs` tests add:

```rust
    #[test]
    fn a_crew_group_is_the_crew_session_and_everything_grouped_with_it() {
        let text = "f/c\tf/c\nbalerix-attach-1a2b3c4d\tf/c\nf/d\t\ng/c\tg/c\nbalerix-attach-9\tg/c\n";
        assert_eq!(
            sessions_in_group(text, "f/c"),
            vec!["f/c", "balerix-attach-1a2b3c4d"]
        );
        assert_eq!(sessions_in_group(text, "f/d"), vec!["f/d"], "ungrouped: itself");
        assert!(sessions_in_group(text, "f/e").is_empty());
        assert!(attach_session_name().starts_with(ATTACH_SESSION_PREFIX));
        assert_ne!(attach_session_name(), attach_session_name());
    }
```

Append to `crates/balerix-runtime/tests/tmux_it.rs` (add `use std::io::{Read, Write};` and `use balerix_runtime::tmux::ATTACH_SESSION_PREFIX;`):

```rust
/// Plugins spec §18.4, §11.1: a real tmux attach through the PTY sees the
/// pane, typed bytes reach it, the resize reaches the client, the grouped
/// session dies with the stream, and `stop_crew` takes the whole group.
#[test]
fn attach_streams_the_pane_and_the_grouped_session_dies_with_the_stream() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("tmux", false));
        return;
    };
    let root = support::temp_root("tmux-attach");
    let socket = format!("balerix-test-attach-{}", std::process::id());
    let r = TmuxRunner::new(tools.tmux.clone(), socket.clone());
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew = id.crew_ref();
    let fleet = id.fleet.clone();
    let agent_dir = root.join("a");
    std::fs::create_dir_all(agent_dir.join("logs")).unwrap();
    let script = agent_dir.join("launch.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho hello-from-agent\nexec cat > typed.log\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = LaunchPlan {
        cwd: agent_dir.clone(),
        env: BTreeMap::new(),
        argv: vec![],
        script: script.clone(),
    };
    let tmux = |args: &[&str]| -> String {
        let out = std::process::Command::new(&tools.tmux)
            .args(["-L", &socket])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let sessions = || tmux(&["list-sessions", "-F", "#{session_name}"]);

    assert!(r.attach(&id).is_err(), "no window yet");
    r.ensure_crew(&crew).unwrap();
    r.ensure_agent(&id, &plan).unwrap();
    wait_for(|| {
        std::fs::read_to_string(agent_dir.join("logs/tmux.log"))
            .is_ok_and(|s| s.contains("hello-from-agent"))
    });

    let stream = r.attach(&id).unwrap();
    let mut reader = stream.reader().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut seen = Vec::new();
    wait_for(|| {
        while let Ok(chunk) = rx.try_recv() {
            seen.extend(chunk);
        }
        String::from_utf8_lossy(&seen).contains("hello-from-agent")
    });
    let mut writer = stream.writer().unwrap();
    assert!(stream.writer().is_err(), "the writer is taken once");
    writer.write_all(b"typed-through-attach\n").unwrap();
    writer.flush().unwrap();
    wait_for(|| {
        std::fs::read_to_string(agent_dir.join("typed.log"))
            .is_ok_and(|s| s.contains("typed-through-attach"))
    });
    stream.resize(100, 30).unwrap();
    wait_for(|| tmux(&["list-clients", "-F", "#{client_width}x#{client_height}"]).contains("100x30"));
    let attach_session = sessions()
        .lines()
        .find(|l| l.starts_with(ATTACH_SESSION_PREFIX))
        .unwrap()
        .to_string();
    assert_eq!(
        tmux(&["display-message", "-p", "-t", &attach_session, "#{window_name}"]).trim(),
        "a",
        "the grouped session shows the agent's window"
    );
    assert_eq!(
        tmux(&["display-message", "-p", "-t", "f/c", "#{window_name}"]).trim(),
        "a",
        "the crew session's current window is whatever it was"
    );

    drop(stream);
    wait_for(|| !sessions().contains(ATTACH_SESSION_PREFIX));
    assert!(sessions().contains("f/c"), "the crew session survives the stream");
    assert!(matches!(
        r.observe(&fleet).unwrap().get(&id),
        Some(ProcessState::Running { .. })
    ));

    // a second attach, then stop_crew must take the group with it
    let again = r.attach(&id).unwrap();
    wait_for(|| sessions().contains(ATTACH_SESSION_PREFIX));
    r.stop_crew(&crew).unwrap();
    wait_for(|| sessions().trim().is_empty());
    drop(again);
    let _ = std::process::Command::new(&tools.tmux)
        .args(["-L", &socket, "kill-server"])
        .status();
}
```

- [ ] **Step 5: Run them to see them fail**

Run: `mise x -- cargo test -p balerix-runtime tmux`
Expected: compile errors (`attach` missing on `TmuxRunner`, no `sessions_in_group`).

- [ ] **Step 6: Implement the tmux attach and the group kill**

In `Cargo.toml` `[workspace.dependencies]` add, after `portable-pty`'s neighbours from Task 3:

```toml
# AgentRunner::attach on tmux (plugins spec §18.4): a PTY for the tmux
# client. The workspace forbids `unsafe`, so the setsid/TIOCSCTTY dance a
# hand-rolled pre_exec would need lives in this crate instead.
portable-pty = "0.9.0"
```

and `portable-pty = { workspace = true }` to `crates/balerix-runtime/Cargo.toml`.

In `crates/balerix-runtime/src/tmux.rs`: imports `use std::io::{self, Read, Write}; use std::sync::atomic::{AtomicU64, Ordering}; use std::time::{SystemTime, UNIX_EPOCH}; use balerix_core::PtyStream; use portable_pty::{CommandBuilder, PtySize, native_pty_system};`. Add after `IDLE_ARGV`:

```rust
/// Prefix of the throwaway grouped session one attach creates (plugins
/// spec §18.4); `observe` ignores it, `stop_crew` kills it with the crew.
pub const ATTACH_SESSION_PREFIX: &str = "balerix-attach-";
const ATTACH_TERM: &str = "xterm-256color";
static ATTACH_SEQ: AtomicU64 = AtomicU64::new(0);

/// `balerix-attach-<8 hex>`, unique per process: the clock, a counter and
/// the pid folded into 32 bits.
fn attach_session_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let seq = ATTACH_SEQ.fetch_add(1, Ordering::Relaxed);
    let mixed = nanos ^ (seq << 32) ^ u64::from(std::process::id());
    let low = u32::try_from(mixed & 0xffff_ffff).unwrap_or(0);
    format!("{ATTACH_SESSION_PREFIX}{low:08x}")
}

/// The session names of `crew`'s group: itself and every session grouped
/// with it. An ungrouped session's `session_group` is empty.
pub(crate) fn sessions_in_group(text: &str, crew: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let (name, group) = l.split_once('\t')?;
            (name == crew || group == crew).then(|| name.to_string())
        })
        .collect()
}

/// One attached client: a tmux client in a PTY on a throwaway session
/// grouped with the crew's, so viewers never fight the operator's own
/// client over the current window. Drop kills the client and the
/// session; if the daemon dies first, the PTY closes, the client detaches
/// and tmux's `destroy-unattached` finishes the job.
pub struct TmuxAttach {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// The port says the writer is taken once; enforced here rather than
    /// relying on what `portable-pty` does on a second `take_writer`.
    writer_taken: std::sync::atomic::AtomicBool,
    tmux: PathBuf,
    socket: String,
    session: String,
}

impl PtyStream for TmuxAttach {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader().map_err(io::Error::other)
    }
    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        if self.writer_taken.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other("the writer was already taken"));
        }
        self.master.take_writer().map_err(io::Error::other)
    }
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }
}

impl Drop for TmuxAttach {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Cmd::new(&self.tmux)
            .args([
                "-L".to_string(),
                self.socket.clone(),
                "kill-session".to_string(),
                "-t".to_string(),
                format!("={}", self.session),
            ])
            .run();
    }
}
```

In `impl AgentRunner for TmuxRunner`, replace `stop_crew` and add `attach`:

```rust
    fn stop_crew(&self, crew: &CrewRef) -> Result<(), RunnerError> {
        let name = crew.to_string();
        // `kill-session` on the crew session alone would leave its
        // windows alive in any grouped attach session (§18.4): every
        // session of the group goes.
        let Some(text) = self.run_optional(
            &name,
            &["list-sessions", "-F", "#{session_name}\t#{session_group}"],
        )?
        else {
            return Ok(());
        };
        for session in sessions_in_group(&text, &name) {
            self.run_optional(&name, &["kill-session", "-t", &format!("={session}")])?;
        }
        Ok(())
    }

    fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError> {
        let id = agent.to_string();
        let crew = agent.crew_ref();
        let session = attach_session_name();
        let fail = |stderr: String| RunnerError::Tool {
            id: id.clone(),
            subcommand: "attach-session".into(),
            args: vec![session.clone()],
            stderr,
        };
        // The window must exist: `select-window` on a missing one would
        // leave the client on the anchor.
        let known = self
            .windows(&crew)?
            .is_some_and(|w| w.contains_key(&agent.agent));
        if !known {
            return Err(fail(format!("no window for {agent}")));
        }
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| fail(format!("openpty: {e}")))?;
        // One command sequence, with the client attached before
        // `destroy-unattached` is set: tmux destroys a detached session the
        // moment that option lands on it.
        let mut cmd = CommandBuilder::new(&self.tmux);
        cmd.args([
            "-L".to_string(),
            self.socket.clone(),
            "new-session".to_string(),
            "-t".to_string(),
            Self::session_target(&crew),
            "-s".to_string(),
            session.clone(),
            ";".to_string(),
            "select-window".to_string(),
            "-t".to_string(),
            format!("={}", agent.agent),
            ";".to_string(),
            "set-option".to_string(),
            "destroy-unattached".to_string(),
            "on".to_string(),
            ";".to_string(),
            "set-option".to_string(),
            "status".to_string(),
            "off".to_string(),
        ]);
        cmd.env("TERM", ATTACH_TERM);
        cmd.cwd("/");
        let child = pty
            .slave
            .spawn_command(cmd)
            .map_err(|e| fail(format!("spawn: {e}")))?;
        drop(pty.slave);
        Ok(Box::new(TmuxAttach {
            master: pty.master,
            child,
            writer_taken: std::sync::atomic::AtomicBool::new(false),
            tmux: self.tmux.clone(),
            socket: self.socket.clone(),
            session,
        }))
    }
```

In `crates/balerix-runtime/src/lib.rs` change the tmux re-export to `pub use tmux::{ANCHOR_WINDOW, ATTACH_SESSION_PREFIX, TmuxAttach, TmuxRunner};`.

- [ ] **Step 7: Run the runtime tests**

Run: `mise x -- cargo test -p balerix-runtime tmux` then `mise run test-it`
Expected: PASS. If `attach_streams_the_pane…` fails on the resize assertion alone, check `list-clients` output by hand (`tmux -L <socket> list-clients -F '#{client_width}x#{client_height}'`); if it fails before "hello-from-agent" arrives, run the same sequence by hand through `script -qec "tmux -L <socket> new-session -t =f/c -s balerix-attach-x \; select-window -t =a \; set-option destroy-unattached on \; set-option status off" /dev/null` and record the verdict for Task 10's §11.1 table; the spec's fallback (`pipe-pane` + `send-keys`, no resize) conflicts with the log pipe and is not to be taken without discussing it.

- [ ] **Step 8: Run the check and commit**

Run: `mise run check`
Expected: PASS (the server compiles: no server code calls `attach` yet, the trait has a new method and both implementors provide it).

```bash
git add -A
git commit -m "Add PtyStream and AgentRunner::attach, on tmux as a grouped session

The port (plugins spec §18.4) is a sync reader/writer/resize the daemon
bridges to a WebSocket next; the fake is an echo. On tmux each attach is
a throwaway session grouped with the crew's, created and attached in one
command sequence with destroy-unattached set once the client is on it —
tmux 3.7c destroys a detached session the moment the option lands — so
viewers never move the operator's own client between windows. Killing
the crew session alone would leave its windows alive in such a group,
so stop_crew now kills every session of the group. portable-pty
provides the PTY: the workspace forbids the unsafe a pre_exec needs.

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

---

### Task 5: The daemon's `attach` and `fleets/watch` streams

**Files:**
- Modify: `crates/balerix-api/src/protocol.rs`, `src/lib.rs` (`ResizeFrame`, `Resize`)
- Create: `crates/balerix-server/src/attach.rs`, `crates/balerix-server/src/watch.rs`
- Modify: `crates/balerix-server/src/daemon.rs` (`runner()`, the change tick, forwarders, `bump`)
- Modify: `crates/balerix-server/src/plugin_api.rs` (two routes), `src/lib.rs`
- Create: `crates/balerix-server/tests/streams_it.rs`
- Modify: `docs/plugin-protocol.md` §3 (two rows and a "Streams" paragraph)

**Interfaces:**
- Consumes: `PtyStream`, `FakeRunner::{close_attach, resizes}`, `PluginRegistry::{has, is_active}`, `Daemon::plugin_fleets`.
- Produces:
  ```rust
  // balerix_api::protocol
  pub struct Resize { pub cols: u16, pub rows: u16 }
  pub struct ResizeFrame { pub resize: Resize }          // { "resize": { "cols", "rows" } }
  impl ResizeFrame { pub fn new(cols: u16, rows: u16) -> Self; pub fn parse(text: &str) -> Option<Self> }   // None unless well-formed and both ≥ 1
  // balerix_server::attach
  pub const CLOSE_NORMAL: u16 = 1000; pub const CLOSE_UNSUPPORTED: u16 = 1003; pub const CLOSE_ERROR: u16 = 1011;
  pub async fn bridge(socket: WebSocket, stream: Box<dyn PtyStream>);
  // balerix_server::watch
  pub const PING_INTERVAL: Duration;                     // 30 s
  pub async fn serve_watch(socket: WebSocket, daemon: Arc<Daemon>);
  // balerix_server::Daemon
  pub fn runner(&self) -> Arc<dyn AgentRunner>;
  pub fn changes(&self) -> tokio::sync::watch::Receiver<u64>;   // bumped on every actor snapshot and registry write
  // routes: GET /v1/plugin-host/fleets/watch (WS, `fleets`), GET /v1/plugin-host/agents/{f}/{c}/{a}/attach (WS, `attach`, 404 unless active)
  ```

- [ ] **Step 1: `ResizeFrame` with its test**

In `crates/balerix-api/src/protocol.rs` append before the tests:

```rust
/// The one text frame an attach socket accepts, both directions of the
/// protocol (plugins spec §18.4): `{ "resize": { "cols", "rows" } }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResizeFrame {
    pub resize: Resize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resize {
    pub cols: u16,
    pub rows: u16,
}

impl ResizeFrame {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            resize: Resize { cols, rows },
        }
    }

    /// `None` for anything but a well-formed frame with both dimensions
    /// at least 1: a zero-sized terminal is a bug on the sender's side.
    pub fn parse(text: &str) -> Option<Self> {
        let frame: Self = serde_json::from_str(text).ok()?;
        (frame.resize.cols >= 1 && frame.resize.rows >= 1).then_some(frame)
    }
}
```

and the test

```rust
    #[test]
    fn resize_frames_round_trip_and_reject_the_malformed() {
        let f = ResizeFrame::new(120, 40);
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            r#"{"resize":{"cols":120,"rows":40}}"#
        );
        assert_eq!(ResizeFrame::parse(r#"{"resize":{"cols":120,"rows":40}}"#), Some(f));
        for bad in [
            "junk",
            r#"{"resize":{"cols":0,"rows":40}}"#,
            r#"{"resize":{"cols":80}}"#,
            r#"{"resize":{"cols":80,"rows":24},"x":1}"#,
            r#"{"cols":80,"rows":24}"#,
        ] {
            assert_eq!(ResizeFrame::parse(bad), None, "{bad}");
        }
    }
```

Add `Resize, ResizeFrame` to the `protocol` re-export in `crates/balerix-api/src/lib.rs`. Run `mise x -- cargo test -p balerix-api` → PASS.

- [ ] **Step 2: Write the failing integration test**

Create `crates/balerix-server/tests/streams_it.rs`:

```rust
//! Plugins spec §18.4 through a real listener: `fleets/watch` frames on
//! every change, and `attach` bridged to the fake runner's echo PTY.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use balerix_api::{
    ActivationState, AgentSettings, CrewSpec, FleetRequest, FleetSpec, GitSettings,
};
use balerix_core::plugin_id;
use balerix_plugin_sdk::{Env, Host, Plugin, bind, run};
use serde_json::{Value, json};
use support::{World, world};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A plugin with every method left at its default.
struct Silent;
impl Plugin for Silent {}

async fn token(w: &World, plugin: &str) -> String {
    w.daemon
        .hook_secret(&plugin_id(&plugin.parse().unwrap()))
        .await
        .unwrap()
}

fn ws_url(w: &World, path: &str) -> String {
    format!("{}{path}", w.api.base.replacen("http://", "ws://", 1))
}

async fn ws(url: &str, token: &str) -> Result<Socket, tungstenite::Error> {
    let mut req = url.into_client_request()?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    connect_async(req).await.map(|(s, _)| s)
}

fn status_of(e: &tungstenite::Error) -> Option<u16> {
    match e {
        tungstenite::Error::Http(r) => Some(r.status().as_u16()),
        _ => None,
    }
}

/// The next text frame, parsed, within five seconds.
async fn next_text(s: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match s.next().await.expect("socket open").unwrap() {
                Message::Text(t) => return serde_json::from_str(&t).unwrap(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    })
    .await
    .expect("a text frame")
}

/// Frames until one satisfies `pred` (the actor publishes several per apply).
async fn wait_frame(s: &mut Socket, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let v = next_text(s).await;
            if pred(&v) {
                return v;
            }
        }
    })
    .await
    .expect("the expected frame")
}

fn spec(agents: &[(&str, &[(&str, Value)])]) -> FleetSpec {
    FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([(
            "c".to_string(),
            CrewSpec {
                repo: "acme/x".into(),
                git_ref: "main".into(),
                git: GitSettings::default(),
                agents: agents
                    .iter()
                    .map(|(n, plugins)| {
                        let s = AgentSettings {
                            plugins: plugins
                                .iter()
                                .map(|(p, c)| (p.to_string(), c.clone()))
                                .collect(),
                            ..Default::default()
                        };
                        (n.to_string(), s)
                    })
                    .collect(),
            },
        )]),
    }
}

/// Starts a silent SDK plugin under `name` and says hello with its token.
async fn start_silent(w: &World, name: &str) -> Host {
    let env = Env {
        api_url: w.api.base.clone(),
        name: name.into(),
        token: token(w, name).await,
        scratch: w.dir.path().join("s"),
    };
    let (listener, listen) = bind().await.unwrap();
    let tok = env.token.clone();
    tokio::spawn(async move { run(listener, Arc::new(Silent), &tok).await });
    let host = Host::new(env).unwrap();
    host.hello("0.1.0", &listen).await.unwrap();
    host
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fleets_watch_sends_the_full_list_on_every_change() {
    let w = world().await;
    let e = ws(&ws_url(&w, "/v1/plugin-host/fleets/watch"), &token(&w, "flow").await)
        .await
        .unwrap_err();
    assert_eq!(status_of(&e), Some(403), "flow lacks `fleets`: {e}");
    let e = ws(&ws_url(&w, "/v1/plugin-host/fleets/watch"), "nope").await.unwrap_err();
    assert_eq!(status_of(&e), Some(401));

    let web = token(&w, "web").await;
    let mut s = ws(&ws_url(&w, "/v1/plugin-host/fleets/watch"), &web).await.unwrap();
    assert_eq!(next_text(&mut s).await, json!([]), "the first frame is the current list");

    // an apply: the record appears, then its agent
    let req = json!(FleetRequest {
        spec: spec(&[("a", &[("flow", json!({ "v": 1 }))])]),
        credentials: Default::default()
    });
    let (st, _) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(st, 200);
    let frame = wait_frame(&mut s, |v| v[0]["status"]["agents"]["f/c/a"].is_object()).await;
    assert_eq!(frame[0]["spec"]["name"], "f");
    assert_eq!(
        frame[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"],
        "pending",
        "the activation overlay is in the frame"
    );

    // a registry-only change: the plugin says hello and the pair activates,
    // with no actor snapshot behind it
    let _flow = start_silent(&w, "flow").await;
    wait_frame(&mut s, |v| {
        v[0]["status"]["agents"]["f/c/a"]["plugins"]["flow"]["state"] == "active"
    })
    .await;

    // purge: the list empties
    let (st, _) = w.api.admin(
        "DELETE",
        "/v1/fleets/f?keep_repos=false&keep_sessions=false&purge=true",
        None,
    );
    assert_eq!(st, 200);
    wait_frame(&mut s, |v| v.as_array().is_some_and(Vec::is_empty)).await;
    s.close(None).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_bridges_the_runner_pty_and_gates_on_activation() {
    let w = world().await;
    let web = token(&w, "web").await;
    let _web_plugin = start_silent(&w, "web").await;
    let req = json!(FleetRequest {
        spec: spec(&[("a", &[("web", json!({}))]), ("b", &[])]),
        credentials: Default::default()
    });
    let (st, _) = w.api.admin("POST", "/v1/fleets", Some(&req));
    assert_eq!(st, 200);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let r = w.daemon.get(&"f".parse().unwrap()).await.unwrap();
            if r.status.observed_generation == 1
                && r.status.agents["f/c/a"]
                    .plugins
                    .get("web")
                    .is_some_and(|p| p.state == ActivationState::Active)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    // gates: capability, activation, token
    let e = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach"), &token(&w, "flow").await)
        .await
        .unwrap_err();
    assert_eq!(status_of(&e), Some(403), "{e}");
    let e = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/b/attach"), &web)
        .await
        .unwrap_err();
    assert_eq!(status_of(&e), Some(404), "b is not active for web: {e}");
    let e = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach"), "nope")
        .await
        .unwrap_err();
    assert_eq!(status_of(&e), Some(401));

    // the bridge: echo, resize, an unsupported text frame closes 1003
    let mut s = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach"), &web)
        .await
        .unwrap();
    s.send(Message::Binary(b"hello".to_vec().into())).await.unwrap();
    let echo = s.next().await.unwrap().unwrap();
    assert_eq!(echo.into_data().as_ref(), b"hello");
    s.send(Message::Text(r#"{"resize":{"cols":120,"rows":40}}"#.into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !w.h.runner.resizes().contains(&("f/c/a".to_string(), 120, 40)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the resize reached the runner");
    s.send(Message::Text("junk".into())).await.unwrap();
    let close = s.next().await.unwrap().unwrap();
    match close {
        Message::Close(Some(f)) => assert_eq!(u16::from(f.code), 1003, "{f:?}"),
        other => panic!("expected a close, got {other:?}"),
    }

    // a second attach, then the window dies: 1000
    let mut s2 = ws(&ws_url(&w, "/v1/plugin-host/agents/f/c/a/attach"), &web)
        .await
        .unwrap();
    s2.send(Message::Binary(b"x".to_vec().into())).await.unwrap();
    assert_eq!(s2.next().await.unwrap().unwrap().into_data().as_ref(), b"x");
    assert!(w.h.runner.close_attach(&"f/c/a".parse().unwrap()));
    let close = s2.next().await.unwrap().unwrap();
    match close {
        Message::Close(Some(f)) => assert_eq!(u16::from(f.code), 1000, "{f:?}"),
        other => panic!("expected a close, got {other:?}"),
    }
    assert_eq!(
        w.h.runner.calls().iter().filter(|c| *c == "attach f/c/a").count(),
        2
    );
}
```

- [ ] **Step 3: Run it to see it fail**

Run: `mise x -- cargo test -p balerix-server --test streams_it`
Expected: both tests fail — the handshakes answer 404 (no such routes).

- [ ] **Step 4: `attach.rs`**

Create `crates/balerix-server/src/attach.rs`:

```rust
//! The daemon's end of `GET /v1/plugin-host/agents/{id}/attach` (plugins
//! spec §18.4): one WebSocket bridged to the runner's `PtyStream`. Binary
//! frames are terminal bytes both ways; the one text frame is a resize.
//! Dropping the stream at the end is what ends the terminal session.

use std::io::{Read, Write};

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use balerix_api::ResizeFrame;
use balerix_core::PtyStream;
use tokio::sync::mpsc;

const READ_CHUNK: usize = 8192;
/// The window closed, or the peer is done.
pub const CLOSE_NORMAL: u16 = 1000;
/// A text frame that is not a resize.
pub const CLOSE_UNSUPPORTED: u16 = 1003;
/// The runner's side failed.
pub const CLOSE_ERROR: u16 = 1011;

pub async fn bridge(mut socket: WebSocket, stream: Box<dyn PtyStream>) {
    let (reader, mut writer) = match (stream.reader(), stream.writer()) {
        (Ok(r), Ok(w)) => (r, w),
        (Err(e), _) | (_, Err(e)) => {
            close(&mut socket, CLOSE_ERROR, &format!("attach: {e}")).await;
            return;
        }
    };
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    // The reader blocks on the PTY; it lives on a blocking thread and ends
    // when the stream is dropped below (the read fails once the client
    // process is gone) or the socket is.
    let pump = tokio::task::spawn_blocking(move || pump_reader(reader, &tx));
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(bytes) => {
                    if socket.send(Message::Binary(Bytes::from(bytes))).await.is_err() {
                        break;
                    }
                }
                None => {
                    close(&mut socket, CLOSE_NORMAL, "the window closed").await;
                    break;
                }
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                        close(&mut socket, CLOSE_ERROR, "the window closed").await;
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => match ResizeFrame::parse(text.as_str()) {
                    Some(frame) => {
                        if let Err(e) = stream.resize(frame.resize.cols, frame.resize.rows) {
                            tracing::debug!("attach resize failed: {e}");
                        }
                    }
                    None => {
                        close(&mut socket, CLOSE_UNSUPPORTED, "expected a resize frame").await;
                        break;
                    }
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    drop(stream);
    drop(pump);
}

fn pump_reader(mut reader: Box<dyn Read + Send>, tx: &mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        }
    }
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}
```

- [ ] **Step 5: `watch.rs` and the daemon's tick**

Create `crates/balerix-server/src/watch.rs`:

```rust
//! `GET /v1/plugin-host/fleets/watch` (plugins spec §18.4): one text frame
//! per change, each the complete `fleets` list — the user fleets with
//! their activation overlay — so a consumer replaces its state and never
//! diffs or handles removals. The daemon's change tick wakes the handler;
//! it recomputes and sends only what differs from the last frame.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use balerix_core::FleetRecord;

use crate::daemon::Daemon;

/// Dead clients are reaped by the ping's failure.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

pub async fn serve_watch(mut socket: WebSocket, daemon: Arc<Daemon>) {
    let mut changes = daemon.changes();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick is immediate
    let mut last: Option<Vec<FleetRecord>> = None;
    loop {
        let now = daemon.plugin_fleets().await;
        if last.as_ref() != Some(&now) {
            let text = match serde_json::to_string(&now) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("fleets/watch: cannot encode: {e}");
                    return;
                }
            };
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
            last = Some(now);
        }
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    return;
                }
            }
            msg = socket.recv() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}
```

In `crates/balerix-server/src/daemon.rs`: import `use tokio::sync::watch;` (extend the existing `tokio::sync` import) and `use balerix_core::AgentRunner;` (extend the existing import); add the field `changes: Arc<watch::Sender<u64>>,`; in `start`, before the `for (record, secrets) in existing` loop, `let changes = Arc::new(watch::channel(0u64).0);` and inside the loop, right after `fleets.insert(name, h);`, spawn the forwarder: `tokio::spawn(Self::forward_changes(fleets_handle_status, changes.clone()));` — concretely, replace `let h = actor::spawn(…); fleets.insert(name, h);` with

```rust
                    let h = actor::spawn(
                        name.clone(),
                        record,
                        secrets,
                        ports.clone(),
                        shared.clone(),
                        true,
                    );
                    tokio::spawn(Self::forward_changes(h.status.clone(), changes.clone()));
                    fleets.insert(name, h);
```

Put `changes` into the `Self { … }` literal. In `apply`, where a new actor is spawned (`None => { … let h = actor::spawn(…); fleets.insert(name.clone(), h.clone()); h }`), add `tokio::spawn(Self::forward_changes(h.status.clone(), self.changes.clone()));` before the insert. Then the methods (after `overlay`):

```rust
    /// Every published snapshot of one actor becomes one tick of the
    /// change counter `fleets/watch` waits on; a final tick when the actor
    /// ends (a purge), so the list without it goes out too.
    async fn forward_changes(
        mut status: watch::Receiver<FleetRecord>,
        changes: Arc<watch::Sender<u64>>,
    ) {
        while status.changed().await.is_ok() {
            changes.send_modify(|n| *n += 1);
        }
        changes.send_modify(|n| *n += 1);
    }

    /// Ticks when any fleet record or activation row changed (§18.4).
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Registry writes have no actor behind them; the writer ticks.
    fn bump(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    pub fn runner(&self) -> Arc<dyn AgentRunner> {
        self.ports.runner.clone()
    }
```

Call `self.bump();` at the end of `plugin_hello` (before `Ok(response)`), at the end of `apply` (before `Ok(self.overlay(record))`), at the end of `down` (before its `Ok(…)`), and in `forget_purged` after `d.fleets.write().await.remove(&name);` as `d.bump();`.

- [ ] **Step 6: The routes**

In `crates/balerix-server/src/plugin_api.rs`, import `use axum::extract::ws::WebSocketUpgrade;` and `use balerix_core::AgentId;` (already), and register in `router`:

```rust
        .route("/v1/plugin-host/fleets/watch", get(watch_fleets))
        .route(
            "/v1/plugin-host/agents/{fleet}/{crew}/{agent}/attach",
            get(attach),
        )
```

(`fleets/watch` is a static segment and wins over `fleets/{name}`). The handlers:

```rust
/// `GET fleets/watch` (WS): every change, as the whole list (§18.4).
async fn watch_fleets(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    caller(&state, &headers, Capability::Fleets).await?;
    let daemon = state.daemon.clone();
    Ok(ws.on_upgrade(move |socket| crate::watch::serve_watch(socket, daemon)))
}

/// `GET agents/{id}/attach` (WS): a terminal on the agent's window, for a
/// plugin active on it (§18.4). The runner attaches on a blocking thread
/// before the upgrade, so a failure is an ordinary 500.
async fn attach(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let plugin = caller(&state, &headers, Capability::Attach).await?;
    let Path((f, c, a)) = path.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let agent: AgentId = format!("{f}/{c}/{a}")
        .parse()
        .map_err(|_: balerix_core::NameError| ApiError::from(DaemonError::NotFound))?;
    if !state.daemon.registry().is_active(&agent, &plugin) {
        return Err(PluginError::NotActive(agent.to_string()).into());
    }
    let runner = state.daemon.runner();
    let id = agent.clone();
    let stream = tokio::task::spawn_blocking(move || runner.attach(&id))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    tracing::info!(plugin = %plugin, agent = %agent, "attach opened");
    Ok(ws.on_upgrade(move |socket| crate::attach::bridge(socket, stream)))
}
```

Add `pub mod attach; pub mod watch;` to `crates/balerix-server/src/lib.rs` (alphabetical), and the re-exports `pub use attach::{CLOSE_ERROR, CLOSE_NORMAL, CLOSE_UNSUPPORTED, bridge}; pub use watch::{PING_INTERVAL, serve_watch};`.

- [ ] **Step 7: Run the streams test**

Run: `mise x -- cargo test -p balerix-server --test streams_it`
Expected: PASS.

- [ ] **Step 8: Protocol doc**

In `docs/plugin-protocol.md` §3, add the table rows after `GET fleets/{name}`, unknown name:

```markdown
| `GET fleets/watch` (WebSocket) | `fleets` | — | one text frame per change, each the complete `GET fleets` body | 101 | `fleets-watch.json` (Task 6) |
| `GET agents/{fleet}/{crew}/{agent}/attach` (WebSocket) | `attach` | — | binary frames are terminal bytes both ways; the one text frame is `{ "resize": { "cols", "rows" } }` | 101 | `attach-resize.json` (Task 6) |
| `GET agents/…/attach`, agent not active for this plugin | `attach` | — | `{ "error": "plugin is not active for agent <id>" }` | 404 | (as for actions) |
```

and, after the **KV** paragraph, a **Streams** paragraph:

```markdown
**Streams** (plugins spec §18.4). `fleets/watch` sends the current list
as its first frame and the whole list again after every change (fleet
records and activation rows alike), pinging every 30 s; a consumer
replaces its state on each frame and reconnects when the socket drops.
`attach` opens a terminal on the agent's window: binary frames carry
bytes both ways, a text frame must be a resize (`{ "resize": { "cols":
120, "rows": 40 } }`, both at least 1) or the daemon closes with 1003;
the daemon closes with 1000 when the window ends and 1011 on a runner
failure. Both take the plugin's bearer on the handshake and answer the
usual 401/403 before upgrading.
```

- [ ] **Step 9: Run the check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A
git commit -m "Serve attach and fleets/watch to plugins over WebSockets

attach (plugins spec §18.4) bridges one socket to the runner's PtyStream
— binary frames both ways, a text frame is a resize, 1003 for anything
else, 1000 when the window ends — for a plugin with the attach
capability that is active on the agent. fleets/watch sends the complete
fleets list on connect and after every change: each actor's snapshot
channel and every registry write tick one counter the handler waits on,
so a consumer replaces its state per frame and never diffs. axum's ws
feature terminates both; the daemon parses no other WebSocket.

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

---

### Task 6: SDK — `routes`, `Host::attach`, `Host::watch_fleets`, `FakeHost` streams, fixtures

**Files:**
- Modify: `crates/balerix-plugin-sdk/Cargo.toml` (tokio-tungstenite, futures-util)
- Modify: `crates/balerix-plugin-sdk/src/plugin.rs` (`Plugin::routes`, nesting)
- Modify: `crates/balerix-plugin-sdk/src/host.rs` (`Clone`, `attach`, `watch_fleets`, `Attach`, `AttachRead`, `AttachWrite`, `FleetWatch`)
- Modify: `crates/balerix-plugin-sdk/src/testing.rs` (`FakeHost::{set_fleets, resizes, attaches}`, the two WS routes, `Harness::{token, get_route}`)
- Modify: `crates/balerix-plugin-sdk/src/lib.rs` (re-exports)
- Modify: `crates/balerix-plugin-sdk/tests/conformance.rs`
- Create: `docs/plugin-protocol/routes.json`, `docs/plugin-protocol/attach-resize.json`, `docs/plugin-protocol/fleets-watch.json`
- Modify: `docs/plugin-protocol.md` §6 (eighteen fixtures, the `transport` field)

**Interfaces:**
- Consumes: `balerix_api::{FleetRecord, ResizeFrame}`, `tokio_tungstenite::{connect_async, tungstenite::Message}`, `futures_util::{StreamExt, SinkExt}`.
- Produces:
  ```rust
  // balerix_plugin_sdk::Plugin
  fn routes(&self) -> Option<axum::Router> { None }         // nested under /v1/routes behind the bearer check
  // balerix_plugin_sdk::host
  #[derive(Clone)] pub struct Host;
  impl Host { pub async fn attach(&self, agent: &str) -> Result<Attach, SdkError>; pub fn watch_fleets(&self) -> FleetWatch }
  pub struct Attach; impl Attach { pub async fn read(&mut self) -> Option<Vec<u8>>; pub async fn write(&mut self, bytes: &[u8]) -> Result<(), SdkError>;
                                    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), SdkError>; pub async fn close(self);
                                    pub fn split(self) -> (AttachRead, AttachWrite) }
  pub struct AttachRead; impl AttachRead { pub async fn read(&mut self) -> Option<Vec<u8>> }
  pub struct AttachWrite; impl AttachWrite { pub async fn write(&mut self, bytes: &[u8]) -> Result<(), SdkError>; pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), SdkError>; pub async fn close(self) }
  pub struct FleetWatch; impl FleetWatch { pub async fn next(&mut self) -> Vec<FleetRecord> }   // reconnects forever; drop to stop
  // balerix_plugin_sdk::testing::FakeHost
  pub fn set_fleets(&self, fleets: Vec<FleetRecord>);        // pushes a watch frame
  pub fn resizes(&self) -> Vec<(String, Value)>;             // (agent, the text frame as JSON)
  pub fn attaches(&self) -> Vec<String>;                     // agents attached, in order
  // balerix_plugin_sdk::testing::Harness
  pub async fn get_route(&self, path: &str, prefix: &str) -> (u16, Vec<(String, String)>, Vec<u8>);   // GET /v1/routes<path> with the bearer and X-Balerix-Forwarded-Prefix
  ```

- [ ] **Step 1: Dependencies and `Plugin::routes`**

`crates/balerix-plugin-sdk/Cargo.toml` `[dependencies]`: add `tokio-tungstenite = { workspace = true }` and `futures-util = { workspace = true }`.

In `crates/balerix-plugin-sdk/src/plugin.rs`, add to the `Plugin` trait after `metrics`:

```rust
    /// The plugin's own HTTP surface, mounted by the daemon under
    /// `/v1/plugins/<name>/` when the manifest says `routes: true`
    /// (plugin-protocol §4.1). Served under `/v1/routes` behind the same
    /// bearer check as every other route; the request carries
    /// `X-Balerix-Forwarded-Prefix` for building links.
    fn routes(&self) -> Option<Router> {
        None
    }
```

and in `router`, between `.with_state(plugin.clone())` and `.layer(middleware::…)`, nest the routes when present. The state is applied first so both routers are `Router<()>`:

```rust
    let base = Router::new()
        .route("/v1/activate", post(activate::<P>))
        .route("/v1/deactivate", post(deactivate::<P>))
        .route("/v1/events", post(events::<P>))
        .route("/v1/intercept", post(intercept::<P>))
        .route("/v1/health", get(health::<P>))
        .route("/v1/metrics", get(metrics::<P>))
        .with_state(plugin.clone());
    let base = match plugin.routes() {
        Some(routes) => base.nest("/v1/routes", routes),
        None => base,
    };
    base.layer(middleware::from_fn_with_state(token, require_daemon_bearer))
        .layer(DefaultBodyLimit::max(1 << 20))
```

Add a test to `plugin.rs`'s tests:

```rust
    #[tokio::test]
    async fn routes_are_nested_behind_the_bearer() {
        struct Routed;
        impl Plugin for Routed {
            fn routes(&self) -> Option<Router> {
                Some(
                    Router::new()
                        .route("/", get(|| async { "root" }))
                        .route("/x", get(|| async { "x" })),
                )
            }
        }
        let (listener, listen) = bind().await.unwrap();
        tokio::spawn(run(listener, Arc::new(Routed), "tok"));
        let c = reqwest::Client::builder().no_proxy().build().unwrap();
        for (path, want) in [("/v1/routes", "root"), ("/v1/routes/x", "x")] {
            let r = c
                .get(format!("http://{listen}{path}"))
                .bearer_auth("tok")
                .send()
                .await
                .unwrap();
            assert_eq!((r.status().as_u16(), r.text().await.unwrap().as_str()), (200, want), "{path}");
        }
        let r = c.get(format!("http://{listen}/v1/routes/x")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 401, "the plugin's routes need the bearer too");
        let r = c
            .get(format!("http://{listen}/v1/routes/nope"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404);
    }
```

Run: `mise x -- cargo test -p balerix-plugin-sdk plugin::tests` → PASS.

- [ ] **Step 2: Write the failing host tests**

In `crates/balerix-plugin-sdk/src/host.rs` tests add:

```rust
    #[tokio::test]
    async fn attach_streams_bytes_and_resizes_through_the_fake_host() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut a = host.attach("payments/backend/bob").await.unwrap();
        a.write(b"ls\n").await.unwrap();
        assert_eq!(a.read().await.as_deref(), Some(&b"ls\n"[..]), "the fake echoes");
        a.resize(120, 40).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while fake.resizes().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            fake.resizes(),
            vec![(
                "payments/backend/bob".to_string(),
                json!({ "resize": { "cols": 120, "rows": 40 } })
            )]
        );
        let (mut rd, mut wr) = a.split();
        wr.write(b"x").await.unwrap();
        assert_eq!(rd.read().await.as_deref(), Some(&b"x"[..]));
        wr.close().await;
        assert_eq!(rd.read().await, None, "closed");
        assert_eq!(fake.attaches(), vec!["payments/backend/bob".to_string()]);
        let mut env = fake.env("web", std::path::Path::new("/s"));
        env.token = "wrong".into();
        let e = Host::new(env).unwrap().attach("f/c/a").await.unwrap_err();
        assert_eq!(e.to_string(), "daemon: HTTP 401: unknown plugin or bad token");
    }

    #[tokio::test]
    async fn watch_fleets_yields_the_current_list_then_every_change_and_reconnects() {
        let fake = FakeHost::start("tok", json!({}), vec![record("payments")]).await;
        let host = Host::new(fake.env("web", std::path::Path::new("/s"))).unwrap();
        let mut watch = host.watch_fleets();
        let first = watch.next().await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name(), "payments");
        fake.set_fleets(vec![record("payments"), record("billing")]);
        let second = watch.next().await;
        assert_eq!(second.len(), 2);
        fake.set_fleets(vec![]);
        assert!(watch.next().await.is_empty());
        // a dropped socket: the next call reconnects and yields the list again
        fake.drop_watchers();
        fake.set_fleets(vec![record("again")]);
        let after = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
            .await
            .expect("reconnected");
        assert_eq!(after[0].name(), "again");
    }
```

(`FakeHost::drop_watchers` closes every open watch socket; part of the fake below.)

- [ ] **Step 3: Run them to see them fail**

Run: `mise x -- cargo test -p balerix-plugin-sdk host::tests`
Expected: compile errors.

- [ ] **Step 4: Implement the streams in `host.rs`**

Add imports:

```rust
use axum::http::HeaderValue;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use balerix_api::ResizeFrame;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
```

change `pub struct Host` to `#[derive(Clone)] pub struct Host`, and add to `impl Host`:

```rust
    /// `ws://` twin of `url`: the streams of §18.4.
    fn ws_url(&self, path: &str) -> String {
        format!(
            "{}/v1/plugin-host/{path}",
            self.env.api_url.replacen("http://", "ws://", 1)
        )
    }

    /// Opens one of the daemon's WebSocket routes with the bearer; a
    /// refused handshake is the daemon's status and error, as for HTTP.
    async fn connect(&self, path: &str) -> Result<Socket, SdkError> {
        let mut req = self
            .ws_url(path)
            .into_client_request()
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", self.env.token))
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        req.headers_mut().insert("authorization", bearer);
        match connect_async(req).await {
            Ok((socket, _)) => Ok(socket),
            Err(tungstenite::Error::Http(resp)) => {
                let status = resp.status().as_u16();
                let body = resp.body().clone().unwrap_or_default();
                Err(Self::status_error(status, &body))
            }
            Err(e) => Err(SdkError::Transport(e.to_string())),
        }
    }

    /// `GET agents/{id}/attach` (WS): a terminal on the agent's window
    /// (plugin-protocol §3 "Streams"). Needs the `attach` capability and
    /// an active pair.
    pub async fn attach(&self, agent: &str) -> Result<Attach, SdkError> {
        let (tx, rx) = self
            .connect(&format!("agents/{agent}/attach"))
            .await?
            .split();
        Ok(Attach {
            rx: AttachRead { rx },
            tx: AttachWrite { tx },
        })
    }

    /// `GET fleets/watch` (WS): the complete fleets list on every change.
    /// Connects lazily and reconnects forever; drop it to stop.
    pub fn watch_fleets(&self) -> FleetWatch {
        FleetWatch {
            host: self.clone(),
            socket: None,
            backoff: BACKOFF_MIN,
        }
    }
```

and the types (after `impl Host`):

```rust
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

fn transport(e: tungstenite::Error) -> SdkError {
    SdkError::Transport(e.to_string())
}

/// The reading half of an attach: terminal output.
pub struct AttachRead {
    rx: SplitStream<Socket>,
}

impl AttachRead {
    /// The next chunk of output; `None` once the daemon closed the stream.
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.rx.next().await {
                Some(Ok(Message::Binary(bytes))) => return Some(bytes.to_vec()),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                Some(Ok(_)) => {}
            }
        }
    }
}

/// The writing half of an attach: keystrokes and resizes.
pub struct AttachWrite {
    tx: SplitSink<Socket, Message>,
}

impl AttachWrite {
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), SdkError> {
        self.tx
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .map_err(transport)
    }

    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), SdkError> {
        let text = serde_json::to_string(&ResizeFrame::new(cols, rows))
            .map_err(|e| SdkError::Transport(e.to_string()))?;
        self.tx.send(Message::Text(text.into())).await.map_err(transport)
    }

    pub async fn close(mut self) {
        let _ = self.tx.send(Message::Close(None)).await;
        let _ = self.tx.close().await;
    }
}

/// A terminal on an agent's window; `split` for a bridge that reads and
/// writes concurrently.
pub struct Attach {
    rx: AttachRead,
    tx: AttachWrite,
}

impl Attach {
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        self.rx.read().await
    }
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), SdkError> {
        self.tx.write(bytes).await
    }
    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<(), SdkError> {
        self.tx.resize(cols, rows).await
    }
    pub async fn close(self) {
        self.tx.close().await;
    }
    pub fn split(self) -> (AttachRead, AttachWrite) {
        (self.rx, self.tx)
    }
}

/// `fleets/watch` as a stream of complete lists (§18.4). Every frame is
/// the whole list, so a reconnect simply yields it again.
pub struct FleetWatch {
    host: Host,
    socket: Option<Socket>,
    backoff: Duration,
}

impl FleetWatch {
    /// The next complete list. Never ends: a dropped socket is reconnected
    /// with a 1–10 s backoff; drop the watch to stop.
    pub async fn next(&mut self) -> Vec<FleetRecord> {
        loop {
            if self.socket.is_none() {
                match self.host.connect("fleets/watch").await {
                    Ok(s) => {
                        self.socket = Some(s);
                        self.backoff = BACKOFF_MIN;
                    }
                    Err(e) => {
                        eprintln!("{}: fleets/watch: {e}; retrying in {:?}", self.host.env.name, self.backoff);
                        tokio::time::sleep(self.backoff).await;
                        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
                        continue;
                    }
                }
            }
            let Some(socket) = self.socket.as_mut() else {
                continue;
            };
            match socket.next().await {
                Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                    Ok(list) => return list,
                    Err(e) => eprintln!("{}: fleets/watch: bad frame: {e}", self.host.env.name),
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => self.socket = None,
                Some(Ok(_)) => {}
            }
        }
    }
}
```

(`Duration` is already imported; `FleetRecord` too. tokio-tungstenite answers pings with pongs on its own, so `Ping` falls into the `_` arm.) In `crates/balerix-plugin-sdk/src/lib.rs` add `pub use host::{Attach, AttachRead, AttachWrite, FleetWatch};`.

- [ ] **Step 5: The fake's streams and helpers**

In `crates/balerix-plugin-sdk/src/testing.rs`: imports `use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade}; use balerix_api::ResizeFrame; use tokio::sync::watch;`. In `Inner`, change `fleets: Vec<FleetRecord>` to `fleets: Mutex<Vec<FleetRecord>>`, and add `fleets_changed: watch::Sender<u64>`, `resizes: Mutex<Vec<(String, Value)>>`, `attaches: Mutex<Vec<String>>`, `watch_epoch: watch::Sender<u64>` (bumped by `drop_watchers`; a watch handler exits when it changes). Initialise them in `start` (`fleets: Mutex::new(fleets)`, `fleets_changed: watch::channel(0).0`, `watch_epoch: watch::channel(0).0`, the vectors empty). The `fleets`/`fleet` handlers read through the mutex (`inner.fleets.lock()…clone()`). Add to `impl FakeHost`:

```rust
    /// Replaces the fleets list and pushes it to every open watch.
    pub fn set_fleets(&self, fleets: Vec<FleetRecord>) {
        *self.inner.fleets.lock().unwrap_or_else(|e| e.into_inner()) = fleets;
        self.inner.fleets_changed.send_modify(|n| *n += 1);
    }

    /// Closes every open watch socket, as a daemon restart would.
    pub fn drop_watchers(&self) {
        self.inner.watch_epoch.send_modify(|n| *n += 1);
    }

    /// Every resize text frame an attach received: (agent, the frame).
    pub fn resizes(&self) -> Vec<(String, Value)> {
        self.inner.resizes.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn attaches(&self) -> Vec<String> {
        self.inner.attaches.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
```

routes in `router`: `.route("/v1/plugin-host/fleets/watch", get(watch_fleets))` before the `fleets/{name}` line, and `.route("/v1/plugin-host/agents/{fleet}/{crew}/{agent}/attach", get(attach))`. The handlers:

```rust
async fn watch_fleets(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let mut changes = inner.fleets_changed.subscribe();
        let mut epoch = inner.watch_epoch.subscribe();
        loop {
            let list = inner.fleets.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let text = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
            tokio::select! {
                changed = changes.changed() => if changed.is_err() { return; },
                _ = epoch.changed() => {
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
                msg = socket.recv() => match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(_)) => {}
                },
            }
        }
    })
}

/// An echo terminal: bytes come back, resizes are recorded, a text
/// frame that is not a resize closes 1003 like the real daemon.
async fn attach(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    AxumPath((f, c, a)): AxumPath<(String, String, String)>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Some(resp) = unauthorized(&inner, &headers) {
        return resp;
    }
    let agent = format!("{f}/{c}/{a}");
    inner
        .attaches
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(agent.clone());
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            match msg {
                Message::Binary(bytes) => {
                    if socket.send(Message::Binary(bytes)).await.is_err() {
                        return;
                    }
                }
                Message::Text(text) => match ResizeFrame::parse(text.as_str()) {
                    Some(frame) => inner
                        .resizes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((agent.clone(), serde_json::to_value(frame).unwrap_or(Value::Null))),
                    None => {
                        let _ = socket
                            .send(Message::Close(Some(CloseFrame {
                                code: 1003,
                                reason: "expected a resize frame".into(),
                            })))
                            .await;
                        return;
                    }
                },
                Message::Close(_) => return,
                _ => {}
            }
        }
    })
}
```

`Harness` gains:

```rust
    /// `GET /v1/routes<path>` as the daemon's proxy would send it: the
    /// bearer and `X-Balerix-Forwarded-Prefix: <prefix>`.
    pub async fn get_route(&self, path: &str, prefix: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let resp = self
            .http
            .get(self.url(&format!("/v1/routes{path}")))
            .bearer_auth(&self.token)
            .header("x-balerix-forwarded-prefix", prefix)
            .send()
            .await
            .unwrap_or_else(|e| panic!("Harness GET {path}: {e}"));
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        (status, headers, body)
    }
```

Run: `mise x -- cargo test -p balerix-plugin-sdk`
Expected: PASS (host, plugin, testing, auth, metrics tests; the conformance count still says 15 — fixed next).

- [ ] **Step 6: Fixtures and the conformance test**

Create `docs/plugin-protocol/routes.json`:

```json
{
  "route": "GET /v1/routes",
  "direction": "daemon-to-plugin",
  "headers": { "authorization": "Bearer tok", "x-balerix-forwarded-prefix": "/v1/plugins/flow" },
  "request": null,
  "status": 200,
  "raw": "aGVsbG8gZnJvbSByb3V0ZXMK"
}
```

`docs/plugin-protocol/attach-resize.json`:

```json
{
  "route": "GET /v1/plugin-host/agents/payments/backend/bob/attach",
  "direction": "plugin-to-daemon",
  "transport": "websocket",
  "frame": { "resize": { "cols": 120, "rows": 40 } },
  "status": 101
}
```

`docs/plugin-protocol/fleets-watch.json`:

```json
{
  "route": "GET /v1/plugin-host/fleets/watch",
  "direction": "plugin-to-daemon",
  "transport": "websocket",
  "frame": [
    {
      "spec": { "name": "payments", "crews": {} },
      "generation": 1,
      "desired": { "state": "up" },
      "stopped": [],
      "status": { "generation": 1, "observed_generation": 1, "phase": "ready", "agents": {} }
    }
  ],
  "status": 101
}
```

In `crates/balerix-plugin-sdk/tests/conformance.rs`: the count becomes `18`; `Reference` implements

```rust
    fn routes(&self) -> Option<axum::Router> {
        Some(axum::Router::new().route(
            "/",
            axum::routing::get(|| async { "hello from routes\n" }),
        ))
    }
```

the router replay filters `.filter(|(_, f)| f["direction"] == "daemon-to-plugin" && f.get("transport").is_none())`, and a third test is added:

```rust
#[tokio::test]
async fn the_host_streams_match_their_fixtures() {
    let fx = fixtures();
    let record: FleetRecord =
        serde_json::from_value(fx["fleets-watch"]["frame"][0].clone()).unwrap();
    let fake = FakeHost::start("tok", json!({}), vec![record]).await;
    let host = Host::new(fake.env("web", Path::new("/s"))).unwrap();
    let mut watch = host.watch_fleets();
    assert_eq!(
        serde_json::to_value(watch.next().await).unwrap(),
        fx["fleets-watch"]["frame"]
    );
    let mut a = host.attach("payments/backend/bob").await.unwrap();
    a.resize(120, 40).await.unwrap();
    a.write(b"ls\n").await.unwrap();
    assert_eq!(a.read().await.as_deref(), Some(&b"ls\n"[..]));
    assert_eq!(fake.resizes()[0].1, fx["attach-resize"]["frame"]);
    assert_eq!(
        fx["attach-resize"]["route"].as_str().unwrap(),
        "GET /v1/plugin-host/agents/payments/backend/bob/attach"
    );
    assert_eq!(fake.attaches(), vec!["payments/backend/bob".to_string()]);
}
```

`balerix-server/tests/protocol_it.rs` also reads fixtures by name and is unaffected. The daemon's `PluginClient` has no `routes` call: the proxy forwards raw requests, so the `routes.json` fixture is replayed by the SDK test alone; note this in §6 of the doc.

In `docs/plugin-protocol.md` §6: "fifteen fixtures" → "eighteen fixtures"; add `Fixtures with \`"transport": "websocket"\` (\`attach-resize.json\`, \`fleets-watch.json\`) describe one frame, not a request/response pair, and are asserted by the SDK's stream test against \`FakeHost\`; \`routes.json\` is replayed through the SDK router alone — the daemon's proxy forwards requests unparsed.`

Run: `mise x -- cargo test -p balerix-plugin-sdk --test conformance` and `mise x -- cargo test -p balerix-server --test protocol_it`
Expected: PASS.

- [ ] **Step 7: Run the check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A
git commit -m "Give the SDK routes, attach and fleets/watch, and the fake host their twins

Plugin::routes nests a plugin's own router under /v1/routes behind the
daemon-bearer check; Host::attach returns a split-able terminal stream
and Host::watch_fleets a stream of complete fleet lists that reconnects
forever (plugins spec §18.4). FakeHost serves both over WebSockets —
an echo terminal recording resizes, a watch fed by set_fleets — and
three fixtures pin the frames and the routes contract (18 in all).

tokio-tungstenite (no TLS) and futures-util's sink/stream adaptors are
the SDK's new dependencies.

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

---

### Task 7: `balerix-plugin-web` — config, the watch-fed cache, the plugin

**Files:**
- Modify: `Cargo.toml` (member path `balerix-plugin-web`)
- Create: `crates/balerix-plugin-web/Cargo.toml`, `src/lib.rs`, `src/config.rs`, `src/state.rs`, `src/plugin.rs`
- Create: `crates/balerix-plugin-web/tests/plugin_it.rs` (the parts that need no routes yet)

**Interfaces:**
- Consumes: `balerix_plugin_sdk::{Env, Host, Plugin, Metrics, SdkError, FleetWatch}`, `balerix_api::{FleetRecord, AgentPhase}`.
- Produces:
  ```rust
  // balerix_plugin_web::config
  pub struct WebConfig { pub enabled: bool }                    // default true; deny_unknown_fields
  pub struct ConfigError { pub path: String, pub message: String }   // Display "<path>: <message>"
  pub fn parse(config: &Value) -> Result<WebConfig, ConfigError>;
  // balerix_plugin_web::state
  pub struct AgentRow { pub id: String, pub phase: AgentPhase, pub message: String }   // Serialize
  pub struct Cache;  impl Cache { pub fn new() -> Self; pub fn set_enabled(&self, agent: &str, enabled: bool); pub fn remove(&self, agent: &str);
                                  pub fn is_enabled(&self, agent: &str) -> bool; pub fn set_fleets(&self, fleets: Vec<FleetRecord>); pub fn rows(&self) -> Vec<AgentRow> }
  pub fn rows_of(enabled: &BTreeSet<String>, fleets: &[FleetRecord]) -> Vec<AgentRow>;
  // balerix_plugin_web::plugin
  pub struct Shared { pub host: Host, pub cache: Cache, pub terminals_open: IntGauge, pub terminals_total: IntCounter }
  pub struct WebPlugin;  impl WebPlugin { pub fn new(host: Host) -> Result<Self, SdkError>; pub fn shared(&self) -> Arc<Shared>; pub fn start_watch(&self) -> tokio::task::JoinHandle<()> }
  impl Plugin for WebPlugin  // activate/deactivate/metrics; routes() arrives in Task 8
  ```

- [ ] **Step 1: The crate skeleton and the failing config tests**

Add `balerix-plugin-web = { path = "crates/balerix-plugin-web" }` under `balerix-plugin-flow` in `Cargo.toml`'s `[workspace.dependencies]`. Create `crates/balerix-plugin-web/Cargo.toml`:

```toml
[package]
name = "balerix-plugin-web"
description = "The web plugin: agents' terminals in a browser (plugins spec §18.5)"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "balerix-plugin-web"
path = "src/main.rs"

[dependencies]
balerix-api = { workspace = true }
balerix-plugin-sdk = { workspace = true }
axum = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
serde_path_to_error = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
anyhow = { workspace = true }

[dev-dependencies]
tokio-tungstenite = { workspace = true }
futures-util = { workspace = true }
reqwest = { workspace = true }

[lints]
workspace = true
```

`src/lib.rs`:

```rust
//! The web plugin (plugins spec §18.5): agents' terminals in a browser.
//! `config` parses the per-agent block, `state` is the cache `fleets/watch`
//! feeds, `routes` serves the pages and bridges the terminal, `plugin` is
//! the `Plugin` impl.

pub mod config;
pub mod plugin;
pub mod routes;
pub mod state;

pub use config::{ConfigError, WebConfig, parse};
pub use plugin::{Shared, WebPlugin};
pub use state::{AgentRow, Cache, rows_of};
```

(`routes` is created in Task 8; until then leave `pub mod routes;` out and add it there.) A placeholder `src/main.rs` so the crate builds: `fn main() {}` (replaced in Task 8).

`src/config.rs` with tests first:

```rust
//! The `plugins.web` block (plugins spec §18.5): `{ enabled: bool }`,
//! `true` by default, nothing else. Validated at `activate`, with the
//! config path in the error like every balerix config error.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebConfig {
    pub enabled: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// One line, config path first; an empty path prints the message alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConfigError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// Serde's message trimmed to its first clause, as the flow plugin does.
pub fn parse(config: &Value) -> Result<WebConfig, ConfigError> {
    serde_path_to_error::deserialize(config.clone()).map_err(|e| {
        let path = match e.path().to_string() {
            p if p == "." => String::new(),
            p => p,
        };
        let inner = e.into_inner().to_string();
        let message = inner
            .split(", expected one of")
            .next()
            .unwrap_or(&inner)
            .split(", expected `")
            .next()
            .unwrap_or(&inner)
            .to_string();
        ConfigError { path, message }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enabled_defaults_true_and_unknown_fields_carry_their_path() {
        assert_eq!(parse(&json!({})).unwrap(), WebConfig { enabled: true });
        assert_eq!(
            parse(&json!({ "enabled": false })).unwrap(),
            WebConfig { enabled: false }
        );
        assert_eq!(
            parse(&json!({ "nope": 1 })).unwrap_err().to_string(),
            "nope: unknown field `nope`"
        );
        let e = parse(&json!({ "enabled": "yes" })).unwrap_err();
        assert_eq!(e.path, "enabled");
        assert!(e.message.starts_with("invalid type"), "{e}");
        assert!(parse(&json!([])).unwrap_err().path.is_empty());
    }
}
```

Run: `mise x -- cargo test -p balerix-plugin-web config` → PASS.

- [ ] **Step 2: `state.rs` with its tests**

```rust
//! What the index shows (plugins spec §18.5): the agents enabled for web,
//! joined with the latest `fleets/watch` frame. The plugin never calls
//! `GET fleets`; an index that is right at all proves the watch path.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

use balerix_api::{AgentPhase, FleetRecord};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    pub id: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub message: String,
}

#[derive(Default)]
struct Inner {
    enabled: BTreeSet<String>,
    fleets: Vec<FleetRecord>,
}

#[derive(Default)]
pub struct Cache {
    inner: Mutex<Inner>,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `activate`: listed when enabled, hidden otherwise.
    pub fn set_enabled(&self, agent: &str, enabled: bool) {
        let mut i = self.lock();
        if enabled {
            i.enabled.insert(agent.to_string());
        } else {
            i.enabled.remove(agent);
        }
    }

    /// `deactivate`.
    pub fn remove(&self, agent: &str) {
        self.lock().enabled.remove(agent);
    }

    pub fn is_enabled(&self, agent: &str) -> bool {
        self.lock().enabled.contains(agent)
    }

    /// One `fleets/watch` frame: the whole list, replaced.
    pub fn set_fleets(&self, fleets: Vec<FleetRecord>) {
        self.lock().fleets = fleets;
    }

    pub fn rows(&self) -> Vec<AgentRow> {
        let i = self.lock();
        rows_of(&i.enabled, &i.fleets)
    }
}

/// The enabled agents in id order with their phase from the fleets;
/// an enabled agent no fleet knows yet is `pending` with no message.
pub fn rows_of(enabled: &BTreeSet<String>, fleets: &[FleetRecord]) -> Vec<AgentRow> {
    enabled
        .iter()
        .map(|id| {
            let status = fleets.iter().find_map(|f| f.status.agents.get(id));
            AgentRow {
                id: id.clone(),
                phase: status.map_or(AgentPhase::Pending, |s| s.phase),
                message: status.map(|s| s.message.clone()).unwrap_or_default(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::FleetSpec;
    use std::collections::BTreeMap;

    fn fleet(name: &str, agents: &[(&str, AgentPhase, &str)]) -> FleetRecord {
        let mut r = FleetRecord::new(FleetSpec {
            name: name.into(),
            crews: BTreeMap::new(),
        });
        for (id, phase, message) in agents {
            let s = r.status.entry(id);
            s.phase = *phase;
            s.message = (*message).to_string();
        }
        r
    }

    #[test]
    fn rows_are_the_enabled_agents_with_phases_from_the_fleets() {
        let c = Cache::new();
        c.set_enabled("f/c/b", true);
        c.set_enabled("f/c/a", true);
        c.set_enabled("f/c/z", false);
        assert!(c.is_enabled("f/c/a") && !c.is_enabled("f/c/z"));
        assert_eq!(
            c.rows(),
            vec![
                AgentRow { id: "f/c/a".into(), phase: AgentPhase::Pending, message: String::new() },
                AgentRow { id: "f/c/b".into(), phase: AgentPhase::Pending, message: String::new() },
            ],
            "no fleets yet: pending"
        );
        c.set_fleets(vec![
            fleet("f", &[("f/c/a", AgentPhase::Ready, ""), ("f/c/c", AgentPhase::Ready, "")]),
            fleet("g", &[("f/c/b", AgentPhase::Dead, "exit 1")]),
        ]);
        let rows = c.rows();
        assert_eq!(rows[0].phase, AgentPhase::Ready);
        assert_eq!((rows[1].phase, rows[1].message.as_str()), (AgentPhase::Dead, "exit 1"));
        assert_eq!(rows.len(), 2, "c is not enabled");
        c.remove("f/c/a");
        c.set_enabled("f/c/b", false);
        assert!(c.rows().is_empty());
        assert_eq!(
            serde_json::to_value(AgentRow { id: "x".into(), phase: AgentPhase::Ready, message: "m".into() }).unwrap(),
            serde_json::json!({ "id": "x", "phase": "ready", "message": "m" })
        );
    }
}
```

Run: `mise x -- cargo test -p balerix-plugin-web state` → PASS.

- [ ] **Step 3: Write the failing plugin integration test**

Create `crates/balerix-plugin-web/tests/plugin_it.rs`:

```rust
//! The web plugin through the SDK harness (plugins spec §18.5): activation
//! validates, the index follows `fleets/watch`, the bridge echoes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use balerix_api::{AgentPhase, FleetRecord, FleetSpec};
use balerix_plugin_sdk::testing::{FakeHost, Harness, metric};
use balerix_plugin_sdk::{Env, Host};
use balerix_plugin_web::WebPlugin;
use serde_json::{Value, json};

const ALICE: &str = "e2e/c/alice";
const BOB: &str = "e2e/c/bob";

fn fleet(agents: &[(&str, AgentPhase)]) -> FleetRecord {
    let mut r = FleetRecord::new(FleetSpec {
        name: "e2e".into(),
        crews: BTreeMap::new(),
    });
    for (id, phase) in agents {
        r.status.entry(id).phase = *phase;
    }
    r
}

async fn world() -> (FakeHost, Env, Harness, tokio::task::JoinHandle<()>) {
    let fake = FakeHost::start("tok", json!({}), vec![]).await;
    let env = fake.env("web", std::path::Path::new("/s"));
    let plugin = WebPlugin::new(Host::new(env.clone()).unwrap()).unwrap();
    let watch = plugin.start_watch();
    let h = Harness::start(&env, plugin).await;
    (fake, env, h, watch)
}

async fn rows(h: &Harness) -> Vec<Value> {
    let (status, _, body) = h.get_route("/agents.json", "/v1/plugins/web").await;
    assert_eq!(status, 200);
    serde_json::from_slice(&body).unwrap()
}

async fn wait_rows(h: &Harness, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let r = rows(h).await;
            if pred(&r) {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the rows to settle")
}

#[tokio::test]
async fn activation_validates_and_the_index_follows_the_watch() {
    let (fake, _, h, _watch) = world().await;
    assert_eq!(h.activate(ALICE, json!({})).await, Ok(()));
    assert_eq!(h.activate(BOB, json!({ "enabled": false })).await, Ok(()));
    assert_eq!(
        h.activate("e2e/c/carol", json!({ "nope": 1 })).await,
        Err("nope: unknown field `nope`".into())
    );
    let r = wait_rows(&h, |r| r.len() == 1).await;
    assert_eq!(r[0], json!({ "id": ALICE, "phase": "pending", "message": "" }), "no frame yet");

    fake.set_fleets(vec![fleet(&[(ALICE, AgentPhase::Ready), (BOB, AgentPhase::Ready)])]);
    let r = wait_rows(&h, |r| r[0]["phase"] == "ready").await;
    assert_eq!(r.len(), 1, "bob is disabled");

    // enabling bob in place, and a config change back
    assert_eq!(h.activate(BOB, json!({ "enabled": true })).await, Ok(()));
    wait_rows(&h, |r| r.len() == 2 && r[1]["id"] == BOB).await;
    h.deactivate(ALICE).await;
    let r = wait_rows(&h, |r| r.len() == 1).await;
    assert_eq!(r[0]["id"], BOB);
    let _ = h.token();
}
```

Run: `mise x -- cargo test -p balerix-plugin-web --test plugin_it`
Expected: compile error (no `WebPlugin`).

- [ ] **Step 4: `plugin.rs`**

```rust
//! The `Plugin` impl (plugins spec §18.5): a per-agent `enabled` flag, the
//! cache `fleets/watch` feeds, and the two terminal metrics. The routes
//! (Task 8) share `Shared` with it.

use std::sync::Arc;

use balerix_plugin_sdk::metrics::{IntCounter, IntGauge};
use balerix_plugin_sdk::{Host, Metrics, Plugin, SdkError};
use serde_json::Value;

use crate::config::parse;
use crate::state::Cache;

/// What the plugin and its routes both hold.
pub struct Shared {
    pub host: Host,
    pub cache: Cache,
    pub terminals_open: IntGauge,
    pub terminals_total: IntCounter,
}

pub struct WebPlugin {
    shared: Arc<Shared>,
    metrics: Metrics,
}

impl std::fmt::Debug for WebPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebPlugin")
            .field("host", &self.shared.host)
            .finish()
    }
}

impl WebPlugin {
    pub fn new(host: Host) -> Result<Self, SdkError> {
        let metrics = Metrics::new(&host.env().name);
        let terminals_open = metrics.int_gauge("terminals_open", "Browser terminals open now")?;
        let terminals_total =
            metrics.int_counter("terminals_total", "Browser terminals opened since start")?;
        Ok(Self {
            shared: Arc::new(Shared {
                host,
                cache: Cache::new(),
                terminals_open,
                terminals_total,
            }),
            metrics,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// Feeds the cache from `fleets/watch` for as long as the task lives.
    /// Started before `serve`: the route needs the token and the `fleets`
    /// capability, not readiness, and the first frame is the current list.
    pub fn start_watch(&self) -> tokio::task::JoinHandle<()> {
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let mut watch = shared.host.watch_fleets();
            loop {
                shared.cache.set_fleets(watch.next().await);
            }
        })
    }
}

impl Plugin for WebPlugin {
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let cfg = parse(&config).map_err(|e| e.to_string())?;
        self.shared.cache.set_enabled(agent, cfg.enabled);
        eprintln!("web: {agent} {}", if cfg.enabled { "listed" } else { "hidden" });
        Ok(())
    }

    async fn deactivate(&self, agent: &str) {
        self.shared.cache.remove(agent);
    }

    fn metrics(&self) -> Option<&Metrics> {
        Some(&self.metrics)
    }
}
```

`Harness::get_route` hits `/v1/routes/agents.json`, which needs Task 8's router; for this task's test to pass, add a minimal `routes()` now in `plugin.rs` that Task 8 replaces:

```rust
    fn routes(&self) -> Option<axum::Router> {
        let shared = self.shared.clone();
        Some(axum::Router::new().route(
            "/agents.json",
            axum::routing::get(move || {
                let shared = shared.clone();
                async move { axum::Json(shared.cache.rows()) }
            }),
        ))
    }
```

Run: `mise x -- cargo test -p balerix-plugin-web`
Expected: PASS.

- [ ] **Step 5: Run the check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A
git commit -m "Add the web plugin crate: enabled flag, watch-fed cache, metrics

balerix-plugin-web (plugins spec §18.5) lists the agents whose
plugins.web block enables it, with phases from a cache that
fleets/watch feeds and nothing else — the plugin never calls GET
fleets, so a correct index is proof the watch path works. The pages,
assets and the terminal bridge follow.

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

---

### Task 8: `balerix-plugin-web` — routes, the vendored xterm.js, the bridge, the binary, the package

**Files:**
- Create: `scripts/vendor-xterm.sh`, `crates/balerix-plugin-web/assets/{xterm.js,xterm.css,addon-fit.js,LICENSE.xterm,VENDOR.md}`
- Create: `crates/balerix-plugin-web/src/routes.rs`, `src/main.rs` (replacing the placeholder), `package/mise.toml`, `package/balerix-plugin.yaml`
- Modify: `crates/balerix-plugin-web/src/lib.rs`, `src/plugin.rs` (`routes()`), `tests/plugin_it.rs`
- Modify: `scripts/package-plugins.sh`

**Interfaces:**
- Consumes: `Shared`, `Host::attach`, `AttachRead`/`AttachWrite`, `ResizeFrame`, axum `ws`.
- Produces:
  ```rust
  // balerix_plugin_web::routes
  pub const PREFIX_HEADER: &str = "x-balerix-forwarded-prefix";
  pub fn router(shared: Arc<Shared>) -> axum::Router;   // GET /, /agents.json, /agents/{f}/{c}/{a}, /agents/{f}/{c}/{a}/ws, /assets/{file}
  pub fn html_escape(s: &str) -> String;
  pub fn index_html(prefix: &str, rows: &[AgentRow]) -> String;
  pub fn terminal_html(prefix: &str, id: &str) -> String;
  pub async fn bridge(browser: WebSocket, shared: Arc<Shared>, agent: String);
  // package: target/plugins/web/{bin/balerix-plugin-web, mise.toml, balerix-plugin.yaml}
  ```

- [ ] **Step 1: Vendor xterm.js**

Create `scripts/vendor-xterm.sh`:

```bash
#!/usr/bin/env bash
# Fetches the pinned xterm.js build into crates/balerix-plugin-web/assets/
# and verifies every file against VENDOR.md (plugins spec §18.5). cargo
# audit and deny.toml do not cover JavaScript; the recorded digests are
# the supply-chain control. To bump: change the versions below, run this,
# copy the printed digests into VENDOR.md, run it again.
set -euo pipefail
cd "$(dirname "$0")/.."

XTERM=6.0.0
FIT=0.11.0
out=crates/balerix-plugin-web/assets
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -sSfL -o "$tmp/xterm.tgz" "https://registry.npmjs.org/@xterm/xterm/-/xterm-$XTERM.tgz"
curl -sSfL -o "$tmp/fit.tgz" "https://registry.npmjs.org/@xterm/addon-fit/-/addon-fit-$FIT.tgz"
mkdir -p "$tmp/xterm" "$tmp/fit" "$out"
tar -xzf "$tmp/xterm.tgz" -C "$tmp/xterm" package/lib/xterm.js package/css/xterm.css package/LICENSE
tar -xzf "$tmp/fit.tgz" -C "$tmp/fit" package/lib/addon-fit.js
cp "$tmp/xterm/package/lib/xterm.js" "$out/xterm.js"
cp "$tmp/xterm/package/css/xterm.css" "$out/xterm.css"
cp "$tmp/xterm/package/LICENSE" "$out/LICENSE.xterm"
cp "$tmp/fit/package/lib/addon-fit.js" "$out/addon-fit.js"

echo "tarballs:"
(cd "$tmp" && sha256sum xterm.tgz fit.tgz)
echo "files:"
(cd "$out" && sha256sum xterm.js xterm.css addon-fit.js) | tee "$tmp/sums"
while read -r sum file; do
  grep -q "$sum" "$out/VENDOR.md" || { echo "VENDOR.md does not record $file $sum" >&2; exit 1; }
done < "$tmp/sums"
echo "assets match VENDOR.md"
```

`chmod +x scripts/vendor-xterm.sh`. Create `crates/balerix-plugin-web/assets/VENDOR.md`:

```markdown
# Vendored assets

Fetched and verified by `scripts/vendor-xterm.sh`; served by
`crates/balerix-plugin-web/src/routes.rs` under `/assets/`. Minified builds
only, kept verbatim (the trailing `sourceMappingURL` comment is harmless:
no map is served). MIT, `LICENSE.xterm`.

| File | Package | sha256 |
|---|---|---|
| `xterm.js` | `@xterm/xterm` 6.0.0, `package/lib/xterm.js` | `14903579ff54664cd72f8e8699e6961a6272c21863ec1c3b118cdc8af5d4a972` |
| `xterm.css` | `@xterm/xterm` 6.0.0, `package/css/xterm.css` | `854a7c0fb70e8b1a083c16797ab827299fb18744f5ad34f227b48337e33293c6` |
| `addon-fit.js` | `@xterm/addon-fit` 0.11.0, `package/lib/addon-fit.js` | `ba3ea256ce0620a0992a197d6c9baea64823fc93d8da07a9e366ca9943c18527` |

Tarballs: `xterm-6.0.0.tgz`
`908e66e04af6c8dc6b00dd3b54de088e2e81e5ed866284fd6c2fb3c2d1c7a3f6`,
`addon-fit-0.11.0.tgz`
`26003b4517a132b64e4ff228fd88a5fda3fff5e606c76093f6dcff772e9ecec0`
(registry.npmjs.org, recorded 2026-09-07).
```

Run: `scripts/vendor-xterm.sh`
Expected: the three files land (xterm.js is 488663 bytes) and the script prints `assets match VENDOR.md`. If a digest differs, the registry served something else than what was recorded during planning: stop and report, do not edit `VENDOR.md` to match.

- [ ] **Step 2: Write the failing route tests**

Add to `crates/balerix-plugin-web/tests/plugin_it.rs` (imports: `use futures_util::{SinkExt, StreamExt}; use tokio_tungstenite::connect_async; use tokio_tungstenite::tungstenite::client::IntoClientRequest; use tokio_tungstenite::tungstenite::Message;`):

```rust
#[tokio::test]
async fn pages_link_through_the_forwarded_prefix_and_assets_are_served() {
    let (fake, _, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    h.activate(BOB, json!({ "enabled": false })).await.unwrap();
    fake.set_fleets(vec![fleet(&[(ALICE, AgentPhase::Ready)])]);
    wait_rows(&h, |r| r[0]["phase"] == "ready").await;

    let (status, headers, body) = h.get_route("", "/v1/plugins/web").await;
    let body = String::from_utf8(body).unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(headers.iter().any(|(k, v)| k == "content-type" && v.starts_with("text/html")));
    assert!(body.contains(r#"href="/v1/plugins/web/agents/e2e/c/alice""#), "{body}");
    assert!(body.contains(r#"fetch("/v1/plugins/web/agents.json")"#), "{body}");
    assert!(!body.contains("bob"), "disabled agents are not listed: {body}");

    let (status, _, body) = h.get_route("/agents/e2e/c/alice", "/v1/plugins/web").await;
    let body = String::from_utf8(body).unwrap();
    assert_eq!(status, 200);
    assert!(body.contains(r#"src="/v1/plugins/web/assets/xterm.js""#), "{body}");
    assert!(body.contains(r#"src="/v1/plugins/web/assets/addon-fit.js""#), "{body}");
    assert!(body.contains(r#"href="/v1/plugins/web/assets/xterm.css""#), "{body}");
    assert!(body.contains("/v1/plugins/web/agents/e2e/c/alice/ws"), "{body}");
    assert!(body.contains("e2e/c/alice"), "{body}");
    let (status, _, _) = h.get_route("/agents/e2e/c/bob", "/v1/plugins/web").await;
    assert_eq!(status, 404, "hidden agents have no page");
    let (status, _, _) = h.get_route("/agents/e2e/c/bob/ws", "/v1/plugins/web").await;
    assert_ne!(status, 200, "and no bridge");

    for (file, kind, len) in [
        ("xterm.js", "text/javascript", 488663),
        ("xterm.css", "text/css", 7112),
        ("addon-fit.js", "text/javascript", 1521),
    ] {
        let (status, headers, body) = h.get_route(&format!("/assets/{file}"), "/v1/plugins/web").await;
        assert_eq!(status, 200, "{file}");
        assert_eq!(body.len(), len, "{file}");
        assert!(headers.iter().any(|(k, v)| k == "content-type" && v.starts_with(kind)), "{file}: {headers:?}");
        assert!(headers.iter().any(|(k, v)| k == "cache-control" && v.contains("immutable")), "{file}");
    }
    let (status, _, _) = h.get_route("/assets/nope.js", "/v1/plugins/web").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_bridge_relays_bytes_and_resizes_to_the_daemon_attach() {
    let (fake, env, h, _watch) = world().await;
    h.activate(ALICE, json!({})).await.unwrap();
    let url = format!("ws://{}/v1/routes/agents/e2e/c/alice/ws", h.listen());
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {}", env.token).parse().unwrap());
    let (mut ws, _) = connect_async(req).await.unwrap();
    ws.send(Message::Binary(b"ls\n".to_vec().into())).await.unwrap();
    let echo = ws.next().await.unwrap().unwrap();
    assert_eq!(echo.into_data().as_ref(), b"ls\n", "browser → plugin → daemon(fake) → back");
    ws.send(Message::Text(r#"{"resize":{"cols":100,"rows":30}}"#.into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while fake.resizes().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fake.resizes()[0], (ALICE.to_string(), json!({ "resize": { "cols": 100, "rows": 30 } })));
    assert_eq!(fake.attaches(), vec![ALICE.to_string()]);
    let text = h.metrics().await;
    assert_eq!(metric(&text, "balerix_plugin_web_terminals_open", &[]), Some(1.0));
    assert_eq!(metric(&text, "balerix_plugin_web_terminals_total", &[]), Some(1.0));
    ws.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while metric(&h.metrics().await, "balerix_plugin_web_terminals_open", &[]) != Some(0.0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the gauge falls when the browser leaves");
    // a bridge for a hidden agent is refused before any attach
    let mut req = format!("ws://{}/v1/routes/agents/e2e/c/bob/ws", h.listen()).into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {}", env.token).parse().unwrap());
    assert!(connect_async(req).await.is_err());
    assert_eq!(fake.attaches().len(), 1);
}
```

Run: `mise x -- cargo test -p balerix-plugin-web --test plugin_it`
Expected: the two new tests fail (404s from the placeholder router).

- [ ] **Step 3: `routes.rs`**

```rust
//! The pages and the bridge (plugins spec §18.5), under the daemon's
//! mount: every link is built from `X-Balerix-Forwarded-Prefix`. The
//! index polls `agents.json`; the terminal page runs the vendored
//! xterm.js against `agents/{id}/ws`, which relays to the daemon's attach.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use balerix_api::ResizeFrame;

use crate::plugin::Shared;
use crate::state::AgentRow;

pub const PREFIX_HEADER: &str = "x-balerix-forwarded-prefix";

const XTERM_JS: &[u8] = include_bytes!("../assets/xterm.js");
const XTERM_CSS: &[u8] = include_bytes!("../assets/xterm.css");
const ADDON_FIT_JS: &[u8] = include_bytes!("../assets/addon-fit.js");
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// How often the index re-fetches `agents.json`, in milliseconds.
const INDEX_POLL_MS: u32 = 2000;

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/agents.json", get(agents_json))
        .route("/agents/{fleet}/{crew}/{agent}", get(terminal))
        .route("/agents/{fleet}/{crew}/{agent}/ws", get(bridge_route))
        .route("/assets/{file}", get(asset))
        .with_state(shared)
}

/// The mount the daemon put us under, without a trailing slash; empty
/// when called directly (tests, curl).
fn prefix(headers: &HeaderMap) -> String {
    headers
        .get(PREFIX_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_default()
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn phase_label(row: &AgentRow) -> String {
    serde_json::to_value(row.phase)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The index: rows rendered server-side, then refreshed by a small script
/// that rebuilds them from `agents.json` with `textContent` — nothing a
/// message says can become markup.
pub fn index_html(prefix: &str, rows: &[AgentRow]) -> String {
    let mut body = String::new();
    for r in rows {
        body.push_str(&format!(
            "<tr><td><a href=\"{p}/agents/{id}\">{id}</a></td><td>{phase}</td><td>{msg}</td></tr>\n",
            p = html_escape(prefix),
            id = html_escape(&r.id),
            phase = html_escape(&phase_label(r)),
            msg = html_escape(&r.message),
        ));
    }
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>balerix</title>
<style>body{{font:14px system-ui,sans-serif;margin:2rem}}table{{border-collapse:collapse}}td,th{{padding:.3rem .8rem;text-align:left;border-bottom:1px solid #ddd}}</style>
</head><body>
<h1>balerix agents</h1>
<table><thead><tr><th>agent</th><th>phase</th><th>message</th></tr></thead>
<tbody id="rows">
{body}</tbody></table>
<p id="empty" hidden>no agents enabled for web</p>
<script>
const prefix = "{p}";
async function refresh() {{
  try {{
    const rows = await (await fetch("{p}/agents.json")).json();
    const tbody = document.getElementById("rows");
    tbody.replaceChildren();
    for (const r of rows) {{
      const tr = document.createElement("tr");
      const a = document.createElement("a");
      a.href = prefix + "/agents/" + r.id;
      a.textContent = r.id;
      const c1 = document.createElement("td"); c1.appendChild(a);
      const c2 = document.createElement("td"); c2.textContent = r.phase;
      const c3 = document.createElement("td"); c3.textContent = r.message;
      tr.append(c1, c2, c3);
      tbody.appendChild(tr);
    }}
    document.getElementById("empty").hidden = rows.length > 0;
  }} catch (e) {{ console.warn("refresh failed", e); }}
}}
document.getElementById("empty").hidden = document.querySelectorAll("#rows tr").length > 0;
setInterval(refresh, {poll});
</script>
</body></html>
"#,
        p = html_escape(prefix),
        poll = INDEX_POLL_MS,
    )
}

/// The terminal page: xterm.js on a full-window div, the bridge socket,
/// resizes on fit and on window resize, a line when the socket closes.
pub fn terminal_html(prefix: &str, id: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{id}</title>
<link rel="stylesheet" href="{p}/assets/xterm.css">
<style>html,body{{height:100%;margin:0;background:#000}}#t{{height:100%}}</style>
</head><body><div id="t"></div>
<script src="{p}/assets/xterm.js"></script>
<script src="{p}/assets/addon-fit.js"></script>
<script>
const term = new Terminal({{ cursorBlink: true, fontSize: 14 }});
const fit = new FitAddon.FitAddon();
term.loadAddon(fit);
term.open(document.getElementById("t"));
fit.fit();
const scheme = location.protocol === "https:" ? "wss://" : "ws://";
const ws = new WebSocket(scheme + location.host + "{p}/agents/{id}/ws");
ws.binaryType = "arraybuffer";
const resize = () => {{
  if (ws.readyState === WebSocket.OPEN) {{
    ws.send(JSON.stringify({{ resize: {{ cols: term.cols, rows: term.rows }} }}));
  }}
}};
ws.onopen = resize;
ws.onmessage = (e) => term.write(new Uint8Array(e.data));
ws.onclose = (e) => term.write("\r\n[disconnected" + (e.reason ? ": " + e.reason : "") + "]\r\n");
const enc = new TextEncoder();
term.onData((d) => {{ if (ws.readyState === WebSocket.OPEN) ws.send(enc.encode(d)); }});
window.addEventListener("resize", () => {{ fit.fit(); resize(); }});
term.focus();
</script>
</body></html>
"#,
        p = html_escape(prefix),
        id = html_escape(id),
    )
}

async fn index(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Html<String> {
    Html(index_html(&prefix(&headers), &shared.cache.rows()))
}

async fn agents_json(State(shared): State<Arc<Shared>>) -> Json<Vec<AgentRow>> {
    Json(shared.cache.rows())
}

async fn terminal(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path((fleet, crew, agent)): Path<(String, String, String)>,
) -> Response {
    let id = format!("{fleet}/{crew}/{agent}");
    if !shared.cache.is_enabled(&id) {
        return (StatusCode::NOT_FOUND, "no such agent").into_response();
    }
    Html(terminal_html(&prefix(&headers), &id)).into_response()
}

async fn bridge_route(
    State(shared): State<Arc<Shared>>,
    Path((fleet, crew, agent)): Path<(String, String, String)>,
    ws: WebSocketUpgrade,
) -> Response {
    let id = format!("{fleet}/{crew}/{agent}");
    if !shared.cache.is_enabled(&id) {
        return (StatusCode::NOT_FOUND, "no such agent").into_response();
    }
    ws.on_upgrade(move |socket| bridge(socket, shared, id))
}

async fn asset(Path(file): Path<String>) -> Response {
    let (kind, bytes) = match file.as_str() {
        "xterm.js" => ("text/javascript; charset=utf-8", XTERM_JS),
        "xterm.css" => ("text/css; charset=utf-8", XTERM_CSS),
        "addon-fit.js" => ("text/javascript; charset=utf-8", ADDON_FIT_JS),
        _ => return (StatusCode::NOT_FOUND, "no such asset").into_response(),
    };
    (
        [(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, IMMUTABLE)],
        Bytes::from_static(bytes),
    )
        .into_response()
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}

/// One browser tab ↔ one daemon attach: binary both ways, the browser's
/// resize text frames forwarded as resizes, everything else ignored.
/// Ends when either side closes.
pub async fn bridge(mut browser: WebSocket, shared: Arc<Shared>, agent: String) {
    let attach = match shared.host.attach(&agent).await {
        Ok(a) => a,
        Err(e) => {
            close(&mut browser, 1011, &format!("attach: {e}")).await;
            return;
        }
    };
    let (mut rd, mut wr) = attach.split();
    shared.terminals_open.inc();
    shared.terminals_total.inc();
    loop {
        tokio::select! {
            frame = rd.read() => match frame {
                Some(bytes) => {
                    if browser.send(Message::Binary(Bytes::from(bytes))).await.is_err() {
                        break;
                    }
                }
                None => {
                    close(&mut browser, 1000, "the terminal closed").await;
                    break;
                }
            },
            msg = browser.recv() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    if wr.write(&bytes).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Some(f) = ResizeFrame::parse(text.as_str())
                        && wr.resize(f.resize.cols, f.resize.rows).await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    wr.close().await;
    shared.terminals_open.dec();
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::AgentPhase;

    #[test]
    fn pages_escape_what_they_render_and_link_through_the_prefix() {
        assert_eq!(html_escape("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
        let rows = vec![AgentRow {
            id: "f/c/a".into(),
            phase: AgentPhase::Dead,
            message: "<script>alert(1)</script>".into(),
        }];
        let html = index_html("/v1/plugins/web", &rows);
        assert!(html.contains(r#"<a href="/v1/plugins/web/agents/f/c/a">f/c/a</a>"#));
        assert!(html.contains("<td>dead</td>"));
        assert!(html.contains("&lt;script&gt;") && !html.contains("<script>alert"));
        assert!(html.contains(r#"fetch("/v1/plugins/web/agents.json")"#));
        let html = index_html("", &[]);
        assert!(html.contains(r#"fetch("/agents.json")"#), "no prefix: relative to the root");
        let page = terminal_html("/v1/plugins/web", "f/c/a");
        assert!(page.contains(r#"src="/v1/plugins/web/assets/xterm.js""#));
        assert!(page.contains(r#""/v1/plugins/web/agents/f/c/a/ws""#));
        let page = terminal_html("/p", "<x>");
        assert!(page.contains("&lt;x&gt;") && !page.contains("<x>"));
    }
}
```

Add `pub mod routes;` to `lib.rs` (and `pub use routes::{bridge, router as routes_router}` is not needed; keep the module public). Replace the placeholder `routes()` in `plugin.rs` with:

```rust
    fn routes(&self) -> Option<axum::Router> {
        Some(crate::routes::router(self.shared.clone()))
    }
```

Run: `mise x -- cargo test -p balerix-plugin-web`
Expected: PASS (unit + the three integration tests).

- [ ] **Step 4: The binary and the package**

`crates/balerix-plugin-web/src/main.rs`:

```rust
//! `balerix-plugin-web`: read the daemon's environment, start the watch,
//! say hello, serve. Failures print `web: …` to stderr and exit 1; that
//! lands in the plugin's tmux window and `plugins/web/logs/`.

use balerix_plugin_sdk::{Env, Host, serve};
use balerix_plugin_web::WebPlugin;

fn run() -> anyhow::Result<()> {
    let env = Env::from_process()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let host = Host::new(env.clone())?;
        let plugin = WebPlugin::new(Host::new(env)?)?;
        let watch = plugin.start_watch();
        eprintln!("web: starting");
        let result = serve(&host, env!("CARGO_PKG_VERSION"), plugin).await;
        watch.abort();
        result?;
        Ok(())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("web: {e:#}");
        std::process::exit(1);
    }
}
```

`crates/balerix-plugin-web/package/balerix-plugin.yaml`:

```yaml
apiVersion: balerix/v1
kind: Plugin
name: web
version: 0.1.0
protocol: 1
start: serve
# no hook subscriptions: phases come from fleets/watch (plugins spec §18.5)
needs: [fleets, attach]
routes: true
```

`crates/balerix-plugin-web/package/mise.toml`:

```toml
# The development package layout (plugins spec §17.6): the binary is copied
# into bin/ by `mise run package-plugins`. A release package pins the binary
# as a mise tool instead and carries no binary of its own.
[tools]

[tasks.serve]
run = "./bin/balerix-plugin-web"
```

In `scripts/package-plugins.sh` change `for name in flow; do` to `for name in flow web; do`.

Run: `mise run package-plugins` → prints `packaged flow -> …` and `packaged web -> …`; `ls target/plugins/web` shows `bin/ balerix-plugin.yaml mise.toml`.

- [ ] **Step 5: Run the check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A
git commit -m "Serve the web plugin's pages, bridge and vendored xterm.js

The index lists the enabled agents and polls agents.json; the terminal
page runs @xterm/xterm 6.0.0 with the fit addon against agents/<id>/ws,
which relays bytes and resizes to the daemon's attach (plugins spec
§18.5). Every link is built from X-Balerix-Forwarded-Prefix. The three
asset files are vendored verbatim with their digests in VENDOR.md and
scripts/vendor-xterm.sh to re-fetch and verify them — cargo audit does
not cover JavaScript, the digest is the control. package-plugins now
assembles target/plugins/web/ next to flow.

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

---

### Task 9: The e2e `web_journey` and `verify-claude`

**Files:**
- Modify: `crates/balerix/Cargo.toml` (dev: tokio-tungstenite, futures-util)
- Modify: `crates/balerix/tests/e2e.rs`
- Modify: `scripts/verify-claude.sh`

**Interfaces:**
- Consumes: everything above through the real binary, nono, mise and tmux; `balerix plugin open`; `target/plugins/{flow,web}`.
- Produces: the §14 / §18.6 e2e verdict and the by-hand browser step.

- [ ] **Step 1: Write the journey**

In `crates/balerix/Cargo.toml` `[dev-dependencies]` add `tokio-tungstenite = { workspace = true }` and `futures-util = { workspace = true }` (and `tokio` is already a dependency).

Append to `crates/balerix/tests/e2e.rs`:

```rust
/// Where `mise run package-plugins` left the web package; `None` when it
/// has not been run.
fn web_package() -> Option<PathBuf> {
    let dir = Path::new(BALERIX).parent()?.parent()?.join("plugins/web");
    dir.join("bin/balerix-plugin-web").exists().then_some(dir)
}

/// `GET` with explicit headers, no redirects followed: status, headers, body.
fn raw_get(url: &str, headers: &[(&str, &str)]) -> (u16, Vec<(String, String)>, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().unwrap();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let text = resp.body_mut().read_to_string().unwrap();
    (status, headers, text)
}

/// Plugins spec §14 and §18.6, "done when": through a real daemon, nono
/// and tmux, `plugin open` mints a login URL, the browser's cookie opens
/// the mount from the daemon's origin only, the index lists alice from
/// the watch-fed cache, a WebSocket through the proxy shows fake-claude's
/// pane and types into it, and `down` empties the index.
#[test]
fn web_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let (Some(flow_pkg), Some(web_pkg)) = (flow_package(), web_package()) else {
        assert!(!require_or_skip(
            "target/plugins/{flow,web} (run `mise run package-plugins`)",
            false
        ));
        return;
    };
    reap_earlier_runs();
    let root = TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), "e2e-web");
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-web-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: flow\n    source: \"{}\"\n  - name: web\n    source: \"{}\"\n",
            flow_pkg.display(),
            web_pkg.display()
        ),
    )
    .unwrap();

    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let fleet = root.join("fleet.yaml");
    fs::write(&fleet, fleet_yaml(&bare, None, Some("{ web: {} }"))).unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();
    wait_plugin_ready(&w, "flow", &w.state().join("plugins/flow"));
    let list = wait_plugin_ready(&w, "web", &w.state().join("plugins/web"));
    assert!(
        list.lines().any(|l| l.starts_with("web") && l.contains("yes")),
        "ROUTES column: {list}"
    );

    let out = w.ok(&[
        "up",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert!(out.contains("web=active"), "{out}");

    // plugin open → a login URL on the daemon's origin; the browser's GET
    // becomes a cookie, once
    let login = w.ok(&["plugin", "open", "web"]).trim().to_string();
    assert!(login.starts_with(&format!("{url}/v1/login/")), "{login}");
    assert!(login.ends_with("?to=/v1/plugins/web/"), "{login}");
    let (status, headers, _) = raw_get(&login, &[]);
    assert_eq!(status, 303);
    let cookie = headers
        .iter()
        .find(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.split(';').next().unwrap().to_string())
        .expect("a session cookie");
    assert!(cookie.starts_with("balerix_session="), "{cookie}");
    let (status, _, _) = raw_get(&login, &[]);
    assert_eq!(status, 404, "single use");

    // the mount: cookie from the daemon's origin only; no cookie, no entry
    let mount = format!("{url}/v1/plugins/web/");
    let (status, _, _) = raw_get(&mount, &[]);
    assert_eq!(status, 401);
    let (status, _, body) = raw_get(&mount, &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")]);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("e2e/c/alice"), "{body}");
    assert!(!body.contains("e2e/c/bob"), "bob has no web block: {body}");
    let (status, _, _) = raw_get(&mount, &[("Cookie", &cookie), ("Origin", "http://evil.example")]);
    assert_eq!(status, 403);
    // the index's rows come from fleets/watch: alice is ready there
    let start = Instant::now();
    let rows = loop {
        let (_, _, body) = raw_get(&format!("{mount}agents.json"), &[("Cookie", &cookie)]);
        let rows: serde_json::Value = serde_json::from_str(&body).unwrap();
        if rows[0]["phase"] == "ready" {
            break rows;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "agents.json never showed alice ready: {body}");
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_eq!(rows[0]["id"], "e2e/c/alice");
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let (status, _, page) = raw_get(&format!("{mount}agents/e2e/c/alice"), &[("Cookie", &cookie)]);
    assert_eq!(status, 200);
    assert!(page.contains("/v1/plugins/web/assets/xterm.js"), "{page}");
    let (status, _, js) = raw_get(&format!("{mount}assets/xterm.js"), &[("Cookie", &cookie)]);
    assert_eq!((status, js.len()), (200, 488663));

    // the terminal: through the proxy, the plugin and the daemon's attach
    // to alice's tmux window — fake-claude's pane appears, typed bytes
    // reach its stdin
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::Message;
        let ws_url = format!(
            "{}/v1/plugins/web/agents/e2e/c/alice/ws",
            url.replacen("http://", "ws://", 1)
        );
        let mut req = ws_url.clone().into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        req.headers_mut().insert("origin", url.parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.expect("the browser's socket through the proxy");
        ws.send(Message::Text(r#"{"resize":{"cols":120,"rows":40}}"#.into())).await.unwrap();
        let mut seen = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match ws.next().await.expect("open").unwrap() {
                    Message::Binary(b) => {
                        seen.extend_from_slice(&b);
                        if String::from_utf8_lossy(&seen).contains("fake-claude") {
                            return;
                        }
                    }
                    Message::Close(f) => panic!("closed early: {f:?}; saw {:?}", String::from_utf8_lossy(&seen)),
                    _ => {}
                }
            }
        })
        .await
        .expect("fake-claude's pane over the terminal");
        ws.send(Message::Binary(b"hello-from-browser\n".to_vec().into())).await.unwrap();
        let stdin = tokio::task::spawn_blocking({
            let path = w.agent_dir("alice").join("home/fake-claude.stdin");
            move || wait_file_until(&path, |s| s.contains("hello-from-browser"))
        })
        .await
        .unwrap();
        assert!(stdin.contains("hello-from-browser"), "{stdin}");
        ws.close(None).await.unwrap();
        // a cross-origin socket is refused at the handshake
        let mut req = ws_url.into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        req.headers_mut().insert("origin", "http://evil.example".parse().unwrap());
        let e = tokio_tungstenite::connect_async(req).await.unwrap_err();
        assert!(matches!(e, tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 403), "{e}");
    });

    // metrics: the proxy counted, the plugin's gauge rose and fell
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let start = Instant::now();
    loop {
        let m = agent
            .get(format!("{url}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        if m.contains("balerix_plugin_proxy_requests_total{plugin=\"web\",status=\"200\"}")
            && m.contains("balerix_plugin_web_terminals_total 1")
            && m.contains("balerix_plugin_web_terminals_open 0")
        {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "metrics never settled:\n{m}");
        std::thread::sleep(Duration::from_millis(200));
    }

    // no attach session survives the socket; the crew session does
    let sessions = String::from_utf8_lossy(
        &Command::new(&w.tmux)
            .args(["-L", &w.socket, "list-sessions", "-F", "#{session_name}"])
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned();
    assert!(!sessions.contains("balerix-attach-"), "{sessions}");
    assert!(sessions.contains("e2e/c"), "{sessions}");

    // no secret leaks: the admin token, the session cookie and the web token
    let token = fs::read_to_string(w.state().join("server/token")).unwrap();
    let log = fs::read_to_string(w.state().join("server/server.log")).unwrap();
    assert!(!log.contains(token.trim()));
    assert!(!log.contains(cookie.trim_start_matches("balerix_session=")));
    let profile = fs::read_to_string(w.state().join("plugins/web/nono-profile.json")).unwrap();
    let web_token = profile
        .split("\"BALERIX_PLUGIN_TOKEN\": \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    assert!(!log.contains(&web_token));

    // down: alice is deactivated and leaves the index; the tmux group is gone
    let out = w.ok(&["down", "e2e", "--keep", "--timeout", "60s"]);
    assert!(out.contains("e2e  down"), "{out}");
    let start = Instant::now();
    loop {
        let (_, _, body) = raw_get(&format!("{mount}agents.json"), &[("Cookie", &cookie)]);
        if body.trim() == "[]" {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "alice still listed: {body}");
        std::thread::sleep(Duration::from_millis(250));
    }
    drop(w);
}
```

- [ ] **Step 2: Run it**

Run: `mise run e2e`
Expected: PASS, `web_journey` included (it takes ~10 s; the plugin toolchain is the empty `[tools]`, so nothing downloads). If the terminal step never sees `fake-claude`, look at `target/tmp/e2e-web-<pid>/home/.local/state/balerix/plugins/web/logs/` (the plugin's stderr says `web: …`) and the daemon's `server.log` for `attach opened`; a `1011` close carries the runner's error text.

- [ ] **Step 3: `verify-claude` gains the browser step**

In `scripts/verify-claude.sh`, after `BALERIX="$target/debug/balerix"` add:

```bash
# The web plugin, when `mise run package-plugins` has assembled it: the
# by-hand check of plugins spec §14 ("a browser shows a live terminal").
WEB_PKG="$target/plugins/web"
if [ -x "$WEB_PKG/bin/balerix-plugin-web" ]; then WEB=1; else WEB=0; say "web plugin not packaged (mise run package-plugins); skipping the browser step"; fi
```

after the `mkdir -p "$ROOT/xdg/config/balerix" …` line:

```bash
if [ "$WEB" = 1 ]; then
  printf 'plugins:\n  - name: web\n    source: "%s"\n' "$WEB_PKG" > "$ROOT/xdg/config/balerix/plugins.yaml"
fi
```

in the fleet file's agent line, replace `      $AGENT: {}` with:

```bash
      $AGENT: { plugins: { $( [ "$WEB" = 1 ] && printf 'web: {}' ) } }
```

(an empty `plugins: {}` when the plugin is absent), and after the `up` block (before the REPORT banner) add:

```bash
if [ "$WEB" = 1 ] && [ "$UP" = ok ]; then
  hr "browser terminal (plugins spec §14)"
  LOGIN="$("$BALERIX" plugin open web 2>/dev/null || true)"
  if [ -n "$LOGIN" ]; then
    say ">>> Open this once in a browser (valid 60 s; it becomes a session cookie):"
    say ">>>     $LOGIN"
  else
    say "plugin open web failed; see $SERVER/server.log"
  fi
fi
```

and in the non-fake pause text, after the `tmux … attach` hint line, `say ">>> or click $AGENT on the browser page above and type there."`. Also print `web: $WEB` in the report's first line next to `fake:`. In fake mode the step only prints the URL; no browser is involved.

Run: `BALERIX_VERIFY_FAKE=1 mise run verify-claude`
Expected: the report shows `up: ok`, and the browser section prints a `http://127.0.0.1:<port>/v1/login/…?to=/v1/plugins/web/` URL.

- [ ] **Step 4: Run the check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A
git commit -m "Add the web e2e journey and the browser step to verify-claude

web_journey drives the whole phase through a real daemon, nono and
tmux: plugin open mints a login URL, the browser's cookie opens the
mount from the daemon's origin only, the index lists alice from the
watch-fed cache, a WebSocket through the proxy shows fake-claude's pane
and types into its stdin, metrics count it, no attach session survives
the socket, no secret reaches the log, and down empties the index.
verify-claude prints the login URL for the by-hand check with a real
claude (plugins spec §14).

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

---

### Task 10: Docs, the threat model, the example, and the spec's §18.7

**Files:**
- Modify: `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, `AGENTS.md`, `README.md`, `examples/payments.yaml`
- Modify: `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` (§11.1 verdicts, new §18.7)

- [ ] **Step 1: Threat model**

In `docs/THREAT-MODEL.md`:

Trust boundaries — amend the *Plugin ↔ daemon* bullet's route list to include `attach`, `fleets/watch` and the proxied `/v1/routes/*`, and append: `The daemon presents the plugin's own token on every call it makes to the plugin.` Add a bullet:

```markdown
- **Browser ↔ daemon** — the proxied plugin routes (`/v1/plugins/<name>/…`) reached from a browser with a session cookie; keystrokes into agents cross it over loopback. **Untrusted input** on the way in (a browser runs other origins' pages too).
```

Adversaries — add: `- **A page from another origin in the same browser** — can make the browser send requests to `127.0.0.1:<port>`. Wants: to drive the fleet through the operator's session.`

Accepted risks — add:

```markdown
- **A plugin with `attach` has a keyboard into every agent it is active for**, and **anyone holding a browser session can type into every agent web lists** — both are the operator's own choices (`needs` in the manifest, `plugin open` on their own machine); the session dies with the daemon and the cookie never leaves the mount.
- **A local process can still reach a plugin's listener**, but every route on it now needs the plugin's own token, which lives only in the plugin's 0600 profile and the daemon's memory.
```

Mitigations table — add rows:

```markdown
| A browser holding the admin token | it never does: `plugin open` mints a 60 s single-use code (32 random bytes, constant-time lookup), the browser exchanges it for a 12 h in-memory session id in an `HttpOnly; SameSite=Strict; Path=/v1/plugins/` cookie; sessions are never persisted | `crates/balerix-server/src/sessions.rs`, `api.rs::{create_session, login}` |
| Cross-site requests riding the cookie | every cookie-authenticated request must be same-origin: `Sec-Fetch-Site` in `same-origin`/`none`, else `Origin` absent or exactly the daemon's `http://127.0.0.1:<port>`; a wrong bearer is refused before the cookie is consulted; the cookie opens the mount and no other route | `sessions.rs::same_origin`, `api.rs::authenticate_browser_or_admin` |
| The proxy leaking credentials to a plugin, or a plugin's headers reaching the browser | `Authorization`, `Cookie`, `Host` and the hop-by-hop headers are stripped on the way in (`Connection`/`Upgrade` kept only on an upgrade), hop-by-hop headers on the way out; 1 MiB request bodies; the plugin's own token and the forwarded prefix are the only additions | `crates/balerix-server/src/proxy.rs` |
| A local process driving a plugin's listener | every daemon → plugin call carries `Authorization: Bearer <that plugin's token>`; the SDK router answers 401 to anything else, the protocol doc requires it of every plugin | `plugins/client.rs`, `balerix-plugin-sdk/src/plugin.rs::require_daemon_bearer` |
| A viewer moving the operator's tmux client, or agents outliving `down` | each attach is a throwaway session grouped with the crew's (`balerix-attach-<hex>`, `destroy-unattached`), never the crew session itself; `stop_crew` kills every session of the group | `crates/balerix-runtime/src/tmux.rs::{attach, stop_crew}` |
| Terminal bytes or fleet data in the wrong hands | `attach` and `fleets/watch` need the plugin's bearer and, for `attach`, an active pair (404 otherwise); the web plugin bridges only agents its config lists (404 otherwise) | `plugin_api.rs::{attach, watch_fleets}`, `balerix-plugin-web/src/routes.rs` |
| Script injection through agent names or messages on the web pages | names are validated identifiers; every rendered value is HTML-escaped and the polled rows are inserted with `textContent` | `balerix-plugin-web/src/routes.rs::{html_escape, index_html}` |
| The vendored JavaScript | verbatim minified files from the pinned npm tarballs, sha256 recorded in `VENDOR.md` and checked by `scripts/vendor-xterm.sh`; served with `immutable` caching under the mount only | `crates/balerix-plugin-web/assets/VENDOR.md` |
```

- [ ] **Step 2: `ARCHITECTURE.md`**

Pieces — `balerix-server`: add `sessions.rs` (login codes and cookies), `proxy.rs` (the mount), `attach.rs` and `watch.rs` (the two streams) to its description. `balerix-plugin-sdk`: add `Host::attach`/`watch_fleets`, `Plugin::routes`. Add a piece: `- \`balerix-plugin-web\` — the second in-tree plugin: a per-agent \`enabled\` flag (\`config.rs\`), a cache fed by \`fleets/watch\` (\`state.rs\`), the index, terminal page and the bridge to the daemon's attach on a vendored xterm.js (\`routes.rs\`, \`assets/\`).`

How it flows — add after **Flow**:

```markdown
**Proxy, attach, watch, web (Spec B, phase 3):** a manifest with `routes:
true` mounts the plugin under `/v1/plugins/<name>/`: the daemon forwards
to `<listen>/v1/routes[/<rest>]` with the plugin's own token and
`X-Balerix-Forwarded-Prefix`, strips credentials and hop-by-hop headers,
and passes a 101 through as raw bytes. A browser gets in through `balerix
plugin open <name>`: a 60 s single-use code becomes a 12 h in-memory
session cookie, accepted on the mount from the daemon's origin only.
Every daemon → plugin call now carries the plugin's own token and the SDK
router checks it. `GET /v1/plugin-host/agents/{id}/attach` bridges a
WebSocket to `AgentRunner::attach` — on tmux a throwaway session grouped
with the crew's, in a `portable-pty` PTY — and `fleets/watch` sends the
whole fleets list on every actor snapshot or activation change. `web`
lists the agents whose `plugins.web` block enables it, with phases from a
`fleets/watch`-fed cache, and bridges each browser tab to one attach.
```

Non-obvious decisions — add:

```markdown
- **The proxy is a byte-level passthrough.** hyper's legacy client
  forwards any method and streams responses; a 101 is upgraded on both
  sides and `copy_bidirectional` does the rest, so the daemon parses no
  WebSocket frame it does not itself terminate and any subprotocol works
  (§18.1). The root of a mount forwards to `/v1/routes` without a slash:
  axum answers a nested router's `/` there.
- **The admin token never enters the browser.** A single-use login code,
  an in-memory session, an `HttpOnly; SameSite=Strict` cookie scoped to
  `/v1/plugins/`, and a same-origin check on every cookie request (§18.2).
- **The daemon presents the plugin's own token.** A plugin's listener is
  a loopback port any local process can reach; the token it already holds
  is what tells the daemon apart (§18.3). Still protocol 1.
- **An attach is a grouped tmux session, set `destroy-unattached` after
  the client is on it.** Attaching the crew session would flip the
  operator's current window; a grouped session has its own. tmux 3.7c
  destroys a detached session the instant the option lands, so the
  create, select, set sequence runs in the PTY with the client attached.
  `kill-session` on the crew alone leaves windows alive in the group, so
  `stop_crew` kills the group (§18.4).
- **`fleets/watch` frames are the whole list.** A consumer replaces its
  state and never diffs or handles removals; the daemon sends only when
  the list differs from the last frame (§18.4).
- **The web plugin never calls `GET fleets`.** Its index comes from the
  watch-fed cache alone, so a correct index is the watch's test (§18.5).
```

- [ ] **Step 3: `AGENTS.md`**

Tasks — add `- \`vendor-xterm\` is a script, not a task: \`scripts/vendor-xterm.sh\` re-fetches and verifies the web plugin's assets against \`crates/balerix-plugin-web/assets/VENDOR.md\`.` Gotchas — append:

```markdown
- `TmuxRunner::stop_crew` lists sessions with `#{session_group}` and kills
  every session of the crew's group: an attach (`balerix-attach-<hex>`)
  is a session grouped with the crew's, and `kill-session` on the crew
  alone would leave its windows — and the agents — alive in the group.
- Never set `destroy-unattached` on an attach session before its client
  is attached: tmux 3.7c destroys a detached session the moment the option
  lands. `TmuxRunner::attach` runs create, select-window and both
  set-options as one command sequence inside the PTY.
- Every daemon → plugin call carries the plugin's own token; the SDK
  router 401s without it. A test plugin outside the SDK (a raw axum
  router) must be given the token or check nothing; `StubScript
  { expect_token: Some(..) }` makes the server's stub demand it.
- The root of a plugin mount forwards to `/v1/routes` (no slash); axum's
  `nest` answers the nested `/` there and 404s `/v1/routes/`. A plugin's
  `routes()` router registers `/`, not `/index`.
- Cookie-authenticated proxy requests need `Sec-Fetch-Site: same-origin`
  or an `Origin` equal to the exact `http://127.0.0.1:<port>` of the login
  URL; `localhost` is another origin. A test client that sends neither
  passes (a navigation sends neither).
- `balerix plugin open <name>` prints a URL valid for 60 s, once. Opening
  it twice is a 404 by design.
- The web plugin's assets are `include_bytes!` of `assets/`; the crate
  does not build without them. Run `scripts/vendor-xterm.sh` after a
  fresh clone only if the files are missing — they are committed.
- `FleetWatch::next` never returns: a plugin that stops wanting frames
  drops the watch (the web plugin aborts its task at exit).
```

- [ ] **Step 4: `README.md` and the example**

Quickstart — add step 10: `` `mise x -- cargo run -q -p balerix -- plugin open web` — prints a single-use login URL (60 s); open it in a browser to reach the web plugin's index and click an agent for a live terminal. `` and extend step 8 to mention `target/plugins/web/`.

Status — replace the paragraph with: `Spec A and Spec B (plugins) are complete: plugin workloads, the event protocol, the \`flow\` and \`web\` plugins, the proxied plugin mount with browser sessions, attach and \`fleets/watch\`.` Add:

```markdown
### Upgrading to Spec B phase 3
- Every daemon → plugin call now carries `Authorization: Bearer
  <BALERIX_PLUGIN_TOKEN>`; a plugin in another language must check it and
  answer 401 otherwise (plugin-protocol §2, §4). SDK plugins need only a
  rebuild.
- `balerix-plugin-sdk`: `router`/`run` take the token; `Host` is `Clone`;
  `Plugin::routes`, `Host::attach`, `Host::watch_fleets` are new.
- `stop_crew` now kills every tmux session grouped with the crew's.
- `mise run package-plugins` assembles `web` next to `flow`; `test` and
  `e2e` depend on it.
```

`examples/payments.yaml`: change `  plugins: {}` under `defaults` to

```yaml
  plugins:
    web: {}          # every agent gets a browser terminal (balerix plugin open web)
```

- [ ] **Step 5: The spec's §11.1 verdicts and §18.7**

In `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` §11.1, replace the three remaining rows' fallback cells with verdicts: `ttyd --base-path` → `**Not used** (2026-09-07): the web plugin serves a vendored xterm.js and bridges the attach itself (§18.5).`; tmux `attach-session` → `**Verified 2026-09-07 (the day Task 4's test first passed)** (tmux 3.7c, portable-pty 0.9.0, \`tmux_it::attach_streams_the_pane…\`): a grouped session attached in a PTY streams the pane, takes input and resizes, and dies with the stream.`; hyper/hyper-util passthrough → `**Verified 2026-09-07 (the day Task 3's test first passed)** (hyper-util 0.1.20, \`browser_it::the_mount_proxies…\`): a WebSocket echo through the mount round-trips.` Then append:

```markdown
### 18.7 Refinements from the phase 3 plan (2026-09-07)

Where the phase 3 implementation plan refined this section:

- **`PluginAddr { listen, token }`** replaces the bare listen address in
  every daemon → plugin call; the registry learns the token at `hello`
  (`set_listen(name, listen, token)`), `ready_addr` hands both out, and
  `Debug` redacts the token (§18.3).
- **The root of a mount forwards to `/v1/routes`, no trailing slash**:
  axum's `nest` answers the nested `/` at `/v1/routes` and 404s
  `/v1/routes/`; `/v1/plugins/<name>` without a slash stays the purge
  route (§18.2).
- **`destroy-unattached` is set after the client is attached**, in one
  command sequence inside the PTY (`new-session -t =<crew> -s
  balerix-attach-<hex> ; select-window -t =<agent> ; set-option
  destroy-unattached on ; set-option status off`): tmux 3.7c destroys a
  detached session the moment the option lands (§18.4).
- **`FleetWatch::next` returns `Vec<FleetRecord>` and never ends**;
  drop the watch to stop (§18.4). **`Host` is `Clone`**; **`Plugin::routes`
  defaults to `None`**.
- **The login `to` path is `[A-Za-z0-9/._~-]` under `/v1/plugins/`**, so
  the login URL carries it unencoded (§18.2).
- **The proxy's status counter labels an unparseable plugin name
  `unknown`** so guesses cannot grow the series.
- **Eighteen fixtures**: `activate-bad-token` (Task 1), `routes`,
  `attach-resize` and `fleets-watch` (Task 6); the last two carry
  `transport: "websocket"` and one `frame`, and are asserted by the SDK's
  stream test rather than replayed over HTTP.
- **`StubScript.expect_token`** makes the server's stub plugin refuse the
  wrong bearer, so `protocol_it` proves the daemon sends it.
- **The web plugin's index polls every 2 s** and renders polled rows with
  `textContent`; the terminal page sends a resize on open, on fit and on
  window resize (§18.5).
- **`balerix plugin open` prints only.** No browser is launched.
- **The §18.6 watch property test is not written**: with every frame the
  whole list, "a consumer that replaces its state ends equal to `GET
  fleets`" holds by construction; `streams_it` asserts the frames the
  daemon sends and `state.rs` the cache that replaces on each.
```

- [ ] **Step 6: Run the check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add -A
git commit -m "Document Spec B phase 3: the proxy, browser sessions, attach, watch and web

The threat model gains the browser boundary and the daemon-to-plugin
bearer; ARCHITECTURE.md the phase 3 flow and its decisions; AGENTS.md
the gotchas the plan found (session groups, destroy-unattached, the
mount root, the same-origin rule); README.md the plugin open step and
the upgrade notes; the example enables web for every agent; the spec
records the §11.1 verdicts and the plan's refinements as §18.7. Spec B
is complete.

Claude-Session: https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q"
```

Then open the pull request for `spec-b3-proxy-attach-web` with `gh pr create`, its description ending with `https://claude.ai/code/session_018qN8Bobnezerfe5JRUiM3q`.

---

## Done when

- `mise run check` passes here and in CI with `BALERIX_REQUIRE_TOOLS=1`, `web_journey` included, within the five-minute budget.
- By hand (`mise run verify-claude` with a real `claude`): the printed login URL opens the index in a browser, the agent's page shows a live Claude terminal, typing there reaches Claude, and the tmux `list-sessions` shows the `balerix-attach-` session only while the tab is open.
- Every §11.1 row of the plugins spec has a recorded verdict.
- `cargo mutants -p balerix-core` still reports no surviving mutants in `reconcile` (nothing in `reconcile` changed).
- No secret — admin token, session id, plugin token, hook secret — appears in `server.log`, a `launch.sh`, or any `Debug` output the tests exercise.

## Deliberately out of scope (spec §18.6)

No `balerix attach` CLI, no persistent sessions, no logout route, no browser launch, no TLS, no live-push index. A plugin's `/v1/routes/*` is the only browser surface.
