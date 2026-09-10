# Balerix Spec B / Phase 2b — Flow Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `balerix-plugin-flow`, the per-agent state machine of plugins spec §8.1, as the first in-tree plugin on the SDK: it blocks tool calls and sends text by rule, keeps its state in the daemon's KV across restarts, and exposes its metrics through a registry the SDK now owns. Ends with `mise run package-plugins` assembling `target/plugins/flow/` and the e2e `flow_journey` proving a block, a `send_text`, a transition in `/metrics`, a rejected `update` and a reset on `down` through a real daemon, nono and tmux.

**Architecture:** Three additions to `balerix-plugin-sdk` first, because the plugin is their first consumer: `Metrics` (a `prometheus::Registry` that prefixes every family with `balerix_plugin_<name>_`, so a plugin cannot trip the daemon's prefix rule), the `Plugin::metrics` signature change to `Option<&Metrics>`, and `testing::Harness` (drives a plugin through the real §4.2 router against `FakeHost`). Then the crate `crates/balerix-plugin-flow`: `config.rs` (typed config, path-qualified validation, compilation into anchored regexes), `machine.rs` (the pure step function and its property tests), `plugin.rs` (the `Plugin` impl owning the agent map, the `Host` for KV and the metrics), `main.rs` (a thin `serve`). `package/` holds the two files a package needs; `scripts/package-plugins.sh` assembles `target/plugins/flow/` and the `test`/`e2e` tasks depend on it. Nothing in `balerix-core`, `balerix-server`, `balerix-config` or `balerix-runtime` changes except the server's two test plugins that returned metrics text.

**Tech Stack:** Rust 1.98.1 (edition 2024); tokio 1.53.1, axum 0.8.9, reqwest 0.13.4 (no TLS), prometheus 0.14.0 (now also in the SDK); **new:** regex 1.13.1 (already in `Cargo.lock` transitively, now a direct exact dependency), serde_path_to_error 0.1.20; serde/serde_json; sha2/hex (config hash); thiserror; anyhow in the binary only; proptest; real `mise`, `nono 0.75.0`, `tmux 3.7c` for the e2e.

**Spec:** `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` (the *plugins spec*), §17 (the phase 2b decisions) on top of §8.1, §7, §9, §11 and §4; `docs/plugin-protocol.md` for the wire contract the plugin speaks. Read §17 whole before any task; §17.2–17.3 before Tasks 3–5; §17.4 before Task 1; §17.5 before Task 2; §17.6 before Task 6; §17.7 before Task 7. Where this plan refines §17 (recorded again in Task 8 as §17.8):

- **`Harness::intercept` takes the event only**: `HookEvent` already carries the agent, so `intercept(agent, event)` in §17.5 would say it twice. `event(agent, name, payload)` is where the agent goes.
- **`Metrics` constructors are `int_counter`, `int_counter_vec`, `int_gauge`, `int_gauge_vec`, plus `render()`**; the SDK re-exports `IntCounter`, `IntCounterVec`, `IntGauge`, `IntGaugeVec`. Registration errors are `SdkError::Metrics`.
- **`metrics.json` is re-recorded**: `TextEncoder` writes a `# HELP` line before `# TYPE`, which the old hand-written fixture lacked. The daemon's `families_ok` already accepts `HELP`.
- **Config errors are a `thiserror` type**, `ConfigError { path, message }` displayed as `<path>: <message>` (AGENTS.md: library errors start with the config path); `activate` turns it into the `String` the SDK expects.
- **Regex errors are collapsed to one line**: `regex`'s syntax errors are multi-line (pattern, caret, `error: …`); only the last line, without `error: `, goes after the path. **Unknown fields** read `unknown field `foo`` (serde's wording without its `, expected one of …` tail).
- **Verify at implementation time** (a §11.1-style row): a binary inside a *directory-source* package is executable under the package's read grant, as the granted `balerix` binary is. Fallback: the manifest's `sandbox` block adds nono's execute grant on the package directory. Task 6 records the verdict in §17.8.

## Global Constraints

Copied from the specs and the phase 2a plan; every task's requirements include these.

- Rust **1.98.1**, `edition = "2024"`, `rust-version = "1.98"`; every tool in `mise.toml` is an exact version. Run cargo as `mise x -- cargo …` or through `mise run <task>`.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass at every commit. `unsafe_code = "forbid"`. No `unwrap`/`expect` outside tests (test modules and `tests/*.rs` carry `#![allow(clippy::unwrap_used, clippy::expect_used)]`). `std::env::set_var` is unsafe in edition 2024 — inject environment through parameters (`Env::from_env`).
- Library crates return `thiserror` errors whose `Display` starts with the config path (`states.working.on[1].match./tool_input/command: …`); only binaries use `anyhow`. The flow binary's failures print `flow: <error>` to stderr and exit 1.
- Dependency direction: `api` leaf → `core` → `config` / `runtime` / `server` / `plugin-sdk` (adapters) → binary. **New layer:** plugin crates (`balerix-plugin-flow`) depend on `balerix-plugin-sdk` and `balerix-api` only — never on `core`, `server`, `runtime` or `config`. `balerix-server`'s dev-dependencies may include the SDK (unchanged).
- New Cargo dependencies go in `[workspace.dependencies]` with an exact version and a reason in the commit message. This plan adds exactly two: `regex = "1.13.1"` and `serde_path_to_error = "0.1.20"` (§17.2). `prometheus` (already a workspace dependency) is added to `balerix-plugin-sdk`. No `futures`, `async-trait`, `tokio-util`.
- Plain HTTP on `127.0.0.1` only (P3-1). Every `reqwest::Client` is built with `.no_proxy()`.
- Secrets never appear in `Debug` output, logs, argv or the environment. The flow plugin logs agent ids, state names and error messages only — never a token; `Host`'s `Debug` already redacts.
- Plugin input is untrusted: the agent's config is validated in full before anything is stored; a `respond` that is not an object is a config error; every `match` value is a regex compiled with a 10 KiB size limit.
- Integration and e2e tests skip with a printed reason when a tool or Landlock is missing; if `BALERIX_REQUIRE_TOOLS=1` is set (CI), a would-be skip panics instead. Temp roots live under `target/tmp` (`CARGO_TARGET_TMPDIR`), never `/tmp`.
- Commit messages: imperative subject, body explains why, and end with the trailer line `Claude-Session: https://claude.ai/code/session_01FRnxV132jJA53c1nVoCoxM`.
- The pre-commit hook runs `mise run precommit` (gitleaks + `check`, e2e included). Every commit below goes through it; the branch is `spec-b2b-flow-plugin`, already holding the §17 spec commit.

## File structure

```
Cargo.toml                                   + regex, serde_path_to_error, balerix-plugin-flow path
mise.toml                                    + package-plugins task; test/e2e depend on it
scripts/package-plugins.sh                   new: build + assemble target/plugins/<name>/
crates/balerix-plugin-sdk/
  Cargo.toml                                 + prometheus
  src/lib.rs                                 + pub mod metrics; pub use metrics::Metrics; SdkError::Metrics
  src/metrics.rs                             new: Metrics (prefixing registry), re-exports
  src/plugin.rs                              Plugin::metrics -> Option<&Metrics>; router encodes
  src/testing.rs                             + Harness, event(), metric(), FakeHost::{kv_json, actions_for}
  tests/conformance.rs                       Reference.metrics via Metrics
crates/balerix-server/tests/events_it.rs     Good via Metrics; Bad as a raw axum router
docs/plugin-protocol/metrics.json            re-recorded raw (HELP + TYPE + sample)
docs/plugin-protocol.md                      §4 metrics paragraph; new §7 Packaging
crates/balerix-plugin-flow/
  Cargo.toml
  src/lib.rs                                 pub mod config; pub mod machine; pub mod plugin; pub use …
  src/config.rs                              FlowConfig/Rule/Send/RuleAction, ConfigError, Compiled, compile()
  src/machine.rs                             step(), Step, matching; proptest
  src/plugin.rs                              FlowPlugin: activate/deactivate/intercept/metrics
  src/main.rs                                Env → Host → serve
  package/mise.toml                          [tools] empty; [tasks.serve] run = "./bin/balerix-plugin-flow"
  package/balerix-plugin.yaml                name flow, all nine events intercepted, needs [actions, kv]
  tests/plugin_it.rs                         Harness-driven plugin tests
crates/balerix/tests/e2e.rs                  + flow_journey
ARCHITECTURE.md, AGENTS.md, README.md, examples/payments.yaml, spec §17.8
```

---

### Task 1: `Metrics` in the SDK and `Plugin::metrics -> Option<&Metrics>`

**Files:**
- Modify: `crates/balerix-plugin-sdk/Cargo.toml`
- Create: `crates/balerix-plugin-sdk/src/metrics.rs`
- Modify: `crates/balerix-plugin-sdk/src/lib.rs`
- Modify: `crates/balerix-plugin-sdk/src/plugin.rs`
- Modify: `crates/balerix-plugin-sdk/tests/conformance.rs:80-84`
- Modify: `docs/plugin-protocol/metrics.json`
- Modify: `docs/plugin-protocol.md:132-135`
- Modify: `crates/balerix-server/tests/events_it.rs:438-480`

**Interfaces:**
- Consumes: `prometheus::{Registry, TextEncoder, Encoder, Opts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec}`; `balerix_server::plugin_api::families_ok` (unchanged, accepts `# HELP`).
- Produces:
  ```rust
  // balerix_plugin_sdk::metrics
  pub struct Metrics { … }
  impl Metrics {
      pub fn new(plugin_name: &str) -> Metrics;
      pub fn plugin_name(&self) -> &str;
      pub fn family(&self, short: &str) -> String;                 // "balerix_plugin_<name>_<short>"
      pub fn int_counter(&self, short: &str, help: &str) -> Result<IntCounter, SdkError>;
      pub fn int_counter_vec(&self, short: &str, help: &str, labels: &[&str]) -> Result<IntCounterVec, SdkError>;
      pub fn int_gauge(&self, short: &str, help: &str) -> Result<IntGauge, SdkError>;
      pub fn int_gauge_vec(&self, short: &str, help: &str, labels: &[&str]) -> Result<IntGaugeVec, SdkError>;
      pub fn render(&self) -> Result<String, SdkError>;            // Prometheus text
  }
  pub use prometheus::{IntCounter, IntCounterVec, IntGauge, IntGaugeVec};
  // balerix_plugin_sdk::SdkError gains `Metrics(String)` — "metrics: {0}"
  // balerix_plugin_sdk::Plugin
  fn metrics(&self) -> Option<&Metrics> { None }                   // was: async -> String
  ```

- [ ] **Step 1: Add `prometheus` to the SDK**

In `crates/balerix-plugin-sdk/Cargo.toml`, under `[dependencies]`, add after `axum`:

```toml
prometheus = { workspace = true }
```

and change the `description` to `"Rust SDK for balerix plugins: environment, the host protocol, metrics, and a test harness"`.

- [ ] **Step 2: Write the failing tests for `Metrics`**

Create `crates/balerix-plugin-sdk/src/metrics.rs` with only the test module for now:

```rust
//! A Prometheus registry that prefixes every family with
//! `balerix_plugin_<name>_` (plugins spec §9, §17.4): the daemon drops a
//! whole scrape when one family lacks the prefix, and a hand-formatted
//! body is how a plugin author trips that rule. Register through this
//! type and the rule cannot be broken.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_is_prefixed_with_the_plugin_name() {
        let m = Metrics::new("flow");
        assert_eq!(m.plugin_name(), "flow");
        assert_eq!(m.family("state"), "balerix_plugin_flow_state");
        let g = m
            .int_gauge_vec("state", "current state", &["agent", "state"])
            .unwrap();
        g.with_label_values(&["a", "working"]).set(1);
        let c = m
            .int_counter_vec("transitions_total", "transitions", &["from", "to"])
            .unwrap();
        c.with_label_values(&["working", "review"]).inc();
        let up = m.int_gauge("up", "1 while serving").unwrap();
        up.set(1);
        let calls = m.int_counter("calls_total", "calls").unwrap();
        calls.inc();
        let text = m.render().unwrap();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let family = match line.strip_prefix('#') {
                Some(rest) => rest.split_whitespace().nth(1).unwrap(),
                None => line.split(|c: char| c == '{' || c == ' ').next().unwrap(),
            };
            assert!(
                family.starts_with("balerix_plugin_flow_"),
                "unprefixed family in: {line}"
            );
        }
        assert!(text.contains("# HELP balerix_plugin_flow_state current state\n"));
        assert!(text.contains("# TYPE balerix_plugin_flow_state gauge\n"));
        assert!(text.contains("balerix_plugin_flow_state{agent=\"a\",state=\"working\"} 1\n"));
        assert!(text.contains(
            "balerix_plugin_flow_transitions_total{from=\"working\",to=\"review\"} 1\n"
        ));
        assert!(text.contains("balerix_plugin_flow_up 1\n"));
        assert!(text.contains("balerix_plugin_flow_calls_total 1\n"));
    }

    #[test]
    fn an_empty_registry_renders_nothing_and_duplicates_are_errors() {
        let m = Metrics::new("web");
        assert_eq!(m.render().unwrap(), "");
        m.int_gauge("up", "x").unwrap();
        let err = m.int_gauge("up", "x").unwrap_err();
        assert!(
            err.to_string().starts_with("metrics: "),
            "{err}"
        );
        let err = m.int_gauge("", "x").unwrap_err();
        assert_eq!(err.to_string(), "metrics: family name is empty");
        let err = m.int_gauge("Bad-Name", "x").unwrap_err();
        assert!(err.to_string().starts_with("metrics: "), "{err}");
    }

    #[test]
    fn debug_shows_the_name_only() {
        let m = Metrics::new("flow");
        assert_eq!(format!("{m:?}"), "Metrics { plugin_name: \"flow\" }");
    }
}
```

In `crates/balerix-plugin-sdk/src/lib.rs` add `pub mod metrics;` after `pub mod host;` and `pub use metrics::Metrics;` after `pub use host::Host;`, and add the variant to `SdkError`:

```rust
    #[error("metrics: {0}")]
    Metrics(String),
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-plugin-sdk metrics`
Expected: compile error — `Metrics` not found.

- [ ] **Step 4: Implement `Metrics`**

Above the test module in `crates/balerix-plugin-sdk/src/metrics.rs`:

```rust
use std::fmt;

use prometheus::{Encoder, Opts, Registry, TextEncoder};
pub use prometheus::{IntCounter, IntCounterVec, IntGauge, IntGaugeVec};

use crate::SdkError;

/// A registry whose every family is `balerix_plugin_<name>_<short>`.
pub struct Metrics {
    plugin_name: String,
    registry: Registry,
}

impl fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Metrics")
            .field("plugin_name", &self.plugin_name)
            .finish()
    }
}

impl Metrics {
    pub fn new(plugin_name: &str) -> Self {
        Self {
            plugin_name: plugin_name.to_string(),
            registry: Registry::new(),
        }
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// The full family name for a short one: `state` →
    /// `balerix_plugin_flow_state`.
    pub fn family(&self, short: &str) -> String {
        format!("balerix_plugin_{}_{short}", self.plugin_name)
    }

    fn opts(&self, short: &str, help: &str) -> Result<Opts, SdkError> {
        if short.is_empty() {
            return Err(SdkError::Metrics("family name is empty".into()));
        }
        Ok(Opts::new(self.family(short), help))
    }

    fn register<C: prometheus::core::Collector + Clone + 'static>(
        &self,
        c: C,
    ) -> Result<C, SdkError> {
        self.registry
            .register(Box::new(c.clone()))
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        Ok(c)
    }

    pub fn int_counter(&self, short: &str, help: &str) -> Result<IntCounter, SdkError> {
        let c = IntCounter::with_opts(self.opts(short, help)?)
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        self.register(c)
    }

    pub fn int_counter_vec(
        &self,
        short: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<IntCounterVec, SdkError> {
        let c = IntCounterVec::new(self.opts(short, help)?, labels)
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        self.register(c)
    }

    pub fn int_gauge(&self, short: &str, help: &str) -> Result<IntGauge, SdkError> {
        let g = IntGauge::with_opts(self.opts(short, help)?)
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        self.register(g)
    }

    pub fn int_gauge_vec(
        &self,
        short: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<IntGaugeVec, SdkError> {
        let g = IntGaugeVec::new(self.opts(short, help)?, labels)
            .map_err(|e| SdkError::Metrics(e.to_string()))?;
        self.register(g)
    }

    /// Prometheus text (`text/plain; version=0.0.4`), every family
    /// prefixed. Empty for an empty registry.
    pub fn render(&self) -> Result<String, SdkError> {
        TextEncoder::new()
            .encode_to_string(&self.registry.gather())
            .map_err(|e| SdkError::Metrics(e.to_string()))
    }
}
```

(`prometheus` validates metric names itself: `Bad-Name` fails at `IntGauge::with_opts`, which the test relies on.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-plugin-sdk metrics`
Expected: 3 passed.

- [ ] **Step 6: Change the trait and the router**

In `crates/balerix-plugin-sdk/src/plugin.rs`:

Replace the `metrics` method of the `Plugin` trait:

```rust
    /// The plugin's registry, rendered by the router as Prometheus text;
    /// every family it holds is already `balerix_plugin_<name>_`-prefixed
    /// (plugins spec §17.4). `None` renders an empty body.
    fn metrics(&self) -> Option<&Metrics> {
        None
    }
```

Add `use crate::{Host, Metrics, SdkError};` (replacing the existing `use crate::{Host, SdkError};`).

Replace the `metrics` handler:

```rust
async fn metrics<P: Plugin>(State(p): State<Arc<P>>) -> Response {
    let body = match p.metrics().map(Metrics::render) {
        None => String::new(),
        Some(Ok(text)) => text,
        Some(Err(e)) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}
```

In the test module of the same file, change `Recorder`: add a field `metrics: Metrics` (so `#[derive(Default)]` goes; write `impl Recorder { fn new() -> Self { let metrics = Metrics::new("rec"); metrics.int_gauge("up", "serving").unwrap().set(1); Self { activated: Mutex::default(), deactivated: Mutex::default(), observed: Mutex::default(), healthy: Mutex::new(false), metrics } } }`), replace its `metrics` impl with:

```rust
        fn metrics(&self) -> Option<&Metrics> {
            Some(&self.metrics)
        }
```

replace `Arc::new(Recorder::default())` with `Arc::new(Recorder::new())`, and change the assertion in `the_router_speaks_section_4_2`:

```rust
        let text = m.text().await.unwrap();
        assert!(
            text.contains("# TYPE balerix_plugin_rec_up gauge\n")
                && text.contains("balerix_plugin_rec_up 1\n"),
            "{text}"
        );
```

`defaults_accept_everything_and_pass_the_response_through` keeps asserting an empty body for `Silent`.

- [ ] **Step 7: Re-record the conformance fixture and its consumers**

In `crates/balerix-plugin-sdk/tests/conformance.rs`, `Reference` becomes a struct holding a registry:

```rust
/// The reference plugin the daemon-to-plugin fixtures describe.
struct Reference {
    metrics: balerix_plugin_sdk::Metrics,
}

impl Reference {
    fn new() -> Self {
        let metrics = balerix_plugin_sdk::Metrics::new("flow");
        metrics
            .int_gauge_vec(
                "state",
                "Current flow state (1 for the current state)",
                &["agent", "state"],
            )
            .unwrap()
            .with_label_values(&["bob", "working"])
            .set(1);
        Self { metrics }
    }
}
```

and its `metrics` impl becomes `fn metrics(&self) -> Option<&balerix_plugin_sdk::Metrics> { Some(&self.metrics) }`; `Arc::new(Reference)` becomes `Arc::new(Reference::new())`.

Replace `docs/plugin-protocol/metrics.json`'s `raw` with the base64 of the three lines `TextEncoder` writes (`# HELP … Current flow state (1 for the current state)`, `# TYPE … gauge`, the sample):

```json
{
  "route": "GET /v1/metrics",
  "direction": "daemon-to-plugin",
  "request": null,
  "status": 200,
  "raw": "IyBIRUxQIGJhbGVyaXhfcGx1Z2luX2Zsb3dfc3RhdGUgQ3VycmVudCBmbG93IHN0YXRlICgxIGZvciB0aGUgY3VycmVudCBzdGF0ZSkKIyBUWVBFIGJhbGVyaXhfcGx1Z2luX2Zsb3dfc3RhdGUgZ2F1Z2UKYmFsZXJpeF9wbHVnaW5fZmxvd19zdGF0ZXthZ2VudD0iYm9iIixzdGF0ZT0id29ya2luZyJ9IDEK"
}
```

(Check it: `echo <raw> | base64 -d` must print exactly those three lines.) `crates/balerix-server/tests/protocol_it.rs` replays the fixture's `raw` through a stub and needs no change.

In `docs/plugin-protocol.md`, replace the `**metrics**` paragraph (§4) with:

```markdown
**`metrics`**: Prometheus text. Every family name — `# TYPE`/`# HELP` lines
and samples alike — must start with `balerix_plugin_<name>_`
(`metrics.json`'s `balerix_plugin_flow_state`); the daemon drops a body
that breaks this rule instead of re-exposing it. The Rust SDK's
`balerix_plugin_sdk::Metrics` registers every family under that prefix and
`Plugin::metrics` returns it for the router to render, so an SDK plugin
cannot break the rule; a plugin in another language formats the text
itself and must apply the prefix.
```

- [ ] **Step 8: Adapt the server's two test plugins**

In `crates/balerix-server/tests/events_it.rs`, test `plugin_metrics_are_re_exported_under_the_prefix_rule`: `Good` keeps using the SDK; `Bad` can no longer produce an unprefixed family through `Metrics`, so it becomes a raw axum router (the daemon's client is what is under test). Replace the two struct definitions and the two `bind`/`spawn` pairs with:

```rust
    struct Good(balerix_plugin_sdk::Metrics);
    impl Plugin for Good {
        fn metrics(&self) -> Option<&balerix_plugin_sdk::Metrics> {
            Some(&self.0)
        }
    }
    let good = balerix_plugin_sdk::Metrics::new("flow");
    good.int_gauge_vec("state", "s", &["agent"])
        .unwrap()
        .with_label_values(&["a"])
        .set(1);
    let (l1, listen1) = bind().await.unwrap();
    tokio::spawn(run(l1, Arc::new(Good(good))));
    // A plugin outside the SDK answering an unprefixed family: the daemon
    // must drop the body whole.
    let bad = axum::Router::new().route(
        "/v1/metrics",
        axum::routing::get(|| async { "balerix_agents{fleet=\"spoof\"} 9\n" }),
    );
    let (l2, listen2) = bind().await.unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l2, bad).await;
    });
```

The assertions below it are unchanged (`balerix_plugin_flow_state{agent="a"} 1` present, `spoof` absent, one scrape failure for `web`).

- [ ] **Step 9: Run the SDK and server tests**

Run: `mise x -- cargo test -p balerix-plugin-sdk && mise x -- cargo test -p balerix-server --test events_it --test protocol_it`
Expected: all pass, including `the_router_answers_every_daemon_to_plugin_fixture` against the re-recorded `metrics.json`.

- [ ] **Step 10: Lint and commit**

Run: `mise x -- cargo fmt --all && mise x -- cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/balerix-plugin-sdk crates/balerix-server/tests/events_it.rs docs/plugin-protocol/metrics.json docs/plugin-protocol.md
git commit -F - <<'EOF'
Give the plugin SDK a prefixing metrics registry

The daemon drops a whole scrape when one family lacks the
balerix_plugin_<name>_ prefix, and a hand-formatted body is exactly how a
plugin author trips that rule. `Metrics` wraps a prometheus registry that
applies the prefix at registration, and `Plugin::metrics` now returns it
for the router to render instead of text. `prometheus` joins the SDK from
the workspace table; the conformance fixture is re-recorded because the
encoder writes a HELP line the hand-written one lacked (plugins spec
§17.4).

Claude-Session: https://claude.ai/code/session_01FRnxV132jJA53c1nVoCoxM
EOF
```

---

### Task 2: `testing::Harness`, `event`, `metric`, and the `FakeHost` helpers

**Files:**
- Modify: `crates/balerix-plugin-sdk/src/testing.rs`
- Modify: `crates/balerix-plugin-sdk/src/plugin.rs` (delete `the_router_speaks_section_4_2`; the harness test below covers it)

**Interfaces:**
- Consumes: `bind`, `run`, `Host::hello`, `FakeHost`, `Metrics` (Task 1).
- Produces:
  ```rust
  // balerix_plugin_sdk::testing
  pub struct Harness { … }
  impl Harness {
      pub async fn start<P: Plugin>(env: &Env, plugin: P) -> Harness;   // binds, serves the router, sends hello(version "test")
      pub fn listen(&self) -> &str;
      pub async fn activate(&self, agent: &str, config: Value) -> Result<(), String>;
      pub async fn deactivate(&self, agent: &str);
      pub async fn observe(&self, events: Vec<HookEvent>);
      pub async fn intercept(&self, event: HookEvent) -> InterceptResponse;           // response_so_far = {}
      pub async fn intercept_with(&self, event: HookEvent, so_far: Value, deadline_ms: u64) -> InterceptResponse;
      pub async fn health(&self) -> Result<(), String>;
      pub async fn metrics(&self) -> String;
      pub async fn restart<P: Plugin>(&mut self, plugin: P);           // new instance, same FakeHost, hello again
  }
  pub fn event(agent: &str, name: &str, payload: Value) -> HookEvent;  // session_id "test", received_at 1
  pub fn metric(text: &str, family: &str, labels: &[(&str, &str)]) -> Option<f64>;
  impl FakeHost {
      pub fn kv_json(&self, key: &str) -> Option<Value>;
      pub fn actions_for(&self, agent: &str) -> Vec<PluginAction>;
  }
  ```

- [ ] **Step 1: Write the failing harness test**

Append to `crates/balerix-plugin-sdk/src/testing.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metrics, Plugin};
    use balerix_api::{HookEvent, InterceptResponse, PluginAction};

    /// Counts intercepts and observed events in gauges; rejects agents
    /// ending in `/bad`; health as constructed.
    struct Counting {
        metrics: Metrics,
        intercepts: crate::metrics::IntGauge,
        observed: crate::metrics::IntGauge,
        healthy: bool,
    }

    impl Counting {
        fn new(healthy: bool) -> Self {
            let metrics = Metrics::new("cnt");
            let intercepts = metrics.int_gauge("intercepts", "n").unwrap();
            let observed = metrics.int_gauge("observed", "n").unwrap();
            Self {
                metrics,
                intercepts,
                observed,
                healthy,
            }
        }
    }

    impl Plugin for Counting {
        async fn activate(&self, agent: &str, _config: Value) -> Result<(), String> {
            if agent.ends_with("/bad") {
                return Err("initial: no state \"x\" declared".into());
            }
            Ok(())
        }
        async fn observe(&self, events: Vec<HookEvent>) {
            self.observed.add(events.len() as i64);
        }
        async fn intercept(
            &self,
            event: HookEvent,
            mut so_far: Value,
            deadline_ms: u64,
        ) -> InterceptResponse {
            self.intercepts.inc();
            so_far["seen"] = json!(event.name);
            so_far["deadline"] = json!(deadline_ms);
            InterceptResponse {
                response: so_far,
                actions: vec![PluginAction::Stop],
            }
        }
        async fn health(&self) -> Result<(), String> {
            if self.healthy { Ok(()) } else { Err("warming up".into()) }
        }
        fn metrics(&self) -> Option<&Metrics> {
            Some(&self.metrics)
        }
    }

    #[tokio::test]
    async fn the_harness_drives_every_route_through_the_wire() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let env = fake.env("cnt", Path::new("/s"));
        let mut h = Harness::start(&env, Counting::new(false)).await;
        assert!(h.listen().starts_with("127.0.0.1:"));
        assert_eq!(fake.hellos().len(), 1, "start said hello");
        assert_eq!(fake.hellos()[0].listen, h.listen());

        assert_eq!(h.activate("f/c/a", json!({ "k": 1 })).await, Ok(()));
        assert_eq!(
            h.activate("f/c/bad", json!({})).await,
            Err("initial: no state \"x\" declared".into())
        );
        h.deactivate("f/c/a").await;
        h.observe(vec![event("f/c/a", "SessionStart", json!({}))]).await;
        let v = h.intercept(event("f/c/a", "PreToolUse", json!({ "tool_name": "Bash" }))).await;
        assert_eq!(v.response, json!({ "seen": "PreToolUse", "deadline": 1500 }));
        assert_eq!(v.actions, vec![PluginAction::Stop]);
        let v = h
            .intercept_with(event("f/c/a", "Stop", json!({})), json!({ "a": 1 }), 7)
            .await;
        assert_eq!(v.response, json!({ "a": 1, "seen": "Stop", "deadline": 7 }));
        assert_eq!(h.health().await, Err("warming up".into()));
        let text = h.metrics().await;
        assert_eq!(metric(&text, "balerix_plugin_cnt_intercepts", &[]), Some(2.0));
        assert_eq!(metric(&text, "balerix_plugin_cnt_observed", &[]), Some(1.0));

        // restart: a new instance, the same FakeHost, a second hello
        h.restart(Counting::new(true)).await;
        assert_eq!(fake.hellos().len(), 2);
        assert_eq!(h.health().await, Ok(()));
        assert_eq!(
            metric(&h.metrics().await, "balerix_plugin_cnt_intercepts", &[]),
            Some(0.0),
            "a fresh instance starts from zero"
        );
    }

    #[test]
    fn event_fills_the_boilerplate_and_metric_parses_the_text_format() {
        let e = event("f/c/a", "PreToolUse", json!({ "x": 1 }));
        assert_eq!(e.agent, "f/c/a");
        assert_eq!(e.name, "PreToolUse");
        assert_eq!(e.session_id.as_deref(), Some("test"));
        assert_eq!(e.payload["x"], 1);

        let text = "# HELP balerix_plugin_flow_state s\n# TYPE balerix_plugin_flow_state gauge\nbalerix_plugin_flow_state{agent=\"a\",fleet=\"f\",state=\"working\"} 1\nbalerix_plugin_flow_state{agent=\"b\",fleet=\"f\",state=\"done\"} 1\nbalerix_plugin_flow_up 1\nbalerix_plugin_flow_ratio 0.5\n";
        assert_eq!(
            metric(text, "balerix_plugin_flow_state", &[("agent", "a")]),
            Some(1.0)
        );
        assert_eq!(
            metric(text, "balerix_plugin_flow_state", &[("agent", "b"), ("state", "done")]),
            Some(1.0)
        );
        assert_eq!(
            metric(text, "balerix_plugin_flow_state", &[("agent", "b"), ("state", "working")]),
            None
        );
        assert_eq!(metric(text, "balerix_plugin_flow_up", &[]), Some(1.0));
        assert_eq!(metric(text, "balerix_plugin_flow_ratio", &[]), Some(0.5));
        assert_eq!(metric(text, "balerix_plugin_flow_nope", &[]), None);
        assert_eq!(
            metric(text, "balerix_plugin_flow_state", &[]),
            Some(1.0),
            "no labels given: the first sample of the family"
        );
    }

    #[tokio::test]
    async fn fake_host_helpers_read_kv_as_json_and_actions_per_agent() {
        let fake = FakeHost::start("tok", json!({}), vec![]).await;
        let host = crate::Host::new(fake.env("p", Path::new("/s"))).unwrap();
        host.kv_put("state/f/c/a", br#"{"state":"working"}"#, false)
            .await
            .unwrap();
        host.kv_put("blob", b"\xff\xfe", false).await.unwrap();
        assert_eq!(fake.kv_json("state/f/c/a"), Some(json!({ "state": "working" })));
        assert_eq!(fake.kv_json("blob"), None, "not JSON");
        assert_eq!(fake.kv_json("missing"), None);
        host.action("f/c/a", &PluginAction::Stop).await.unwrap();
        host.action("f/c/b", &PluginAction::Restart).await.unwrap();
        host.action(
            "f/c/a",
            &PluginAction::SendText {
                text: "hi".into(),
                submit: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fake.actions_for("f/c/a"),
            vec![
                PluginAction::Stop,
                PluginAction::SendText {
                    text: "hi".into(),
                    submit: true
                }
            ]
        );
        assert_eq!(fake.actions_for("f/c/b"), vec![PluginAction::Restart]);
        assert!(fake.actions_for("f/c/zz").is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-plugin-sdk testing::`
Expected: compile errors — `Harness`, `event`, `metric`, `kv_json`, `actions_for` not found.

- [ ] **Step 3: Implement the harness and helpers**

In `crates/balerix-plugin-sdk/src/testing.rs`, extend the module doc's first line to `//! `FakeHost`: an axum app speaking the daemon's plugin-host wire format …; `Harness`: the plugin side, driving a `Plugin` through the real §4.2 router (plugins spec §17.5).` Add to the imports: `use balerix_api::{…, HookEvent, InterceptRequest, InterceptResponse, Timestamp, CHAIN_BUDGET_MS};` and `use crate::{Env, Host, Plugin, SdkError, bind, run};` (replacing `use crate::Env;`).

Add the two `FakeHost` methods inside its `impl`:

```rust
    /// A kv entry parsed as JSON; `None` if absent or not JSON.
    pub fn kv_json(&self, key: &str) -> Option<Value> {
        self.kv()
            .get(key)
            .and_then(|(bytes, _)| serde_json::from_slice(bytes).ok())
    }

    /// The actions posted for one agent, in order.
    pub fn actions_for(&self, agent: &str) -> Vec<PluginAction> {
        self.actions()
            .into_iter()
            .filter(|(a, _)| a == agent)
            .map(|(_, action)| action)
            .collect()
    }
```

Then, after the `delete_key` handler, the harness:

```rust
/// A `HookEvent` with the boilerplate filled: `session_id` "test",
/// `received_at` 1.
pub fn event(agent: &str, name: &str, payload: Value) -> HookEvent {
    HookEvent {
        agent: agent.to_string(),
        name: name.to_string(),
        session_id: Some("test".into()),
        received_at: Timestamp(1),
        payload,
    }
}

/// The value of the first sample of `family` whose labels include every
/// `(name, value)` given — a small reader of the Prometheus text format
/// so a test asserts one number, not a substring.
pub fn metric(text: &str, family: &str, labels: &[(&str, &str)]) -> Option<f64> {
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return None;
        }
        let (name, rest) = match line.find(['{', ' ']) {
            Some(i) => (&line[..i], &line[i..]),
            None => return None,
        };
        if name != family {
            return None;
        }
        let (label_text, value) = match rest.strip_prefix('{') {
            Some(r) => {
                let end = r.find('}')?;
                (&r[..end], r[end + 1..].trim())
            }
            None => ("", rest.trim()),
        };
        let have: Vec<(&str, &str)> = label_text
            .split(',')
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                let (k, v) = p.split_once('=')?;
                Some((k, v.trim_matches('"')))
            })
            .collect();
        if labels.iter().all(|want| have.contains(want)) {
            value.parse().ok()
        } else {
            None
        }
    })
}

/// A plugin served through the real router, spoken to over HTTP.
pub struct Harness {
    host: Host,
    http: reqwest::Client,
    listen: String,
    server: tokio::task::JoinHandle<Result<(), SdkError>>,
}

impl Harness {
    /// Binds a loopback port, serves `plugin`'s router on it and sends
    /// `hello` (version "test") to the `FakeHost` behind `env`.
    pub async fn start<P: Plugin>(env: &Env, plugin: P) -> Harness {
        let host = Host::new(env.clone()).unwrap_or_else(|e| panic!("Harness host: {e}"));
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap_or_else(|e| panic!("Harness client: {e}"));
        let (listen, server) = Self::spawn(&host, plugin).await;
        Harness {
            host,
            http,
            listen,
            server,
        }
    }

    async fn spawn<P: Plugin>(
        host: &Host,
        plugin: P,
    ) -> (String, tokio::task::JoinHandle<Result<(), SdkError>>) {
        let (listener, listen) = bind()
            .await
            .unwrap_or_else(|e| panic!("Harness bind: {e}"));
        let server = tokio::spawn(run(listener, Arc::new(plugin)));
        host.hello("test", &listen)
            .await
            .unwrap_or_else(|e| panic!("Harness hello: {e}"));
        (listen, server)
    }

    pub fn listen(&self) -> &str {
        &self.listen
    }

    /// Stops the served instance and serves `plugin` in its place, with
    /// a fresh `hello`: the "plugin restarted" case. The `FakeHost` and
    /// its kv are untouched.
    pub async fn restart<P: Plugin>(&mut self, plugin: P) {
        self.server.abort();
        let _ = (&mut self.server).await;
        let (listen, server) = Self::spawn(&self.host, plugin).await;
        self.listen = listen;
        self.server = server;
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.listen)
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let resp = self
            .http
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("Harness POST {path}: {e}"));
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn message(body: &Value) -> String {
        body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| body.to_string())
    }

    pub async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let (status, body) = self
            .post("/v1/activate", &json!({ "agent": agent, "config": config }))
            .await;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(Self::message(&body))
        }
    }

    pub async fn deactivate(&self, agent: &str) {
        let (status, body) = self.post("/v1/deactivate", &json!({ "agent": agent })).await;
        assert_eq!(status, 200, "deactivate: {body}");
    }

    pub async fn observe(&self, events: Vec<HookEvent>) {
        let (status, body) = self.post("/v1/events", &json!({ "events": events })).await;
        assert_eq!(status, 200, "events: {body}");
    }

    /// `intercept` with `response_so_far` `{}` and the full chain budget.
    pub async fn intercept(&self, event: HookEvent) -> InterceptResponse {
        self.intercept_with(event, json!({}), CHAIN_BUDGET_MS).await
    }

    pub async fn intercept_with(
        &self,
        event: HookEvent,
        so_far: Value,
        deadline_ms: u64,
    ) -> InterceptResponse {
        let req = InterceptRequest {
            event,
            response_so_far: so_far,
            deadline_ms,
        };
        let body = serde_json::to_value(&req).unwrap_or_else(|e| panic!("intercept body: {e}"));
        let (status, body) = self.post("/v1/intercept", &body).await;
        assert_eq!(status, 200, "intercept: {body}");
        serde_json::from_value(body).unwrap_or_else(|e| panic!("intercept reply: {e}"))
    }

    pub async fn health(&self) -> Result<(), String> {
        let resp = self
            .http
            .get(self.url("/v1/health"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("Harness GET health: {e}"));
        if resp.status().is_success() {
            Ok(())
        } else {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            Err(Self::message(&body))
        }
    }

    pub async fn metrics(&self) -> String {
        self.http
            .get(self.url("/v1/metrics"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("Harness GET metrics: {e}"))
            .text()
            .await
            .unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}
```

(`InterceptRequest` derives `Serialize` — `crates/balerix-api/src/protocol.rs:123` — and `CHAIN_BUDGET_MS` is re-exported by `balerix_api` — `lib.rs:27`.)

Delete `the_router_speaks_section_4_2` and the `Recorder` struct from `crates/balerix-plugin-sdk/src/plugin.rs`'s test module (the harness test above covers every route, through the same router); keep `Silent`, `defaults_accept_everything_and_pass_the_response_through`, `serve_binds_says_hello_and_runs` and `a_failed_hello_stops_the_server_and_frees_the_port`. Remove the now-unused imports from that test module (`PluginAction`, `Timestamp`, `Mutex`, `Metrics` — whatever `cargo clippy` flags).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-plugin-sdk`
Expected: all pass, including the three new `testing::tests`.

- [ ] **Step 5: Lint and commit**

Run: `mise x -- cargo fmt --all && mise x -- cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/balerix-plugin-sdk
git commit -F - <<'EOF'
Add the SDK test harness for the plugin side

`FakeHost` covered only the daemon side; every plugin test had to hand-build
hook events and post JSON at the router. `testing::Harness` serves a plugin
through the real §4.2 router and speaks to it over the wire — activate with
its rejection message, intercept with a response so far, health, metrics —
and `restart` swaps the instance against the same fake host so "a restarted
plugin resumes" is one call. `event` and `metric` keep tests to one
assertion per rule (plugins spec §17.5). The router's own route test moves
onto it.

Claude-Session: https://claude.ai/code/session_01FRnxV132jJA53c1nVoCoxM
EOF
```

---

### Task 3: The flow crate and its config language

**Files:**
- Modify: `Cargo.toml` (workspace)
- Create: `crates/balerix-plugin-flow/Cargo.toml`
- Create: `crates/balerix-plugin-flow/src/lib.rs`
- Create: `crates/balerix-plugin-flow/src/config.rs`
- Create: `crates/balerix-plugin-flow/src/main.rs` (a stub that compiles; Task 6 fills it)

**Interfaces:**
- Consumes: `balerix_api::HOOK_EVENTS`; `regex::RegexBuilder`; `serde_path_to_error::deserialize`; `sha2::Sha256`.
- Produces:
  ```rust
  // balerix_plugin_flow::config
  pub struct FlowConfig { pub initial: String, pub states: BTreeMap<String, StateConfig> }
  pub struct StateConfig { pub on: Vec<Rule> }
  pub struct Rule { pub event: String, pub matches: BTreeMap<String, String>, pub goto: Option<String>,
                    pub respond: Option<serde_json::Map<String, Value>>, pub send: Option<Send>, pub action: Option<RuleAction> }
  pub struct Send { pub text: String, pub submit: bool }        // submit defaults to true
  pub enum RuleAction { Stop, Restart }
  #[error("{path}: {message}")] pub struct ConfigError { pub path: String, pub message: String }  // path "" → message alone
  pub struct Compiled { pub initial: String, pub states: BTreeMap<String, Vec<CompiledRule>>, pub hash: String }
  pub struct CompiledRule { pub event: String, pub matches: Vec<(String, regex::Regex)>, pub goto: Option<String>,
                            pub respond: serde_json::Map<String, Value>, pub send: Option<Send>, pub action: Option<RuleAction> }
  pub const REGEX_SIZE_LIMIT: usize = 10 * 1024;
  pub fn parse(config: &Value) -> Result<FlowConfig, ConfigError>;
  pub fn compile(config: &Value) -> Result<Compiled, ConfigError>;    // parse + validate + regexes + hash
  pub fn config_hash(config: &Value) -> String;                        // sha256 hex of the canonical JSON
  ```

- [ ] **Step 1: Register the crate and the two dependencies**

In the workspace `Cargo.toml`, add to `[workspace.dependencies]`:

```toml
balerix-plugin-flow = { path = "crates/balerix-plugin-flow" }
```

after `balerix-plugin-sdk`, and after `prometheus`:

```toml
# The flow plugin's rule matcher (plugins spec §8.1, §17.2): full-match
# regexes with a 10 KiB size limit. Already in the lock through
# tracing-subscriber; now a direct, exact dependency.
regex = "1.13.1"
# Attaches the config path (`states.working.on[1].foo`) to serde's error so
# a plugin config error reads like every other balerix config error
# (plugins spec §17.2); a hand-written walker would duplicate the schema.
serde_path_to_error = "0.1.20"
```

Create `crates/balerix-plugin-flow/Cargo.toml`:

```toml
[package]
name = "balerix-plugin-flow"
description = "The flow plugin: a per-agent state machine over hook events (plugins spec §8.1)"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "balerix-plugin-flow"
path = "src/main.rs"

[dependencies]
balerix-api = { workspace = true }
balerix-plugin-sdk = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
serde_path_to_error = { workspace = true }
regex = { workspace = true }
sha2 = { workspace = true }
hex = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
anyhow = { workspace = true }

[dev-dependencies]
proptest = { workspace = true }

[lints]
workspace = true
```

Create `crates/balerix-plugin-flow/src/lib.rs`:

```rust
//! The flow plugin (plugins spec §8.1, §17): a per-agent state machine over
//! hook events. `config` parses and compiles an agent's `plugins.flow`
//! block, `machine` is the pure step, `plugin` is the `Plugin` impl that
//! owns the agents, the KV-backed state and the metrics.

pub mod config;

pub use config::{Compiled, ConfigError, FlowConfig, compile};
```

Create `crates/balerix-plugin-flow/src/main.rs` as a stub for now:

```rust
fn main() {}
```

- [ ] **Step 2: Write the failing config tests**

Create `crates/balerix-plugin-flow/src/config.rs` with the test module first:

```rust
//! The `plugins.flow` block (plugins spec §8.1, §17.2): typed, validated
//! with config-path errors, compiled into anchored regexes. Pure.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn example() -> Value {
        json!({
            "initial": "working",
            "states": {
                "working": { "on": [
                    { "event": "PreToolUse",
                      "match": { "/tool_input/command": "rm -rf.*" },
                      "respond": { "decision": "block", "reason": "no recursive deletes" } },
                    { "event": "Stop", "goto": "review",
                      "send": { "text": "Run the tests and fix any failures.", "submit": true } }
                ] },
                "review": { "on": [ { "event": "Stop", "goto": "done", "action": "stop" } ] },
                "done": {}
            }
        })
    }

    #[test]
    fn the_spec_example_parses_and_compiles() {
        let c = parse(&example()).unwrap();
        assert_eq!(c.initial, "working");
        assert_eq!(c.states.len(), 3);
        let r = &c.states["working"].on[0];
        assert_eq!(r.event, "PreToolUse");
        assert_eq!(r.matches["/tool_input/command"], "rm -rf.*");
        assert_eq!(r.respond.as_ref().unwrap()["decision"], "block");
        assert!(r.goto.is_none() && r.send.is_none() && r.action.is_none());
        let s = c.states["working"].on[1].send.as_ref().unwrap();
        assert_eq!((s.text.as_str(), s.submit), ("Run the tests and fix any failures.", true));
        assert_eq!(c.states["review"].on[0].action, Some(RuleAction::Stop));
        assert!(c.states["done"].on.is_empty(), "`done: {{}}` is a state with no rules");

        let compiled = compile(&example()).unwrap();
        assert_eq!(compiled.initial, "working");
        let (ptr, re) = &compiled.states["working"][0].matches[0];
        assert_eq!(ptr, "/tool_input/command");
        assert!(re.is_match("rm -rf /tmp/x"));
        assert!(!re.is_match("echo rm -rf"), "full match: anchored at both ends");
        assert_eq!(compiled.hash.len(), 64);
        assert_eq!(compiled.hash, config_hash(&example()));
    }

    #[test]
    fn send_submit_defaults_to_true() {
        let c = parse(&json!({
            "initial": "a",
            "states": { "a": { "on": [ { "event": "Stop", "send": { "text": "go" } } ] } }
        }))
        .unwrap();
        assert!(c.states["a"].on[0].send.as_ref().unwrap().submit);
    }

    #[test]
    fn errors_carry_the_config_path() {
        let cases: Vec<(Value, &str)> = vec![
            (
                json!({ "initial": "foo", "states": { "a": {} } }),
                "initial: no state \"foo\" declared",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Foo" } ] } } }),
                "states.a.on[0].event: unknown event \"Foo\"",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop" }, { "event": "Stop", "goto": "x" } ] } } }),
                "states.a.on[1].goto: no state \"x\" declared",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "match": { "tool_input": "x" } } ] } } }),
                "states.a.on[0].match.tool_input: not a JSON pointer (must start with \"/\")",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "match": { "/x": "[" } } ] } } }),
                "states.a.on[0].match./x: unclosed character class",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "foo": 1 } ] } } }),
                "states.a.on[0].foo: unknown field `foo`",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "respond": "block" } ] } } }),
                "states.a.on[0].respond: invalid type: string \"block\", expected a map",
            ),
            (
                json!({ "initial": "a", "states": { "a": { "on": [ { "event": "Stop", "action": "explode" } ] } } }),
                "states.a.on[0].action: unknown variant `explode`",
            ),
            (
                json!({ "states": { "a": {} } }),
                "missing field `initial`",
            ),
            (
                json!({ "initial": "a", "states": { "a": {} }, "extra": true }),
                "extra: unknown field `extra`",
            ),
            (json!("nope"), "invalid type: string \"nope\", expected struct FlowConfig"),
        ];
        for (input, want) in cases {
            let got = compile(&input).unwrap_err().to_string();
            assert_eq!(got, want, "for {input}");
        }
    }

    #[test]
    fn an_oversized_regex_is_rejected_with_the_path() {
        let huge = "a{1,5000}".to_string();
        let err = compile(&json!({
            "initial": "a",
            "states": { "a": { "on": [ { "event": "Stop", "match": { "/x": huge } } ] } }
        }))
        .unwrap_err();
        assert_eq!(err.path, "states.a.on[0].match./x");
        assert!(
            err.message.contains("size limit"),
            "one line, mentions the limit: {}",
            err.message
        );
        assert!(!err.to_string().contains('\n'));
    }

    #[test]
    fn the_hash_is_canonical() {
        let a = json!({ "initial": "a", "states": { "a": {} } });
        let b = json!({ "states": { "a": {} }, "initial": "a" });
        assert_eq!(config_hash(&a), config_hash(&b), "key order does not matter");
        let c = json!({ "initial": "a", "states": { "a": { "on": [] } } });
        assert_ne!(config_hash(&a), config_hash(&c), "an explicit empty `on` is a different document");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-plugin-flow config`
Expected: compile errors — `parse`, `compile`, `config_hash`, `RuleAction` not found.

- [ ] **Step 4: Implement the config module**

Above the test module in `crates/balerix-plugin-flow/src/config.rs`:

```rust
use std::collections::BTreeMap;

use balerix_api::HOOK_EVENTS;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// `RegexBuilder::size_limit` for every `match` pattern (§8.1).
pub const REGEX_SIZE_LIMIT: usize = 10 * 1024;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowConfig {
    pub initial: String,
    pub states: BTreeMap<String, StateConfig>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StateConfig {
    pub on: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub event: String,
    /// JSON pointer into the event payload → full-match regex.
    #[serde(default, rename = "match")]
    pub matches: BTreeMap<String, String>,
    #[serde(default)]
    pub goto: Option<String>,
    /// Top-level keys replace those of the chain's response so far.
    #[serde(default)]
    pub respond: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub send: Option<Send>,
    #[serde(default)]
    pub action: Option<RuleAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Send {
    pub text: String,
    #[serde(default = "default_true")]
    pub submit: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Stop,
    Restart,
}

/// One line, config path first: `states.working.on[1].goto: no state
/// "foo" declared`. An empty path (a top-level serde error) prints the
/// message alone.
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

impl ConfigError {
    fn at(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// A config with its regexes compiled and its states checked.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub initial: String,
    pub states: BTreeMap<String, Vec<CompiledRule>>,
    /// `config_hash` of the block this was compiled from.
    pub hash: String,
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub event: String,
    pub matches: Vec<(String, Regex)>,
    pub goto: Option<String>,
    pub respond: serde_json::Map<String, Value>,
    pub send: Option<Send>,
    pub action: Option<RuleAction>,
}

/// Typed parse with the path of the failing element. serde's own
/// messages are trimmed to their first clause (`unknown field `foo``,
/// not `…, expected one of …`).
pub fn parse(config: &Value) -> Result<FlowConfig, ConfigError> {
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

/// sha256 of the canonical JSON text (serde_json's `Value` keeps its
/// object keys sorted, so key order in the source does not matter).
pub fn config_hash(config: &Value) -> String {
    hex::encode(Sha256::digest(config.to_string().as_bytes()))
}

/// `regex` syntax errors span several lines (the pattern, a caret, then
/// `error: …`); a config error is one line, so keep the last line.
fn one_line(e: regex::Error) -> String {
    match e {
        regex::Error::Syntax(s) => s
            .lines()
            .last()
            .unwrap_or("")
            .trim_start_matches("error: ")
            .to_string(),
        other => other.to_string(),
    }
}

/// Parse, validate every reference and event, compile every regex.
pub fn compile(config: &Value) -> Result<Compiled, ConfigError> {
    let parsed = parse(config)?;
    if !parsed.states.contains_key(&parsed.initial) {
        return Err(ConfigError::at(
            "initial",
            format!("no state {:?} declared", parsed.initial),
        ));
    }
    let mut states = BTreeMap::new();
    for (name, state) in &parsed.states {
        let mut rules = Vec::with_capacity(state.on.len());
        for (i, rule) in state.on.iter().enumerate() {
            let at = |field: &str| format!("states.{name}.on[{i}].{field}");
            if !HOOK_EVENTS.contains(&rule.event.as_str()) {
                return Err(ConfigError::at(
                    at("event"),
                    format!("unknown event {:?}", rule.event),
                ));
            }
            if let Some(goto) = &rule.goto
                && !parsed.states.contains_key(goto)
            {
                return Err(ConfigError::at(
                    at("goto"),
                    format!("no state {goto:?} declared"),
                ));
            }
            let mut matches = Vec::with_capacity(rule.matches.len());
            for (pointer, pattern) in &rule.matches {
                let path = at(&format!("match.{pointer}"));
                if !pointer.starts_with('/') {
                    return Err(ConfigError::at(
                        path,
                        "not a JSON pointer (must start with \"/\")",
                    ));
                }
                let re = RegexBuilder::new(&format!("^(?:{pattern})$"))
                    .size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .map_err(|e| ConfigError::at(path, one_line(e)))?;
                matches.push((pointer.clone(), re));
            }
            rules.push(CompiledRule {
                event: rule.event.clone(),
                matches,
                goto: rule.goto.clone(),
                respond: rule.respond.clone().unwrap_or_default(),
                send: rule.send.clone(),
                action: rule.action,
            });
        }
        states.insert(name.clone(), rules);
    }
    Ok(Compiled {
        initial: parsed.initial,
        states,
        hash: config_hash(config),
    })
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-plugin-flow config`
Expected: 5 passed. If an error text in `errors_carry_the_config_path` differs (serde and regex wording is theirs, not ours), read the actual message, confirm it still starts with the right path and is one line, and correct the expected string in the test — the *path* is the contract, the vendor wording is not. Record any such change in the commit body.

- [ ] **Step 6: Lint and commit**

Run: `mise x -- cargo fmt --all && mise x -- cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add Cargo.toml Cargo.lock crates/balerix-plugin-flow
git commit -F - <<'EOF'
Add the flow plugin crate and its config language

`balerix-plugin-flow` is the first in-tree plugin on the SDK (plugins spec
§8.1, §17.2). This is its config half: the typed `plugins.flow` block,
validated with config-path errors the daemon prefixes into
`crews.<c>.agents.<a>.plugins.flow: …`, and compiled into full-match
regexes under a 10 KiB size limit. Two exact dependencies join the
workspace: `regex` 1.13.1 (already in the lock through tracing-subscriber)
and `serde_path_to_error` 0.1.20, which attaches `states.working.on[1].foo`
to serde's message so a plugin config error reads like every other balerix
config error.

Claude-Session: https://claude.ai/code/session_01FRnxV132jJA53c1nVoCoxM
EOF
```

---

### Task 4: The pure step function

**Files:**
- Create: `crates/balerix-plugin-flow/src/machine.rs`
- Modify: `crates/balerix-plugin-flow/src/lib.rs`

**Interfaces:**
- Consumes: `Compiled`, `CompiledRule`, `Send`, `RuleAction` (Task 3); `balerix_api::{HookEvent, PluginAction}`.
- Produces:
  ```rust
  // balerix_plugin_flow::machine
  pub struct Step { pub fired: Option<usize>, pub response: Value, pub actions: Vec<PluginAction>, pub next: Option<String> }
  pub fn step(compiled: &Compiled, state: &str, event: &HookEvent, so_far: Value) -> Step;
  pub fn rule_matches(rule: &CompiledRule, event: &HookEvent) -> bool;
  pub fn text_at(payload: &Value, pointer: &str) -> Option<String>;
  ```

- [ ] **Step 1: Write the failing tests, unit and property**

Create `crates/balerix-plugin-flow/src/machine.rs` with the test module first:

```rust
//! The step (plugins spec §17.3): given the compiled config, the current
//! state and one event, which rule fires and what follows. Pure; the
//! plugin applies the transition and the KV write.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::compile;
    use balerix_plugin_sdk::testing::event;
    use serde_json::json;

    fn compiled() -> Compiled {
        compile(&json!({
            "initial": "working",
            "states": {
                "working": { "on": [
                    { "event": "PreToolUse", "match": { "/tool_input/command": "rm -rf.*", "/tool_name": "Bash" },
                      "respond": { "decision": "block", "reason": "no" } },
                    { "event": "PreToolUse", "match": { "/tool_input/n": "3" }, "respond": { "n": "three" } },
                    { "event": "PreToolUse", "respond": { "decision": "allow" } },
                    { "event": "Stop", "goto": "review", "send": { "text": "tests", "submit": false }, "action": "restart" },
                    { "event": "SessionEnd", "goto": "working" }
                ] },
                "review": { "on": [ { "event": "Stop", "goto": "done", "action": "stop" } ] },
                "done": {}
            }
        }))
        .unwrap()
    }

    #[test]
    fn text_at_reads_strings_and_stringifies_the_rest() {
        let p = json!({ "a": "x", "n": 3, "b": true, "o": { "k": [1] }, "z": null });
        assert_eq!(text_at(&p, "/a").as_deref(), Some("x"));
        assert_eq!(text_at(&p, "/n").as_deref(), Some("3"));
        assert_eq!(text_at(&p, "/b").as_deref(), Some("true"));
        assert_eq!(text_at(&p, "/o").as_deref(), Some("{\"k\":[1]}"));
        assert_eq!(text_at(&p, "/o/k/0").as_deref(), Some("1"));
        assert_eq!(text_at(&p, "/z").as_deref(), Some("null"));
        assert_eq!(text_at(&p, "/missing"), None);
        assert_eq!(text_at(&p, "/a/deeper"), None);
    }

    #[test]
    fn the_first_rule_whose_event_and_every_match_hold_fires() {
        let c = compiled();
        let blocked = event("f/c/a", "PreToolUse", json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf /" } }));
        let s = step(&c, "working", &blocked, json!({}));
        assert_eq!(s.fired, Some(0));
        assert_eq!(s.response, json!({ "decision": "block", "reason": "no" }));
        assert!(s.actions.is_empty() && s.next.is_none());

        // same command, different tool: rule 0 needs every entry
        let other_tool = event("f/c/a", "PreToolUse", json!({ "tool_name": "Write", "tool_input": { "command": "rm -rf /" } }));
        assert_eq!(step(&c, "working", &other_tool, json!({})).fired, Some(2));

        // a number is matched through its JSON text
        let n = event("f/c/a", "PreToolUse", json!({ "tool_input": { "n": 3 } }));
        let s = step(&c, "working", &n, json!({}));
        assert_eq!(s.fired, Some(1));
        assert_eq!(s.response, json!({ "n": "three" }));

        // a missing pointer never matches, so the catch-all fires
        let bare = event("f/c/a", "PreToolUse", json!({}));
        assert_eq!(step(&c, "working", &bare, json!({})).fired, Some(2));
    }

    #[test]
    fn no_rule_means_pass_through() {
        let c = compiled();
        let e = event("f/c/a", "Notification", json!({}));
        let s = step(&c, "working", &e, json!({ "keep": 1 }));
        assert_eq!(s.fired, None);
        assert_eq!(s.response, json!({ "keep": 1 }));
        assert!(s.actions.is_empty() && s.next.is_none());
        // a state with no rules
        let s = step(&c, "done", &event("f/c/a", "Stop", json!({})), json!({}));
        assert_eq!(s.fired, None);
        // an unknown state (never produced by the plugin) also passes through
        let s = step(&c, "nowhere", &event("f/c/a", "Stop", json!({})), json!({ "x": 1 }));
        assert_eq!((s.fired, s.response), (None, json!({ "x": 1 })));
    }

    #[test]
    fn respond_replaces_top_level_keys_only() {
        let c = compiled();
        let e = event("f/c/a", "PreToolUse", json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf /" } }));
        let so_far = json!({ "decision": "allow", "reason": { "nested": true }, "other": 1 });
        let s = step(&c, "working", &e, so_far);
        assert_eq!(s.response, json!({ "decision": "block", "reason": "no", "other": 1 }));
        // a non-object so far (the daemon never sends one) is replaced, not merged into
        let s = step(&c, "working", &e, json!(7));
        assert_eq!(s.response, json!({ "decision": "block", "reason": "no" }));
    }

    #[test]
    fn send_action_and_goto_are_carried() {
        let c = compiled();
        let s = step(&c, "working", &event("f/c/a", "Stop", json!({})), json!({}));
        assert_eq!(s.fired, Some(3));
        assert_eq!(s.response, json!({}));
        assert_eq!(
            s.actions,
            vec![
                PluginAction::SendText { text: "tests".into(), submit: false },
                PluginAction::Restart
            ],
            "send before action"
        );
        assert_eq!(s.next.as_deref(), Some("review"));
        let s = step(&c, "review", &event("f/c/a", "Stop", json!({})), json!({}));
        assert_eq!(s.actions, vec![PluginAction::Stop]);
        assert_eq!(s.next.as_deref(), Some("done"));
        // a self-transition is still a transition
        let s = step(&c, "working", &event("f/c/a", "SessionEnd", json!({})), json!({}));
        assert_eq!(s.next.as_deref(), Some("working"));
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        const EVENTS: [&str; 3] = ["PreToolUse", "Stop", "Notification"];
        const PATTERNS: [&str; 4] = ["x", "x.*", ".*", "[0-9]+"];
        const VALUES: [&str; 4] = ["x", "xyz", "42", ""];
        const STATES: [&str; 3] = ["a", "b", "c"];

        fn rule() -> impl Strategy<Value = Value> {
            (
                0..3usize,
                proptest::option::of((0..4usize, 0..4usize)),
                proptest::option::of(0..3usize),
                proptest::option::of(proptest::collection::btree_map("[a-c]", 0..3u8, 0..3)),
            )
                .prop_map(|(ev, m, goto, respond)| {
                    let mut r = json!({ "event": EVENTS[ev] });
                    if let Some((ptr, pat)) = m {
                        let pointer = format!("/k{ptr}");
                        r["match"] = json!({ pointer: PATTERNS[pat] });
                    }
                    if let Some(g) = goto {
                        r["goto"] = json!(STATES[g]);
                    }
                    if let Some(resp) = respond {
                        r["respond"] = json!(resp);
                    }
                    r
                })
        }

        fn config() -> impl Strategy<Value = Value> {
            (
                0..3usize,
                proptest::collection::vec(proptest::collection::vec(rule(), 0..4), 3),
            )
                .prop_map(|(initial, rules)| {
                    let mut states = serde_json::Map::new();
                    for (name, on) in STATES.iter().zip(rules) {
                        states.insert(name.to_string(), json!({ "on": on }));
                    }
                    json!({ "initial": STATES[initial], "states": states })
                })
        }

        fn payload() -> impl Strategy<Value = Value> {
            proptest::collection::btree_map(0..4usize, 0..4usize, 0..4).prop_map(|m| {
                let mut p = serde_json::Map::new();
                for (k, v) in m {
                    p.insert(format!("k{k}"), json!(VALUES[v]));
                }
                Value::Object(p)
            })
        }

        /// The obvious matcher, written independently of `step`.
        fn naive_first(c: &Compiled, state: &str, e: &HookEvent) -> Option<usize> {
            let rules = c.states.get(state)?;
            rules.iter().position(|r| {
                r.event == e.name
                    && r.matches.iter().all(|(ptr, re)| {
                        e.payload
                            .pointer(ptr)
                            .map(|v| match v {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .is_some_and(|s| re.is_match(&s))
                    })
            })
        }

        proptest! {
            #[test]
            fn step_fires_the_first_matching_rule_and_lands_in_a_declared_state(
                cfg in config(), state in 0..3usize, ev in 0..3usize, p in payload()
            ) {
                let c = compile(&cfg).unwrap();
                let e = event("f/c/a", EVENTS[ev], p);
                let s = step(&c, STATES[state], &e, json!({}));
                prop_assert_eq!(s.fired, naive_first(&c, STATES[state], &e));
                if let Some(next) = &s.next {
                    prop_assert!(c.states.contains_key(next));
                }
                prop_assert!(s.response.is_object());
                if let Some(i) = s.fired {
                    let rule = &c.states[STATES[state]][i];
                    for (k, v) in &rule.respond {
                        prop_assert_eq!(&s.response[k], v);
                    }
                    prop_assert_eq!(s.next.as_deref(), rule.goto.as_deref());
                }
            }
        }
    }
}
```

Add `pub mod machine;` to `lib.rs` after `pub mod config;`, and `pub use machine::{Step, step};`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-plugin-flow machine`
Expected: compile errors — `step`, `text_at`, `Step` not found.

- [ ] **Step 3: Implement the step**

Above the test module in `machine.rs`:

```rust
use balerix_api::{HookEvent, PluginAction};
use serde_json::Value;

use crate::config::{Compiled, CompiledRule, RuleAction};

/// What one event does in one state.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Index of the rule that fired in the state's `on` list.
    pub fired: Option<usize>,
    /// The verdict: `so_far` with the rule's `respond` keys replaced.
    pub response: Value,
    /// `send` first, then `action`.
    pub actions: Vec<PluginAction>,
    /// The rule's `goto`, if any — a self-transition included.
    pub next: Option<String>,
}

/// The payload value at a JSON pointer as text: strings as they are,
/// anything else (numbers, booleans, `null`, objects) as its JSON text.
/// `None` when the pointer resolves to nothing (§17.3).
pub fn text_at(payload: &Value, pointer: &str) -> Option<String> {
    payload.pointer(pointer).map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// The event name equals the rule's and every `match` entry holds.
pub fn rule_matches(rule: &CompiledRule, event: &HookEvent) -> bool {
    rule.event == event.name
        && rule
            .matches
            .iter()
            .all(|(pointer, re)| text_at(&event.payload, pointer).is_some_and(|s| re.is_match(&s)))
}

pub fn step(compiled: &Compiled, state: &str, event: &HookEvent, so_far: Value) -> Step {
    let Some((i, rule)) = compiled
        .states
        .get(state)
        .and_then(|rules| rules.iter().enumerate().find(|(_, r)| rule_matches(r, event)))
    else {
        return Step {
            fired: None,
            response: so_far,
            actions: Vec::new(),
            next: None,
        };
    };
    let mut response = match so_far {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    for (k, v) in &rule.respond {
        response.insert(k.clone(), v.clone());
    }
    let mut actions = Vec::new();
    if let Some(send) = &rule.send {
        actions.push(PluginAction::SendText {
            text: send.text.clone(),
            submit: send.submit,
        });
    }
    match rule.action {
        Some(RuleAction::Stop) => actions.push(PluginAction::Stop),
        Some(RuleAction::Restart) => actions.push(PluginAction::Restart),
        None => {}
    }
    Step {
        fired: Some(i),
        response: Value::Object(response),
        actions,
        next: rule.goto.clone(),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-plugin-flow machine`
Expected: 6 passed (5 unit + the property test).

- [ ] **Step 5: Lint and commit**

Run: `mise x -- cargo fmt --all && mise x -- cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/balerix-plugin-flow
git commit -F - <<'EOF'
Add the flow plugin's pure step

Given the compiled config, the current state and one hook event: the
first rule of the state whose event and every match entry hold fires;
its respond keys replace the chain's top-level keys, its send and action
become verdict actions, its goto is the next state (plugins spec §17.3).
No rule means pass-through. The property test checks the fired rule
against a naive matcher and that every goto lands in a declared state.

Claude-Session: https://claude.ai/code/session_01FRnxV132jJA53c1nVoCoxM
EOF
```

---

### Task 5: `FlowPlugin` — activation, KV-backed state, verdicts, metrics

**Files:**
- Create: `crates/balerix-plugin-flow/src/plugin.rs`
- Modify: `crates/balerix-plugin-flow/src/lib.rs`
- Create: `crates/balerix-plugin-flow/tests/plugin_it.rs`

**Interfaces:**
- Consumes: `compile`, `Compiled`, `step` (Tasks 3–4); `balerix_plugin_sdk::{Host, Metrics, Plugin, SdkError}`, `metrics::{IntCounterVec, IntGaugeVec}`; `testing::{FakeHost, Harness, event, metric}` (Task 2).
- Produces:
  ```rust
  // balerix_plugin_flow::plugin
  pub struct FlowPlugin { … }
  impl FlowPlugin { pub fn new(host: Host) -> Result<FlowPlugin, SdkError>; pub fn state_key(agent: &str) -> String /* "state/<agent>" */; }
  impl Plugin for FlowPlugin { activate, deactivate, intercept, metrics }
  #[derive(Serialize, Deserialize)] pub struct Stored { pub state: String, pub config: String }   // the KV document
  ```

- [ ] **Step 1: Write the failing integration tests**

Create `crates/balerix-plugin-flow/tests/plugin_it.rs`:

```rust
//! The flow plugin through the SDK harness (plugins spec §17.3, §17.7):
//! every call crosses the wire as the daemon's would.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use balerix_api::PluginAction;
use balerix_plugin_flow::plugin::{FlowPlugin, Stored};
use balerix_plugin_sdk::testing::{FakeHost, Harness, event, metric};
use balerix_plugin_sdk::{Env, Host};
use serde_json::{Value, json};

const ALICE: &str = "e2e/c/alice";

fn config() -> Value {
    json!({
        "initial": "working",
        "states": {
            "working": { "on": [
                { "event": "PreToolUse", "match": { "/tool_input/command": "rm -rf.*" },
                  "respond": { "decision": "block", "reason": "flow: no recursive deletes" } },
                { "event": "Stop", "goto": "review", "send": { "text": "flow says: run the tests" } }
            ] },
            "review": { "on": [ { "event": "Stop", "goto": "done", "action": "stop" } ] },
            "done": {}
        }
    })
}

async fn world() -> (FakeHost, Env, Harness) {
    let fake = FakeHost::start("tok", json!({}), vec![]).await;
    let env = fake.env("flow", std::path::Path::new("/s"));
    let plugin = FlowPlugin::new(Host::new(env.clone()).unwrap()).unwrap();
    let h = Harness::start(&env, plugin).await;
    (fake, env, h)
}

fn rm() -> balerix_api::HookEvent {
    event(ALICE, "PreToolUse", json!({ "tool_name": "Bash", "tool_input": { "command": "rm -rf /tmp/x" } }))
}

fn stop() -> balerix_api::HookEvent {
    event(ALICE, "Stop", json!({ "stop_hook_active": false }))
}

#[tokio::test]
async fn activation_validates_and_a_rejection_carries_the_path() {
    let (fake, _, h) = world().await;
    assert_eq!(h.activate(ALICE, config()).await, Ok(()));
    assert_eq!(
        fake.kv_json(&FlowPlugin::state_key(ALICE)),
        Some(json!({ "state": "working", "config": balerix_plugin_flow::config::config_hash(&config()) }))
    );
    let mut bad = config();
    bad["states"]["working"]["on"][0]["match"]["/tool_input/command"] = json!("[");
    assert_eq!(
        h.activate("e2e/c/bob", bad).await,
        Err("states.working.on[0].match./tool_input/command: unclosed character class".into())
    );
    assert_eq!(fake.kv_json(&FlowPlugin::state_key("e2e/c/bob")), None, "nothing stored for a rejected agent");
}

#[tokio::test]
async fn a_verdict_is_merged_over_the_response_so_far_and_carries_actions() {
    let (_, _, h) = world().await;
    h.activate(ALICE, config()).await.unwrap();
    let v = h
        .intercept_with(rm(), json!({ "decision": "allow", "other": 1 }), 900)
        .await;
    assert_eq!(
        v.response,
        json!({ "decision": "block", "reason": "flow: no recursive deletes", "other": 1 })
    );
    assert!(v.actions.is_empty());
    // a command the rule does not match passes through
    let v = h
        .intercept(event(ALICE, "PreToolUse", json!({ "tool_input": { "command": "ls" } })))
        .await;
    assert_eq!(v.response, json!({}));

    let v = h.intercept(stop()).await;
    assert_eq!(v.response, json!({}));
    assert_eq!(
        v.actions,
        vec![PluginAction::SendText { text: "flow says: run the tests".into(), submit: true }]
    );
    let v = h.intercept(stop()).await;
    assert_eq!(v.actions, vec![PluginAction::Stop]);
    let v = h.intercept(stop()).await;
    assert!(v.actions.is_empty(), "done has no rules");
}

#[tokio::test]
async fn transitions_move_the_gauge_count_and_persist() {
    let (fake, _, h) = world().await;
    h.activate(ALICE, config()).await.unwrap();
    let labels = [("fleet", "e2e"), ("crew", "c"), ("agent", "alice")];
    let m = h.metrics().await;
    assert!(m.starts_with("# HELP balerix_plugin_flow_"), "{m}");
    assert_eq!(metric(&m, "balerix_plugin_flow_state", &[("agent", "alice"), ("state", "working")]), Some(1.0));

    h.intercept(stop()).await;
    let m = h.metrics().await;
    assert_eq!(metric(&m, "balerix_plugin_flow_state", &[("agent", "alice"), ("state", "review")]), Some(1.0));
    assert_eq!(
        metric(&m, "balerix_plugin_flow_state", &[("agent", "alice"), ("state", "working")]),
        None,
        "the previous state's series is removed"
    );
    let mut t = labels.to_vec();
    t.extend([("from", "working"), ("to", "review")]);
    assert_eq!(metric(&m, "balerix_plugin_flow_transitions_total", &t), Some(1.0));
    let stored: Stored = serde_json::from_value(fake.kv_json(&FlowPlugin::state_key(ALICE)).unwrap()).unwrap();
    assert_eq!(stored.state, "review");
}

#[tokio::test]
async fn a_restart_resumes_the_stored_state() {
    let (_, env, mut h) = world().await;
    h.activate(ALICE, config()).await.unwrap();
    h.intercept(stop()).await; // working → review
    h.restart(FlowPlugin::new(Host::new(env.clone()).unwrap()).unwrap()).await;
    // the daemon re-sends activate after hello with the same config
    h.activate(ALICE, config()).await.unwrap();
    let m = h.metrics().await;
    assert_eq!(metric(&m, "balerix_plugin_flow_state", &[("agent", "alice"), ("state", "review")]), Some(1.0));
    assert_eq!(
        metric(&m, "balerix_plugin_flow_transitions_total", &[("agent", "alice")]),
        None,
        "counters start empty on a fresh process"
    );
    let v = h.intercept(stop()).await;
    assert_eq!(v.actions, vec![PluginAction::Stop], "review's rule, not working's");
}

#[tokio::test]
async fn deactivate_and_a_changed_config_reset_to_initial() {
    let (fake, _, h) = world().await;
    h.activate(ALICE, config()).await.unwrap();
    h.intercept(stop()).await; // → review

    // deactivate: entry and key gone, gauge series gone, events pass through
    h.deactivate(ALICE).await;
    assert_eq!(fake.kv_json(&FlowPlugin::state_key(ALICE)), None);
    assert_eq!(metric(&h.metrics().await, "balerix_plugin_flow_state", &[("agent", "alice")]), None);
    let v = h.intercept_with(rm(), json!({ "k": 1 }), 900).await;
    assert_eq!(v.response, json!({ "k": 1 }), "an unknown agent passes through");

    // activate again: back at initial
    h.activate(ALICE, config()).await.unwrap();
    assert_eq!(fake.kv_json(&FlowPlugin::state_key(ALICE)).unwrap()["state"], "working");
    h.intercept(stop()).await; // → review

    // a changed config (the daemon sends deactivate then activate): initial again
    let mut changed = config();
    changed["states"]["review"]["on"][0]["action"] = json!("restart");
    h.deactivate(ALICE).await;
    h.activate(ALICE, changed.clone()).await.unwrap();
    let stored = fake.kv_json(&FlowPlugin::state_key(ALICE)).unwrap();
    assert_eq!(stored["state"], "working");
    assert_eq!(stored["config"], balerix_plugin_flow::config::config_hash(&changed));
}

#[tokio::test]
async fn activation_fails_loudly_when_kv_is_unavailable() {
    let fake = FakeHost::start("tok", json!({}), vec![]).await;
    let mut env = fake.env("flow", std::path::Path::new("/s"));
    env.token = "wrong".into(); // every kv call is a 401
    let plugin = FlowPlugin::new(Host::new(env.clone()).unwrap()).unwrap();
    let good = fake.env("flow", std::path::Path::new("/s"));
    let h = Harness::start(&good, plugin).await;
    let err = h.activate(ALICE, config()).await.unwrap_err();
    assert!(err.starts_with("kv: daemon: HTTP 401"), "{err}");
}

#[tokio::test]
async fn health_is_always_ok() {
    let (_, _, h) = world().await;
    assert_eq!(h.health().await, Ok(()));
}
```

Add `pub mod plugin;` and `pub use plugin::FlowPlugin;` to `lib.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo test -p balerix-plugin-flow --test plugin_it`
Expected: compile error — `balerix_plugin_flow::plugin` not found.

- [ ] **Step 3: Implement `FlowPlugin`**

Create `crates/balerix-plugin-flow/src/plugin.rs`:

```rust
//! The `Plugin` impl (plugins spec §17.3, §17.4): one entry per active
//! agent, the current state mirrored to the daemon's KV under
//! `state/<agent>` so a restart resumes, and the two metric families.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use balerix_api::{HookEvent, InterceptResponse};
use balerix_plugin_sdk::metrics::{IntCounterVec, IntGaugeVec};
use balerix_plugin_sdk::{Host, Metrics, Plugin, SdkError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Compiled, compile};
use crate::machine::step;

/// The KV document under `state/<agent>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub state: String,
    /// `config_hash` of the config the state belongs to.
    pub config: String,
}

struct AgentFlow {
    compiled: Arc<Compiled>,
    state: String,
}

pub struct FlowPlugin {
    host: Host,
    agents: Mutex<HashMap<String, AgentFlow>>,
    metrics: Metrics,
    state_gauge: IntGaugeVec,
    transitions: IntCounterVec,
}

impl std::fmt::Debug for FlowPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowPlugin").field("host", &self.host).finish()
    }
}

const STATE_LABELS: [&str; 4] = ["fleet", "crew", "agent", "state"];
const TRANSITION_LABELS: [&str; 5] = ["fleet", "crew", "agent", "from", "to"];

impl FlowPlugin {
    pub fn new(host: Host) -> Result<Self, SdkError> {
        let metrics = Metrics::new(&host.env().name);
        let state_gauge = metrics.int_gauge_vec(
            "state",
            "Current flow state (1 for the current state)",
            &STATE_LABELS,
        )?;
        let transitions = metrics.int_counter_vec(
            "transitions_total",
            "Flow transitions since the plugin started",
            &TRANSITION_LABELS,
        )?;
        Ok(Self {
            host,
            agents: Mutex::new(HashMap::new()),
            metrics,
            state_gauge,
            transitions,
        })
    }

    pub fn state_key(agent: &str) -> String {
        format!("state/{agent}")
    }

    /// `fleet/crew/agent` → the three labels; an id of another shape
    /// (never produced by the daemon) keeps its text in `agent`.
    fn labels(agent: &str) -> [String; 3] {
        let mut parts = agent.splitn(3, '/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(f), Some(c), Some(a)) => [f.into(), c.into(), a.into()],
            _ => [String::new(), String::new(), agent.into()],
        }
    }

    fn set_state_gauge(&self, agent: &str, from: Option<&str>, to: &str) {
        let [f, c, a] = Self::labels(agent);
        if let Some(from) = from {
            // Err only when the series was never set; nothing to do then.
            let _ = self.state_gauge.remove_label_values(&[&f, &c, &a, from]);
        }
        self.state_gauge.with_label_values(&[&f, &c, &a, to]).set(1);
    }

    fn count_transition(&self, agent: &str, from: &str, to: &str) {
        let [f, c, a] = Self::labels(agent);
        self.transitions
            .with_label_values(&[&f, &c, &a, from, to])
            .inc();
    }

    async fn store(&self, agent: &str, stored: &Stored) -> Result<(), SdkError> {
        let bytes = serde_json::to_vec(stored)
            .map_err(|e| SdkError::Transport(format!("encode state: {e}")))?;
        self.host
            .kv_put(&Self::state_key(agent), &bytes, false)
            .await
    }
}

impl Plugin for FlowPlugin {
    /// Compile, then resume the stored state when it belongs to this very
    /// config and is still declared; otherwise start at `initial` and
    /// store that. A KV fault rejects: the operator's `up` should fail
    /// loudly on a daemon-side error (§17.3).
    async fn activate(&self, agent: &str, config: Value) -> Result<(), String> {
        let compiled = Arc::new(compile(&config).map_err(|e| e.to_string())?);
        let key = Self::state_key(agent);
        let previous = self
            .host
            .kv_get(&key)
            .await
            .map_err(|e| format!("kv: {e}"))?
            .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok());
        let resumed = previous
            .filter(|s| s.config == compiled.hash && compiled.states.contains_key(&s.state))
            .map(|s| s.state);
        let state = match resumed {
            Some(state) => state,
            None => {
                let stored = Stored {
                    state: compiled.initial.clone(),
                    config: compiled.hash.clone(),
                };
                self.store(agent, &stored)
                    .await
                    .map_err(|e| format!("kv: {e}"))?;
                stored.state
            }
        };
        let old = {
            let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
            agents
                .insert(agent.to_string(), AgentFlow { compiled, state: state.clone() })
                .map(|a| a.state)
        };
        self.set_state_gauge(agent, old.as_deref(), &state);
        eprintln!("flow: {agent} active in state {state:?}");
        Ok(())
    }

    /// Forget the agent and its stored state: `down`, a dropped block and
    /// a config change all reset to `initial` on the next `activate`.
    async fn deactivate(&self, agent: &str) {
        let old = self
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent)
            .map(|a| a.state);
        if let Some(old) = old {
            let [f, c, a] = Self::labels(agent);
            let _ = self.state_gauge.remove_label_values(&[&f, &c, &a, &old]);
        }
        if let Err(e) = self.host.kv_delete(&Self::state_key(agent)).await {
            eprintln!("flow: kv delete for {agent}: {e}");
        }
    }

    /// One step; the transition is applied in memory first, then written
    /// to KV before the verdict returns. A KV failure is logged and the
    /// in-memory state stands — a hook never fails on it (§17.3).
    async fn intercept(
        &self,
        event: HookEvent,
        so_far: Value,
        _deadline_ms: u64,
    ) -> InterceptResponse {
        let agent = event.agent.clone();
        let (outcome, transition) = {
            let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
            let Some(flow) = agents.get_mut(&agent) else {
                return InterceptResponse {
                    response: so_far,
                    actions: Vec::new(),
                };
            };
            let s = step(&flow.compiled, &flow.state, &event, so_far);
            let transition = s.next.clone().map(|to| {
                let from = std::mem::replace(&mut flow.state, to.clone());
                (from, to, flow.compiled.hash.clone())
            });
            (s, transition)
        };
        if let Some((from, to, hash)) = transition {
            self.count_transition(&agent, &from, &to);
            self.set_state_gauge(&agent, Some(&from), &to);
            eprintln!("flow: {agent} {from} -> {to} on {}", event.name);
            let stored = Stored { state: to, config: hash };
            if let Err(e) = self.store(&agent, &stored).await {
                eprintln!("flow: kv put for {agent}: {e}");
            }
        }
        InterceptResponse {
            response: outcome.response,
            actions: outcome.actions,
        }
    }

    fn metrics(&self) -> Option<&Metrics> {
        Some(&self.metrics)
    }
}
```

(`Host::env()` is public on the SDK's `Host` — `host.rs:70`.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo test -p balerix-plugin-flow`
Expected: all pass — config, machine and the 7 `plugin_it` tests.

- [ ] **Step 5: Lint and commit**

Run: `mise x -- cargo fmt --all && mise x -- cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/balerix-plugin-flow
git commit -F - <<'EOF'
Implement the flow plugin over the SDK

`FlowPlugin` keeps one entry per active agent and mirrors the current
state to the daemon's KV under `state/<agent>` with the config's hash, so
a plugin or daemon restart resumes and a changed config resets; a KV
fault at activate rejects loudly while one during a hook is only logged
(plugins spec §17.3). The state gauge and the transitions counter live in
the SDK's prefixed registry (§17.4). Every test runs through the SDK
harness, so each call crosses the wire the daemon's way.

Claude-Session: https://claude.ai/code/session_01FRnxV132jJA53c1nVoCoxM
EOF
```

---

### Task 6: The binary, the package and `mise run package-plugins`

**Files:**
- Modify: `crates/balerix-plugin-flow/src/main.rs`
- Create: `crates/balerix-plugin-flow/package/mise.toml`
- Create: `crates/balerix-plugin-flow/package/balerix-plugin.yaml`
- Create: `scripts/package-plugins.sh` (executable)
- Modify: `mise.toml`

**Interfaces:**
- Consumes: `FlowPlugin::new`, `balerix_plugin_sdk::{Env, Host, serve}`; `balerix plugin package <dir>` (validates a package directory and writes a tarball).
- Produces: `target/plugins/flow/{bin/balerix-plugin-flow, mise.toml, balerix-plugin.yaml}`, a directory source for `plugins.yaml`; the task `package-plugins`, which `test` and `e2e` depend on.

- [ ] **Step 1: The binary**

Replace `crates/balerix-plugin-flow/src/main.rs`:

```rust
//! `balerix-plugin-flow`: read the daemon's environment, say hello, serve.
//! Failures print `flow: …` to stderr and exit 1; that lands in the
//! plugin's tmux window and `plugins/flow/logs/`.

use balerix_plugin_flow::FlowPlugin;
use balerix_plugin_sdk::{Env, Host, serve};

fn run() -> anyhow::Result<()> {
    let env = Env::from_process()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let host = Host::new(env.clone())?;
        let plugin = FlowPlugin::new(Host::new(env)?)?;
        eprintln!("flow: starting");
        serve(&host, env!("CARGO_PKG_VERSION"), plugin).await?;
        Ok(())
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("flow: {e:#}");
        std::process::exit(1);
    }
}
```

- [ ] **Step 2: The package files**

Create `crates/balerix-plugin-flow/package/mise.toml`:

```toml
# The development package layout (plugins spec §17.6): the binary is copied
# into bin/ by `mise run package-plugins`. A release package pins the binary
# as a mise tool instead and carries no binary of its own.
[tools]

[tasks.serve]
run = "./bin/balerix-plugin-flow"
```

Create `crates/balerix-plugin-flow/package/balerix-plugin.yaml`:

```yaml
apiVersion: balerix/v1
kind: Plugin
name: flow
version: 0.1.0
protocol: 1
start: serve
hooks:
  # every event: a rule may name any of the nine (spec §8.1)
  intercept: [SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, SubagentStop, PreCompact]
  observe: []
needs: [actions, kv]
routes: false
```

(`version` must equal `CARGO_PKG_VERSION`, which is the workspace `0.1.0`.)

- [ ] **Step 3: The packaging script and the tasks**

Create `scripts/package-plugins.sh`, then `chmod +x` it:

```bash
#!/usr/bin/env bash
# Assembles each in-tree plugin as a directory source under target/plugins/
# (plugins spec §17.6): the crate's package/ files plus the freshly built
# binary in bin/. The e2e names these directories in plugins.yaml; by hand,
# so can you. One loop: a new in-tree plugin is one more name.
set -euo pipefail
cd "$(dirname "$0")/.."

for name in flow; do
  crate="balerix-plugin-$name"
  cargo build -q -p "$crate"
  out="target/plugins/$name"
  rm -rf "$out"
  mkdir -p "$out/bin"
  cp "target/debug/$crate" "$out/bin/$crate"
  cp "crates/$crate/package/mise.toml" "crates/$crate/package/balerix-plugin.yaml" "$out/"
  echo "packaged $name -> $out"
done
```

In `mise.toml`, add after `[tasks.test]`'s block:

```toml
[tasks.package-plugins]
description = "Build the in-tree plugins and assemble them as directory sources under target/plugins/<name>/ (the e2e and `plugins.yaml` point at these)"
run = "scripts/package-plugins.sh"
```

and add `depends = ["package-plugins"]` to both `[tasks.test]` and `[tasks.e2e]` (a line before `run`).

- [ ] **Step 4: Run it and validate the package with the CLI**

Run:

```bash
mise run package-plugins
ls -l target/plugins/flow target/plugins/flow/bin
mise x -- cargo run -q -p balerix -- plugin package target/plugins/flow --out target/tmp/flow-0.1.0.tar.gz
```

Expected: the three files present, the binary executable; `plugin package` validates the manifest and `mise.toml` and prints a `sha256` line (the tarball is discarded; it only proves the layout passes the same validation `sync` runs).

Then the by-hand smoke that §11.1's new row asks for — a binary inside a directory-source package runs under the package read grant:

```bash
mkdir -p target/tmp/serve/home/.config/balerix
printf 'plugins:\n  - name: flow\n    source: %s/target/plugins/flow\n' "$PWD" > target/tmp/serve/home/.config/balerix/plugins.yaml
printf '[tools]\n' > target/tmp/serve/home/.config/balerix/mise.toml
mise run serve
```

In a second terminal, with the same four environment overrides the `serve` task sets (`HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME` under `target/tmp/serve/home`), run `mise x -- cargo run -q -p balerix -- plugin list`. Expected within a few seconds: `flow  0.1.0  ready  127.0.0.1:<port>`. If it shows `pending` and `target/tmp/serve/home/.local/state/balerix/plugins/flow/logs/nono.log` says the exec was refused (`Permission denied` on `bin/balerix-plugin-flow`), the fallback of the plan header applies: add to `package/balerix-plugin.yaml` a `sandbox:` block granting nono's execute permission on the package directory (nono's profile schema names the field; `crates/balerix-runtime/src/sandbox.rs` shows the base profile's shape), re-run `package-plugins` and `serve`, and record the verdict either way in Task 8. Stop the daemon with Ctrl-C.

- [ ] **Step 5: Run the check task**

Run: `mise run check`
Expected: `package-plugins` runs before nextest; everything passes (the e2e journeys of phases 1–3 and 2a still run; `flow_journey` arrives in Task 7).

- [ ] **Step 6: Commit**

```bash
git add crates/balerix-plugin-flow/src/main.rs crates/balerix-plugin-flow/package scripts/package-plugins.sh mise.toml
git commit -F - <<'EOF'
Add the flow binary, its package and `mise run package-plugins`

The binary is Env → Host → serve. `package/` holds the two files a
plugin package needs; `scripts/package-plugins.sh` builds the crate and
assembles target/plugins/flow/ with the binary in bin/, a directory
source for plugins.yaml. `test` and `e2e` depend on it so `check` and CI
assemble the package before nextest (plugins spec §17.6). This is the
development layout; a release package pins the binary as a mise tool.

Claude-Session: https://claude.ai/code/session_01FRnxV132jJA53c1nVoCoxM
EOF
```

---

### Task 7: The e2e `flow_journey`

**Files:**
- Modify: `crates/balerix/tests/e2e.rs`

**Interfaces:**
- Consumes: `World`, `fleet_yaml`, `wait_file`, `wait_plugin_ready`, `require_or_skip`, `landlock_works`, `git` (all in the file); `target/plugins/flow` (Task 6); `balerix dev fake-claude`'s event order `SessionStart`, `Notification`, `PreToolUse` (`rm -rf /tmp/x`), `Stop`.
- Produces: the §17.7 assertions.

- [ ] **Step 1: Write the journey**

Append to `crates/balerix/tests/e2e.rs`:

```rust
/// Where `mise run package-plugins` left the flow package: next to this
/// binary's target dir. `None` when it has not been run.
fn flow_package() -> Option<PathBuf> {
    let dir = Path::new(BALERIX).parent()?.parent()?.join("plugins/flow");
    dir.join("bin/balerix-plugin-flow").exists().then_some(dir)
}

/// alice's `plugins:` block for the journey, in YAML flow style, with the
/// `rm -rf` pattern injectable so the rejection case can break it.
fn flow_block(pattern: &str) -> String {
    format!(
        "{{ flow: {{ initial: working, states: {{ \
           working: {{ on: [ \
             {{ event: PreToolUse, match: {{ /tool_input/command: \"{pattern}\" }}, \
                respond: {{ decision: block, reason: \"flow: no recursive deletes\" }} }}, \
             {{ event: Stop, goto: review, send: {{ text: \"flow says: run the tests\" }} }} ] }}, \
           review: {{ on: [ {{ event: Stop, goto: done }} ] }}, \
           done: {{}} }} }} }}"
    )
}

/// Plugins spec §13 item 2b, "done when" (§17.7): the packaged flow plugin
/// through a real daemon, nono and tmux — a block, a send_text, a
/// transition visible in /metrics and in KV, a rejected `update` that
/// leaves the fleet running, and a reset on `down`.
#[test]
fn flow_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let Some(pkg) = flow_package() else {
        assert!(!require_or_skip(
            "target/plugins/flow (run `mise run package-plugins`)",
            false
        ));
        return;
    };
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-flow-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("balerix-e2e-flow-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/balerix");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    fs::write(
        cfg.join("plugins.yaml"),
        format!("plugins:\n  - name: flow\n    source: \"{}\"\n", pkg.display()),
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
    fs::write(&fleet, fleet_yaml(&bare, None, Some(&flow_block("rm -rf.*")))).unwrap();

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
    let plugin_dir = w.state().join("plugins/flow");
    // as in plugin_protocol_journey: the pair must activate inline, before
    // fake-claude's PreToolUse fires
    wait_plugin_ready(&w, "flow", &plugin_dir);

    let out = w.ok(&[
        "up",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert!(out.contains("flow=active"), "{out}");

    // the block came back through the HTTP hook, with flow's reason; bob is pass-through
    let reply = wait_file(&w.agent_dir("alice").join("home/fake-claude.PreToolUse.reply"));
    assert!(reply.contains("\"decision\":\"block\""), "{reply}");
    assert!(reply.contains("flow: no recursive deletes"), "{reply}");
    let bob_reply = wait_file(&w.agent_dir("bob").join("home/fake-claude.PreToolUse.reply"));
    assert_eq!(bob_reply.trim(), "{}", "bob has no plugin: pass-through");

    // Stop → review carried the send_text to alice's stdin through tmux
    let stdin = wait_file(&w.agent_dir("alice").join("home/fake-claude.stdin"));
    assert!(stdin.contains("flow says: run the tests"), "{stdin}");

    // the transition is in the daemon's /metrics (re-exported from the
    // plugin's scrape) and in KV; poll: the KV put follows the verdict
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let state = "balerix_plugin_flow_state{agent=\"alice\",crew=\"c\",fleet=\"e2e\",state=\"review\"} 1";
    let transition = "balerix_plugin_flow_transitions_total{agent=\"alice\",crew=\"c\",fleet=\"e2e\",from=\"working\",to=\"review\"} 1";
    let start = Instant::now();
    let metrics = loop {
        let m = agent
            .get(format!("{url}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        if m.contains(state) && m.contains(transition) {
            break m;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "metrics never showed the transition within 10s; last scrape:\n{m}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        !metrics.contains("state=\"working\"} 1"),
        "the previous state's series is gone: {metrics}"
    );
    let kv = plugin_dir.join("kv/state/e2e/c/alice");
    let stored = wait_file_until(&kv, |s| s.contains("\"review\""));
    assert!(stored.contains("\"state\":\"review\""), "{stored}");

    // a bad regex is rejected with the full config path and the fleet keeps running
    fs::write(&fleet, fleet_yaml(&bare, None, Some(&flow_block("[")))).unwrap();
    let out = w.run(&[
        "update",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "60s",
    ]);
    assert!(!out.status.success(), "update with a bad regex must fail");
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        err.contains(
            "crews.c.agents.alice.plugins.flow: states.working.on[0].match./tool_input/command: "
        ),
        "{err}"
    );
    let rec = w.status();
    assert_eq!(rec.status.phase, FleetPhase::Ready, "the rejected update changed nothing");
    assert_eq!(
        rec.status.agents["e2e/c/alice"].plugins["flow"].state,
        balerix_api::ActivationState::Active
    );

    // down deactivates: the key is deleted
    let out = w.ok(&["down", "e2e", "--keep", "--timeout", "60s"]);
    assert!(out.contains("e2e  down"), "{out}");
    let start = Instant::now();
    while kv.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "kv/state/e2e/c/alice still present after down"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(w);
}
```

(`wait_file_until`, `wait_file` and the `e2e  down` line `down --keep` prints are already in the file, from `serve_up_update_down_journey`.)

- [ ] **Step 2: Run the journey**

Run: `mise run e2e` (this runs `package-plugins` first)
Expected: `flow_journey` passes alongside the three existing journeys. If it fails, `target/tmp/e2e-flow-<pid>/home/.local/state/balerix/plugins/flow/logs/` holds the plugin's stderr (`flow: …` lines) and nono's log; the agent's window log is under `fleets/e2e/crews/c/agents/alice/logs/`.

- [ ] **Step 3: Run the whole gate and commit**

Run: `mise run check`
Expected: passes.

```bash
git add crates/balerix/tests/e2e.rs
git commit -F - <<'EOF'
Add the flow e2e: block, send_text, transition, rejection and reset

Through a real daemon, nono and tmux, the packaged flow plugin blocks
fake-claude's `rm -rf`, sends text on Stop, and its working→review
transition shows in the daemon's /metrics and in its KV file; an `update`
with a bad regex fails with the full config path and leaves the fleet
running; `down` deletes the stored state (plugins spec §17.7). The test
skips with a reason when `mise run package-plugins` has not run and
fails under BALERIX_REQUIRE_TOOLS.

Claude-Session: https://claude.ai/code/session_01FRnxV132jJA53c1nVoCoxM
EOF
```

---

### Task 8: Docs, the example, and the spec's §17.8

**Files:**
- Modify: `docs/plugin-protocol.md` (new §7)
- Modify: `ARCHITECTURE.md:41-44`, `:70-94`, non-obvious decisions
- Modify: `AGENTS.md` (tasks, conventions, gotchas)
- Modify: `README.md:7-30`, `:38-44`
- Modify: `examples/payments.yaml`
- Modify: `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md` (append §17.8)

- [ ] **Step 1: The protocol doc's packaging section**

Append to `docs/plugin-protocol.md`:

```markdown
## 7. Packaging and distribution

A package is a directory (or a tarball of one) with `mise.toml` and
`balerix-plugin.yaml` at its root (plugins spec §2). The manifest's
`start` names a task in that `mise.toml`; the daemon runs `mise trust` and
`mise install` on it, then `mise run <start>` inside the sandbox with the
package directory as the working directory. Two shapes:

- **Development**: the binary sits inside the package (`bin/…`) and the
  task runs it by relative path. `mise run package-plugins` assembles the
  in-tree plugins this way under `target/plugins/<name>/`; a
  `plugins.yaml` entry names such a directory as its `source` and it is
  used in place. Host-only by construction.
- **Release**: the package carries **no binary**. Its `mise.toml` pins
  the plugin binary as a mise tool (for example a `ubi:` or `github:`
  backend entry against a release asset, exact version) and the task runs
  it by name; the daemon's `mise install` fetches the asset for the host
  platform, exactly as it installs `node` for an agent, and the sandbox
  already grants read on the mise data dir. One platform-neutral tarball,
  one `sha256`. Cross-compiling the per-platform assets is the plugin
  repository's release pipeline (Linux x86_64 and aarch64 while the
  sandbox is Landlock; static builds avoid libc mismatches).
```

- [ ] **Step 2: ARCHITECTURE.md**

In the crate list, after the `balerix-plugin-sdk` bullet, add:

```markdown
- `balerix-plugin-flow` — the first in-tree plugin, on the SDK: a per-agent
  state machine over hook events (`config.rs` parses and compiles the
  `plugins.flow` block, `machine.rs` is the pure step, `plugin.rs` owns the
  agents, the KV-mirrored state and the metrics). Plugin crates depend on
  the SDK and `api` only.
```

and extend the `balerix-plugin-sdk` bullet's list with `Metrics` (the prefixing registry) and `testing::Harness`.

After the **Plugins (Spec B, phase 1)** paragraph's continuation about phase 2a, add:

```markdown
**Flow (Spec B, phase 2b):** an agent whose settings carry `plugins.flow`
is activated on the flow plugin with that block; the plugin compiles it
(full-match regexes, config-path errors the daemon prefixes into
`crews.<c>.agents.<a>.plugins.flow: …`) and resumes or resets the agent's
state from KV `state/<agent>`. Every hook event of that agent runs one
step: the first rule of the current state whose event and `match` entries
hold sets the verdict's keys, carries `send`/`action` as verdict actions,
and moves the state — written back to KV before the verdict returns.
`mise run package-plugins` assembles `target/plugins/flow/`, the directory
source the e2e loads.
```

Add to the non-obvious decisions, before the metrics-prefix bullet:

```markdown
- **Flow's regexes are full-match.** `match: { /tool_input/command: "rm" }`
  matches only the command `rm`; write `rm.*`. A pattern silently matching
  every command containing it is the worse surprise (§17.2).
- **Flow's state survives restarts and resets on `deactivate`.** Plugin and
  daemon restarts re-`activate` without a `deactivate`, so the KV state is
  resumed; `down`, a config change and dropping the block go through
  `deactivate`, which deletes it (§17.3).
- **Plugin metrics are registered through the SDK.** `Metrics` prefixes
  every family, so an SDK plugin cannot trip the daemon's prefix rule
  (§17.4).
```

- [ ] **Step 3: AGENTS.md**

In the tasks list, after `e2e`, add:

```markdown
- `package-plugins` — builds the in-tree plugins and assembles each as a
  directory source under `target/plugins/<name>/`; `test` and `e2e` depend
  on it, and the flow e2e skips (fails under `BALERIX_REQUIRE_TOOLS`) without it.
```

In conventions, extend the ports bullet with: `Plugin crates (`balerix-plugin-flow`) depend on `balerix-plugin-sdk` and `balerix-api` only.`

Add gotchas at the end:

```markdown
- Flow `match` regexes are full-match (`^(?:…)$`); `"rm -rf"` does not match
  `rm -rf /x`, `"rm -rf.*"` does. Compiled at `activate` with a 10 KiB size
  limit; errors carry the path
  `states.<s>.on[<i>].match.<pointer>: …`.
- Flow's state is KV `state/<agent>` (`plugins/flow/kv/state/<fleet>/<crew>/<agent>`
  on disk). A plugin or daemon restart resumes it; `down`, a config change
  or dropping the block resets it (`deactivate` deletes the key). To reset
  by hand: `down` and `up`.
- `Plugin::metrics` returns `Option<&Metrics>`; register families through
  `Metrics` (short names, the SDK adds `balerix_plugin_<name>_`). A plugin
  in another language must apply the prefix itself or its whole scrape is
  dropped.
- Test a plugin through `balerix_plugin_sdk::testing::Harness`: it serves
  the real router and speaks to it over HTTP; `restart` swaps the instance
  against the same `FakeHost` (KV kept).
- `target/plugins/<name>/` is the *development* package layout (binary in
  `bin/`). A release package pins the binary as a mise tool and ships no
  binary (`docs/plugin-protocol.md` §7).
```

- [ ] **Step 4: README and the example**

In `README.md`, replace the `## Status` paragraph:

```markdown
Spec A is complete. Spec B (plugins) is in progress: phase 1 (plugin
workloads), phase 2a (the event protocol) and phase 2b (the `flow` plugin,
`mise run package-plugins`, the SDK's metrics registry and test harness)
are done; phase 3, the proxy, attach, `fleets/watch` and `web`, is next.
```

and add after the `### Upgrading to Spec B phase 2a` list:

```markdown
### Upgrading to Spec B phase 2b
- `Plugin::metrics` in the SDK returns `Option<&Metrics>` instead of text;
  register families through `Metrics` and the prefix is applied for you.
- `mise run test` and `mise run e2e` now run `package-plugins` first.
```

In the quickstart, after item 7, add:

```markdown
8. `mise run package-plugins` — assembles the in-tree `flow` plugin under
   `target/plugins/flow/`; point a `plugins.yaml` entry's `source` at that
   directory and give an agent a `plugins: { flow: … }` block to drive it
   by rule: block a tool call, send text on `Stop`, move between states.
   `examples/payments.yaml` shows one.
```

(renumber the following item to 9.)

In `examples/payments.yaml`, replace `      alice: {}` with:

```yaml
      alice:
        # The flow plugin (declare it in plugins.yaml first): block recursive
        # deletes, and after Claude's first Stop ask for the tests once.
        plugins:
          flow:
            initial: working
            states:
              working:
                on:
                  - event: PreToolUse
                    match: { /tool_input/command: "rm -rf.*" }   # full-match
                    respond: { decision: block, reason: "no recursive deletes" }
                  - event: Stop
                    goto: review
                    send: { text: "Run the tests and fix any failures." }
              review:
                on:
                  - event: Stop
                    goto: done
              done: {}
```

Run: `mise x -- cargo run -q -p balerix -- config resolve examples/payments.yaml --no-host-defaults`
Expected: resolves; alice's settings show the `plugins.flow` block. (Any golden snapshot of the example — `crates/balerix-config/tests/snapshots/` — that changes must be read, checked against this block, then `mise x -- cargo insta accept`.)

- [ ] **Step 5: The spec's §17.8**

Append to `docs/superpowers/specs/2026-09-06-balerix-b-plugins-design.md`:

```markdown
### 17.8 Refinements from the phase 2b plan (2026-09-07)

Where the phase 2b implementation plan refined this section:

- **`Harness::intercept` takes the event only**; the agent is in the
  `HookEvent` that `event(agent, name, payload)` builds. `intercept_with`
  adds `response_so_far` and `deadline_ms`.
- **`Metrics` constructors** are `int_counter`, `int_counter_vec`,
  `int_gauge`, `int_gauge_vec` and `render()`; the vector types are
  re-exported from `balerix_plugin_sdk::metrics`; registration errors are
  `SdkError::Metrics`.
- **`metrics.json` was re-recorded**: the encoder writes a `# HELP` line
  the hand-written fixture lacked; `families_ok` accepted it already.
- **Config errors are `ConfigError { path, message }`** (thiserror,
  displayed `<path>: <message>`); `activate` stringifies it. Regex errors
  keep only their last line without `error: `; unknown fields read
  `unknown field `foo``.
- **§11.1 row, verified 2026-09-07:** a binary inside a directory-source
  package is executable under the package's read grant — `<verdict from
  Task 6: "yes, no sandbox block needed" or "no; the manifest's sandbox
  block grants execute on the package directory">`.
- **`test` and `e2e` depend on `package-plugins`**, so `mise run check`
  and CI assemble `target/plugins/flow/` before nextest.
```

Fill the verdict placeholder with what Task 6 step 4 found before committing.

- [ ] **Step 6: Final gate and commit**

Run: `mise run check`
Expected: passes.

```bash
git add docs ARCHITECTURE.md AGENTS.md README.md examples/payments.yaml crates/balerix-config/tests/snapshots
git commit -F - <<'EOF'
Document Spec B phase 2b: the flow plugin, its packaging and the SDK additions

The protocol doc gains the two package shapes (binary inside for
development, binary as a mise tool for release); ARCHITECTURE.md the flow
crate and the plugin-crate layer; AGENTS.md the package-plugins task and
the gotchas found (full-match regexes, the KV reset rule, the metrics
registry, the harness); README the status and an upgrade note; the
example fleet a flow block for alice. The spec's §17.8 records what the
plan refined and the executable-grant verdict.

Claude-Session: https://claude.ai/code/session_01FRnxV132jJA53c1nVoCoxM
EOF
```

---

## Done when

- `mise run check` passes here and in CI with `BALERIX_REQUIRE_TOOLS=1`, `flow_journey` included, inside the five-minute budget.
- `flow_journey`: `up` prints `flow=active`, alice's `PreToolUse` reply carries the block and `flow: no recursive deletes`, her stdin carries `flow says: run the tests`, `/metrics` shows the `review` state and the `working`→`review` transition, `plugins/flow/kv/state/e2e/c/alice` exists then disappears on `down`, and the bad-regex `update` fails with the full config path.
- Every `docs/plugin-protocol/*.json` fixture is replayed by both conformance tests.
- `mise run mutants` still reports no surviving mutants in `reconcile` (nothing in `balerix-core` changed).
- `docs/plugin-protocol.md`, `ARCHITECTURE.md`, `AGENTS.md`, `README.md`, `examples/payments.yaml` and the spec's §17.8 say what the code does.

## Deliberately deferred (phase 3 of the plugins spec)

- `GET /v1/plugin-host/fleets/watch` (§17.1), the `/v1/plugins/<name>/*` reverse proxy with WebSocket passthrough, `AgentRunner::attach`, `balerix-plugin-web`, `balerix_plugin_proxy_requests_total`.
- A release mode for `package-plugins` (a package pinning the binary as a mise tool): no release pipeline exists yet (§17.6).
- A `balerix stop <agent>` / `balerix restart <agent>` command on top of `SetStopped` (phase 2a's deferral, unchanged).
