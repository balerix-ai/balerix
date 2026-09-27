# Spec M part 1: Restricted Settings Surface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A fleet file a plugin applies can no longer choose what its agents run as (`claude.binary`, `claude.args`, `env`, `sandbox`, and `claude.settings.{env,apiKeyHelper}`); the operator sets those per plugin through a `fleetDefaults` layer in `plugins.yaml`; and `PUT /v1/plugin-host/fleets/{name}` answers the record only to a plugin that also declares `fleets`, 204 otherwise.

**Architecture:** `balerix-config`'s resolver gains a restricted mode (a per-layer key check before merging) and an operator layer slotted between the host's `settings.json` and the file's `defaults`. The `FleetResolver` port carries the operator layer; the binary's `HostResolver` always resolves restricted. `PluginEntry.fleetDefaults` travels through `ResolvedPlugin` into the registry, where `manage_fleet` reads it. The route gates its answer on `Capability::Fleets`; the SDK's `apply_fleet` answers `Option<FleetRecord>`, `FakeHost` gains a knob for the 204, and two conformance fixtures land. The e2e's managed journey moves fake-claude into `fleetDefaults`.

**Tech Stack:** Rust 1.98 (edition 2024), axum 0.8, serde, cargo-nextest, the e2e under `target/tmp` with nono/tmux/mise.

**Spec:** `docs/superpowers/specs/2026-09-22-balerix-m-github-plugin-design.md` §12.1, §12.2, §12.3 (M-13, M-14). Part 2 is `2026-09-27-balerix-m2-delivery-confirmation.md`; part 3 is `2026-09-27-balerix-m3-github-plugin.md`. This part first: the plugin of part 3 needs the daemon behaviour it describes.

## Global Constraints

- Run cargo through mise: `mise x -- cargo …`, or `mise run <task>`. `mise run check` (lint + test) must pass at the end of every task; `mise run e2e` for Task 7.
- Ports live in `balerix-core`; `balerix-server` depends on `core` and `api` only and never imports `balerix-config` or `balerix-runtime`; only the `balerix` binary wires adapters to ports (AGENTS.md).
- Library crates return `thiserror` errors whose messages start with the config path (`crews.repo.defaults.env: …`); only the binary uses `anyhow`.
- `unsafe_code = "forbid"`; clippy `all` warn, `unwrap_used`/`expect_used` warn outside tests. Never `std::env::set_var`.
- No new dependencies in this plan.
- Every wire-type change is backward compatible on read: `#[serde(default)]` for the new `PluginEntry` field, `skip_serializing_if` so an entry without it serialises as before, so an older `plugins.yaml` still loads and `plugin add` still writes the same text.
- The exact refusal message is the spec's: `<layer path>.<key>: not allowed in a plugin-applied fleet file; the host's default applies`.
- The two new fixtures make `docs/plugin-protocol/` hold 28 files; `crates/balerix-plugin-sdk/tests/conformance.rs` asserts the count and `docs/plugin-protocol.md` §6 states it in words. Change both.
- Secrets never enter argv, env or logs. The plugin never sees the credential bundle.
- Commit after every task. The final PR title is `feat(core): restricted settings surface for plugin-applied fleet files, the operator's fleetDefaults layer, and the fleets-gated PUT answer (Spec M §12)`; it touches `crates/balerix-api/` and `crates/balerix-plugin-sdk/`, so it releases core and every plugin (AGENTS.md).

## Review Focus

1. **A refused key set to `null`** (`env: null`, `sandbox: ~`) in a plugin file must be refused like any other value, never read as "unset the operator's value" (the merge treats null as removal, which would be a way to strip a sandbox the operator set). Test: `a_null_is_still_a_refused_key` in Task 2.
2. **The host's own `settings.json` carrying `env` or `apiKeyHelper`** must still resolve: the check reads the file's raw layers, never the merged result. Test: `the_host_settings_may_carry_what_the_file_may_not` in Task 2.
3. **A `fleetDefaults` that is not a mapping** (`fleetDefaults: 3`) must fail the plugins sync with the entry's path (`plugins[0].fleetDefaults: expected a mapping`), before any plugin starts. Test: `a_non_mapping_fleet_defaults_fails_the_sync_with_its_path` in Task 3.
4. **A `PUT` from a manage-only plugin that the daemon refuses** must still be the 400 with the message, never a bare 204: the gate applies to the success answer only. Test: `a_manage_only_plugin_still_gets_the_400_with_its_message` in Task 4.
5. **An older `plugins.yaml` and an entry written by `plugin add`** must load and round-trip byte-identical when they carry no `fleetDefaults`. Test: `an_entry_without_fleet_defaults_round_trips_as_before` in Task 1.

---

## File Structure

**Created**

- `docs/plugin-protocol/fleet-put-silent.json` — the 204 answer to a manage-only plugin.
- `docs/plugin-protocol/fleet-put-restricted.json` — the §12.1 refusal.

**Modified**

- `crates/balerix-api/src/plugin.rs` — `PluginEntry.fleet_defaults`.
- `crates/balerix-config/src/resolve.rs`, `src/restricted.rs` (new, the per-layer check), `src/lib.rs`, `tests/resolve_golden.rs` — `ResolveOptions::{operator_layer, restricted}`, the check, the layer order.
- `crates/balerix-core/src/plugin.rs`, `src/ports.rs`, `src/fakes.rs` — `ResolvedPlugin.fleet_defaults`, the port's third argument, `FakeResolver::layers`.
- `crates/balerix-server/src/plugins/{host,registry}.rs`, `src/daemon.rs`, `src/plugin_api.rs`, `src/testing.rs`, `tests/support/mod.rs`, `tests/manage_it.rs` — the layer through the registry to `manage_fleet`; the gated answer; the harness's `fleetDefaults` support; the tests.
- `crates/balerix/src/wiring.rs`, `src/commands/{config,dev,fleet}.rs`, `tests/e2e.rs` — restricted `HostResolver`; the widened `ResolveOptions` literals; `manage_from_config` over `Option`; the managed journey on `fleetDefaults`.
- `crates/balerix-plugin-sdk/src/{host,testing}.rs`, `tests/conformance.rs` — `apply_fleet -> Option`, `FakeHost::answer_manage_records`, the two fixtures replayed.
- `docs/plugin-protocol.md`, `docs/THREAT-MODEL.md`, `ARCHITECTURE.md`, the Spec L file (§7, §9 notes), the Spec M file (§16 "Recorded at implementation").

---

### Task 1: `PluginEntry.fleetDefaults` (`balerix-api`)

**Files:**
- Modify: `crates/balerix-api/src/plugin.rs:73-89` (the `PluginEntry` struct) and its tests module.

**Interfaces:**
- Produces: `PluginEntry.fleet_defaults: serde_json::Value`, serialised `fleetDefaults`, default `{}`, omitted from output when it is an empty object.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module at the bottom of `crates/balerix-api/src/plugin.rs`:

```rust
    #[test]
    fn an_entry_without_fleet_defaults_round_trips_as_before() {
        let yaml = "name: gh\nsource: ./gh\nconfig:\n  appId: 1\n";
        let e: PluginEntry = serde_norway::from_str(yaml).unwrap();
        assert_eq!(e.fleet_defaults, json!({}));
        let out = serde_json::to_value(&e).unwrap();
        assert!(
            out.get("fleetDefaults").is_none(),
            "an empty layer is not written back: {out}"
        );
    }

    #[test]
    fn fleet_defaults_is_read_under_its_camel_case_name() {
        let yaml = "name: gh\nsource: ./gh\nfleetDefaults:\n  claude: { binary: /opt/claude }\n  sandbox: { network: { block: false } }\n";
        let e: PluginEntry = serde_norway::from_str(yaml).unwrap();
        assert_eq!(e.fleet_defaults["claude"]["binary"], "/opt/claude");
        assert_eq!(e.fleet_defaults["sandbox"]["network"]["block"], false);
        let out = serde_json::to_value(&e).unwrap();
        assert_eq!(out["fleetDefaults"]["claude"]["binary"], "/opt/claude");
        let err = serde_norway::from_str::<PluginEntry>("name: gh\nsource: ./gh\nfleet_defaults: {}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `fleet_defaults`"), "{err}");
    }
```

If the tests module does not already import `serde_json::json` and `serde_norway`, add `use serde_json::json;` there; `serde_norway` is already a dev-dependency of `balerix-api` if any existing test parses YAML — check with `grep -n serde_norway crates/balerix-api/Cargo.toml`; if absent, add `serde_norway = { workspace = true }` under `[dev-dependencies]` (it is a workspace dependency already, no new crate).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-api fleet_defaults`
Expected: FAIL, `no field fleet_defaults`.

- [ ] **Step 3: Add the field**

In `PluginEntry`, after `config`:

```rust
    /// An operator-written settings layer beneath every fleet file this
    /// plugin applies (Spec M §12.1): the same shape as a fleet file's
    /// `defaults`, and the one place `claude.binary`, `claude.args`,
    /// `env` and `sandbox` may be set for a plugin's agents, since the
    /// plugin's own file may not. Layered between the host's
    /// `settings.json` and the file's `defaults`.
    #[serde(
        default = "empty_object",
        rename = "fleetDefaults",
        skip_serializing_if = "is_empty_object"
    )]
    pub fleet_defaults: Value,
```

And beside `empty_object` in the same file:

```rust
fn is_empty_object(v: &Value) -> bool {
    v.as_object().is_some_and(|m| m.is_empty())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-api plugin`
Expected: PASS, including every existing `PluginEntry` test.

- [ ] **Step 5: Check the CLI still builds and commit**

Run: `mise run check`
Expected: PASS once the two `PluginEntry` literals in `crates/balerix/src/commands/plugin.rs` (the `plugin add` one at line 210 and the test one at line 431) gain `fleet_defaults: serde_json::Value::Object(serde_json::Map::new())`.

```bash
git add crates/balerix-api crates/balerix
git commit -m "feat(api): PluginEntry.fleetDefaults, the operator's layer for a plugin's managed fleets (Spec M §12.1)"
```

---

### Task 2: Restricted resolution and the operator layer (`balerix-config`)

**Files:**
- Create: `crates/balerix-config/src/restricted.rs`
- Modify: `crates/balerix-config/src/resolve.rs:15-68`, `src/lib.rs:8-17`, `tests/resolve_golden.rs:16-22`.

**Interfaces:**
- Consumes: `merge_layers`, `expect_mapping`, `tools_layer`, `validate_agent` as they are.
- Produces:
  - `ResolveOptions { name_override, host_claude_settings, operator_layer: Option<Value>, restricted: bool }` (`Default` still derives).
  - `restricted::check_layer(path: &str, layer: &Value) -> Result<(), ConfigError>`.
  - `restricted::REFUSED_KEYS: [&str; 4]` and `restricted::REFUSED_SETTINGS: [&str; 2]`.

- [ ] **Step 1: Write the failing tests for the check**

Create `crates/balerix-config/src/restricted.rs`:

```rust
//! The restricted settings surface for a plugin-applied fleet file (Spec
//! M §12.1): the file cannot choose what its agents run as. Checked on
//! each raw layer before merging, so the host's own `settings.json` and
//! the operator's `fleetDefaults`, which may carry these keys, are never
//! read here.

use serde_json::Value;

use crate::ConfigError;

/// Keys a plugin-applied file may not set at any layer.
pub const REFUSED_KEYS: [&str; 4] = ["claude.binary", "claude.args", "env", "sandbox"];

/// Keys inside `claude.settings` a plugin-applied file may not set: the
/// two that redirect where credentials go. An open set that drifts with
/// Claude Code releases; reviewed when the pinned `claude` moves.
pub const REFUSED_SETTINGS: [&str; 2] = ["env", "apiKeyHelper"];

const WHY: &str = "not allowed in a plugin-applied fleet file; the host's default applies";

/// Refuses `layer` (a `defaults`, crew `defaults` or agent block, already
/// known to be a mapping or null) when it carries a refused key,
/// present with any value including null. The message names
/// `<path>.<key>`.
pub fn check_layer(path: &str, layer: &Value) -> Result<(), ConfigError> {
    let refused = |key: &str| ConfigError::Invalid {
        path: format!("{path}.{key}"),
        message: WHY.to_string(),
    };
    for key in REFUSED_KEYS {
        if layer.pointer(&format!("/{}", key.replace('.', "/"))).is_some() {
            return Err(refused(key));
        }
    }
    for key in REFUSED_SETTINGS {
        if layer.pointer(&format!("/claude/settings/{key}")).is_some() {
            return Err(refused(&format!("claude.settings.{key}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn each_refused_key_is_named_with_the_layer_path() {
        let cases = [
            (json!({ "claude": { "binary": "/bin/sh" } }), "claude.binary"),
            (json!({ "claude": { "args": ["-c", "id"] } }), "claude.args"),
            (json!({ "env": { "X": "1" } }), "env"),
            (json!({ "sandbox": { "extends": "none" } }), "sandbox"),
            (json!({ "claude": { "settings": { "env": {} } } }), "claude.settings.env"),
            (
                json!({ "claude": { "settings": { "apiKeyHelper": "curl x" } } }),
                "claude.settings.apiKeyHelper",
            ),
        ];
        for (layer, key) in cases {
            let e = check_layer("crews.repo.defaults", &layer).unwrap_err();
            assert_eq!(
                e.to_string(),
                format!(
                    "crews.repo.defaults.{key}: not allowed in a plugin-applied fleet file; the host's default applies"
                )
            );
        }
    }

    #[test]
    fn a_null_is_still_a_refused_key() {
        let e = check_layer("defaults", &json!({ "env": null })).unwrap_err();
        assert_eq!(
            e.to_string(),
            "defaults.env: not allowed in a plugin-applied fleet file; the host's default applies"
        );
        assert!(check_layer("defaults", &json!({ "sandbox": null })).is_err());
    }

    #[test]
    fn the_allowed_keys_pass() {
        let layer = json!({
            "tools": { "node": "22.11.0" },
            "claude": { "settings": { "model": "opus", "permissions": { "allow": ["Bash"] } }, "resume": true },
            "runner": { "type": "tmux" },
            "plugins": { "github": { "kind": "issue", "number": 12 } },
            "branch": "feature/x"
        });
        check_layer("crews.repo.agents.issue-12", &layer).unwrap();
        check_layer("defaults", &Value::Null).unwrap();
        check_layer("defaults", &json!({})).unwrap();
    }
}
```

- [ ] **Step 2: Register the module and run the tests**

In `crates/balerix-config/src/lib.rs`, add `pub mod restricted;` after `pub mod resolve;`, and to the re-exports `pub use restricted::{REFUSED_KEYS, REFUSED_SETTINGS, check_layer};`.

Run: `mise x -- cargo nextest run -p balerix-config restricted`
Expected: PASS (the module is self-contained).

- [ ] **Step 3: Write the failing resolver tests**

Add to the `tests` module of `crates/balerix-config/src/resolve.rs`:

```rust
    fn restricted() -> ResolveOptions {
        ResolveOptions {
            restricted: true,
            ..opts()
        }
    }

    #[test]
    fn restricted_mode_refuses_the_keys_at_every_layer() {
        let at_fleet = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ndefaults:\n  env: { X: \"1\" }\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: {}\n";
        assert_eq!(
            resolve(&file(at_fleet), &restricted()).unwrap_err().to_string(),
            "defaults.env: not allowed in a plugin-applied fleet file; the host's default applies"
        );
        let at_crew = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    defaults:\n      sandbox: { extends: none }\n    agents:\n      a: {}\n";
        assert_eq!(
            resolve(&file(at_crew), &restricted()).unwrap_err().to_string(),
            "crews.c.defaults.sandbox: not allowed in a plugin-applied fleet file; the host's default applies"
        );
        let at_agent = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a:\n        claude: { binary: /bin/sh }\n";
        assert_eq!(
            resolve(&file(at_agent), &restricted()).unwrap_err().to_string(),
            "crews.c.agents.a.claude.binary: not allowed in a plugin-applied fleet file; the host's default applies"
        );
        // unrestricted, the same files resolve (or fail for their own reasons)
        assert!(resolve(&file(at_fleet), &opts()).is_ok());
        assert!(resolve(&file(at_agent), &opts()).is_ok());
    }

    #[test]
    fn the_host_settings_may_carry_what_the_file_may_not() {
        let host = json!({ "env": { "ANTHROPIC_BASE_URL": "https://proxy" }, "apiKeyHelper": "helper" });
        let spec = resolve(
            &file(BASE),
            &ResolveOptions {
                host_claude_settings: Some(host),
                ..restricted()
            },
        )
        .unwrap();
        assert_eq!(
            spec.crews["c"].agents["a"].claude.settings["env"]["ANTHROPIC_BASE_URL"],
            "https://proxy"
        );
    }

    #[test]
    fn the_operator_layer_sits_beneath_the_file_and_is_not_restricted() {
        let operator = json!({
            "claude": { "binary": "/opt/balerix", "args": ["dev", "fake-claude"], "settings": { "model": "haiku" } },
            "sandbox": { "network": { "block": false } },
            "env": { "OPERATOR": "1" }
        });
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ndefaults:\n  claude: { settings: { model: opus } }\ncrews:\n  c:\n    repo: o/r\n    agents:\n      a: {}\n";
        let spec = resolve(
            &file(yaml),
            &ResolveOptions {
                operator_layer: Some(operator),
                ..restricted()
            },
        )
        .unwrap();
        let a = &spec.crews["c"].agents["a"];
        assert_eq!(a.claude.binary, "/opt/balerix");
        assert_eq!(a.claude.args, vec!["dev".to_string(), "fake-claude".to_string()]);
        assert_eq!(a.claude.settings["model"], "opus", "the file's defaults win over the operator's");
        assert_eq!(a.sandbox["network"]["block"], false);
        assert_eq!(a.env["OPERATOR"], "1");
    }

    #[test]
    fn a_non_mapping_operator_layer_is_rejected_with_its_path() {
        let e = resolve(
            &file(BASE),
            &ResolveOptions {
                operator_layer: Some(json!([1])),
                ..restricted()
            },
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "fleetDefaults: expected a mapping");
    }

    #[test]
    fn a_file_setting_only_allowed_keys_resolves_identically_in_both_modes() {
        let yaml = "apiVersion: balerix/v1\nkind: Fleet\nname: f\ndefaults:\n  tools: { node: \"22.11.0\" }\n  claude: { settings: { model: opus }, resume: true }\ncrews:\n  c:\n    repo: o/r\n    git: { push: false, auth: none }\n    agents:\n      a: { branch: feature/x, plugins: { github: { kind: issue, number: 12 } } }\n";
        assert_eq!(
            resolve(&file(yaml), &restricted()).unwrap(),
            resolve(&file(yaml), &opts()).unwrap()
        );
    }
```

- [ ] **Step 4: Run them to verify they fail**

Run: `mise x -- cargo nextest run -p balerix-config resolve`
Expected: FAIL to compile, `no field restricted`.

- [ ] **Step 5: Extend `ResolveOptions` and `resolve`**

Replace `ResolveOptions` in `resolve.rs`:

```rust
/// Inputs to resolution that do not come from the file itself.
#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// `--name` on the CLI; wins over the file's `name`.
    pub name_override: Option<String>,
    /// The host's `~/.claude/settings.json`, layered beneath everything.
    ///
    /// A `hooks` key in this value is dropped before layering, since balerix
    /// owns that key downstream (see `validate::validate_agent`).
    pub host_claude_settings: Option<Value>,
    /// The operator's `fleetDefaults` for the plugin applying this file
    /// (Spec M §12.1): a settings layer between the host settings and the
    /// file's `defaults`, never subject to `restricted`.
    pub operator_layer: Option<Value>,
    /// Spec M §12.1: refuse `restricted::REFUSED_KEYS` at each of the
    /// file's layers. Set for a plugin-applied file, never for `up`.
    pub restricted: bool,
}
```

In `resolve`, after the `host_layer` computation and before `expect_mapping("defaults", …)`:

```rust
    if let Some(layer) = &opts.operator_layer {
        expect_mapping("fleetDefaults", layer)?;
    }
    let check = |path: &str, layer: &Value| -> Result<(), ConfigError> {
        if opts.restricted {
            crate::restricted::check_layer(path, layer)
        } else {
            Ok(())
        }
    };
    expect_mapping("defaults", &file.defaults)?;
    check("defaults", &file.defaults)?;
```

(remove the original `expect_mapping("defaults", &file.defaults)?;` line so it is not run twice). After the crew's `expect_mapping(&format!("{crew_path}.defaults"), …)?;` add `check(&format!("{crew_path}.defaults"), &crew.defaults)?;`. After the agent's `expect_mapping(&agent_path, layer)?;` add `check(&agent_path, layer)?;`. Then change the merge to include the operator layer:

```rust
            let merged = merge_layers(
                host_layer
                    .iter()
                    .chain(opts.operator_layer.iter())
                    .chain([&file.defaults, &crew.defaults, layer]),
            );
```

- [ ] **Step 6: Fix the struct literals**

`ResolveOptions` is built with every field in: `resolve.rs` tests `opts()` (line 121), `tests/resolve_golden.rs:18`, `crates/balerix/src/commands/config.rs:19`, `commands/dev.rs:32`, `commands/fleet.rs:172`, `crates/balerix/src/wiring.rs:59`. Add `..ResolveOptions::default()` to each (the `wiring.rs` one is rewritten in Task 3; give it the `..Default` now so it compiles).

- [ ] **Step 7: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-config` and `mise x -- cargo nextest run -p balerix`
Expected: PASS, including the golden test (its file sets none of the refused keys through restricted mode: it is unrestricted).

- [ ] **Step 8: Commit**

```bash
git add crates/balerix-config crates/balerix
git commit -m "feat(config): restricted resolution and the operator layer for plugin-applied fleet files (Spec M §12.1)"
```

---

### Task 3: The layer through the port to `manage_fleet` (`balerix-core`, `balerix-server`, `balerix`)

**Files:**
- Modify: `crates/balerix-core/src/plugin.rs:59-67` (`ResolvedPlugin`), `src/ports.rs:278-290` (`FleetResolver`), `src/fakes.rs:679-720` (`FakeResolver`) and its test at `:1141`.
- Modify: `crates/balerix-server/src/plugins/registry.rs:31-56` (`PluginInfo`), `:91-112` (`replace_plugins`), the tests building `PluginInfo`/`ResolvedPlugin` (`:313`, `:409`, `:549`); `src/plugins/host.rs:161-222` (`resolve`); `src/daemon.rs:827-858` (`manage_fleet`); `src/testing.rs` wherever `ResolvedPlugin` is built.
- Modify: `crates/balerix/src/wiring.rs:44-131`.
- Modify: `crates/balerix-server/tests/support/mod.rs:177-217`, `tests/manage_it.rs`.

**Interfaces:**
- Consumes: `ResolveOptions::{operator_layer, restricted}` from Task 2; `PluginEntry.fleet_defaults` from Task 1.
- Produces:
  - `ResolvedPlugin.fleet_defaults: Value` (not part of `hash()`).
  - `FleetResolver::resolve(&self, file: &Value, name: &FleetName, operator_layer: &Value) -> Result<FleetSpec, String>`.
  - `FakeResolver::layers(&self) -> Vec<Value>` (the operator layer per call, in order; `calls()` unchanged).
  - `PluginInfo.fleet_defaults: Value`.
  - `support::world_with_entries(extra: &[(&str, &str, &str)]) -> World` (name, manifest lines, extra entry YAML lines).

- [ ] **Step 1: Write the failing core tests**

In `crates/balerix-core/src/fakes.rs`, replace the test `the_fake_resolver_answers_under_the_requested_name_and_records_files` body's calls with the three-argument form and add the layers assertion:

```rust
    #[test]
    fn the_fake_resolver_answers_under_the_requested_name_and_records_files() {
        let r = FakeResolver::default();
        let name: FleetName = "f".parse().unwrap();
        let none = json!({});
        assert_eq!(
            r.resolve(&json!({}), &name, &none),
            Err("name: no resolver answer configured".to_string())
        );
        r.set(Ok(FleetSpec {
            name: "other".into(),
            ..Default::default()
        }));
        let layer = json!({ "env": { "A": "b" } });
        let spec = r.resolve(&json!({ "kind": "Fleet" }), &name, &layer).unwrap();
        assert_eq!(spec.name, "f", "the fixed spec is renamed to the request");
        r.set(Err("crews.c.repo: invalid repo".into()));
        assert_eq!(
            r.resolve(&json!({}), &name, &none),
            Err("crews.c.repo: invalid repo".to_string())
        );
        assert_eq!(
            r.calls(),
            vec![
                ("f".to_string(), json!({})),
                ("f".to_string(), json!({ "kind": "Fleet" })),
                ("f".to_string(), json!({})),
            ]
        );
        assert_eq!(r.layers(), vec![json!({}), layer, json!({})]);
        assert_eq!(
            FakeResolver::failing("x").resolve(&json!({}), &name, &none),
            Err("x".to_string())
        );
        assert_eq!(
            FakeResolver::answering(FleetSpec::default())
                .resolve(&json!({}), &name, &none)
                .unwrap()
                .name,
            "f"
        );
    }

    #[test]
    fn fleet_defaults_do_not_enter_the_plugin_hash() {
        let base = ResolvedPlugin {
            name: "p".parse().unwrap(),
            package: "/pkg".into(),
            manifest: serde_json::from_value(json!({
                "apiVersion": "balerix/v1", "kind": "Plugin", "name": "p",
                "version": "0.1.0", "protocol": 1, "start": "serve"
            }))
            .unwrap(),
            config: json!({}),
            fleet_defaults: json!({}),
            digest: None,
        };
        let edited = ResolvedPlugin {
            fleet_defaults: json!({ "env": { "A": "b" } }),
            ..base.clone()
        };
        assert_eq!(base.hash(), edited.hash(), "an operator edit does not restart the plugin");
    }
```

(Use the `ResolvedPlugin` literal style the file's existing tests use; `Clone` is derived on it already, else derive it.)

- [ ] **Step 2: Run to verify failure**

Run: `mise x -- cargo nextest run -p balerix-core fake_resolver fleet_defaults`
Expected: FAIL to compile.

- [ ] **Step 3: Change the core types**

`crates/balerix-core/src/plugin.rs`, in `ResolvedPlugin` after `config`:

```rust
    /// The operator's `fleetDefaults` from `plugins.yaml` (Spec M §12.1),
    /// the layer beneath every fleet file this plugin applies. Not part
    /// of `hash()`: an edit takes effect at the plugin's next apply, and
    /// restarting the plugin for it would gain nothing.
    pub fleet_defaults: Value,
```

Add `.field("fleet_defaults", &self.fleet_defaults)` to its `Debug` (it is operator config, not a secret).

`crates/balerix-core/src/ports.rs`, the trait:

```rust
pub trait FleetResolver: Send + Sync {
    /// `file` is the YAML fleet file's structure as JSON, `name` the fleet
    /// it must resolve to (a `name` inside the file has already been
    /// checked against it), `operator_layer` the applying plugin's
    /// `fleetDefaults` (Spec M §12.1; `{}` when it has none). The host's
    /// `claude.settings` are folded in beneath both, and the file is held
    /// to the restricted surface. `Err` is the resolver's own message,
    /// config path first.
    fn resolve(
        &self,
        file: &serde_json::Value,
        name: &FleetName,
        operator_layer: &serde_json::Value,
    ) -> Result<FleetSpec, String>;
}
```

`crates/balerix-core/src/fakes.rs`, `FakeResolver`: add `layers: Mutex<Vec<Value>>`, push `operator_layer.clone()` in `resolve`, and:

```rust
    /// The operator layer handed to each call, in order.
    pub fn layers(&self) -> Vec<Value> {
        lock(&self.layers).clone()
    }
```

- [ ] **Step 4: Thread it through the server**

`crates/balerix-server/src/plugins/host.rs`, in `resolve`, the `ResolvedPlugin` literal gains:

```rust
                fleet_defaults: {
                    if !entry.fleet_defaults.is_object() {
                        return Err(entry_error(i, "fleetDefaults", "expected a mapping".into()));
                    }
                    entry.fleet_defaults.clone()
                },
```

`crates/balerix-server/src/plugins/registry.rs`: `PluginInfo` gains `pub fleet_defaults: Value,` (with a doc line: "The operator's layer for the fleets this plugin applies (Spec M §12.1); refreshed on every sync, changed or not."), its `Debug` prints it, and `replace_plugins` sets `fleet_defaults: p.fleet_defaults.clone(),` for every plugin, kept or not. Every test literal of `PluginInfo` and `ResolvedPlugin` in `registry.rs` and `src/testing.rs` gains `fleet_defaults: json!({})`.

`crates/balerix-server/src/daemon.rs`, `manage_fleet`, replace the resolve call:

```rust
        let layer = self
            .registry()
            .plugin(plugin)
            .map(|p| p.fleet_defaults)
            .unwrap_or_else(|| serde_json::json!({}));
        let resolver = self.ports.resolver.clone();
        let resolve_name = name.clone();
        let spec = tokio::task::spawn_blocking(move || {
            resolver.resolve(&file, &resolve_name, &layer)
        })
        .await
        .map_err(|e| DaemonError::Internal(e.to_string()))?
        .map_err(DaemonError::Invalid)?;
```

Update the doc comment above `manage_fleet` to say the file is resolved "beneath the plugin's `fleetDefaults` and held to the restricted surface (Spec M §12.1)".

- [ ] **Step 5: Rewrite `HostResolver`**

`crates/balerix/src/wiring.rs`:

```rust
/// The pure half: `file` resolved as fleet `name` beneath `defaults` and
/// the operator's `layer`, held to the restricted surface (Spec M §12.1).
pub fn resolve_file(
    file: &Value,
    name: &FleetName,
    defaults: &HostDefaults,
    layer: &Value,
) -> Result<FleetSpec, String> {
    let file = from_value(file).map_err(|e| e.to_string())?;
    let spec = resolve(
        &file,
        &ResolveOptions {
            name_override: Some(name.to_string()),
            host_claude_settings: defaults.claude_settings.clone(),
            operator_layer: Some(layer.clone()),
            restricted: true,
        },
    )
    .map_err(|e| e.to_string())?;
    // names and repos, as `load_request` checks before any request
    Fleet::try_from(spec.clone()).map_err(|e| e.to_string())?;
    Ok(spec)
}
```

and in `impl FleetResolver for HostResolver`, the signature gains `layer: &Value` and the body ends `resolve_file(file, name, &defaults, layer)`. Tests: pass `&json!({})` in the two existing tests, and add:

```rust
    #[test]
    fn the_host_resolver_is_restricted_and_layers_the_operators_defaults() {
        let name: FleetName = "f".parse().unwrap();
        let mut f = file();
        f["defaults"] = json!({ "sandbox": { "extends": "none" } });
        let e = resolve_file(&f, &name, &HostDefaults::default(), &json!({})).unwrap_err();
        assert_eq!(
            e,
            "defaults.sandbox: not allowed in a plugin-applied fleet file; the host's default applies"
        );
        let layer = json!({ "claude": { "binary": "/opt/balerix", "args": ["dev", "fake-claude"] } });
        let spec = resolve_file(&file(), &name, &HostDefaults::default(), &layer).unwrap();
        assert_eq!(spec.crews["c"].agents["a"].claude.binary, "/opt/balerix");
    }
```

- [ ] **Step 6: Extend the server harness and write the failing `manage_it` tests**

`crates/balerix-server/tests/support/mod.rs`: rename the body of `world_with` into

```rust
/// `world`, plus one package per `(name, manifest lines, entry lines)`
/// in `extra`, declared after `flow` and `web`; `entry lines` is YAML
/// appended under the entry, four-space indented (`"    fleetDefaults:
/// { env: { A: b } }\n"`), or "".
pub async fn world_with_entries(extra: &[(&str, &str, &str)]) -> World {
```

with the loop writing `plugins_yaml.push_str(&format!("  - name: {name}\n    source: ./{name}-pkg\n{entry}"));`, and make `world_with(extra: &[(&str, &str)])` map each pair to `(name, manifest, "")` and call it. Then in `crates/balerix-server/tests/manage_it.rs` add:

```rust
/// Spec M §12.1: the operator's `fleetDefaults` reaches the resolver as
/// the layer for every file this plugin applies, and only this plugin's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_layer_reaches_the_resolver_per_plugin() {
    let w = world_with_entries(&[
        (
            "gh",
            "needs: [fleets, manage]\n",
            "    fleetDefaults: { env: { OPERATOR: \"1\" }, sandbox: { network: { block: false } } }\n",
        ),
        ("other", "needs: [fleets, manage]\n", ""),
    ])
    .await;
    let _gh = start_silent(&w, "gh").await;
    let _other = start_silent(&w, "other").await;
    w.h.resolver.set(Ok(spec("f")));
    let (s, _) = w.api.plugin(
        &token(&w, "gh").await,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!(s, 200);
    w.h.resolver.set(Ok(spec("g")));
    let (s, _) = w.api.plugin(
        &token(&w, "other").await,
        "PUT",
        "/v1/plugin-host/fleets/g",
        Some(&json!({ "file": file("g") })),
    );
    assert_eq!(s, 200);
    assert_eq!(
        w.h.resolver.layers(),
        vec![
            json!({ "env": { "OPERATOR": "1" }, "sandbox": { "network": { "block": false } } }),
            json!({}),
        ]
    );
}

/// Review focus 3: a layer that is not a mapping fails the sync with the
/// entry's path, before any plugin starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_mapping_fleet_defaults_fails_the_sync_with_its_path() {
    let w = world_with_entries(&[("gh", "needs: [manage]\n", "    fleetDefaults: 3\n")]).await;
    let e = w.daemon.sync_plugins().await.unwrap_err().to_string();
    assert_eq!(e, "plugins.yaml: plugins[2].fleetDefaults: expected a mapping");
}
```

`world_with_entries` calls `sync_plugins().await.unwrap()` inside; for the second test that would panic. Make `world_with_entries` tolerate a failing first sync: change the line to `let _ = daemon.sync_plugins().await;` and add a comment ("a test may want the failure itself; the ones that need plugins up call `start_silent`, which fails loudly if they are not"). Import `world_with_entries` in `manage_it.rs`.

- [ ] **Step 7: Run everything**

Run: `mise run check`
Expected: PASS, the two new tests included. (`gh` is `plugins[2]` because `flow` and `web` come first.)

- [ ] **Step 8: Commit**

```bash
git add crates/balerix-core crates/balerix-server crates/balerix
git commit -m "feat(core): the operator's fleetDefaults reach the resolver, which holds a plugin's file to the restricted surface (Spec M §12.1)"
```

---

### Task 4: The fleets-gated `PUT` answer (`balerix-server`)

**Files:**
- Modify: `crates/balerix-server/src/plugin_api.rs:130-151` (`put_fleet`), `tests/manage_it.rs`.

**Interfaces:**
- Produces: `PUT /v1/plugin-host/fleets/{name}` answers `200 FleetRecord` when the caller declares `fleets`, `204` with an empty body otherwise; every error status is unchanged.

- [ ] **Step 1: Write the failing tests**

Add to `crates/balerix-server/tests/manage_it.rs`:

```rust
/// Spec M §12.2 (M-14): the record carries the resolved spec with the
/// host's settings folded in, so it goes only to a plugin that may read
/// records; a manage-only plugin gets 204 and nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manage_only_plugin_gets_204_and_a_fleets_plugin_the_record() {
    let w = world_with(&[("silent", "needs: [manage]\n"), ("gh", "needs: [fleets, manage]\n")]).await;
    let _silent = start_silent(&w, "silent").await;
    let _gh = start_silent(&w, "gh").await;
    w.h.resolver.set(Ok(spec("f")));
    let (s, v) = w.api.plugin(
        &token(&w, "silent").await,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!((s, v), (204, Value::String(String::new())));
    assert_eq!(
        w.daemon.get(&"f".parse().unwrap()).await.map(|r| r.owner),
        Some(Some("silent".into())),
        "applied all the same"
    );
    w.h.resolver.set(Ok(spec("g")));
    let (s, v) = w.api.plugin(
        &token(&w, "gh").await,
        "PUT",
        "/v1/plugin-host/fleets/g",
        Some(&json!({ "file": file("g") })),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["owner"], "gh");
    assert_eq!(v["spec"]["name"], "g");
}

/// Review focus 4: the gate is on the success answer only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manage_only_plugin_still_gets_the_400_with_its_message() {
    let w = world_with(&[("silent", "needs: [manage]\n")]).await;
    let _silent = start_silent(&w, "silent").await;
    let bad = "crews.c.defaults.sandbox: not allowed in a plugin-applied fleet file; the host's default applies";
    w.h.resolver.set(Err(bad.into()));
    let (s, v) = w.api.plugin(
        &token(&w, "silent").await,
        "PUT",
        "/v1/plugin-host/fleets/f",
        Some(&json!({ "file": file("f") })),
    );
    assert_eq!((s, v["error"].as_str()), (400, Some(bad)));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `mise x -- cargo nextest run -p balerix-server --test manage_it manage_only`
Expected: FAIL, the first asserts `(200, {...})` against `(204, "")`.

- [ ] **Step 3: Gate the answer**

In `crates/balerix-server/src/plugin_api.rs`, change `put_fleet`'s return type to `Result<Response, ApiError>` and its tail to:

```rust
    let record = state.daemon.manage_fleet(&plugin, &name, body.file).await?;
    // Spec M §12.2: the record carries the resolved spec, host settings
    // folded in; reading records is what `fleets` means.
    if state.daemon.registry().has(&plugin, Capability::Fleets) {
        Ok(Json(record).into_response())
    } else {
        Ok(StatusCode::NO_CONTENT.into_response())
    }
```

Update the handler's doc comment: "`manage` gates it; the answer is the record for a caller with `fleets`, 204 otherwise (Spec M §12.2)".

- [ ] **Step 4: Run the tests**

Run: `mise x -- cargo nextest run -p balerix-server --test manage_it`
Expected: PASS. The existing `a_plugin_with_manage_applies_and_downs_a_fleet_it_owns` still passes: its `gh` declares `fleets`.

- [ ] **Step 5: Commit**

```bash
git add crates/balerix-server
git commit -m "feat(server): PUT fleets/{name} answers the record only to a plugin with fleets, 204 otherwise (Spec M §12.2)"
```

---

### Task 5: The SDK, `FakeHost`, the fixtures and the protocol doc

**Files:**
- Modify: `crates/balerix-plugin-sdk/src/host.rs:157-173` (`apply_fleet`), `src/testing.rs` (`Inner`, `FakeHost`, `put_fleet`), `tests/conformance.rs:27-32, 317-355`.
- Modify: `crates/balerix/src/commands/dev.rs:335-352` (`manage_from_config`).
- Create: `docs/plugin-protocol/fleet-put-silent.json`, `docs/plugin-protocol/fleet-put-restricted.json`.
- Modify: `docs/plugin-protocol.md:61-64, 141-156, 268`.

**Interfaces:**
- Produces:
  - `Host::apply_fleet(&self, name: &str, file: &Value) -> Result<Option<FleetRecord>, SdkError>` (`None` on 204).
  - `FakeHost::answer_manage_records(&self, answer: bool)` (default `true`; `false` makes `PUT` answer 204 while still recording and bumping the watch).

- [ ] **Step 1: Write the fixtures**

`docs/plugin-protocol/fleet-put-silent.json`:

```json
{
  "route": "PUT /v1/plugin-host/fleets/gh-acme-api",
  "direction": "plugin-to-daemon",
  "request": {
    "file": {
      "apiVersion": "balerix/v1",
      "kind": "Fleet",
      "name": "gh-acme-api",
      "crews": {
        "repo": {
          "repo": "acme/api",
          "agents": { "issue-12": { "branch": "feature/issue-12" } }
        }
      }
    }
  },
  "status": 204,
  "response": null,
  "note": "The caller declares manage without fleets (Spec M §12.2): applied, and answered with nothing."
}
```

`docs/plugin-protocol/fleet-put-restricted.json`:

```json
{
  "route": "PUT /v1/plugin-host/fleets/gh-acme-api",
  "direction": "plugin-to-daemon",
  "request": {
    "file": {
      "apiVersion": "balerix/v1",
      "kind": "Fleet",
      "name": "gh-acme-api",
      "defaults": { "sandbox": { "extends": "none" } },
      "crews": {
        "repo": {
          "repo": "acme/api",
          "agents": { "issue-12": {} }
        }
      }
    }
  },
  "status": 400,
  "response": {
    "error": "defaults.sandbox: not allowed in a plugin-applied fleet file; the host's default applies"
  }
}
```

- [ ] **Step 2: Write the failing conformance steps**

In `crates/balerix-plugin-sdk/tests/conformance.rs`, change the count `26` to `28`, and after the `fake.fail_manage(None);` at line 355 add:

```rust
    // Spec M §12.1: the restricted refusal is a 400 like any other.
    let restricted = fx["fleet-put-restricted"]["response"]["error"]
        .as_str()
        .unwrap();
    fake.fail_manage(Some((400, restricted)));
    let e = host
        .apply_fleet("gh-acme-api", &fx["fleet-put-restricted"]["request"]["file"])
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), format!("daemon: HTTP 400: {restricted}"));
    fake.fail_manage(None);

    // Spec M §12.2: a manage-only caller is answered 204 and `None`.
    fake.answer_manage_records(false);
    let file = fx["fleet-put-silent"]["request"]["file"].clone();
    assert_eq!(host.apply_fleet("gh-acme-api", &file).await.unwrap(), None);
    let resp = c
        .put(format!("{}/v1/plugin-host/fleets/gh-acme-api", fake.url))
        .bearer_auth("tok")
        .json(&fx["fleet-put-silent"]["request"])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        fx["fleet-put-silent"]["status"].as_u64().unwrap() as u16
    );
    assert!(resp.bytes().await.unwrap().is_empty());
    fake.answer_manage_records(true);
```

Also change the earlier `host.apply_fleet("gh-acme-api", &file).await.unwrap()` (line 324) to `.unwrap().unwrap()` since it now answers `Option`.

- [ ] **Step 3: Run to verify failure**

Run: `mise x -- cargo nextest run -p balerix-plugin-sdk --test conformance`
Expected: FAIL to compile (`answer_manage_records`, and the `Option`).

- [ ] **Step 4: Change the SDK**

`crates/balerix-plugin-sdk/src/host.rs`, `apply_fleet`:

```rust
    /// `PUT fleets/{name}` (Spec L §3.1): applies an unresolved fleet
    /// file — the YAML fleet file's structure as JSON — as a fleet this
    /// plugin owns. Needs `manage`. Answers the record when this plugin
    /// also declares `fleets`, `None` otherwise (204, Spec M §12.2). 400
    /// with the resolver's message (config path first) when the file does
    /// not resolve, which includes the restricted surface (Spec M §12.1:
    /// `claude.binary`, `claude.args`, `env`, `sandbox` are the operator's
    /// to set, in `plugins.yaml`); 409 `fleet <name> is managed by plugin
    /// <p>` or `… is not managed by a plugin` when the name belongs to
    /// someone else. The call returns before the fleet is ready: watch
    /// `fleets/watch` or poll `fleet` for that.
    pub async fn apply_fleet(
        &self,
        name: &str,
        file: &Value,
    ) -> Result<Option<FleetRecord>, SdkError> {
        let (status, bytes) = self
            .send(
                self.http
                    .put(self.url(&format!("fleets/{name}")))
                    .timeout(APPLY_TIMEOUT)
                    .json(&json!({ "file": file })),
            )
            .await?;
        match status {
            204 => Ok(None),
            200..=299 => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| SdkError::Transport(format!("bad reply: {e}"))),
            _ => Err(Self::status_error(status, &bytes)),
        }
    }
```

`crates/balerix-plugin-sdk/src/testing.rs`: `Inner` gains `/// `answer_manage_records(false)`: `PUT fleets/{name}` answers 204.` `manage_silent: Mutex<bool>` (initialised `false` where `Inner` is built); `FakeHost` gains:

```rust
    /// Whether `PUT fleets/{name}` answers the record (the daemon's answer
    /// to a plugin with `fleets`) or 204 with nothing (Spec M §12.2, a
    /// plugin with `manage` alone). The call is recorded and the watch
    /// bumped either way. Default: the record.
    pub fn answer_manage_records(&self, answer: bool) {
        *self
            .inner
            .manage_silent
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = !answer;
    }
```

and `put_fleet`'s tail becomes:

```rust
    inner.fleets_changed.send_modify(|n| *n += 1);
    if *inner.manage_silent.lock().unwrap_or_else(|e| e.into_inner()) {
        return StatusCode::NO_CONTENT.into_response();
    }
    Json(record).into_response()
```

`crates/balerix/src/commands/dev.rs`, `manage_from_config`:

```rust
        (Some(fleet), Some(file)) => match host.apply_fleet(fleet, file).await {
            Ok(Some(record)) => serde_json::to_value(record)?,
            Ok(None) => json!({ "applied": true }),
            Err(e) => json!({ "error": e.to_string() }),
        },
```

- [ ] **Step 5: Run the SDK and binary tests**

Run: `mise x -- cargo nextest run -p balerix-plugin-sdk` and `mise x -- cargo nextest run -p balerix`
Expected: PASS.

- [ ] **Step 6: Update the protocol doc**

`docs/plugin-protocol.md`, replace the `PUT fleets/{name}` row (line 61) with two rows and add the restricted row after the rejected one:

```
| `PUT fleets/{name}` | `manage` + `fleets` | `{ file }` — the fleet file's structure as JSON (`apiVersion`, `kind`, `name`?, `defaults`, `crews`) | `FleetRecord`, `owner` set to this plugin; the record as applied, so waiting for its agents to turn `Ready` goes through `fleets/watch` (or `GET fleets/{name}`) | 200 | `fleet-put.json` |
| `PUT fleets/{name}`, caller without `fleets` | `manage` | same | — (applied all the same; the record carries the resolved spec, which only `fleets` may read, Spec M §12.2) | 204 | `fleet-put-silent.json` |
| `PUT fleets/{name}`, file does not resolve | `manage` | same | `{ error }`, config path first | 400 | `fleet-put-rejected.json` |
| `PUT fleets/{name}`, file sets `claude.binary`, `claude.args`, `env`, `sandbox`, or `claude.settings.{env,apiKeyHelper}` at any layer | `manage` | same | `{ error }`: `<layer>.<key>: not allowed in a plugin-applied fleet file; the host's default applies` (Spec M §12.1) | 400 | `fleet-put-restricted.json` |
```

In the "**Managed fleets** (Spec L)" paragraph (line 141), after "folds the host's `claude.settings` in," insert: "then the entry's `fleetDefaults` from `plugins.yaml` (Spec M §12.1: the operator's layer, the same shape as a fleet file's `defaults`, and the only place a plugin's agents get their `claude.binary`, `claude.args`, `env` and `sandbox`; the file itself may not set those, nor `claude.settings.env` or `apiKeyHelper`, at any layer),". Replace "The call returns once the spec is applied, not when the fleet is ready: watch `fleets/watch`." with "The call returns once the spec is applied, not when the fleet is ready: watch `fleets/watch`. The record is the answer only for a plugin that also declares `fleets`; a manage-only plugin gets 204 (Spec M §12.2)."

In §6 (line 268) change "twenty-six" to "twenty-eight". In §7's `secrets` paragraph (line 329), add a sentence at its end: "An entry may also carry `fleetDefaults`, the settings layer beneath every fleet the plugin applies (§3, `PUT fleets/{name}`); unlike `secrets` it is not part of the plugin's restart hash, so an edit takes effect at the plugin's next apply."

- [ ] **Step 7: Run the whole check and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add crates/balerix-plugin-sdk crates/balerix docs/plugin-protocol docs/plugin-protocol.md
git commit -m "feat(sdk): apply_fleet answers Option<FleetRecord> for the fleets-gated PUT, with the silent and restricted fixtures (Spec M §12.2)"
```

---

### Task 6: The e2e's managed journey on `fleetDefaults`

**Files:**
- Modify: `crates/balerix/tests/e2e.rs:195-219` (`managed_fleet_file`), `:1017-1030` (the `plugins.yaml` write).

- [ ] **Step 1: Move fake-claude into the operator's layer**

Replace `managed_fleet_file` with:

```rust
/// The fleet a managed-fleet journey's plugin applies: `fleet_yaml`'s
/// shape as the JSON object `PUT fleets/{name}` takes, one agent. What
/// the agent runs as (fake-claude, its sandbox) is not the file's to say
/// (Spec M §12.1): it comes from the entry's `fleetDefaults`, see
/// `managed_fleet_defaults`.
fn managed_fleet_file(bare: &Path) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "balerix/v1", "kind": "Fleet", "name": "managed",
        "defaults": {
            "claude": { "settings": { "model": "sonnet" } },
            "tools": {}
        },
        "crews": {
            "c": {
                "repo": format!("file://{}", bare.display()),
                "ref": "main",
                "git": { "push": false, "auth": "none" },
                "agents": { "alice": {} }
            }
        }
    })
}

/// The operator's layer for the fake plugin's fleets: fake-claude under a
/// real nono profile, exactly what `fleet_yaml` sets for a CLI fleet.
fn managed_fleet_defaults() -> serde_json::Value {
    serde_json::json!({
        "claude": { "binary": BALERIX, "args": ["dev", "fake-claude", "--verbose"] },
        "sandbox": { "network": { "block": false } }
    })
}
```

and the `plugins.yaml` write in the managed journey:

```rust
    // JSON is YAML: each object rides on one line after its key
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: fake\n    source: \"{}\"\n    config: {}\n    fleetDefaults: {}\n",
            pkg.display(),
            config,
            managed_fleet_defaults()
        ),
    )
    .unwrap();
```

- [ ] **Step 2: Add the restricted refusal to the journey**

The fake plugin applies at hello, from `config.manage`. Give the journey a second, refused apply by a second plugin entry: after the readiness loop (the `managed fleet never ready` assertion), add a second package `refused` whose `config.manage.file` is `managed_fleet_file(&bare)` with `"defaults": { "sandbox": { "extends": "none" } }` merged in and `"name": "refused"`, no `fleetDefaults`; append it to `plugins.yaml`, run `w.ok(&["plugin", "sync"])` (the CLI command that makes the daemon re-read `plugins.yaml`), wait for `plugin list` to show `refused` ready, then:

```rust
    let outcome = wait_file(&w.state().join("plugins/refused/scratch/fake-plugin.manage"));
    let outcome: serde_json::Value = serde_json::from_str(&outcome).unwrap();
    assert_eq!(
        outcome["error"],
        "daemon: HTTP 400: defaults.sandbox: not allowed in a plugin-applied fleet file; the host's default applies",
        "{outcome}"
    );
    assert!(w.ok(&["list"]).lines().all(|l| !l.starts_with("refused ")));
```

- [ ] **Step 3: Run the e2e**

Run: `mise run e2e`
Expected: PASS. If the managed fleet never turns Ready, read `target/tmp/.../fleets/managed/.../logs`: the likely cause is the layer not reaching the resolver (Task 3) or `--verbose` missing from the args.

- [ ] **Step 4: Commit**

```bash
git add crates/balerix/tests/e2e.rs
git commit -m "test(e2e): the managed journey runs fake-claude from fleetDefaults and sees the restricted refusal (Spec M §12.1)"
```

---

### Task 7: Documentation

**Files:**
- Modify: `docs/THREAT-MODEL.md:79, 106`; `ARCHITECTURE.md:175-185, 199-202`; `docs/superpowers/specs/2026-09-22-balerix-l-managed-fleets-design.md:208-216, 254-257`; `docs/superpowers/specs/2026-09-22-balerix-m-github-plugin-design.md` (append a §16).

- [ ] **Step 1: Rewrite the threat-model bullet**

Replace the `manage` bullet (line 79) with:

```
- **A plugin with `manage` runs agents under the host's defaults with the operator's credentials.** It can apply any fleet file, so any repository the operator's gh token reaches, and down what it created. The operator's choice in `needs`, like `attach` and `workspace`; bounded by the owner rule (it cannot touch a fleet it did not create, and the CLI cannot silently take over its fleets), by the same validation `up` applies, and by every agent still running inside its nono profile. The credentials never cross the wire: the route takes a file, the daemon resolves it against the host's `~/.claude/settings.json` (a malformed one is a 400 carrying the resolver's message) and then reads the bundle (`crates/balerix-server/src/daemon.rs::manage_fleet`); a `CredentialSource` failure reaches the plugin as a 500 whose message may name host files (e.g. `~/.claude/.credentials.json: invalid JSON`) but never a token value. What the agent runs as is not the file's to say (Spec M §12.1): the resolver refuses `claude.binary`, `claude.args`, `env`, `sandbox` and `claude.settings.{env,apiKeyHelper}` at every layer of a plugin-applied file (`crates/balerix-config/src/restricted.rs`), so a `manage` plugin can prompt an agent that holds the credentials, which any permitted person already can, but cannot run its own binary beside them, redirect them, or widen the sandbox. Those keys are the operator's, per plugin, in `plugins.yaml` `fleetDefaults`. The `PUT` answer carries the resolved spec (host `settings.json` folded in) only to a plugin that also declares `fleets`; a manage-only plugin gets 204 (Spec M §12.2). The inner `claude.settings` list is an open set reviewed when the pinned `claude` moves.
```

In the table row (line 106), append to the second column: "; the file is held to the restricted surface (`claude.binary`, `claude.args`, `env`, `sandbox`, `claude.settings.{env,apiKeyHelper}` refused at every layer, 400 with the path) beneath the entry's `fleetDefaults`; the record is answered only with `fleets` (204 otherwise)", and to the third column: ", `balerix-config/src/restricted.rs`".

- [ ] **Step 2: ARCHITECTURE and Spec L**

`ARCHITECTURE.md`, in the "**Plugin-managed fleets (Spec L)**" paragraph, after "resolves it through the `FleetResolver` port," insert "beneath the entry's `fleetDefaults` from `plugins.yaml` and held to the restricted surface (Spec M §12.1: a plugin's file cannot set `claude.binary`, `claude.args`, `env`, `sandbox` or `claude.settings.{env,apiKeyHelper}`; those are the operator's, per plugin),". After the "`claude.settings.hooks` is balerix-owned" invariant (lines 199-202) add a sibling:

```
- **A plugin's fleet file cannot choose what its agents run as.** The
  restricted surface (Spec M §12.1) is checked in `balerix-config` on the
  file's raw layers, so the host's `settings.json` and the operator's
  `fleetDefaults` may carry what the file may not.
```

Spec L: at the end of the §7 bullet (line 216, "…deferred to Spec M.") append " Settled by Spec M §12 (2026-09-27): the daemon refuses those keys from a plugin's file, the operator sets them in `plugins.yaml` `fleetDefaults`, and the `PUT` answer needs `fleets`." At the end of the §9 bullet (line 257) append " Settled by Spec M §12.1."

- [ ] **Step 3: Record in Spec M**

Append to `docs/superpowers/specs/2026-09-22-balerix-m-github-plugin-design.md`:

```
## 16. Recorded at implementation, part 1 (daemon side)

- The check is `balerix_config::restricted::check_layer`, run on each
  raw layer inside `resolve` when `ResolveOptions::restricted` is set; a
  refused key present with any value, null included, is refused.
- `ResolveOptions::operator_layer` is the `fleetDefaults` mapping;
  `expect_mapping("fleetDefaults", …)` names it when it is not one, and
  the plugin sync refuses a non-mapping with
  `plugins.yaml: plugins[i].fleetDefaults: expected a mapping`.
- `FleetResolver::resolve` took a third argument, the layer;
  `FakeResolver::layers()` records it. The layer is not in
  `ResolvedPlugin::hash()`.
- `Host::apply_fleet` answers `Option<FleetRecord>`; `FakeHost::
  answer_manage_records(false)` fakes the 204. Fixtures
  `fleet-put-silent.json` and `fleet-put-restricted.json`; the count is
  twenty-eight.
- The e2e's managed journey runs fake-claude from `fleetDefaults` and
  asserts the refusal through a second, manage-only fake plugin.
```

- [ ] **Step 4: Final check and commit**

Run: `mise run check && mise run plugins`
Expected: PASS (`plugins` proves the SDK change did not break common, flow, web or matrix; none of them calls `apply_fleet`).

```bash
git add docs ARCHITECTURE.md
git commit -m "docs: the restricted settings surface and fleetDefaults in the threat model, architecture and Spec L (Spec M §12.3)"
```

Open the PR with the title from Global Constraints; its body closes nothing yet (#64 closes when part 3 lands the plugin whose issue it is, per Spec M §15.9), but says "Settles #64's three questions; the plugin that exercises them is part 3."

---

## Done when

1. `mise run check`, `mise run plugins` and `mise run e2e` pass.
2. A plugin-applied file setting any of `claude.binary`, `claude.args`, `env`, `sandbox`, `claude.settings.env` or `claude.settings.apiKeyHelper` at any layer is a 400 naming `<layer>.<key>`; `up` with the same file behaves as before.
3. `plugins.yaml` `fleetDefaults` reaches the resolver for that plugin's files only, beneath the file's `defaults`, and the e2e's managed fleet runs fake-claude from it.
4. `PUT fleets/{name}` answers the record with `fleets` and 204 without; the SDK's `apply_fleet` answers `Option`.
5. The threat model, the protocol doc, ARCHITECTURE and Spec L say so.
