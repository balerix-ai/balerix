# Mise Pool Hierarchy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every agent a private, writable mise install directory that
falls back through read-only crew, fleet and daemon pools, so `mise install`
in an agent's worktree works without re-downloading what the fleet already
installed.

**Architecture:** Tools declared at each level of the fleet file are
installed by the daemon into that level's own mise data directory. Agents
run with their own data directory inside `home/` and reach the three pools
through mise's `MISE_SHARED_INSTALL_DIRS`, which is read-only by
construction. Because mise skips a version it finds in a shared directory,
each level's install lands only what that level added.

**Tech Stack:** Rust 2024 edition, `mise` 2026.9.x, `nono` 0.75.0,
`cargo-nextest`, `insta` snapshots, `toml`, `sha2`.

**Spec:** `docs/superpowers/specs/2026-09-09-balerix-e-mise-pool-hierarchy-design.md`

## Global Constraints

- Every tool version in a `mise.toml` is exact. `is_exact_version`
  (`crates/balerix-core/src/version.rs`) is the single judge; reuse it.
- Ports in `balerix-core` stay synchronous. No `tokio` in `balerix-core`.
- `balerix-core` never depends on adapter crates. Paths come from
  `StateLayout`, binaries from `ToolPaths`; nothing in `balerix-runtime`
  reads the process environment.
- The reconciler's `plan()` is a pure function and must not be touched by
  this work. Only `execute.rs` changes.
- Stored `fleet.json` written by an older daemon must still deserialize.
  Every new serde field carries `#[serde(default)]`.
- Run `mise run check` before every commit. The pre-commit hook runs the
  full check itself, so redirect its output to a file and keep one commit
  per issue.
- Integration tests that need real tools use
  `support::require_or_skip(...)` so they fail under
  `BALERIX_REQUIRE_TOOLS=1` and skip otherwise.
- `balerix-runtime` already depends on `sha2` and `hex`. Do not add
  dependencies for this work.

---

### Task 1: Pool paths in the layout

**Files:**
- Modify: `crates/balerix-runtime/src/layout.rs`
- Modify: `crates/balerix-runtime/src/lib.rs` (export `FleetPaths`)
- Test: `crates/balerix-runtime/src/layout.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct FleetPaths { pub root: PathBuf, pub mise_toml: PathBuf }`
  - `impl FleetPaths { pub fn mise_pool(&self) -> PathBuf; pub fn installed_marker(&self) -> PathBuf }`
  - `impl CrewPaths { pub fn mise_toml(&self) -> PathBuf; pub fn mise_pool(&self) -> PathBuf; pub fn installed_marker(&self) -> PathBuf }`
  - `impl AgentPaths { pub fn mise_data_dir(&self) -> PathBuf }`
  - `impl StateLayout { pub fn fleet(&self, f: &FleetName) -> FleetPaths; pub fn system_installed_marker(&self) -> PathBuf; pub fn agent_pools(&self, id: &AgentId) -> Vec<PathBuf>; pub fn shared_install_dirs(&self, id: &AgentId) -> String }`
  - `agent_pools` returns pool **roots** nearest-first: crew, fleet, daemon.
  - `shared_install_dirs` returns those roots' `installs/` subdirectories
    joined with `:`, which is the value of `MISE_SHARED_INSTALL_DIRS`.

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` in `crates/balerix-runtime/src/layout.rs`:

```rust
    #[test]
    fn pool_paths_hang_off_their_owner() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        let fleet = l.fleet(&"payments".parse().unwrap());
        assert_eq!(
            fleet.root,
            PathBuf::from("/h/.local/state/balerix/fleets/payments")
        );
        assert_eq!(
            fleet.mise_toml,
            PathBuf::from("/h/.local/state/balerix/fleets/payments/mise.toml")
        );
        assert_eq!(
            fleet.mise_pool(),
            PathBuf::from("/h/.local/state/balerix/fleets/payments/mise")
        );
        assert_eq!(
            fleet.installed_marker(),
            PathBuf::from("/h/.local/state/balerix/fleets/payments/mise.installed")
        );

        let crew = l.crew(&"payments/backend".parse().unwrap());
        let base = "/h/.local/state/balerix/fleets/payments/crews/backend";
        assert_eq!(crew.mise_toml(), PathBuf::from(format!("{base}/mise.toml")));
        assert_eq!(crew.mise_pool(), PathBuf::from(format!("{base}/mise")));
        assert_eq!(
            crew.installed_marker(),
            PathBuf::from(format!("{base}/mise.installed"))
        );

        let a = l.agent(&"payments/backend/alice".parse().unwrap());
        assert_eq!(
            a.mise_data_dir(),
            PathBuf::from(format!("{base}/agents/alice/home/.local/share/mise")),
            "the agent's own installs live inside its writable home"
        );
        assert_eq!(
            l.system_installed_marker(),
            PathBuf::from("/h/.local/share/balerix/mise.installed")
        );
    }

    #[test]
    fn agent_pools_run_nearest_first_and_join_into_the_shared_list() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        let id: AgentId = "payments/backend/alice".parse().unwrap();
        let state = "/h/.local/state/balerix/fleets/payments";
        assert_eq!(
            l.agent_pools(&id),
            vec![
                PathBuf::from(format!("{state}/crews/backend/mise")),
                PathBuf::from(format!("{state}/mise")),
                PathBuf::from("/h/.local/share/balerix/mise"),
            ],
            "crew beats fleet beats daemon"
        );
        assert_eq!(
            l.shared_install_dirs(&id),
            format!(
                "{state}/crews/backend/mise/installs:{state}/mise/installs:\
                 /h/.local/share/balerix/mise/installs"
            )
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p balerix-runtime --lib layout:: 2>&1 | tail -20`
Expected: FAIL, `no method named 'fleet' found for struct 'StateLayout'`.

- [ ] **Step 3: Write the implementation**

Add the struct next to `CrewPaths` in `crates/balerix-runtime/src/layout.rs`:

```rust
/// Where one fleet's own files live. `mise_pool()` holds the tools declared
/// in the fleet file's top-level `defaults.tools`, shared read-only with
/// every agent of every crew in the fleet (Spec E §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetPaths {
    pub root: PathBuf,
    pub mise_toml: PathBuf,
}
```

Add the accessors:

```rust
impl FleetPaths {
    /// A complete mise data dir; only `installs/` is exported to children.
    pub fn mise_pool(&self) -> PathBuf {
        self.root.join("mise")
    }
    /// sha256 of the `mise.toml` this pool was last installed from.
    pub fn installed_marker(&self) -> PathBuf {
        self.root.join("mise.installed")
    }
}

impl CrewPaths {
    pub fn mise_toml(&self) -> PathBuf {
        self.root.join("mise.toml")
    }
    pub fn mise_pool(&self) -> PathBuf {
        self.root.join("mise")
    }
    pub fn installed_marker(&self) -> PathBuf {
        self.root.join("mise.installed")
    }
}
```

Add to `impl AgentPaths`, beside `mise_cache_dir`:

```rust
    /// The agent's own `MISE_DATA_DIR`: writable, inside `home/`, so an
    /// agent can `mise install` a tool the fleet never declared without
    /// touching any shared pool (Spec E, E-2).
    pub fn mise_data_dir(&self) -> PathBuf {
        self.xdg_data().join("mise")
    }
```

Add to `impl StateLayout`, beside `crew`:

```rust
    pub fn fleet(&self, f: &FleetName) -> FleetPaths {
        let root = self.fleet_dir(f);
        FleetPaths {
            mise_toml: root.join("mise.toml"),
            root,
        }
    }
    /// sha256 of the system tool table the daemon pool was installed from.
    pub fn system_installed_marker(&self) -> PathBuf {
        self.data_root.join("mise.installed")
    }
    /// The read-only pools an agent's mise falls back through, nearest
    /// first: crew, fleet, daemon.
    pub fn agent_pools(&self, id: &AgentId) -> Vec<PathBuf> {
        vec![
            self.crew(&id.crew_ref()).mise_pool(),
            self.fleet(&id.fleet).mise_pool(),
            self.mise_data_dir(),
        ]
    }
    /// `MISE_SHARED_INSTALL_DIRS` for an agent: every pool's `installs/`,
    /// colon-separated. mise searches its own `MISE_DATA_DIR` first, then
    /// these in order, and never writes into them.
    pub fn shared_install_dirs(&self, id: &AgentId) -> String {
        shared_list(&self.agent_pools(id))
    }
```

Add the free function at the bottom of the file, above `mod tests`:

```rust
/// Joins pool roots into a `MISE_SHARED_INSTALL_DIRS` value.
pub fn shared_list(pools: &[PathBuf]) -> String {
    pools
        .iter()
        .map(|p| p.join("installs").display().to_string())
        .collect::<Vec<_>>()
        .join(":")
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p balerix-runtime --lib layout:: 2>&1 | tail -20`
Expected: PASS, all layout tests green.

- [ ] **Step 5: Export the new type**

In `crates/balerix-runtime/src/lib.rs`, change the layout re-export line to:

```rust
pub use layout::{AgentPaths, CrewPaths, FleetPaths, PluginPaths, StateLayout, shared_list};
```

- [ ] **Step 6: Run the full check**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`
Expected: PASS, no clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/balerix-runtime/src/layout.rs crates/balerix-runtime/src/lib.rs
git commit -m "Add fleet, crew and agent mise pool paths to the layout"
```

---

### Task 2: `tools` on the fleet and crew spec types

**Files:**
- Modify: `crates/balerix-api/src/fleet.rs`
- Modify: `crates/balerix-core/src/fleet.rs`
- Test: `crates/balerix-api/src/fleet.rs` and `crates/balerix-core/src/fleet.rs` (inline `mod tests`)
- Modify (mechanical, compiler-driven): every file holding a `FleetSpec { … }`
  or `CrewSpec { … }` literal. There are 41 and 22 of them across 28 files.

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `FleetSpec.tools: BTreeMap<String, String>` — the fleet file's
    `defaults.tools`.
  - `CrewSpec.tools: BTreeMap<String, String>` — that crew's
    `defaults.tools` layer only, never merged with the fleet's.
  - `balerix_core::Fleet.tools` and `balerix_core::Crew.tools`, same types,
    copied through `TryFrom<FleetSpec>`.

**Why both structs derive `Default` here:** the literals must be edited
anyway, and `..Default::default()` means the next field added to either
struct will not break them again.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/balerix-api/src/fleet.rs` (create the module
at the end of the file if it does not exist, with `use super::*;`):

```rust
    #[test]
    fn tools_default_to_empty_and_are_omitted_when_empty() {
        let json = r#"{"name":"f","crews":{"c":{"repo":"o/r","ref":"main"}}}"#;
        let spec: FleetSpec = serde_json::from_str(json).unwrap();
        assert!(spec.tools.is_empty(), "an older fleet.json still loads");
        assert!(spec.crews["c"].tools.is_empty());
        let back = serde_json::to_string(&spec).unwrap();
        assert!(
            !back.contains("tools"),
            "an empty table stays out of the wire format: {back}"
        );
    }

    #[test]
    fn tools_round_trip_at_both_levels() {
        let json = r#"{"name":"f","tools":{"node":"22.11.0"},
            "crews":{"c":{"repo":"o/r","ref":"main","tools":{"python":"3.12.8"}}}}"#;
        let spec: FleetSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.tools["node"], "22.11.0");
        assert_eq!(spec.crews["c"].tools["python"], "3.12.8");
    }
```

Add to `mod tests` in `crates/balerix-core/src/fleet.rs`:

```rust
    #[test]
    fn conversion_carries_the_fleet_and_crew_tool_tables() {
        let spec = FleetSpec {
            name: "f".into(),
            tools: BTreeMap::from([("node".to_string(), "22.11.0".to_string())]),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "o/r".into(),
                    git_ref: "main".into(),
                    tools: BTreeMap::from([("python".to_string(), "3.12.8".to_string())]),
                    ..Default::default()
                },
            )]),
        };
        let fleet = Fleet::try_from(spec).unwrap();
        assert_eq!(fleet.tools["node"], "22.11.0");
        assert_eq!(fleet.crews[&"c".parse().unwrap()].tools["python"], "3.12.8");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p balerix-api -p balerix-core --lib fleet:: 2>&1 | tail -20`
Expected: FAIL, `struct 'FleetSpec' has no field named 'tools'`.

- [ ] **Step 3: Add the fields**

In `crates/balerix-api/src/fleet.rs`, add `Default` to both derives and the
field to each struct:

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetSpec {
    pub name: String,
    /// Tools declared in the fleet file's top-level `defaults`. Installed
    /// once into the fleet pool and shared read-only with every agent in
    /// the fleet (Spec E §3).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub crews: BTreeMap<String, CrewSpec>,
}
```

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrewSpec {
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[serde(default)]
    pub git: GitSettings,
    /// This crew's own `defaults.tools` layer, never merged with the
    /// fleet's: the fleet pool is already a parent of the crew pool.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSettings>,
}
```

In `crates/balerix-core/src/fleet.rs`, add the fields to `Fleet` and `Crew`:

```rust
pub struct Fleet {
    pub name: FleetName,
    /// Tools for the fleet pool, verbatim from `FleetSpec`.
    pub tools: BTreeMap<String, String>,
    pub crews: BTreeMap<CrewName, Crew>,
}
```

```rust
pub struct Crew {
    pub repo: RepoRef,
    pub git_ref: String,
    pub git: GitSettings,
    /// Tools for this crew's pool, verbatim from `CrewSpec`.
    pub tools: BTreeMap<String, String>,
    pub agents: BTreeMap<AgentName, AgentSettings>,
}
```

Carry them through the conversion. In `TryFrom<FleetSpec> for Fleet`, the
final expression becomes:

```rust
        Ok(Self {
            name,
            tools: spec.tools,
            crews,
        })
```

`spec.tools` must be read before the `for (crew_name, crew) in spec.crews`
loop consumes `spec`, so bind it first:

```rust
        let tools = spec.tools;
        let mut crews = BTreeMap::new();
        for (crew_name, crew) in spec.crews {
```

and then use `tools` in the struct literal. In `convert_crew`, add
`tools: crew.tools,` to the returned `Crew`.

- [ ] **Step 4: Fix every broken literal, compiler-driven**

Run: `cargo check --workspace --all-targets 2>&1 | grep -E "^error" | head -40`

Every error is `missing field 'tools' in initializer`. For each one, add
`..Default::default()` as the last element of the literal. Where a literal
already lists every other field, that is the whole fix:

```rust
    FleetSpec {
        name: "f".into(),
        crews: BTreeMap::from([("c".to_string(), CrewSpec {
            repo: "o/r".into(),
            git_ref: "main".into(),
            ..Default::default()
        })]),
        ..Default::default()
    }
```

Repeat until `cargo check --workspace --all-targets` is clean. Do not change
any test's intent; these literals only gain an empty tools table.

Two literals in `crates/balerix-core/src/plugin.rs` build a `Crew` directly
(the synthetic plugin fleet). Give them `tools: BTreeMap::new()` and leave
the comment about the placeholder crew as it is.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p balerix-api -p balerix-core 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Run the full check**

Run: `mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log`
Expected: PASS. If a `plan_golden` snapshot moved, the tools field leaked
into a serialized form it should not have; check `skip_serializing_if`
before accepting anything.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "Carry fleet and crew tool tables through the spec types"
```

---

### Task 3: Extract the tool tables during resolution

**Files:**
- Modify: `crates/balerix-config/src/resolve.rs`
- Modify: `crates/balerix-config/src/validate.rs`
- Test: `crates/balerix-config/src/resolve.rs` (inline `mod tests`)
- Review: `crates/balerix-config/tests/snapshots/resolve_golden__payments.snap`,
  `crates/balerix-config/tests/snapshots/resolve_golden__overrides.snap`

**Interfaces:**
- Consumes: `FleetSpec.tools` and `CrewSpec.tools` from Task 2.
- Produces:
  - `pub fn tools_layer(path: &str, layer: &Value) -> Result<BTreeMap<String, String>, ConfigError>`
    in `validate.rs`, exported from the crate root. Reads `layer["tools"]`,
    treats absent or null as empty, drops `null` values (the file's delete
    marker), rejects a non-object `tools`, rejects a non-string value, and
    rejects a version `is_exact_version` refuses.
  - `resolve()` fills `FleetSpec.tools` from `file.defaults` and each
    `CrewSpec.tools` from that crew's `defaults`.

**Deliberate behaviour:** a fleet tool a crew deletes with `null` still
lands in the fleet pool. The pool holds what its level declared, not what
survives downstream merging. It costs one unused install and avoids
reachability analysis across the merge.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/balerix-config/src/resolve.rs`:

```rust
    #[test]
    fn fleet_and_crew_tool_layers_are_kept_beside_the_merged_agent_table() {
        let f = file(
            "apiVersion: balerix/v1\nkind: Fleet\nname: f\n\
             defaults:\n  tools: { node: \"22.11.0\", python: \"3.12.8\" }\n\
             crews:\n  c:\n    repo: o/r\n    defaults:\n      tools: { python: null, go: \"1.23.4\" }\n\
             agents:\n      a: { tools: { ripgrep: \"14.1.1\" } }\n",
        );
        let spec = resolve(&f, &ResolveOptions::default()).unwrap();
        assert_eq!(
            spec.tools,
            BTreeMap::from([
                ("node".to_string(), "22.11.0".to_string()),
                ("python".to_string(), "3.12.8".to_string()),
            ]),
            "the fleet pool holds what the fleet declared, deletions included"
        );
        assert_eq!(
            spec.crews["c"].tools,
            BTreeMap::from([("go".to_string(), "1.23.4".to_string())]),
            "a null deletes rather than becoming an entry"
        );
        let merged = &spec.crews["c"].agents["a"].tools;
        assert_eq!(merged["node"], "22.11.0");
        assert_eq!(merged["go"], "1.23.4");
        assert_eq!(merged["ripgrep"], "14.1.1");
        assert!(!merged.contains_key("python"), "the crew deleted it");
    }

    #[test]
    fn a_fuzzy_version_in_a_defaults_layer_is_rejected_with_its_path() {
        let f = file(
            "apiVersion: balerix/v1\nkind: Fleet\nname: f\n\
             defaults:\n  tools: { node: \"22\" }\ncrews:\n  c:\n    repo: o/r\n",
        );
        let e = resolve(&f, &ResolveOptions::default()).unwrap_err();
        assert!(
            e.to_string().starts_with("defaults.tools.node:"),
            "got {e}"
        );

        let g = file(
            "apiVersion: balerix/v1\nkind: Fleet\nname: f\ncrews:\n  c:\n    repo: o/r\n\
             defaults:\n      tools: { node: \"22\" }\n",
        );
        let e = resolve(&g, &ResolveOptions::default()).unwrap_err();
        assert!(
            e.to_string().starts_with("crews.c.defaults.tools.node:"),
            "got {e}"
        );
    }

    #[test]
    fn a_non_mapping_tools_layer_is_rejected() {
        let f = file(
            "apiVersion: balerix/v1\nkind: Fleet\nname: f\n\
             defaults:\n  tools: [node]\ncrews:\n  c:\n    repo: o/r\n",
        );
        let e = resolve(&f, &ResolveOptions::default()).unwrap_err();
        assert!(e.to_string().starts_with("defaults.tools:"), "got {e}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p balerix-config --lib resolve:: 2>&1 | tail -20`
Expected: FAIL, the fleet-level assertion fires because `spec.tools` is
empty.

- [ ] **Step 3: Write `tools_layer`**

Add to `crates/balerix-config/src/validate.rs`, after `validate_agent`:

```rust
/// The `tools` table of one settings layer, as the pool for that level
/// wants it: absent or null is empty, a `null` value is the file's delete
/// marker and is dropped, and every surviving version must be exact.
/// `path` is the layer's config path, e.g. `defaults` or
/// `crews.web.defaults`.
pub fn tools_layer(
    path: &str,
    layer: &Value,
) -> Result<std::collections::BTreeMap<String, String>, ConfigError> {
    let invalid = |suffix: &str, message: String| ConfigError::Invalid {
        path: format!("{path}.tools{suffix}"),
        message,
    };
    let mut out = std::collections::BTreeMap::new();
    let tools = match layer.get("tools") {
        None | Some(Value::Null) => return Ok(out),
        Some(Value::Object(t)) => t,
        Some(_) => return Err(invalid("", "expected a mapping".to_string())),
    };
    for (tool, value) in tools {
        let version = match value {
            Value::Null => continue,
            Value::String(s) => s,
            _ => {
                return Err(invalid(
                    &format!(".{tool}"),
                    "expected a version string".to_string(),
                ));
            }
        };
        if !is_exact_version(version) {
            return Err(invalid(
                &format!(".{tool}"),
                format!(
                    "expected an exact version, got {version:?} (try: mise latest {tool}@{version})"
                ),
            ));
        }
        out.insert(tool.clone(), version.clone());
    }
    Ok(out)
}
```

Export it from the crate root. In `crates/balerix-config/src/lib.rs`, add
`tools_layer` to the existing `pub use validate::{…};` list.

- [ ] **Step 4: Fill the tables in `resolve`**

In `crates/balerix-config/src/resolve.rs`, import the helper alongside
`validate_agent`:

```rust
use crate::validate::{tools_layer, validate_agent};
```

Immediately after the existing `expect_mapping("defaults", &file.defaults)?;`
line, add:

```rust
    let fleet_tools = tools_layer("defaults", &file.defaults)?;
```

Inside the crew loop, after
`expect_mapping(&format!("{crew_path}.defaults"), &crew.defaults)?;`, add:

```rust
        let crew_tools = tools_layer(&format!("{crew_path}.defaults"), &crew.defaults)?;
```

Add `tools: crew_tools,` to the `CrewSpec { … }` literal built at the end of
the crew loop, and `tools: fleet_tools,` to the `FleetSpec { … }` literal
that becomes `spec`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p balerix-config --lib resolve:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Review and accept the moved golden snapshots**

Run: `cargo insta test -p balerix-config --review` — or, without the
interactive reviewer, `cargo test -p balerix-config` then inspect each
`.snap.new` by hand.

Expected, and nothing else:
- `resolve_golden__payments.snap` gains a top-level `tools: { node: 22.11.0 }`
  and, under `crews.backend`, `tools: { python: 3.12.8 }`.
- `resolve_golden__overrides.snap` gains a top-level
  `tools: { node: 22.11.0, python: 3.12.8 }` and, under `crews.web`, no
  `tools` key at all, because that crew's only entry was `python: null`
  and `skip_serializing_if` omits the empty table.

Accept with `cargo insta accept -p balerix-config` once both match.

- [ ] **Step 7: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Resolve the fleet and crew tool layers into their spec tables"
```

---

### Task 4: `Toolchain::install_level` with a content-hash marker

**Files:**
- Modify: `crates/balerix-runtime/src/toolchain.rs`
- Modify: `crates/balerix-runtime/src/lib.rs` (export `render_level_toml`)
- Test: `crates/balerix-runtime/src/toolchain.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `shared_list` from Task 1.
- Produces:
  - `pub fn render_level_toml(label: &str, tools: &BTreeMap<String, String>) -> String`
    — the pool file's contents, same shape as `render_mise_toml` but headed
    by a free-text label rather than an `AgentId`.
  - `impl Toolchain { pub fn install_level(&self, label: &str, toml_path: &Path, pool: &Path, parents: &[PathBuf], marker: &Path, tools: &BTreeMap<String, String>, log: &Path) -> Result<(), MaterializeError> }`
    — writes the file, returns early when the marker already holds its
    sha256, then runs `mise trust` and `mise install` against `pool`, and
    writes the marker on success.
  - `pub fn level_env(config: &Path, pool: &Path, parents: &[PathBuf]) -> BTreeMap<String, String>`
    — the daemon-side mise environment for one pool.

`render_mise_toml` keeps its signature and delegates its header to
`render_level_toml`, so no caller of it changes.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/balerix-runtime/src/toolchain.rs`:

```rust
    #[test]
    fn a_level_file_is_headed_by_its_label_and_lists_only_its_own_tools() {
        let tools = BTreeMap::from([
            ("node".to_string(), "22.11.0".to_string()),
            ("cargo:x".to_string(), "1.0.0".to_string()),
        ]);
        assert_eq!(
            render_level_toml("fleet payments", &tools),
            "# generated by balerix for fleet payments; edit the fleet file, not this\n\
             [tools]\n\"cargo:x\" = \"1.0.0\"\nnode = \"22.11.0\"\n"
        );
        assert_eq!(
            render_level_toml("crew payments/backend", &BTreeMap::new()),
            "# generated by balerix for crew payments/backend; edit the fleet file, not this\n\
             [tools]\n",
            "an empty level still renders a file, so its marker can settle"
        );
    }

    #[test]
    fn level_env_points_mise_at_the_pool_and_its_parents() {
        let config = std::path::PathBuf::from("/p/crew/mise.toml");
        let pool = std::path::PathBuf::from("/p/crew/mise");
        let parents = vec![
            std::path::PathBuf::from("/p/fleet/mise"),
            std::path::PathBuf::from("/p/daemon/mise"),
        ];
        let env = level_env(&config, &pool, &parents);
        assert_eq!(env["MISE_GLOBAL_CONFIG_FILE"], "/p/crew/mise.toml");
        assert_eq!(env["MISE_DATA_DIR"], "/p/crew/mise");
        assert_eq!(
            env["MISE_SHARED_INSTALL_DIRS"],
            "/p/fleet/mise/installs:/p/daemon/mise/installs",
            "parents are read-only; mise never writes into them"
        );
        assert_eq!(env["MISE_AUTO_INSTALL"], "false");
        assert_eq!(env["MISE_YES"], "1");
        assert!(
            !env.contains_key("MISE_SHARED_INSTALL_DIRS_UNSET"),
            "no stray keys"
        );

        let alone = level_env(&config, &pool, &[]);
        assert!(
            !alone.contains_key("MISE_SHARED_INSTALL_DIRS"),
            "the daemon pool has no parents, so the variable is absent \
             rather than empty"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p balerix-runtime --lib toolchain:: 2>&1 | tail -20`
Expected: FAIL, `cannot find function 'render_level_toml' in this scope`.

- [ ] **Step 3: Write the implementation**

In `crates/balerix-runtime/src/toolchain.rs`, add the imports the new code
needs at the top of the file:

```rust
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::layout::shared_list;
```

Replace the body of `render_mise_toml`'s final rendering block by
delegating. Add the new function above it:

```rust
/// One level's generated `mise.toml`. `label` names the owner in the header
/// comment: `agent payments/backend/alice`, `fleet payments`, `crew
/// payments/backend`, `system`.
pub fn render_level_toml(label: &str, tools: &BTreeMap<String, String>) -> String {
    let mut out =
        format!("# generated by balerix for {label}; edit the fleet file, not this\n[tools]\n");
    for (k, v) in tools {
        out.push_str(&format!("{} = {:?}\n", toml_key(k), v));
    }
    out
}
```

In `render_mise_toml`, replace the trailing block

```rust
    let mut out =
        format!("# generated by balerix for {id}; edit the fleet file, not this\n[tools]\n");
    for (k, v) in table {
        out.push_str(&format!("{} = {:?}\n", toml_key(k), v));
    }
    out
```

with

```rust
    let owned: BTreeMap<String, String> = table
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    render_level_toml(&format!("agent {id}"), &owned)
```

This changes the agent header from `for payments/backend/alice` to
`for agent payments/backend/alice`; the golden snapshot in Task 6 records
it, and the existing
`render_merges_user_over_system_and_drops_gh_without_auth` test's expected
string must gain the `agent ` prefix in the same commit.

Add the pool environment and the install operation:

```rust
/// The daemon-side mise environment for one pool: read `config`, install
/// into `pool`, resolve through `parents` (read-only). Empty `parents`
/// leaves `MISE_SHARED_INSTALL_DIRS` unset rather than empty — the daemon
/// pool is the root of the chain and has nothing to fall back to.
pub fn level_env(config: &Path, pool: &Path, parents: &[PathBuf]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([
        (
            "MISE_GLOBAL_CONFIG_FILE".to_string(),
            config.display().to_string(),
        ),
        ("MISE_DATA_DIR".to_string(), pool.display().to_string()),
        ("MISE_YES".to_string(), "1".to_string()),
        ("MISE_QUIET".to_string(), "1".to_string()),
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
    ]);
    if !parents.is_empty() {
        env.insert("MISE_SHARED_INSTALL_DIRS".to_string(), shared_list(parents));
    }
    env
}
```

Add to `impl Toolchain<'_>`:

```rust
    /// Installs one level's tools into its own pool. Writes `toml_path`,
    /// then does nothing more when `marker` already holds that file's
    /// sha256; otherwise `mise trust` + `mise install` against `pool` with
    /// `parents` as read-only fallbacks, and the marker is written on
    /// success. `cwd=/` for the same reason `install` uses it: no project
    /// `mise.toml` on the way up may be discovered.
    #[allow(clippy::too_many_arguments)]
    pub fn install_level(
        &self,
        label: &str,
        toml_path: &Path,
        pool: &Path,
        parents: &[PathBuf],
        marker: &Path,
        tools: &BTreeMap<String, String>,
        log: &Path,
    ) -> Result<(), MaterializeError> {
        let text = render_level_toml(label, tools);
        let digest = hex::encode(Sha256::digest(text.as_bytes()));
        let io = |path: &Path, e: std::io::Error| MaterializeError::Io {
            id: label.to_string(),
            path: path.to_path_buf(),
            message: e.to_string(),
        };
        write_atomic(toml_path, text.as_bytes(), 0o644).map_err(|e| io(toml_path, e))?;
        if std::fs::read_to_string(marker).ok().as_deref() == Some(digest.as_str()) {
            return Ok(());
        }
        let env = level_env(toml_path, pool, parents);
        let run = |args: &[&str]| {
            Cmd::new(&self.tools.mise)
                .args(args.iter().copied())
                .envs(&env)
                .cwd(Path::new("/"))
                .log(log)
                .run()
                .map(|_| ())
                .map_err(|f| MaterializeError::Tool {
                    id: label.to_string(),
                    tool: f.tool,
                    subcommand: f.subcommand,
                    args: f.args,
                    stderr: f.stderr,
                })
        };
        run(&["trust", &toml_path.display().to_string()])?;
        run(&["install"])?;
        write_atomic(marker, digest.as_bytes(), 0o644).map_err(|e| io(marker, e))
    }
```

`env["MISE_GLOBAL_CONFIG_FILE"]`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p balerix-runtime --lib toolchain:: 2>&1 | tail -20`
Expected: PASS, including the updated agent-header assertion.

- [ ] **Step 5: Export the new functions**

In `crates/balerix-runtime/src/lib.rs`, extend the toolchain re-export:

```rust
pub use toolchain::{
    Toolchain, embedded_system_tools, level_env, mise_env, render_level_toml, render_mise_toml,
    system_tools,
};
```

- [ ] **Step 6: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Add Toolchain::install_level with a content-hash marker"
```

---

### Task 5: Install the system, fleet and crew pools in `ensure_crew`

**Files:**
- Modify: `crates/balerix-core/src/ports.rs` (the `Materializer` trait)
- Modify: `crates/balerix-core/src/reconcile/execute.rs` (the `ensure_crew` helper)
- Modify: `crates/balerix-core/src/fakes.rs` (`FakeMaterializer`)
- Modify: `crates/balerix-server/src/plugins/materializer.rs` (`PluginMaterializer` and its test)
- Modify: `crates/balerix-runtime/src/materializer.rs` (`Runtime::ensure_crew`)
- Modify: `crates/balerix-runtime/tests/materialize_it.rs` (three call sites)
- Test: `crates/balerix-core/src/reconcile/execute.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `Fleet.tools` and `Crew.tools` (Task 2), `FleetPaths` and
  `CrewPaths` accessors (Task 1), `Toolchain::install_level` (Task 4),
  `system_tools` (existing).
- Produces:
  - `pub struct CrewTools<'a> { pub fleet: &'a BTreeMap<String, String>, pub crew: &'a BTreeMap<String, String> }`
    in `balerix-core::ports`, re-exported from the crate root.
  - `Materializer::ensure_crew` gains a final `tools: CrewTools<'_>`
    parameter.
  - `Runtime::ensure_crew` installs the system pool, then the fleet pool,
    then the crew pool, before cloning the repo.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/balerix-core/src/reconcile/execute.rs`:

```rust
    #[test]
    fn ensure_crew_passes_the_fleet_and_crew_tool_tables_through() {
        let mut h = harness_with_tools(
            BTreeMap::from([("node".to_string(), "22.11.0".to_string())]),
            BTreeMap::from([("python".to_string(), "3.12.8".to_string())]),
        );
        let seen = h.m.crew_tools();
        assert!(seen.is_empty(), "nothing recorded before the pass");
        let _ = execute(&[Step::EnsureCrew("f/c".parse().unwrap())], &mut h.status, &h.ctx());
        assert_eq!(
            h.m.crew_tools(),
            vec![("f/c".to_string(), vec!["node=22.11.0".to_string()], vec!["python=3.12.8".to_string()])],
            "the materializer sees each level's own table, unmerged"
        );
    }
```

Add the two helpers this test needs. Beside the existing harness
constructor in the same `mod tests`, add:

```rust
    fn harness_with_tools(
        fleet_tools: BTreeMap<String, String>,
        crew_tools: BTreeMap<String, String>,
    ) -> Harness {
        let mut h = harness();
        if let Some(f) = h.desired.as_mut() {
            f.tools = fleet_tools;
            if let Some(c) = f.crews.get_mut(&"c".parse().unwrap()) {
                c.tools = crew_tools;
            }
        }
        h
    }
```

Adapt the field names to whatever the existing `harness()` in that module
actually returns; the point is a `Fleet` whose `tools` and whose crew's
`tools` are set. In `crates/balerix-core/src/fakes.rs`, add a recorder to
`FakeMaterializer`:

```rust
    /// Each `ensure_crew` call as (crew, fleet tools, crew tools), with
    /// each table flattened to sorted `name=version` strings.
    pub fn crew_tools(&self) -> Vec<(String, Vec<String>, Vec<String>)> {
        lock(&self.crew_tools).clone()
    }
```

backed by a `crew_tools: Mutex<Vec<(String, Vec<String>, Vec<String>)>>`
field on the struct, defaulted in its constructor exactly like the existing
`rec` field.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p balerix-core --lib execute:: 2>&1 | tail -20`
Expected: FAIL, `no method named 'crew_tools'`.

- [ ] **Step 3: Change the port**

In `crates/balerix-core/src/ports.rs`, add the struct above the trait:

```rust
/// The two read-only tool tables a crew's pools are built from: the fleet's
/// own `defaults.tools` and this crew's `defaults.tools` layer. Each is that
/// level's declaration, never a merge of the level above (Spec E §4).
#[derive(Debug, Clone, Copy)]
pub struct CrewTools<'a> {
    pub fleet: &'a BTreeMap<String, String>,
    pub crew: &'a BTreeMap<String, String>,
}
```

Add `use std::collections::BTreeMap;` to that file if it is not already
imported, and export `CrewTools` from `crates/balerix-core/src/lib.rs`
alongside the other `ports::` re-exports.

Add the parameter to the trait method:

```rust
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        repo: &RepoRef,
        git_ref: &str,
        git: &GitSettings,
        creds: &CredentialBundle,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError>;
```

- [ ] **Step 4: Pass the tables from the reconciler**

In `crates/balerix-core/src/reconcile/execute.rs`, the `ensure_crew` helper
becomes:

```rust
fn ensure_crew(ctx: &ReconcileContext, crew: &CrewRef) -> Result<(), String> {
    let fleet = ctx
        .desired
        .ok_or_else(|| format!("{crew}: not in the desired fleet"))?;
    let desired = fleet
        .crews
        .get(&crew.crew)
        .ok_or_else(|| format!("{crew}: not in the desired fleet"))?;
    ctx.materializer
        .ensure_crew(
            crew,
            &desired.repo,
            &desired.git_ref,
            &desired.git,
            ctx.creds,
            CrewTools {
                fleet: &fleet.tools,
                crew: &desired.tools,
            },
        )
        .map_err(|e| e.to_string())?;
    ctx.runner.ensure_crew(crew).map_err(|e| e.to_string())
}
```

Import `CrewTools` at the top of the file.

- [ ] **Step 5: Update the two non-runtime implementations**

In `crates/balerix-core/src/fakes.rs`, record the tables:

```rust
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        _: &RepoRef,
        _: &str,
        _: &GitSettings,
        _: &CredentialBundle,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        let flat = |t: &BTreeMap<String, String>| {
            t.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>()
        };
        lock(&self.crew_tools).push((crew.to_string(), flat(tools.fleet), flat(tools.crew)));
        self.check("ensure_crew", &crew.to_string(), "git")
    }
```

In `crates/balerix-server/src/plugins/materializer.rs`, the plugin wrapper
stays a no-op; add the parameter as another `_`:

```rust
    fn ensure_crew(
        &self,
        _: &CrewRef,
        _: &RepoRef,
        _: &str,
        _: &GitSettings,
        _: &CredentialBundle,
        _: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        Ok(())
    }
```

Its test around line 156 calls `m.ensure_crew(...)`; add a final argument:

```rust
        let empty = BTreeMap::new();
        m.ensure_crew(
            &agents[0].id.crew_ref(),
            &agents[0].repo,
            "none",
            &agents[0].git,
            &CredentialBundle::default(),
            CrewTools { fleet: &empty, crew: &empty },
        )
        .unwrap();
```

- [ ] **Step 6: Run the core tests to verify they pass**

Run: `cargo test -p balerix-core 2>&1 | tail -20`
Expected: PASS, including the new `ensure_crew_passes_the_fleet_and_crew_tool_tables_through`.

- [ ] **Step 7: Install the pools in `Runtime::ensure_crew`**

In `crates/balerix-runtime/src/materializer.rs`, import what the new code
needs:

```rust
use balerix_core::CrewTools;
```

Replace `Runtime::ensure_crew` with:

```rust
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        repo: &RepoRef,
        git_ref: &str,
        git: &GitSettings,
        creds: &CredentialBundle,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        let id = crew.to_string();
        self.install_pools(crew, tools)?;
        if git.auth == GitAuth::Gh {
            let token = creds.gh_token.as_deref().ok_or_else(|| MaterializeError::Invalid {
                id: id.clone(),
                message: "git.auth is gh but no gh token was provided (run `gh auth login` on the client)".into(),
            })?;
            Workspace::write_fleet_gh_config(&self.layout.fleet_gh_dir(&crew.fleet), token, &id)?;
        }
        self.workspace(&crew.fleet, git)
            .ensure_repo(&id, &self.layout.crew(crew), repo, git_ref)
    }
```

Add the helper to the inherent `impl Runtime` block, beside
`install_and_validate`:

```rust
    /// The three daemon-owned pools, outermost first: the system table into
    /// the daemon pool, the fleet's tools into the fleet pool, this crew's
    /// into the crew pool. Each is skipped when its marker already matches
    /// its rendered table, and each resolves through the pools above it, so
    /// a version an outer pool already holds is never downloaded twice
    /// (Spec E §5).
    pub fn install_pools(
        &self,
        crew: &CrewRef,
        tools: CrewTools<'_>,
    ) -> Result<(), MaterializeError> {
        let tc = Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        };
        let fleet = self.layout.fleet(&crew.fleet);
        let crew_paths = self.layout.crew(crew);
        let daemon_pool = self.layout.mise_data_dir();
        let log = crew_paths.root.join("logs").join("mise.pools.log");

        let system = system_tools(&self.layout, &crew.fleet.to_string())?;
        tc.install_level(
            "system",
            &self.layout.system_mise_toml_generated(),
            &daemon_pool,
            &[],
            &self.layout.system_installed_marker(),
            &system,
            &log,
        )?;
        tc.install_level(
            &format!("fleet {}", crew.fleet),
            &fleet.mise_toml,
            &fleet.mise_pool(),
            &[daemon_pool.clone()],
            &fleet.installed_marker(),
            tools.fleet,
            &log,
        )?;
        tc.install_level(
            &format!("crew {crew}"),
            &crew_paths.mise_toml(),
            &crew_paths.mise_pool(),
            &[fleet.mise_pool(), daemon_pool],
            &crew_paths.installed_marker(),
            tools.crew,
            &log,
        )
    }
```

Two details this needs:

`system_tools` currently takes `&AgentId` only to fill an error's `id`.
Widen it to take the id as a string. In
`crates/balerix-runtime/src/toolchain.rs` change its signature to
`pub fn system_tools(layout: &StateLayout, id: &str) -> Result<BTreeMap<String, String>, MaterializeError>`
and replace `id: id.to_string()` with `id: id.to_string()` unchanged (a
`&str` still has `to_string`). Update its one existing caller in
`render_agent` to pass `&id.to_string()`.

The system table's own generated file needs a path. Add to `impl
StateLayout` in `crates/balerix-runtime/src/layout.rs`:

```rust
    /// The generated copy of the system table that the daemon pool is
    /// installed from. `system_mise_toml()` is the admin's hand-written
    /// input; this is balerix's rendering of it, so the marker has a stable
    /// file to hash.
    pub fn system_mise_toml_generated(&self) -> PathBuf {
        self.data_root.join("mise.toml")
    }
```

- [ ] **Step 8: Update the runtime integration call sites**

In `crates/balerix-runtime/tests/materialize_it.rs`, the three
`rt.ensure_crew(...)` calls each gain a final argument. Add near the top of
the file:

```rust
fn no_tools() -> BTreeMap<String, String> {
    BTreeMap::new()
}
```

and at each call site:

```rust
    let (f, c) = (no_tools(), no_tools());
    rt.ensure_crew(
        &crew,
        &agent.repo,
        &agent.git_ref,
        &agent.git,
        &creds,
        balerix_core::CrewTools { fleet: &f, crew: &c },
    )
```

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: PASS.

Run: `mise run test-it 2>&1 | tail -20`
Expected: PASS. `Runtime::ensure_crew` now shells out to mise three times
per crew; with empty tables each install is a no-op after the first.

- [ ] **Step 10: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Install the system, fleet and crew pools when ensuring a crew"
```

---

### Task 6: Point the agent at its private dir and the pool chain

**Files:**
- Modify: `crates/balerix-runtime/src/env.rs`
- Modify: `crates/balerix-runtime/src/toolchain.rs` (`mise_env`, `Toolchain::install`)
- Modify: `crates/balerix-runtime/src/sandbox.rs` (`balerix_grants`)
- Modify: `crates/balerix-runtime/tests/toolchain_it.rs` (the `mise_env` call)
- Test: the inline `mod tests` of `env.rs` and `sandbox.rs`
- Review: `crates/balerix-runtime/tests/snapshots/generated_golden__payments_generated.snap`

**Interfaces:**
- Consumes: `agent_pools` and `shared_install_dirs` (Task 1).
- Produces:
  - `agent_env` emits `MISE_DATA_DIR` = the agent's private dir,
    `MISE_SHARED_INSTALL_DIRS`, `MISE_CEILING_PATHS` = the agent root, and
    `MISE_AUTO_INSTALL=false`. The row count rises from 22 to 24.
  - `mise_env(id: &AgentId, paths: &AgentPaths, layout: &StateLayout)` —
    the daemon-side agent install env, same data dir and pool chain.
  - `balerix_grants` reads the crew and fleet pools in addition to the
    daemon pool.

- [ ] **Step 1: Write the failing tests**

In `crates/balerix-runtime/src/env.rs`, replace the assertions inside
`isolation_rows_come_first_and_user_env_cannot_override_them` that name
`MISE_DATA_DIR`, `MISE_CEILING_PATHS` and `env.len()`, and add the new rows:

```rust
        let base = "/h/.local/state/balerix/fleets/payments/crews/backend";
        assert_eq!(
            env["MISE_DATA_DIR"],
            format!("{base}/agents/alice/home/.local/share/mise"),
            "the agent installs into its own home, never a shared pool"
        );
        assert_eq!(
            env["MISE_SHARED_INSTALL_DIRS"],
            format!(
                "{base}/mise/installs:/h/.local/state/balerix/fleets/payments/mise/installs:\
                 /h/.local/share/balerix/mise/installs"
            ),
            "crew, then fleet, then daemon"
        );
        assert_eq!(
            env["MISE_CEILING_PATHS"],
            format!("{base}/agents/alice"),
            "the walk stops above the worktree, so the repo's own mise.toml \
             is discovered and nothing outside the agent is"
        );
        assert_eq!(
            env["MISE_AUTO_INSTALL"], "false",
            "launch resolves; installing is the agent's explicit act"
        );
        assert_eq!(env.len(), 24);
```

In `crates/balerix-runtime/src/sandbox.rs`, add to its `mod tests`:

```rust
    #[test]
    fn every_pool_the_agent_resolves_through_is_readable() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let id: AgentId = "payments/backend/alice".parse().unwrap();
        let paths = layout.agent(&id);
        let crew = layout.crew(&id.crew_ref());
        let g = balerix_grants(
            &paths,
            &crew,
            &layout,
            Path::new("/tools/balerix"),
            Path::new("/tools/mise"),
        );
        for pool in layout.agent_pools(&id) {
            assert!(
                g.read.contains(&pool),
                "{} must be readable inside the sandbox",
                pool.display()
            );
        }
        assert!(
            !g.allow.iter().any(|p| p.ends_with("mise")),
            "no pool is ever writable"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p balerix-runtime --lib 'env::' 'sandbox::' 2>&1 | tail -20`
Expected: FAIL on the `MISE_DATA_DIR` assertion and on the missing crew
pool in `read`.

- [ ] **Step 3: Write the environment implementation**

In `crates/balerix-runtime/src/env.rs`, replace the `MISE_CEILING_PATHS`
comment block and the `MISE_DATA_DIR` row. The ceiling comment becomes:

```rust
        // Stops mise's upward config walk at the agent root, one level
        // above the worktree. The worktree's own `mise.toml` is therefore
        // discovered — an agent may `mise install` what its repository
        // declares, into the private data dir below — while nothing outside
        // the agent is: a config the profile does not grant would make mise
        // exit on the read error. Installing is never a side effect of
        // launch; `MISE_AUTO_INSTALL=false` keeps `mise exec claude`
        // resolving even when the worktree names a tool nobody installed.
        ("MISE_CEILING_PATHS".to_string(), s(&paths.root)),
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
        // The agent's own installs, writable, inside `home/`; the pools are
        // read-only fallbacks searched crew-first (Spec E §6).
        ("MISE_DATA_DIR".to_string(), s(&paths.mise_data_dir())),
        (
            "MISE_SHARED_INSTALL_DIRS".to_string(),
            layout.shared_install_dirs(id),
        ),
```

In `crates/balerix-runtime/src/toolchain.rs`, `mise_env` gains the agent id
and the same two rows:

```rust
/// The daemon-side environment for an agent's own `mise install`: the same
/// private data dir and pool chain the sandbox will use, so what the daemon
/// installs is exactly what the agent resolves.
pub fn mise_env(
    id: &AgentId,
    paths: &AgentPaths,
    layout: &StateLayout,
) -> BTreeMap<String, String> {
    let s = |p: std::path::PathBuf| p.display().to_string();
    BTreeMap::from([
        (
            "MISE_GLOBAL_CONFIG_FILE".to_string(),
            s(paths.mise_toml.clone()),
        ),
        ("MISE_DATA_DIR".to_string(), s(paths.mise_data_dir())),
        (
            "MISE_SHARED_INSTALL_DIRS".to_string(),
            layout.shared_install_dirs(id),
        ),
        ("MISE_CONFIG_DIR".to_string(), s(paths.mise_config_dir())),
        ("MISE_STATE_DIR".to_string(), s(paths.mise_state_dir())),
        ("MISE_CACHE_DIR".to_string(), s(paths.mise_cache_dir())),
        ("MISE_YES".to_string(), "1".to_string()),
        ("MISE_QUIET".to_string(), "1".to_string()),
        ("MISE_AUTO_INSTALL".to_string(), "false".to_string()),
    ])
}
```

`Toolchain::install` calls `mise_env(paths, self.layout)`; change it to
`mise_env(id, paths, self.layout)`. The `id` is already its first parameter.

- [ ] **Step 4: Write the sandbox implementation**

In `crates/balerix-runtime/src/sandbox.rs`, `balerix_grants` currently does
`read.push(layout.mise_data_dir());`. Replace that single line with the
whole chain, and widen the signature to take the agent id:

```rust
pub fn balerix_grants(
    id: &AgentId,
    paths: &AgentPaths,
    crew: &CrewPaths,
    layout: &StateLayout,
    balerix: &Path,
    mise: &Path,
) -> Grants {
    let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
    // Every pool the agent resolves through: crew, fleet, daemon. All
    // read-only — an agent installs into its own `home/` and can never
    // change a binary a crew-mate executes (Spec E, E-2).
    read.extend(layout.agent_pools(id));
```

Update the one production caller in
`crates/balerix-runtime/src/materializer.rs::render_agent` to pass `id`
first, and the test helper in `crates/balerix-runtime/tests/sandbox_it.rs`
that builds grants (it currently references `layout.mise_data_dir()` around
line 29; give it the id it already has in scope and assert against
`layout.agent_pools(&id)` instead).

- [ ] **Step 5: Fix the remaining `mise_env` caller**

In `crates/balerix-runtime/tests/toolchain_it.rs`, the `mise exec` probe
calls `.envs(&mise_env(&paths, &layout))`. Change it to
`.envs(&mise_env(&id, &paths, &layout))`.

That test seeds `gh` into the daemon pool and asserts `mise exec` resolves
it while the pool is read-only. Under the new environment the agent's data
dir is empty and `gh` is found through `MISE_SHARED_INSTALL_DIRS`, which is
exactly the behaviour the test should now be proving. Update its comment
from "read-only shared dir" to name the pool chain.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p balerix-runtime --lib 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 7: Review and accept the moved golden snapshot**

Run: `cargo test -p balerix-runtime --test generated_golden 2>&1 | tail -20`

Inspect `crates/balerix-runtime/tests/snapshots/generated_golden__payments_generated.snap.new`.
Expected changes, and nothing else, for both alice and bob:
- `MISE_DATA_DIR` moves from `<root>/data/mise` to
  `<root>/state/fleets/payments/crews/backend/agents/<name>/home/.local/share/mise`.
- `MISE_SHARED_INSTALL_DIRS` appears, listing the crew, fleet and daemon
  `installs/` dirs in that order.
- `MISE_AUTO_INSTALL` appears with value `false`.
- `MISE_CEILING_PATHS` loses its `/workspace` suffix.
- `filesystem.read` gains the crew and fleet pools beside `<root>/data/mise`.
- The generated `mise.toml` header gains the `agent ` prefix from Task 4.

Accept with `cargo insta accept -p balerix-runtime` once it matches.

- [ ] **Step 8: Run the full check and the tool-backed tests**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
mise run test-it 2>&1 | tail -20
```
Expected: both PASS.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "Give each agent a private mise data dir over the pool chain"
```

---

### Task 7: Integration test for the whole hierarchy

**Files:**
- Modify: `crates/balerix-runtime/tests/toolchain_it.rs`

**Interfaces:**
- Consumes: everything from Tasks 1 through 6.
- Produces: no new production interface.

**Why these tools:** the test must not depend on the network. It seeds each
pool by copying an install the host already has, exactly as the existing
`seed_gh` helper does, and then asserts placement rather than downloading.
`jq` and `fd` are in this repo's own `mise.toml`, so a developer machine and
CI both have them.

- [ ] **Step 1: Write the failing test**

Add to `crates/balerix-runtime/tests/toolchain_it.rs`. Generalise the
existing seeding helper first:

```rust
/// Copies the host's `<tool>@<version>` install into `pool/installs`.
/// Returns false if the host does not have it.
fn seed_into(
    tools: &balerix_runtime::ToolPaths,
    pool: &std::path::Path,
    tool: &str,
    version: &str,
) -> bool {
    let out = Command::new(&tools.mise)
        .args(["where", &format!("{tool}@{version}")])
        .output()
        .unwrap();
    if !out.status.success() {
        return false;
    }
    let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dst = pool.join("installs").join(tool).join(version);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    Command::new("cp")
        .args(["-r", &src, &dst.display().to_string()])
        .status()
        .unwrap()
        .success()
}

/// The version this repo pins for `tool`, read from its own `mise.toml`.
fn repo_pin(tool: &str) -> String {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../mise.toml"))
        .unwrap();
    let doc: toml::Table = text.parse().unwrap();
    doc["tools"][tool].as_str().unwrap().to_string()
}
```

`repo_pin` needs `toml` as a dev-dependency of the test; `balerix-runtime`
already depends on `toml`, so it is available.

Then the test itself:

```rust
#[test]
fn each_level_installs_into_its_own_pool_and_the_agent_resolves_them_all() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise", false));
        return;
    };
    let root = support::temp_root("pools");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let crew_ref = id.crew_ref();
    let fleet_paths = layout.fleet(&id.fleet);
    let crew_paths = layout.crew(&crew_ref);
    let paths = layout.agent(&id);
    std::fs::create_dir_all(&paths.root).unwrap();
    std::fs::create_dir_all(crew_paths.root.join("logs")).unwrap();

    // Seed one tool per shared level from the host so nothing downloads.
    let jq = repo_pin("jq");
    let fd = repo_pin("fd");
    let gh = repo_pin("gh");
    let seeded = seed_into(&tools, &layout.mise_data_dir(), "gh", &gh)
        && seed_into(&tools, &fleet_paths.mise_pool(), "jq", &jq)
        && seed_into(&tools, &crew_paths.mise_pool(), "fd", &fd);
    if !support::require_or_skip("host installs of gh, jq and fd to seed from", seeded) {
        return;
    }

    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
    };
    let log = crew_paths.root.join("logs").join("mise.pools.log");
    let daemon_pool = layout.mise_data_dir();
    let fleet_tools = BTreeMap::from([("jq".to_string(), jq.clone())]);
    let crew_tools = BTreeMap::from([("fd".to_string(), fd.clone())]);

    tc.install_level(
        "fleet f",
        &fleet_paths.mise_toml,
        &fleet_paths.mise_pool(),
        &[daemon_pool.clone()],
        &fleet_paths.installed_marker(),
        &fleet_tools,
        &log,
    )
    .unwrap();
    tc.install_level(
        "crew f/c",
        &crew_paths.mise_toml(),
        &crew_paths.mise_pool(),
        &[fleet_paths.mise_pool(), daemon_pool.clone()],
        &crew_paths.installed_marker(),
        &crew_tools,
        &log,
    )
    .unwrap();

    // The agent's table names all three; only what no pool holds is private.
    let agent_tools = BTreeMap::from([
        ("jq".to_string(), jq.clone()),
        ("fd".to_string(), fd.clone()),
        ("gh".to_string(), gh.clone()),
    ]);
    tc.write(&id, &paths, &BTreeMap::new(), &agent_tools, false)
        .unwrap();
    tc.install(&id, &paths).unwrap();

    let holds = |pool: &std::path::Path, tool: &str| pool.join("installs").join(tool).exists();
    assert!(holds(&fleet_paths.mise_pool(), "jq"));
    assert!(!holds(&fleet_paths.mise_pool(), "fd"), "crew tools stay out of the fleet pool");
    assert!(holds(&crew_paths.mise_pool(), "fd"));
    assert!(!holds(&crew_paths.mise_pool(), "jq"), "the fleet pool already had it");
    assert!(
        !paths.mise_data_dir().join("installs").exists()
            || std::fs::read_dir(paths.mise_data_dir().join("installs"))
                .unwrap()
                .next()
                .is_none(),
        "every tool came from a pool, so nothing landed privately"
    );

    // Each tool resolves, and from the pool that owns it.
    for (tool, pool) in [
        ("jq", fleet_paths.mise_pool()),
        ("fd", crew_paths.mise_pool()),
        ("gh", daemon_pool.clone()),
    ] {
        let out = Command::new(&tools.mise)
            .args(["which", tool])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &paths.home)
            .envs(&mise_env(&id, &paths, &layout))
            .current_dir("/")
            .output()
            .unwrap();
        let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            out.status.success(),
            "mise which {tool}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            resolved.starts_with(&pool.display().to_string()),
            "{tool} resolved to {resolved}, expected it under {}",
            pool.display()
        );
    }

    // The markers make a second pass a no-op.
    let before = std::fs::read_to_string(fleet_paths.installed_marker()).unwrap();
    tc.install_level(
        "fleet f",
        &fleet_paths.mise_toml,
        &fleet_paths.mise_pool(),
        &[daemon_pool],
        &fleet_paths.installed_marker(),
        &fleet_tools,
        &log,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(fleet_paths.installed_marker()).unwrap(),
        before
    );
}

#[test]
fn a_crew_pins_its_own_version_without_disturbing_the_fleets() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("mise", false));
        return;
    };
    let root = support::temp_root("pools-versions");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let fleet_paths = layout.fleet(&id.fleet);
    let crew_paths = layout.crew(&id.crew_ref());
    std::fs::create_dir_all(crew_paths.root.join("logs")).unwrap();

    // Two versions of one tool: the host's pin, and a second real version
    // taken from `mise ls-remote` so nothing is invented.
    let jq = repo_pin("jq");
    let out = Command::new(&tools.mise)
        .args(["ls-remote", "jq"])
        .output()
        .unwrap();
    let other = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != jq)
        .next_back()
        .map(str::to_string);
    let Some(other) = other.filter(|_| out.status.success()) else {
        assert!(!support::require_or_skip("a second jq version from mise ls-remote", false));
        return;
    };
    if !support::require_or_skip(
        "a host jq install to seed from",
        seed_into(&tools, &fleet_paths.mise_pool(), "jq", &jq),
    ) {
        return;
    }
    // The crew's version is seeded too: this test is about placement, not
    // downloading. Reuse the same payload under the other version's name.
    let src = fleet_paths.mise_pool().join("installs").join("jq").join(&jq);
    let dst = crew_paths
        .mise_pool()
        .join("installs")
        .join("jq")
        .join(&other);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    assert!(
        Command::new("cp")
            .args(["-r", &src.display().to_string(), &dst.display().to_string()])
            .status()
            .unwrap()
            .success()
    );

    let tc = Toolchain {
        tools: &tools,
        layout: &layout,
    };
    let log = crew_paths.root.join("logs").join("mise.pools.log");
    tc.install_level(
        "crew f/c",
        &crew_paths.mise_toml(),
        &crew_paths.mise_pool(),
        &[fleet_paths.mise_pool(), layout.mise_data_dir()],
        &crew_paths.installed_marker(),
        &BTreeMap::from([("jq".to_string(), other.clone())]),
        &log,
    )
    .unwrap();

    assert!(
        fleet_paths
            .mise_pool()
            .join("installs")
            .join("jq")
            .join(&jq)
            .exists(),
        "the fleet's version survives a crew that pins another"
    );
    assert!(
        crew_paths
            .mise_pool()
            .join("installs")
            .join("jq")
            .join(&other)
            .exists()
    );
    assert!(
        !crew_paths
            .mise_pool()
            .join("installs")
            .join("jq")
            .join(&jq)
            .exists(),
        "the crew pool holds only what the crew declared"
    );
}
```

Add the imports the new tests need at the top of the file:

```rust
use std::collections::BTreeMap;

use balerix_core::AgentId;
use balerix_runtime::{Toolchain, embedded_system_tools, mise_env};
```

(`BTreeMap` and `AgentId` are already imported; add nothing twice.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p balerix-runtime --test toolchain_it 2>&1 | tail -30`
Expected: FAIL before Tasks 1 through 6 are in place. Once they are, this
step is a confirmation that the tests actually exercise the new code: run
them, and if either passes without any pool directory existing, the
assertions are vacuous and must be tightened.

- [ ] **Step 3: Run the tests to verify they pass**

Run: `BALERIX_REQUIRE_TOOLS=1 cargo test -p balerix-runtime --test toolchain_it 2>&1 | tail -30`
Expected: PASS, three tests, none skipped.

Per the recorded flakiness of this suite under host load, if a test fails
here, rerun it alone before investigating:
`cargo test -p balerix-runtime --test toolchain_it -- --test-threads=1`.

- [ ] **Step 4: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Cover the pool hierarchy with tool-backed integration tests"
```

---

### Task 8: Documentation

**Files:**
- Modify: `ARCHITECTURE.md`
- Modify: `docs/THREAT-MODEL.md`
- Modify: `AGENTS.md`

**Interfaces:**
- Consumes: the behaviour built in Tasks 1 through 7.
- Produces: no code interface.

- [ ] **Step 1: Update ARCHITECTURE.md**

Replace the ceiling bullet near line 197:

```markdown
- **Tool installs are a pool hierarchy.** Each level of the fleet file is
  installed into its own read-only mise data dir — the system table into the
  daemon pool, `defaults.tools` into the fleet pool, a crew's
  `defaults.tools` into the crew pool — and every agent runs with its own
  writable data dir in `home/`, reaching the three pools through
  `MISE_SHARED_INSTALL_DIRS` (crew, then fleet, then daemon). An agent can
  therefore `mise install` what its worktree declares without touching a
  binary any other agent executes.
- **The sandbox pins mise's config walk** (`MISE_CEILING_PATHS` = the agent
  root) so the walk sees the worktree's own `mise.toml` and nothing above
  it. `MISE_AUTO_INSTALL=false` keeps installing an explicit act rather than
  a side effect of launch.
```

In the same file, update the layout diagram so `fleets/<fleet>/` shows
`mise.toml` and `mise/`, and `crews/<crew>/` shows the same pair.

- [ ] **Step 2: Update docs/THREAT-MODEL.md**

In the accepted-risks list, replace the line reading "A plugin can read its
package and the shared mise install dir" so it names the daemon pool, and
add:

```markdown
- **An agent can install and run tools its worktree declares.** Its
  `MISE_DATA_DIR` is inside its own `home/`; the crew, fleet and daemon
  pools are read-only grants. This is no new capability — the agent could
  already download and run anything the sandbox allows — and it reaches no
  other agent: no pool is writable, and one fleet's agents never see
  another fleet's pool.
```

In the sandbox row of the controls table, change "shared mise dir" to "the
daemon, fleet and crew pools".

- [ ] **Step 3: Update AGENTS.md**

The `verify-claude` bullet describes its data root as "the shared
`MISE_DATA_DIR`". Change that phrase to "the daemon pool" so the term
matches the rest of the documentation.

- [ ] **Step 4: Verify the documentation against the code**

Run: `grep -rn "MISE_CEILING_PATHS\|shared mise" ARCHITECTURE.md docs/THREAT-MODEL.md AGENTS.md`
Expected: no stale claim that the repo's mise config is invisible to agents,
and no remaining "shared mise dir" phrasing.

- [ ] **Step 5: Run the full check and commit**

```bash
mise run check > /tmp/check.log 2>&1; tail -30 /tmp/check.log
git add -A
git commit -m "Document the mise pool hierarchy"
```

---

## Self-Review

**Spec coverage.** Every section of Spec E maps to a task: §3 layout to Task
1, §4 resolution to Tasks 2 and 3, §5 install pipeline to Tasks 4 and 5, §6
environment and sandbox to Task 6, §7 testing to Tasks 1 through 7 and the
documentation half to Task 8, §8 corrections to Task 8.

**Two gaps found and closed while reviewing.**

The spec's §5 table says the system level "runs in `ensure_crew`, first",
but the system table had no generated file to hash. Task 5 adds
`StateLayout::system_mise_toml_generated()` for that, distinct from the
admin's hand-written `system_mise_toml()`.

The spec did not say what `mise_env` becomes. It is the daemon-side env for
the agent's own install, so it must move in lockstep with `agent_env` or the
daemon would install into a directory the sandbox never reads. Task 6 covers
both, and its signature gains the agent id.

**Deferred deliberately.** Spec E §9 defers pruning the daemon pool, a
private data dir for plugins, and any agent-writable pool. No task touches
them.

**One behaviour worth re-reading before implementing.** A fleet tool that a
crew deletes with `null` still lands in the fleet pool. Task 3's first test
asserts this on purpose. It costs one unused install and keeps the rule
"a pool holds what its level declared" true without reachability analysis.
