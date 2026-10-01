# Spec N Amendment: Sandboxed Daemon Git Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run every git call the daemon makes in an existing agent clone at removal and branch change (the harvest's `upload-pack` and all of `agent_git`) under a read-only, network-blocked nono profile the daemon renders itself, so a rewritten clone can serve nothing the agent could not read. Closes #68 and #70.

**Architecture:** All the change is in `balerix-runtime`. `sandbox.rs` gains `render_git_profile` and `write_git_profile` (a per-agent profile at `agents/<a>/nono-git-profile.json`: read on the clone, the cache's objects, the `no-hooks` directory and the git binary; no `allow`; network blocked; the hardening variables in `set_vars`). `workspace.rs` routes `agent_git` and the harvest's `--upload-pack` through `nono run --profile <git profile>`, writes the profile just before use, and keeps `check_clone` and `check_clone_config` as the second layer. A hook between the checks and the harvest, reachable only through the `testing` module, lets `workspace_it` replay #70's race. No port, wire type or plugin changes.

**Tech Stack:** Rust 1.98 (edition 2024), real `git` and `nono` 0.79.0 in the `balerix-runtime` integration tests (`mise run test-it`), `serde_json` for the profile.

**Spec:** `docs/superpowers/specs/2026-10-01-balerix-n-sandboxed-daemon-git-design.md` (amends `2026-09-25-balerix-n-private-clones-design.md`)

## Global Constraints

- Run cargo through mise: `mise x -- cargo …`, or `mise run <task>`. `mise run check` and `mise run test-it` must pass at the end of Tasks 1 and 2; `mise run e2e` at the end of Task 2.
- One integration test: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test <file> <test_name> -- --nocapture`. With `BALERIX_REQUIRE_TOOLS=1` a skip is a failure.
- Every test root is under `target/tmp` (`support::temp_root`), never `/tmp`: nono grants `/tmp` by default, and an escape assertion there would pass vacuously.
- **No fallback to unsandboxed git** (NS-5). When nono cannot run, the call fails.
- `check_clone` and `check_clone_config` stay, with their messages unchanged (NS-4). No code path skips them.
- The git profile takes no user `sandbox` block and no agent `env`. Its `set_vars` are exactly: `GIT_OPTIONAL_LOCKS=0`, `GIT_NO_LAZY_FETCH=1`, `GIT_TERMINAL_PROMPT=0`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CEILING_DIRECTORIES=<canonical agent root>`.
- A failure of a sandboxed git call is a `MaterializeError::Tool` whose `tool` is `git` and whose `subcommand` is the git subcommand, so messages keep the shape `f/c/a: git <subcommand>: <first stderr line>`.
- `inspect.rs`, `create_clone` and every git call in the cache are not touched. `harden_agent_git` stays for `inspect.rs`.
- `unsafe_code = "forbid"`; clippy `unwrap_used`/`expect_used` warn outside tests. Never `std::env::set_var`.
- No new dependency.
- Commit after every task. The PR title is `fix(runtime): run daemon git in an agent's clone under a read-only nono profile (#68, #70)`.

## Review Focus

1. **A state root whose path holds a space or a single quote.** The `--upload-pack` string is run by a shell; every word must be quoted, and the harvest must still work. Test: `a_state_root_with_a_space_and_a_quote_is_harvested` in Task 2.
2. **A `git` that is not under `/usr`** (mise, nix, homebrew). The git profile must grant the binary, or every sandboxed call dies with exit 127. Test: the existing shim tests (`an_unreadable_head_without_a_marker_fails_the_removal`, `an_unreadable_head_fails_a_branch_change`), whose git is a script under `target/tmp`; Task 2 Step 9 names them.
3. **nono missing or unable to sandbox.** The removal must fail with the clone kept and nothing harvested, never fall back. Test: `a_harvest_without_a_working_nono_fails_and_keeps_the_clone` in Task 2.
4. **A daemon environment carrying `NONO_ALLOW`, `GIT_DIR` or the like.** The sandboxed calls must start from an empty environment. Test: `env_clear_drops_the_inherited_environment` in Task 2.
5. **A stale or hand-edited `nono-git-profile.json`.** It is rewritten before every use. Test: the tamper assertion in `daemon_git_in_a_clone_runs_under_the_git_profile`, Task 2.

---

### Task 1: The git profile

**Files:**
- Modify: `crates/balerix-runtime/src/layout.rs` (`impl CrewPaths`, `impl AgentPaths`)
- Modify: `crates/balerix-runtime/src/sandbox.rs`
- Modify: `crates/balerix-runtime/src/workspace.rs:58-73` (`harden_agent_git` uses `crew.no_hooks()`)
- Modify: `crates/balerix-runtime/src/lib.rs` (re-exports)
- Test: `crates/balerix-runtime/src/sandbox.rs` (unit), `crates/balerix-runtime/tests/sandbox_it.rs`

**Interfaces:**
- Consumes: `SYSTEM_READ`, `strs`, `write_profile_at`, `validate_profile_at`, `balerix_grants` (all in `sandbox.rs`).
- Produces:
  - `CrewPaths::no_hooks(&self) -> PathBuf` (`<crew root>/no-hooks`)
  - `AgentPaths::git_profile(&self) -> PathBuf` (`<agent root>/nono-git-profile.json`)
  - `pub fn render_git_profile(id: &AgentId, paths: &AgentPaths, crew: &CrewPaths, git: &Path) -> serde_json::Value`
  - `pub fn write_git_profile(tools: &ToolPaths, id: &str, paths: &AgentPaths, crew: &CrewPaths) -> Result<(), MaterializeError>`

- [ ] **Step 1: Write the failing unit tests**

Add to the `tests` module of `crates/balerix-runtime/src/sandbox.rs`:

```rust
    /// Spec N amendment §4: what the daemon's own git in a clone may reach.
    #[test]
    fn the_git_profile_reads_only_and_carries_nothing_of_the_users() {
        let layout = StateLayout::from_env(Path::new("/h"), |_| None);
        let id: AgentId = "f/c/a".parse().unwrap();
        let paths = layout.agent(&id);
        let crew = layout.crew(&id.crew_ref());
        let git = Path::new("/opt/git/bin/git");
        let p = render_git_profile(&id, &paths, &crew, git);

        assert_eq!(p["meta"]["name"], "balerix-git-f-c-a");
        let fs = p["filesystem"].as_object().unwrap();
        assert_eq!(
            fs.keys().collect::<Vec<_>>(),
            ["read"],
            "no allow, no write: the daemon's git never writes the clone"
        );
        assert_eq!(p["workdir"]["access"], "none");
        assert_eq!(p["network"], json!({ "block": true }));
        assert_eq!(p["environment"]["deny_vars"], json!(["*"]));
        assert_eq!(
            p["environment"]["set_vars"],
            json!({
                "GIT_CEILING_DIRECTORIES": paths.root.display().to_string(),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_NO_LAZY_FETCH": "1",
                "GIT_OPTIONAL_LOCKS": "0",
                "GIT_TERMINAL_PROMPT": "0",
            })
        );

        // a subset of what the agent itself can read, plus the empty
        // hooks directory and the git binary
        let agent = balerix_grants(
            &id,
            &paths,
            &crew,
            &layout,
            Path::new("/opt/balerix"),
            Path::new("/opt/mise"),
        );
        let read: Vec<PathBuf> = p["filesystem"]["read"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| PathBuf::from(v.as_str().unwrap()))
            .collect();
        for path in &read {
            assert!(
                agent.read.contains(path)
                    || agent.allow.contains(path)
                    || *path == crew.no_hooks()
                    || path == git,
                "{} is not something the agent can read",
                path.display()
            );
        }
        assert!(read.contains(&paths.workspace));
        assert!(read.contains(&crew.cache_objects()));
        assert!(!read.contains(&paths.home), "the agent's home is not git's");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `mise x -- cargo test -p balerix-runtime --lib the_git_profile_reads_only`
Expected: FAIL to compile, `cannot find function render_git_profile` and `no method named no_hooks`.

- [ ] **Step 3: Add the two paths**

In `crates/balerix-runtime/src/layout.rs`, inside `impl CrewPaths`, after `cache_objects`:

```rust
    /// The empty directory `core.hooksPath` names on every git call the
    /// daemon makes in an agent's clone, so no hook the agent wrote runs.
    pub fn no_hooks(&self) -> PathBuf {
        self.root.join("no-hooks")
    }
```

Inside `impl AgentPaths`, after `branch_marker`:

```rust
    /// The nono profile the daemon's own git runs under in this agent's
    /// clone (Spec N amendment 2026-10-01 §4). Daemon-owned: the agent
    /// root is outside every sandbox grant, so the agent can neither read
    /// nor change it.
    pub fn git_profile(&self) -> PathBuf {
        self.root.join("nono-git-profile.json")
    }
```

In `crates/balerix-runtime/src/workspace.rs`, in `harden_agent_git`, replace

```rust
    let no_hooks = crew.root.join("no-hooks");
```

with

```rust
    let no_hooks = crew.no_hooks();
```

- [ ] **Step 4: Render and write the profile**

In `crates/balerix-runtime/src/sandbox.rs`, after `render_profile`:

```rust
/// The profile the daemon's own git runs under in an agent's existing
/// clone: the harvest's `upload-pack` and the probes around it (Spec N
/// amendment 2026-10-01 §4, #68, #70). The daemon reads every repository
/// its uid can; the agent reads its own clone and the crew cache. Under
/// this profile git reads exactly those two (plus the empty hooks
/// directory and its own binary), writes nothing and has no network, so
/// a clone pointed at another repository serves nothing the agent could
/// not already read, and a promisor remote fetches nothing on any git.
///
/// Not derived from the agent's profile: that one carries write access,
/// open network, the user's `sandbox` block and the agent's `env`, none
/// of which the daemon's git should have. `set_vars` holds the hardening
/// `harden_agent_git` puts on a command's environment, since nono drops
/// every variable this list does not name. `git` comes from the host and
/// need not sit under `/usr`; granted as a single file, canonical since
/// Landlock rules bind to what the path resolves to.
pub fn render_git_profile(id: &AgentId, paths: &AgentPaths, crew: &CrewPaths, git: &Path) -> Value {
    let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
    read.push(std::fs::canonicalize(git).unwrap_or_else(|_| git.to_path_buf()));
    read.push(crew.cache_objects());
    read.push(crew.no_hooks());
    read.push(paths.workspace.clone());
    // git only honours a ceiling that matches the resolved path
    let ceiling = paths
        .root
        .canonicalize()
        .unwrap_or_else(|_| paths.root.clone());
    json!({
        "meta": {
            "name": format!("balerix-git-{}-{}-{}", id.fleet, id.crew, id.agent),
            "description": format!("generated by balerix for its own git in {id}'s clone"),
        },
        "filesystem": { "read": strs(&read) },
        "workdir": { "access": "none" },
        "network": { "block": true },
        "environment": {
            "deny_vars": ["*"],
            "set_vars": {
                "GIT_OPTIONAL_LOCKS": "0",
                "GIT_NO_LAZY_FETCH": "1",
                "GIT_TERMINAL_PROMPT": "0",
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CEILING_DIRECTORIES": ceiling.display().to_string(),
            },
        },
    })
}

/// Writes the git profile just before the daemon uses it, so nothing
/// depends on the order of the materialize steps or on a file an earlier
/// pass (or anything else) left there. Validated only when the bytes
/// changed. Creates what nono needs to exist: the hooks directory the
/// profile names, nono's `$HOME` and the log directory.
pub fn write_git_profile(
    tools: &ToolPaths,
    id: &str,
    paths: &AgentPaths,
    crew: &CrewPaths,
) -> Result<(), MaterializeError> {
    let agent_id: AgentId = id.parse().map_err(|_| MaterializeError::Invalid {
        id: id.to_string(),
        message: "not an agent id".into(),
    })?;
    for dir in [crew.no_hooks(), paths.nono_home.clone(), paths.logs.clone()] {
        std::fs::create_dir_all(&dir).map_err(|e| MaterializeError::Io {
            id: id.to_string(),
            path: dir.clone(),
            message: e.to_string(),
        })?;
    }
    let profile = render_git_profile(&agent_id, paths, crew, &tools.git);
    let path = paths.git_profile();
    if write_profile_at(&agent_id, &path, &profile)? {
        validate_profile_at(
            tools,
            &agent_id,
            &path,
            &paths.nono_home,
            &paths.logs.join("nono.validate.log"),
        )?;
    }
    Ok(())
}
```

In `crates/balerix-runtime/src/lib.rs`, extend the `sandbox` re-export:

```rust
pub use sandbox::{
    Grants, balerix_grants, check_conflicts, merge_profile, render_git_profile, render_profile,
    validate_profile, validate_profile_at, write_git_profile, write_profile, write_profile_at,
};
```

If `AgentId::from_str`'s error type makes `map_err(|_| …)` trip a clippy lint, bind it (`|e|`) and put `e.to_string()` in the message.

- [ ] **Step 5: Run the unit test**

Run: `mise x -- cargo test -p balerix-runtime --lib the_git_profile_reads_only`
Expected: PASS.

- [ ] **Step 6: Write the enforcement test**

Add to `crates/balerix-runtime/tests/sandbox_it.rs`:

```rust
/// Spec N amendment §4: under the git profile the clone and the cache's
/// objects read; the clone cannot be written, the agent's home cannot be
/// read, nothing outside can be read, and there is no network.
#[test]
fn the_git_profile_reads_the_clone_and_the_cache_and_writes_nothing() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox-git");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    let objects = crew.cache_objects();
    for d in [&paths.home, &paths.workspace, &objects.join("ab")] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(paths.workspace.join("tracked"), "clone-ok\n").unwrap();
    std::fs::write(objects.join("ab/probe"), "cache-ok\n").unwrap();
    std::fs::write(paths.home.join("secret"), "home\n").unwrap();
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), "outside\n").unwrap();

    balerix_runtime::write_git_profile(&tools, "f/c/a", &paths, &crew).unwrap();
    assert_eq!(
        std::fs::metadata(paths.git_profile()).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let script = format!(
        "cat {ws}/tracked && cat {objects}/ab/probe \
         && (echo x > {ws}/nope 2>/dev/null && echo CLONE_WRITABLE || echo clone-denied) \
         && (cat {home}/secret 2>/dev/null && echo HOME_READABLE || echo home-denied) \
         && (cat {outside}/secret 2>/dev/null && echo OUTSIDE_READABLE || echo outside-denied) \
         && echo LOCKS=$GIT_OPTIONAL_LOCKS LAZY=$GIT_NO_LAZY_FETCH FOO=$FOO",
        ws = paths.workspace.display(),
        objects = objects.display(),
        home = paths.home.display(),
        outside = outside.display(),
    );
    let out = Command::new(&tools.nono)
        .args([
            "-s",
            "run",
            "--profile",
            &paths.git_profile().display().to_string(),
            "--",
            "/bin/sh",
            "-c",
            &script,
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.nono_home)
        .env("FOO", "leak")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "nono run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout,
        "clone-ok\ncache-ok\nclone-denied\nhome-denied\noutside-denied\nLOCKS=0 LAZY=1 FOO=\n"
    );
    assert!(!paths.workspace.join("nope").exists());
}
```

Add `use std::os::unix::fs::PermissionsExt;` at the top of the file if it is not there.

- [ ] **Step 7: Run it**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test sandbox_it the_git_profile -- --nocapture`
Expected: PASS. If nono rejects `"network": {"block": true}` together with no `open_port`, read nono's message and fix the profile, not the test; the probe on 2026-10-01 accepted this shape.

- [ ] **Step 8: Gate and commit**

Run: `mise run check && mise run test-it`
Expected: both pass.

```bash
git add crates/balerix-runtime/src/layout.rs crates/balerix-runtime/src/sandbox.rs \
        crates/balerix-runtime/src/workspace.rs crates/balerix-runtime/src/lib.rs \
        crates/balerix-runtime/tests/sandbox_it.rs
git commit -m "feat(runtime): a read-only nono profile for the daemon's git in a clone (#68)"
```

---

### Task 2: Run the clone step's git under the profile

**Files:**
- Modify: `crates/balerix-runtime/src/tools.rs` (`Cmd::env_clear`)
- Modify: `crates/balerix-runtime/src/workspace.rs` (`agent_git`, `harvest`, `ensure_clone`, `harvest_and_remove`, doc comments)
- Modify: `crates/balerix-runtime/src/testing.rs` (`harvest_and_remove_racing`)
- Test: `crates/balerix-runtime/src/tools.rs` (unit), `crates/balerix-runtime/tests/workspace_it.rs`, `crates/balerix-runtime/tests/materialize_it.rs` (Landlock gate only)

**Interfaces:**
- Consumes: `write_git_profile(tools: &ToolPaths, id: &str, paths: &AgentPaths, crew: &CrewPaths) -> Result<(), MaterializeError>`, `AgentPaths::git_profile()`, `CrewPaths::no_hooks()` (Task 1); `crate::launch::outer_path(tools: &ToolPaths) -> String`; `crate::quote::sh_quote(s: &str) -> String`.
- Produces:
  - `Cmd::env_clear(self) -> Self` (`pub(crate)`)
  - `Workspace::harvest_and_remove_after(&self, id: &str, crew: &CrewPaths, agent: &AgentPaths, after_checks: impl FnOnce()) -> Result<(), MaterializeError>` (`pub(crate)`)
  - `balerix_runtime::testing::harvest_and_remove_racing(ws: &Workspace<'_>, id: &str, crew: &CrewPaths, agent: &AgentPaths, rewrite: impl FnOnce()) -> Result<(), MaterializeError>` (`pub`)
  - `Workspace::harvest_and_remove` keeps its signature.

- [ ] **Step 1: Add the hook between the checks and the harvest (no behaviour change)**

In `crates/balerix-runtime/src/workspace.rs`, replace the body of `harvest_and_remove` and add the inner function after it:

```rust
    pub fn harvest_and_remove(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<(), MaterializeError> {
        self.harvest_and_remove_after(id, crew, agent, || {})
    }

    /// `harvest_and_remove`, calling `after_checks` once both checks have
    /// passed and before any git reads the clone's objects. Production
    /// passes a no-op. `testing::harvest_and_remove_racing` passes a
    /// rewrite of the clone: #70's race, a process the agent detached
    /// past the tmux kill changing the clone under a verdict already
    /// given. The checks always run; nothing here skips them.
    pub(crate) fn harvest_and_remove_after(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        after_checks: impl FnOnce(),
    ) -> Result<(), MaterializeError> {
        if crew.repo.join(".git").is_dir() && agent.workspace.join(".git").is_dir() {
            check_clone(id, crew, agent)?;
            self.check_clone_config(id, crew, agent)?;
            after_checks();
            if let Some(branch) = self.assigned_branch(id, crew, agent)? {
                self.harvest(id, crew, agent, &branch)?;
            }
        }
        remove_tree(id, &agent.workspace)
    }
```

Keep the existing doc comment on `harvest_and_remove`.

In `crates/balerix-runtime/src/testing.rs`, add at the end of the non-test code (before `#[cfg(test)]`):

```rust
/// `Workspace::harvest_and_remove` with the clone rewritten between the
/// checks and the harvest: how `workspace_it` replays #70, where a
/// process the agent detached outlives the tmux kill and changes the
/// clone after `check_clone` has passed it. `rewrite` runs once, after
/// `check_clone` and `check_clone_config` and before any git reads the
/// clone's objects.
pub fn harvest_and_remove_racing(
    ws: &crate::Workspace<'_>,
    id: &str,
    crew: &crate::CrewPaths,
    agent: &crate::AgentPaths,
    rewrite: impl FnOnce(),
) -> Result<(), balerix_core::MaterializeError> {
    ws.harvest_and_remove_after(id, crew, agent, rewrite)
}
```

Update the module doc of `testing.rs` with one sentence: `Also the one hook integration tests need inside the clone step (\`harvest_and_remove_racing\`).`

- [ ] **Step 2: Write the failing race test**

Add to `crates/balerix-runtime/tests/workspace_it.rs`, after `a_clone_pointed_at_another_repository_is_refused_and_nothing_is_harvested`:

```rust
/// #70 and #68: the checks are a verdict on the clone as it was. A
/// process the agent detached past the tmux kill can rewrite the clone
/// after they pass. Each vector the checks refuse is applied here
/// *between* the checks and the harvest; the daemon's git runs under the
/// git profile, which cannot read the foreign repository, so nothing of
/// it reaches the cache whatever the clone says.
#[test]
fn a_clone_rewritten_after_the_checks_serves_nothing_foreign() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use balerix_runtime::testing::harvest_and_remove_racing;

    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-raced");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let crew = layout.crew(&"f/c".parse().unwrap());
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();

    let foreign = root.join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    git(&foreign, &["init", "-q", "-b", "main"]);
    std::fs::write(foreign.join("SECRET"), "another crew's code\n").unwrap();
    git(&foreign, &["add", "."]);
    git(&foreign, &["commit", "-q", "-m", "foreign"]);
    git(&foreign, &["repack", "-q", "-a", "-d"]);
    let foreign_sha = git(&foreign, &["rev-parse", "HEAD"]).trim().to_string();
    let foreign_git = foreign.join(".git");

    let ran = root.join("uploadpack-ran");
    let script = root.join("uploadpack.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch {}\nexec git-upload-pack \"$@\"\n",
            ran.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let raced = |name: &str, rewrite: &dyn Fn(&balerix_runtime::AgentPaths)| {
        let id = format!("f/c/{name}");
        let paths = layout.agent(&id.parse().unwrap());
        ws.ensure_clone(&id, &crew, &paths, &repo, &format!("balerix/{id}"), "main")
            .unwrap();
        let outcome = harvest_and_remove_racing(&ws, &id, &crew, &paths, || rewrite(&paths));
        assert!(
            !git_ok(&crew.repo, &["cat-file", "-e", &foreign_sha]),
            "{id}: the foreign commit reached the cache"
        );
        assert!(!ran.exists(), "{id}: remote.evil.uploadpack ran");
        let e = outcome.unwrap_err().to_string();
        assert!(e.starts_with(&format!("{id}: git ")), "{e}");
        assert!(paths.workspace.exists(), "{id}: a failed harvest deletes nothing");
    };
    let point_ref = |paths: &balerix_runtime::AgentPaths, name: &str| {
        std::fs::write(
            paths
                .workspace
                .join(format!(".git/refs/heads/balerix/f/c/{name}")),
            format!("{foreign_sha}\n"),
        )
        .unwrap();
    };

    // (A) alternates appended, the assigned ref written by hand
    raced("a", &|p| {
        let alternates = p.workspace.join(".git/objects/info/alternates");
        let mut lines = std::fs::read_to_string(&alternates).unwrap();
        lines.push_str(&format!("{}\n", foreign_git.join("objects").display()));
        std::fs::write(&alternates, lines).unwrap();
        point_ref(p, "a");
    });

    // (B) `.git` swapped for a symlink to the foreign repository's
    raced("b", &|p| {
        std::fs::remove_dir_all(p.workspace.join(".git")).unwrap();
        symlink(&foreign_git, p.workspace.join(".git")).unwrap();
    });

    // (B') a pack symlinked to the foreign pack
    raced("c", &|p| {
        for entry in std::fs::read_dir(foreign_git.join("objects/pack")).unwrap() {
            let path = entry.unwrap().path();
            symlink(
                &path,
                p.workspace
                    .join(".git/objects/pack")
                    .join(path.file_name().unwrap()),
            )
            .unwrap();
        }
        point_ref(p, "c");
    });

    // (C) `.git/commondir`
    raced("d", &|p| {
        std::fs::write(
            p.workspace.join(".git/commondir"),
            foreign_git.display().to_string(),
        )
        .unwrap();
    });

    // (E) a promisor remote with its own upload-pack program
    raced("e", &|p| {
        for (k, v) in [
            ("core.repositoryformatversion", "1"),
            ("extensions.partialClone", "evil"),
            ("remote.evil.promisor", "true"),
            ("remote.evil.url", &foreign.display().to_string()),
            ("remote.evil.uploadpack", &script.display().to_string()),
        ] {
            git(&p.workspace, &["config", k, v]);
        }
        point_ref(p, "e");
    });

    // an honest clone goes through the same path untouched
    let id = "f/c/f";
    let paths = layout.agent(&id.parse().unwrap());
    ws.ensure_clone(id, &crew, &paths, &repo, "balerix/f/c/f", "main")
        .unwrap();
    harvest_and_remove_racing(&ws, id, &crew, &paths, || {}).unwrap();
    assert!(!paths.workspace.exists());
    assert!(git_ok(
        &crew.repo,
        &["rev-parse", "--verify", "--quiet", "refs/heads/balerix/f/c/f"]
    ));
}
```

- [ ] **Step 3: Run it to verify it fails for the right reason**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test workspace_it a_clone_rewritten_after_the_checks -- --nocapture`
Expected: FAIL at vector (A) with `f/c/a: the foreign commit reached the cache`. That is #70 reproduced: the daemon-privileged `upload-pack` served the foreign commit. If it fails anywhere else, fix the test before going on.

- [ ] **Step 4: Write the remaining failing tests**

In the `tests` module of `crates/balerix-runtime/src/tools.rs`:

```rust
    /// A sandboxed git call starts from nothing the daemon inherited
    /// (`NONO_ALLOW`, `GIT_DIR`, …): only what the call sets.
    #[test]
    fn env_clear_drops_the_inherited_environment() {
        let out = Cmd::new(Path::new("/usr/bin/env"))
            .env_clear()
            .env("ONLY", "this")
            .run()
            .unwrap();
        assert_eq!(out.stdout, "ONLY=this\n");
    }
```

In `crates/balerix-runtime/tests/workspace_it.rs`:

```rust
/// Spec N amendment §5: every git call in an existing clone, and the
/// harvest's `upload-pack`, goes through `nono run --profile <git
/// profile>`; the profile is rewritten before use whatever was there.
#[test]
fn daemon_git_in_a_clone_runs_under_the_git_profile() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-sandboxed");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    assert!(
        !paths.git_profile().exists(),
        "a fresh clone needs no git profile: the agent has not touched it"
    );
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);

    // whatever sits at the profile's path is replaced before use
    std::fs::write(paths.git_profile(), "{\"filesystem\":{\"allow\":[\"/\"]}}").unwrap();
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();

    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha,
        "the harvest still works through the sandbox"
    );
    let profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(paths.git_profile()).unwrap()).unwrap();
    assert!(profile["filesystem"].get("allow").is_none(), "{profile}");
    assert_eq!(profile["network"]["block"], true);

    let log = std::fs::read_to_string(crew.root.join("logs/git.log")).unwrap();
    let profile_arg = format!("run --profile {}", paths.git_profile().display());
    let in_clone: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with("$ ") && l.contains(&paths.workspace.display().to_string()))
        .filter(|l| !l.contains(" clone ") && !l.contains("checkout"))
        .collect();
    assert!(!in_clone.is_empty(), "{log}");
    for line in in_clone {
        assert!(
            line.contains(&profile_arg) || line.contains("--upload-pack=env -i "),
            "a git call in the existing clone ran outside the git profile: {line}"
        );
    }
    assert!(
        log.contains("--upload-pack=env -i ") && log.contains("upload-pack --strict"),
        "{log}"
    );
}

/// Review focus 1: the `--upload-pack` string is run by a shell, and the
/// state root is the operator's to name.
#[test]
fn a_state_root_with_a_space_and_a_quote_is_harvested() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-quoting");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root.join("it's a root"));
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools: &tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    let sha = git(&paths.workspace, &["rev-parse", "HEAD"]);
    ws.harvest_and_remove("f/c/a", &crew, &paths).unwrap();
    assert_eq!(
        git(&crew.repo, &["rev-parse", "refs/heads/balerix/f/c/a"]),
        sha
    );
}

/// NS-5: no fallback. Without a nono that runs, the removal fails, the
/// clone stays and nothing is harvested.
#[test]
fn a_harvest_without_a_working_nono_fails_and_keeps_the_clone() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-no-nono");
    let layout = support::layout(&root);
    let repo = bare_repo(&root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    Workspace {
        tools: &tools,
        gh_config_dir: None,
    }
    .ensure_repo("f/c", &crew, &repo, "main")
    .unwrap();
    Workspace {
        tools: &tools,
        gh_config_dir: None,
    }
    .ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
    .unwrap();

    let no_nono = balerix_runtime::ToolPaths {
        nono: root.join("no-such-nono"),
        ..tools.clone()
    };
    let e = Workspace {
        tools: &no_nono,
        gh_config_dir: None,
    }
    .harvest_and_remove("f/c/a", &crew, &paths)
    .unwrap_err()
    .to_string();
    assert!(e.starts_with("f/c/a: "), "{e}");
    assert!(e.contains("no-such-nono"), "the message names what is missing: {e}");
    assert!(paths.workspace.exists(), "a failed removal deletes nothing");
    assert!(!git_ok(
        &crew.repo,
        &["rev-parse", "--verify", "--quiet", "refs/heads/balerix/f/c/a"]
    ));
}
```

If `serde_json` is not already a dev-dependency of `balerix-runtime`, it is a normal dependency of the crate (`sandbox.rs` uses it), which integration tests can use; no manifest change.

- [ ] **Step 5: Run them to verify they fail**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test workspace_it -- daemon_git_in_a_clone a_harvest_without_a_working_nono`
Expected: `daemon_git_in_a_clone_runs_under_the_git_profile` FAILS (the tampered profile is still there with `allow`); `a_harvest_without_a_working_nono_fails_and_keeps_the_clone` FAILS (the harvest succeeds without nono). `env_clear…` does not compile yet. `a_state_root_with_a_space_and_a_quote_is_harvested` passes today and must still pass after Step 7.

- [ ] **Step 6: `Cmd::env_clear`**

In `crates/balerix-runtime/src/tools.rs`: add a field `env_clear: bool` to `struct Cmd`, `env_clear: false` in `Cmd::new`, and after `env_remove`:

```rust
    /// Starts the child from an empty environment: only what `env`/`envs`
    /// set on this `Cmd` reaches it. For a `nono run` the daemon makes on
    /// its own behalf, where an inherited `NONO_ALLOW` would widen the
    /// sandbox.
    pub(crate) fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }
```

In `exec`, replace

```rust
        let mut c = Command::new(&self.program);
        c.args(&self.args).envs(&self.env);
```

with

```rust
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        if self.env_clear {
            c.env_clear();
        }
        c.envs(&self.env);
```

- [ ] **Step 7: Route `agent_git` and the harvest through the profile**

In `crates/balerix-runtime/src/workspace.rs`:

Add imports:

```rust
use crate::launch::outer_path;
use crate::quote::sh_quote;
use crate::sandbox::write_git_profile;
```

Add to `impl Workspace<'_>`, before `agent_git`:

```rust
    /// `nono`'s arguments up to and including the git binary, for a git
    /// call in `agent`'s existing clone: `-s --log-file <logs>/nono-git.log
    /// run --profile <git profile> -- <git>`. The profile is
    /// `sandbox::render_git_profile`; `write_git_profile` must have run.
    fn sandbox_args(&self, agent: &AgentPaths) -> Vec<String> {
        vec![
            "-s".into(),
            "--log-file".into(),
            agent.logs.join("nono-git.log").display().to_string(),
            "run".into(),
            "--profile".into(),
            agent.git_profile().display().to_string(),
            "--".into(),
            self.tools.git.display().to_string(),
        ]
    }
```

Replace `agent_git` (keep its signature) with:

```rust
    /// One git call inside the agent's existing clone, run under the git
    /// profile (`sandbox::render_git_profile`; Spec N amendment
    /// 2026-10-01, #68, #70): it reads the clone and the crew cache's
    /// objects, writes nothing and has no network, so whatever the clone
    /// points at, git sees no more than the agent could. The checks
    /// before it give the operator a readable refusal; the profile is the
    /// boundary, and holds even when the clone changes after the checks.
    ///
    /// nono starts from an empty environment (`HOME` is the agent's
    /// `nono/`, as for `launch.sh`), and the profile's `set_vars` carry
    /// the hardening `harden_agent_git` sets for the workspace reader;
    /// the `-c` pairs are the same. `--git-dir` names `workspace/.git`
    /// exactly: with `-C` alone, a `.git` git rejects (say, its `HEAD`
    /// deleted) makes git take `workspace/` itself for a bare repository.
    /// `accepted` are the exit codes that count as success (0 included).
    /// A failure is reported as git's, with git's subcommand, not nono's.
    fn agent_git(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
        args: &[&str],
        accepted: &[i32],
    ) -> Result<String, MaterializeError> {
        let cmd = Cmd::new(&self.tools.nono)
            .env_clear()
            .env("HOME", agent.nono_home.display().to_string())
            .env("PATH", outer_path(self.tools))
            .log(&crew.root.join("logs").join("git.log"))
            .args(self.sandbox_args(agent))
            .args(["-c", "core.fsmonitor=false"])
            .args([
                "-c".to_string(),
                format!("core.hooksPath={}", crew.no_hooks().display()),
                "-C".to_string(),
                agent.workspace.display().to_string(),
                format!("--git-dir={}", agent.workspace.join(".git").display()),
                format!("--work-tree={}", agent.workspace.display()),
            ])
            .args(args.iter().copied());
        cmd.run_with_exit_codes(accepted)
            .map(|o| o.stdout)
            .map_err(|f| MaterializeError::Tool {
                id: id.to_string(),
                tool: "git".into(),
                subcommand: args.first().copied().unwrap_or_default().to_string(),
                args: f.args,
                stderr: f.stderr,
            })
    }
```

In `harvest`, replace the `--upload-pack` argument

```rust
                &format!(
                    "--upload-pack='{}' upload-pack --strict",
                    self.tools.git.display().to_string().replace('\'', "'\\''")
                ),
```

with a string built before the `self.git(…)` call:

```rust
        // Run by a shell on the fetch's serving side: every word quoted.
        // `env -i` for the same reason `agent_git` clears its environment.
        let mut upload_pack = format!(
            "--upload-pack=env -i HOME={} PATH={} {}",
            sh_quote(&agent.nono_home.display().to_string()),
            sh_quote(&outer_path(self.tools)),
            sh_quote(&self.tools.nono.display().to_string()),
        );
        for word in self.sandbox_args(agent) {
            upload_pack.push(' ');
            upload_pack.push_str(&sh_quote(&word));
        }
        upload_pack.push_str(" upload-pack --strict");
```

and pass `&upload_pack` in its place. In `harvest`'s doc comment, replace the clause "`upload-pack` in the clone takes no repo-local hook or program config, and the only write is into the daemon-owned cache." with:

```
/// `upload-pack` runs in the clone under the git profile (`agent_git`),
/// so it serves only what the agent could read, and the only write is
/// into the daemon-owned cache.
```

Write the profile before the first sandboxed call, at both call sites. In `harvest_and_remove_after`:

```rust
            check_clone(id, crew, agent)?;
            write_git_profile(self.tools, id, agent, crew)?;
            self.check_clone_config(id, crew, agent)?;
            after_checks();
```

In `ensure_clone`, after the marker's reuse `return Ok(())` and before `self.check_clone_config(id, crew, agent)?;`:

```rust
            write_git_profile(self.tools, id, agent, crew)?;
```

`harden_agent_git` is now used by `inspect.rs` and the unit test only; leave it, and in its doc comment change "shared by the workspace reader (`inspect.rs`) and the clone step (Spec N §4 step 1, #62)" to "for the workspace reader (`inspect.rs`); the clone step runs under the git profile instead, whose `set_vars` carry the same variables (`sandbox::render_git_profile`)". In `check_clone`'s doc comment, replace the sentence "The session is stopped before both callers run (…), so no live writer races this check and the git calls after it." with:

```
/// The session is stopped before both callers run, but a process the
/// agent detached can outlive the tmux kill and rewrite the clone after
/// this check (#70). That is why the git calls after it run under the
/// git profile (`agent_git`): this check names the problem for the
/// operator, the profile is what holds.
```

- [ ] **Step 8: Run the new tests**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test workspace_it -- a_clone_rewritten_after_the_checks daemon_git_in_a_clone a_state_root_with a_harvest_without_a_working_nono --nocapture`
and: `mise x -- cargo test -p balerix-runtime --lib env_clear_drops`
Expected: all PASS.

If a vector in `a_clone_rewritten_after_the_checks_serves_nothing_foreign` passes the two foreign-commit assertions but the harvest returns `Ok` (git served an empty or honest branch), that vector's `unwrap_err` is wrong for this git version, not the sandbox: report the git version and the outcome, and relax only that vector's error assertion. Never relax `the foreign commit reached the cache`.

- [ ] **Step 9: Gate the tests that now need Landlock, and run the whole file**

Every test that runs git in an existing clone now needs Landlock. In `crates/balerix-runtime/tests/workspace_it.rs`, in each test that calls `harvest_and_remove` or calls `ensure_clone` on an existing clone, add directly after its `let root = support::temp_root(…);` line:

```rust
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
```

Those are: `a_changed_branch_moves_a_clean_clone_and_keeps_the_old_branch`, `a_changed_branch_on_a_dirty_clone_fails_and_keeps_the_tree`, `an_agent_switching_branches_itself_is_left_alone_on_an_unchanged_setting`, `a_clone_without_a_marker_is_judged_by_head_once_then_recorded`, `a_changed_branch_the_clone_already_sits_on_is_recorded_without_a_move`, `the_clone_step_runs_no_program_from_the_clone_config`, `an_unpushed_commit_survives_removal_and_seeds_the_next_clone`, `removal_harvests_head_without_a_marker_and_skips_what_is_not_there`, `a_branch_the_cache_has_checked_out_is_harvested_too`, `a_broken_clone_fails_the_removal_and_stays`, `a_worktree_from_0_1_is_refused_and_keep_repos_migrates_it`, `a_clone_pointed_at_another_repository_is_refused_and_nothing_is_harvested`, `an_agent_on_the_default_branch_materializes`, `a_promisor_remote_in_the_clone_fetches_nothing_and_runs_nothing`, `an_unreadable_head_without_a_marker_fails_the_removal`, `an_unreadable_head_fails_a_branch_change`. In `crates/balerix-runtime/tests/materialize_it.rs`: `materialize_then_remove_round_trip` and `a_purge_deletes_a_broken_clone_without_harvesting`.

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo test -p balerix-runtime --test workspace_it --test materialize_it`
Expected: all PASS, with no assertion in an existing test changed. Two things to check by name:

- `an_unreadable_head_without_a_marker_fails_the_removal` and `an_unreadable_head_fails_a_branch_change` replace git with a script under `target/tmp`. They pass only because the git profile grants the git binary's own path (Review focus 2). If they fail with exit 126 or 127, the grant in `render_git_profile` is wrong.
- `the_clone_step_runs_no_program_from_the_clone_config` and the promisor test assert that no program from the clone's config ran. They must pass unchanged.

If an existing assertion fails because a message changed, the implementation is wrong (Global Constraints: error shape); fix the code, not the test.

- [ ] **Step 10: Gate and commit**

Run: `mise run check && mise run test-it && mise run e2e`
Expected: all pass.

```bash
git add crates/balerix-runtime/src/tools.rs crates/balerix-runtime/src/workspace.rs \
        crates/balerix-runtime/src/testing.rs crates/balerix-runtime/tests/workspace_it.rs \
        crates/balerix-runtime/tests/materialize_it.rs
git commit -m "fix(runtime): run daemon git in an agent's clone under the git profile (#68, #70)"
```

---

### Task 3: Documents, issues, pull request

**Files:**
- Modify: `docs/THREAT-MODEL.md` (the bullet starting "**The daemon runs read-only `git` in a repository an agent can write to.**")
- Modify: `ARCHITECTURE.md` (after the bullet "**Harvest is a fetch from the cache, not a push from the clone.**")
- Modify: `AGENTS.md` (Gotchas, after the bullet that begins "`Workspace::git` (`crates/balerix-runtime/src/workspace.rs`) (`scrub_git_env`)")
- Modify: `docs/superpowers/specs/2026-09-25-balerix-n-private-clones-design.md` (§12, new last bullet)

**Interfaces:**
- Consumes: the names from Tasks 1 and 2 (`render_git_profile`, `nono-git-profile.json`, `agent_git`, `harvest_and_remove_racing`, the two test names).
- Produces: nothing code depends on.

- [ ] **Step 1: `docs/THREAT-MODEL.md`**

Append to the end of the bullet "**The daemon runs read-only `git` in a repository an agent can write to.**" (same paragraph, after its last sentence):

```
At removal and at a branch change the daemon's git in the clone, and the `upload-pack` that serves the harvest, do not run with the daemon's privileges at all: they run under a nono profile the daemon renders for itself (`sandbox::render_git_profile`, `agents/<a>/nono-git-profile.json`) that reads the clone, the crew cache's objects and nothing else of balerix's, writes nothing and has no network. A clone pointed at another repository (alternates, a symlinked `.git` or pack, `commondir`) therefore serves nothing the agent could not read itself, a promisor remote fetches nothing on any git version, and this holds when a process the agent detached rewrites the clone after `check_clone` has passed (#70). `check_clone` and `check_clone_config` remain as the second layer and as the source of the operator's message. Evidence: `workspace_it` (`a_clone_rewritten_after_the_checks_serves_nothing_foreign`), `sandbox_it` (`the_git_profile_reads_the_clone_and_the_cache_and_writes_nothing`). Residue: the workspace reader (`diff`, `version`) still runs with the daemon's privileges behind the refusals and overrides described above; and a process the agent detached outlives `stop`, inside the agent's sandbox, until it exits.
```

Then read the whole bullet once: if an earlier sentence in it says the stopped session is what makes the clone step safe, delete that sentence.

- [ ] **Step 2: `ARCHITECTURE.md`**

Insert after the bullet "**Harvest is a fetch from the cache, not a push from the clone.**" (after that bullet's last line):

```
- **Daemon git in an agent's clone runs under a read-only nono profile.**
  The harvest's `upload-pack` and the probes around it (`config`,
  `symbolic-ref`, `rev-parse`, `status`) read a repository the agent
  wrote. Run as the daemon they are a confused deputy: a clone pointed at
  another repository would have the daemon copy it into the cache. So
  they run under `agents/<a>/nono-git-profile.json`, rendered by the
  daemon just before use: read on the clone and the cache's objects, no
  write, no network, none of the user's `sandbox` block or the agent's
  `env`. Not the agent's own profile, which grants more than git needs.
  `check_clone` stays for the message it gives; the profile is what
  holds when the clone changes after the check (#70). The workspace
  reader is not yet under it.
```

- [ ] **Step 3: `AGENTS.md`**

Insert in Gotchas, after the bullet that begins "`Workspace::git` (`crates/balerix-runtime/src/workspace.rs`) (`scrub_git_env`)":

```
- The clone step's git (`Workspace::agent_git` and the harvest's
  `upload-pack`) runs inside `nono run --profile nono-git-profile.json`
  from an empty environment. Its hardening variables live in that
  profile's `set_vars` (`sandbox::render_git_profile`), not on the
  command: a variable added to `harden_agent_git` alone reaches the
  workspace reader and not these calls. Tests that run git in an existing
  clone need Landlock and gate on `support::landlock_works`.
```

- [ ] **Step 4: Spec N §12**

Append as the last bullet of §12 in `docs/superpowers/specs/2026-09-25-balerix-n-private-clones-design.md`:

```
- 2026-10-01 (#68, #70): the durable alternative named above landed, as
  `2026-10-01-balerix-n-sandboxed-daemon-git-design.md`. Every git call
  in an existing clone at removal and branch change, and the harvest's
  `upload-pack`, runs under a daemon-rendered read-only nono profile
  (`nono-git-profile.json`), not the agent's own. "The session is
  stopped before both call sites, so nothing races the check" was wrong:
  a detached process survives the tmux kill; the profile, not the check,
  is now what a rewritten clone cannot get past. §5 and §7 are read with
  that amendment.
```

- [ ] **Step 5: Gate and commit**

Run: `mise run check`
Expected: pass.

```bash
git add docs/THREAT-MODEL.md ARCHITECTURE.md AGENTS.md \
        docs/superpowers/specs/2026-09-25-balerix-n-private-clones-design.md
git commit -m "docs: daemon git in an agent's clone runs under a read-only nono profile (#68, #70)"
```

- [ ] **Step 6: Open the two follow-up issues**

```bash
gh issue create --title "Runner: stop should end the sandbox's whole process tree and wait for it to be empty" --label bug --body "Split from #70 (Spec N amendment 2026-10-01, NS-1).

\`TmuxRunner::stop_agent\` kills the tmux window. A process the agent started with \`setsid\`/\`nohup\` inside the sandbox outlives it. Since the amendment it can no longer steer the daemon's git (those calls run under the read-only git profile), but it keeps running inside the agent's sandbox, with network, after \`down\`, and can write into \`workspace/\` while the daemon deletes it.

- [ ] Track the agent's process group or cgroup (nono may expose the sandbox's pid namespace or cgroup) and terminate it in \`stop\`, waiting for it to be empty.
- [ ] \`runtime\` integration test: an agent that \`setsid\`s a sleeper, \`stop\`, assert no process of the session survives."

gh issue create --title "Workspace reader: run diff and version under the git profile" --label enhancement --body "Split from #68 (Spec N amendment 2026-10-01, NS-2).

\`inspect.rs\` runs \`git diff\`/\`version\` in an agent's clone with the daemon's privileges, behind \`refuse_filters\` and \`harden_agent_git\`, while the agent is live. A clone pointed at another repository shows that repository's objects to the operator through the web plugin (never written to the cache).

- [ ] Run those calls under \`nono-git-profile.json\` as the clone step does (\`Workspace::agent_git\`).
- [ ] Measure the per-call cost on the web plugin's poll (about 55 ms per sandboxed call on 2026-10-01) and decide whether \`version\` needs batching."
```

- [ ] **Step 7: Push and open the pull request**

```bash
git push -u origin spec-n-sandboxed-daemon-git
gh pr create --title "fix(runtime): run daemon git in an agent's clone under a read-only nono profile (#68, #70)" --body "Closes #68. Closes #70.

Every git call the daemon makes in an existing agent clone at removal and branch change, and the \`upload-pack\` that serves the harvest, now runs under a nono profile the daemon renders for itself: read on the clone and the crew cache's objects, no write, no network. \`check_clone\` and \`check_clone_config\` stay as the second layer.

Spec: \`docs/superpowers/specs/2026-10-01-balerix-n-sandboxed-daemon-git-design.md\`.

#70 is closed by making the daemon immune to a rewritten clone; killing processes that outlive \`stop\`, and moving the workspace reader under the profile, are the two follow-up issues linked below.

Verified: \`mise run check\`, \`mise run test-it\`, \`mise run e2e\`."
```

Add the two new issue numbers to the PR body once Step 6 has printed them.
