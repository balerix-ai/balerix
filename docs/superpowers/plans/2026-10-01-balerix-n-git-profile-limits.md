# Git Profile Limits (#109) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The daemon's sandboxed git gets its `--exec-path` directory granted, a sandbox failure after the canary can no longer read as git's "no", and three tool bumps land in the same PR.

**Architecture:** `write_git_profile` asks the host git for its exec-path from an empty environment and hands the canonical directory to `render_git_profile`, which grants it read-only. `Cmd`'s output carries stderr, and `Workspace::agent_git` turns an accepted non-zero exit that printed anything into a `MaterializeError::Tool`. `prepare_sandbox` prefixes a failed canary with a pointer to `nono-git.log`.

**Tech Stack:** Rust (edition 2024, `unsafe` forbidden), `cargo nextest` through mise, nono 0.79.0 (Landlock), git, shell shims as test fixtures.

**Spec:** `docs/superpowers/specs/2026-10-01-balerix-n-sandboxed-daemon-git-design.md` §12 (read §4 to §6 for the profile and the calls it amends).

## Global Constraints

- Branch `fix-109-git-profile-limits`. Run cargo only through mise: `mise x -- cargo …` or `mise run <task>`.
- `mise run check` passes before every commit.
- No fallback to unsandboxed git, and no profile written without the exec-path grant (NS-5, §12.1).
- Library prefixes are not granted, by setting or by derivation (NS-6). The user `sandbox` block never reaches the git profile.
- No new Cargo dependency. No `unsafe`, so no `std::env::set_var` in tests.
- Test roots live under `target/tmp` (`support::temp_root`), never `/tmp`.
- Tests that run git in an existing clone, or anything under nono, gate on `support::landlock_works` and skip with a printed reason; `BALERIX_REQUIRE_TOOLS=1` makes the skip a failure.
- Every tool version in `mise.toml` is exact: rust `1.99.0`, claude `2.1.287`, trivy `0.75.0`. `rust-version = "1.98"` in the manifests is the MSRV and does not move.
- Error text: `the sandbox did not start; see <agent>/logs/nono-git.log: ` is a prefix on the first stderr line (`MaterializeError::Tool` displays only that line).
- PR title: `fix(runtime): grant git's exec-path in the git profile; a sandbox failure is never git's "no" (#109)`.

## Review Focus

Each line names the task whose tests pin it.

1. The daemon's environment carries `GIT_EXEC_PATH` (or anything else git reads): the grant must name the directory the *sandboxed* git uses, so the query starts from an empty environment. Task 2, `the_exec_path_query_starts_from_an_empty_environment`.
2. The exec-path is a symlink, or has a space in it: the profile holds the resolved directory and nono accepts it. Task 2, `the_git_profile_grants_gits_exec_path` (fixture uses both).
3. git prints a warning on stderr on a call that exits 0: the call still succeeds and the harvest happens. Task 3, `a_git_warning_on_a_successful_probe_does_not_fail_the_harvest`.
4. git prints a warning beside a real exit 1: the step fails and the clone stays, rather than the warning being ignored. Task 3, `a_git_warning_beside_a_no_fails_the_removal_and_keeps_the_clone`.
5. An accepted exit 1 whose stderr is only whitespace: still git's "no". Task 3, unit test `an_accepted_exit_is_gits_answer_only_when_silent`.

## File Structure

| File | Change |
|------|--------|
| `mise.toml` | the three bumps (already in the working tree) |
| `crates/balerix-runtime/src/sandbox.rs` | `git_exec_path` (new, private); `render_git_profile` gains `exec_path`; `write_git_profile` queries before rendering; doc comment; unit test |
| `crates/balerix-runtime/src/tools.rs` | `CmdOutput.stderr` |
| `crates/balerix-runtime/src/workspace.rs` | `is_gits_answer` (new, private); `agent_git` applies it; `prepare_sandbox` adds the hint; unit test |
| `crates/balerix-runtime/tests/sandbox_it.rs` | three exec-path tests and their shim helper |
| `crates/balerix-runtime/tests/workspace_it.rs` | two nono-shim tests, two noisy-git tests, the hint assertion |
| `AGENTS.md`, `README.md`, `docs/THREAT-MODEL.md` | the limit as it now stands |

---

### Task 1: Tool bumps

**Files:**
- Modify: `mise.toml` (already modified, uncommitted)
- Modify: whatever rust 1.99.0's clippy or rustfmt flags

**Interfaces:**
- Consumes: nothing.
- Produces: a branch that builds and lints clean on rust 1.99.0; later tasks assume it.

- [ ] **Step 1: Confirm the diff is exactly the three bumps**

Run: `git diff mise.toml`
Expected: three changed lines only: `rust … "1.99.0"`, `claude = "2.1.287"`, `trivy = "0.75.0"`.

- [ ] **Step 2: Install the tools**

Run: `mise install`
Expected: exits 0; `mise x -- rustc --version` prints `rustc 1.99.0`, `mise x -- claude --version` prints `2.1.287`.

- [ ] **Step 3: Run the core gate**

Run: `mise run check`
Expected: PASS. If clippy or rustfmt on 1.99.0 reports something new, fix the code the way the lint's own suggestion says. Do not add `#[allow]` and do not edit `clippy.toml`. Re-run until it passes.

- [ ] **Step 4: Run the plugin gate**

Run: `mise run plugins`
Expected: PASS for common, flow, web, matrix and github. Fix new lints the same way.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "build: rust 1.99.0, claude 2.1.287, trivy 0.75.0"
```

(`git add -A` is right here only if `git status --short` shows `mise.toml` and lint fixes and nothing else; otherwise add the files by name.)

---

### Task 2: Grant git's exec-path in the git profile

**Files:**
- Modify: `crates/balerix-runtime/src/sandbox.rs:103-185` (doc comment, `render_git_profile`, `write_git_profile`, new `git_exec_path`) and its unit test at `:592-652`
- Test: `crates/balerix-runtime/tests/sandbox_it.rs` (append)
- Modify: `AGENTS.md`, `README.md`, `docs/THREAT-MODEL.md`

**Interfaces:**
- Consumes: `crate::tools::{Cmd, ToolPaths}`, `crate::launch::outer_path(tools: &ToolPaths) -> String`.
- Produces:
  - `pub fn render_git_profile(id: &AgentId, paths: &AgentPaths, crew: &CrewPaths, git: &Path, exec_path: &Path) -> Value` (one new parameter, last).
  - `pub fn write_git_profile(tools: &ToolPaths, id: &str, paths: &AgentPaths, crew: &CrewPaths) -> Result<(), MaterializeError>` (signature unchanged; now fails with `MaterializeError::Tool { tool: "git", subcommand: "--exec-path", … }` when git cannot name an existing exec-path directory).

- [ ] **Step 1: Update the unit test to the new signature and assertion**

In `crates/balerix-runtime/src/sandbox.rs`, test `the_git_profile_reads_only_and_carries_nothing_of_the_users`, replace

```rust
        let git = Path::new("/opt/git/bin/git");
        let p = render_git_profile(&id, &paths, &crew, git);
```

with

```rust
        let git = Path::new("/opt/git/bin/git");
        let exec_path = Path::new("/opt/git/libexec/git-core");
        let p = render_git_profile(&id, &paths, &crew, git, exec_path);
```

replace the comment and the loop's condition

```rust
        // a subset of what the agent itself can read, plus the empty
        // hooks directory and the git binary
```
```rust
                agent.read.contains(path)
                    || agent.allow.contains(path)
                    || *path == crew.no_hooks()
                    || path == git,
```

with

```rust
        // a subset of what the agent itself can read, plus the empty
        // hooks directory, the git binary and git's exec-path
```
```rust
                agent.read.contains(path)
                    || agent.allow.contains(path)
                    || *path == crew.no_hooks()
                    || path == git
                    || path == exec_path,
```

and add, after `assert!(read.contains(&crew.cache_objects()));`:

```rust
        assert!(
            read.contains(&exec_path.to_path_buf()),
            "upload-pack spawns pack-objects through git's exec-path (#109)"
        );
```

- [ ] **Step 2: Run it to verify it fails**

Run: `mise x -- cargo nextest run -p balerix-runtime --lib the_git_profile_reads_only`
Expected: does not compile: `this function takes 4 arguments but 5 arguments were supplied`.

- [ ] **Step 3: Write the failing integration tests**

Append to `crates/balerix-runtime/tests/sandbox_it.rs`:

```rust
/// `tools` with `git` replaced by a shell script: `#!/bin/sh` and `body`.
fn git_shim(
    root: &std::path::Path,
    tools: &balerix_runtime::ToolPaths,
    name: &str,
    body: &str,
) -> balerix_runtime::ToolPaths {
    let shim = root.join(name);
    std::fs::write(&shim, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    balerix_runtime::ToolPaths {
        git: shim,
        ..tools.clone()
    }
}

/// Spec N amendment §12.1 (#109): the profile grants the directory git
/// names as its exec-path, resolved, wherever it is. The fixture's is a
/// symlink to a directory with a space in its name, outside every other
/// grant. A harvest through such a git would not prove the grant (git
/// falls back to the host `git` on `PATH` for its helpers), so the read
/// is asserted directly.
#[test]
fn the_git_profile_grants_gits_exec_path() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox-git-exec-path");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    for d in [&paths.workspace, &crew.cache_objects()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let real = root.join("git core");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("helper"), "helper-ok\n").unwrap();
    let link = root.join("exec-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let shimmed = git_shim(
        &root,
        &tools,
        "git-exec-path.sh",
        &format!(
            "exec {} --exec-path='{}' \"$@\"",
            tools.git.display(),
            link.display()
        ),
    );

    balerix_runtime::write_git_profile(&shimmed, "f/c/a", &paths, &crew).unwrap();
    let granted = std::fs::canonicalize(&real).unwrap();
    let profile = std::fs::read_to_string(paths.git_profile()).unwrap();
    assert!(
        profile.contains(&format!("\"{}\"", granted.display())),
        "the resolved exec-path is granted: {profile}"
    );
    assert!(
        !profile.contains("exec-link"),
        "Landlock binds to what the path resolves to: {profile}"
    );

    let out = Command::new(&tools.nono)
        .args([
            "-s",
            "run",
            "--profile",
            &paths.git_profile().display().to_string(),
            "--",
            "/bin/cat",
            &granted.join("helper").display().to_string(),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &paths.nono_home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "nono run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "helper-ok\n");
}

/// §12.1: a git that cannot name an existing exec-path directory fails the
/// step, and no profile is written without the grant (NS-5).
#[test]
fn a_git_that_cannot_name_its_exec_path_writes_no_profile() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    let root = support::temp_root("sandbox-git-no-exec-path");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());

    let refusing = git_shim(
        &root,
        &tools,
        "git-refusing.sh",
        "echo 'shim: no exec path' >&2\nexit 3",
    );
    let e = balerix_runtime::write_git_profile(&refusing, "f/c/a", &paths, &crew)
        .unwrap_err()
        .to_string();
    assert_eq!(e, "f/c/a: git --exec-path: shim: no exec path");
    assert!(!paths.git_profile().exists());

    let missing = root.join("missing");
    let lost = git_shim(
        &root,
        &tools,
        "git-lost.sh",
        &format!("echo '{}'", missing.display()),
    );
    let e = balerix_runtime::write_git_profile(&lost, "f/c/a", &paths, &crew)
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with(&format!("f/c/a: git --exec-path: {}: ", missing.display())),
        "{e}"
    );
    assert!(!paths.git_profile().exists());

    let file = root.join("a-file");
    std::fs::write(&file, "").unwrap();
    let wrong = git_shim(
        &root,
        &tools,
        "git-wrong.sh",
        &format!("echo '{}'", file.display()),
    );
    let e = balerix_runtime::write_git_profile(&wrong, "f/c/a", &paths, &crew)
        .unwrap_err()
        .to_string();
    assert!(e.ends_with(": not a directory"), "{e}");
    assert!(!paths.git_profile().exists());
}

/// §12.1: the query runs from an empty environment, as the sandboxed git
/// starts. A `GIT_EXEC_PATH` the daemon inherited would otherwise make the
/// grant name a directory the sandboxed git never uses. The workspace
/// forbids `set_var`, so the shim refuses when it sees `HOME`, which this
/// test process has.
#[test]
fn the_exec_path_query_starts_from_an_empty_environment() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("nono", false));
        return;
    };
    assert!(
        std::env::var_os("HOME").is_some(),
        "this test proves nothing without HOME in its own environment"
    );
    let root = support::temp_root("sandbox-git-exec-path-env");
    let layout = support::layout(&root);
    let id: AgentId = "f/c/a".parse().unwrap();
    let paths = layout.agent(&id);
    let crew = layout.crew(&id.crew_ref());
    for d in [&paths.workspace, &crew.cache_objects()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let strict = git_shim(
        &root,
        &tools,
        "git-strict-env.sh",
        &format!(
            "if [ -n \"${{HOME+x}}\" ]; then\n  echo 'shim: inherited environment' >&2\n  exit 3\nfi\nexec {} \"$@\"",
            tools.git.display()
        ),
    );
    balerix_runtime::write_git_profile(&strict, "f/c/a", &paths, &crew).unwrap();
    assert!(paths.git_profile().exists());
}
```

- [ ] **Step 4: Write the implementation**

In `crates/balerix-runtime/src/sandbox.rs`, add to the imports:

```rust
use crate::launch::outer_path;
```

Replace the last sentence of `render_git_profile`'s doc comment

```rust
/// every variable this list does not name. `git` comes from the host and
/// need not sit under `/usr`; granted as a single file, canonical since
/// Landlock rules bind to what the path resolves to.
pub fn render_git_profile(id: &AgentId, paths: &AgentPaths, crew: &CrewPaths, git: &Path) -> Value {
    let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
    read.push(std::fs::canonicalize(git).unwrap_or_else(|_| git.to_path_buf()));
```

with

```rust
/// every variable this list does not name. `git` comes from the host:
/// the binary is granted as a single file, canonical since Landlock rules
/// bind to what the path resolves to, and `exec_path` (`git_exec_path`)
/// as a directory, since `upload-pack` spawns `pack-objects` through it
/// (#109). Its libraries are not granted: a git that loads them from
/// outside `SYSTEM_READ` (nix, Linuxbrew), or a mise shim, cannot run
/// under this profile and the calls fail closed (Spec N amendment §12.1,
/// NS-6).
pub fn render_git_profile(
    id: &AgentId,
    paths: &AgentPaths,
    crew: &CrewPaths,
    git: &Path,
    exec_path: &Path,
) -> Value {
    let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
    read.push(std::fs::canonicalize(git).unwrap_or_else(|_| git.to_path_buf()));
    read.push(exec_path.to_path_buf());
```

Add, between `render_git_profile` and `write_git_profile`:

```rust
/// The directory the host git runs its helpers from, resolved. Asked of
/// git itself, unsandboxed: it names no repository and reads nothing the
/// agent wrote. From an empty environment but `PATH`, as the sandboxed
/// call starts: a `GIT_EXEC_PATH` the daemon inherited would make the
/// grant name a directory the sandboxed git never uses. A git that
/// cannot answer, or names something that is not a directory, fails the
/// step: no profile is written without the grant.
fn git_exec_path(tools: &ToolPaths, id: &str) -> Result<PathBuf, MaterializeError> {
    let tool_error = |stderr: String| MaterializeError::Tool {
        id: id.to_string(),
        tool: "git".into(),
        subcommand: "--exec-path".into(),
        args: vec!["--exec-path".into()],
        stderr,
    };
    let out = Cmd::new(&tools.git)
        .env_clear()
        .env("PATH", outer_path(tools))
        .args(["--exec-path"])
        .run()
        .map_err(|f| tool_error(f.stderr))?;
    let named = PathBuf::from(out.stdout.trim_end_matches(['\r', '\n']));
    let dir = std::fs::canonicalize(&named)
        .map_err(|e| tool_error(format!("{}: {e}", named.display())))?;
    if !dir.is_dir() {
        return Err(tool_error(format!("{}: not a directory", named.display())));
    }
    Ok(dir)
}
```

In `write_git_profile`, replace

```rust
    let profile = render_git_profile(&agent_id, paths, crew, &tools.git);
```

with

```rust
    let exec_path = git_exec_path(tools, id)?;
    let profile = render_git_profile(&agent_id, paths, crew, &tools.git, &exec_path);
```

The order in `write_git_profile` is then: parse the id, create the directories, ask for the exec-path, render, write, validate. The query sits before the write, so a git that cannot answer leaves no profile.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --lib the_git_profile_reads_only`
Expected: PASS.

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test sandbox_it`
Expected: PASS, including the three new tests and `the_git_profile_reads_the_clone_and_the_cache_and_writes_nothing`.

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test workspace_it`
Expected: PASS. The existing git shims (`failing_git`) answer `--exec-path` through the real git.

- [ ] **Step 6: Update the documents**

`AGENTS.md`, in the gotcha that begins "The clone step's git (`Workspace::agent_git` and the harvest's", replace

```
  The profile grants the system prefixes (`/usr`, `/lib`, `/lib64`,
  `/bin`) and the `git` binary as a single file, not its shared libraries
  or `libexec/git-core`, so a `git` installed elsewhere (nix, Linuxbrew,
  a mise shim) cannot run under it. That fails closed: `down` without
```

with

```
  The profile grants the system prefixes (`/usr`, `/lib`, `/lib64`,
  `/bin`), the `git` binary as a single file and the directory `git
  --exec-path` names (asked from an empty environment by
  `sandbox::git_exec_path`; a git that cannot answer fails the step),
  not its shared libraries, so a `git` that loads them from elsewhere
  (nix, Linuxbrew) or is a mise shim cannot run under it (#109, Spec N
  amendment NS-6). That fails closed: `down` without
```

`README.md`, replace

```
  config, needs no particular version). That `git` must live under
  `/usr`, `/lib`, `/lib64` or `/bin`: one installed elsewhere (nix,
  Linuxbrew, a mise shim) cannot run in the sandbox balerix's own git
  calls in an agent's clone use, so `down` (without `--purge`), `remove`
  and a branch change fail on such a host for now; `down --purge` is the
  way past.
```

with

```
  config, needs no particular version). That `git` must load its
  libraries from `/usr`, `/lib`, `/lib64` or `/bin`: the binary and its
  `--exec-path` directory may be anywhere, but a git that brings its own
  libraries (nix, Linuxbrew), or a mise shim, cannot run in the sandbox
  balerix's own git calls in an agent's clone use, so `down` (without
  `--purge`), `remove` and a branch change fail on such a host for now;
  `down --purge` is the way past.
```

`docs/THREAT-MODEL.md`, in the row "The daemon runs read-only `git` in a repository an agent can write to", replace

```
that reads the clone, the crew cache's objects and nothing else of balerix's, writes nothing and has no network.
```

with

```
that reads the clone, the crew cache's objects and nothing else of balerix's (plus the host git's binary and the directory `git --exec-path` names, asked of the host git from an empty environment, never of the clone), writes nothing and has no network.
```

and in the same row replace

```
`sandbox_it` (`the_git_profile_reads_the_clone_and_the_cache_and_writes_nothing`).
```

with

```
`sandbox_it` (`the_git_profile_reads_the_clone_and_the_cache_and_writes_nothing`, `the_git_profile_grants_gits_exec_path`).
```

- [ ] **Step 7: Run the gate and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add crates/balerix-runtime/src/sandbox.rs crates/balerix-runtime/tests/sandbox_it.rs AGENTS.md README.md docs/THREAT-MODEL.md
git commit -m "fix(runtime): grant git's exec-path in the git profile (#109)"
```

---

### Task 3: A sandbox failure is never git's "no"

**Files:**
- Modify: `crates/balerix-runtime/src/tools.rs:62-68` and `:334`
- Modify: `crates/balerix-runtime/src/workspace.rs:525-609` (`prepare_sandbox`, `agent_git`, new `is_gits_answer`) and its unit tests at `:1007`
- Test: `crates/balerix-runtime/tests/workspace_it.rs` (append; one assertion added to the existing test at `:2374`)
- Modify: `AGENTS.md`

**Interfaces:**
- Consumes: `Cmd::run_with_exit_codes(&self, accepted: &[i32]) -> Result<CmdOutput, CmdFailure>`.
- Produces:
  - `pub(crate) struct CmdOutput { pub stdout: String, pub stderr: String, pub code: i32 }`.
  - `fn is_gits_answer(out: &CmdOutput) -> bool` in `workspace.rs` (module-private).
  - `Workspace::agent_git` (signature unchanged) returns `MaterializeError::Tool { tool: "git", subcommand: <git's>, stderr: <what was printed> }` for an accepted non-zero exit with non-blank stderr.
  - A failed canary's error displays as `<id>: git version: the sandbox did not start; see <agent>/logs/nono-git.log: <first stderr line>`.

- [ ] **Step 1: Write the failing unit test**

In `crates/balerix-runtime/src/workspace.rs`, `mod tests`, change the imports to

```rust
    use crate::tools::{Cmd, CmdOutput};
    use crate::workspace::{
        CloneDecision, decide_clone, file_url, harden_agent_git, is_gits_answer, scrub_git_env,
    };
```

and add:

```rust
    /// Spec N amendment §12.2 (#109): git's yes/no probes are silent on
    /// exit 1; nono's own failure also exits 1 and prints `nono: …`.
    #[test]
    fn an_accepted_exit_is_gits_answer_only_when_silent() {
        let out = |code: i32, stderr: &str| CmdOutput {
            stdout: String::new(),
            stderr: stderr.to_string(),
            code,
        };
        assert!(is_gits_answer(&out(0, "")));
        assert!(
            is_gits_answer(&out(0, "warning: something\n")),
            "exit 0 is never inspected"
        );
        assert!(is_gits_answer(&out(1, "")));
        assert!(is_gits_answer(&out(1, " \n")), "blank is silent");
        assert!(!is_gits_answer(&out(
            1,
            "nono: Profile read error at /x: profile file not found\n"
        )));
        assert!(
            !is_gits_answer(&out(1, "warning: something\n")),
            "a git warning beside a no fails closed too"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `mise x -- cargo nextest run -p balerix-runtime --lib an_accepted_exit_is_gits_answer`
Expected: does not compile: `unresolved import … is_gits_answer` and `struct CmdOutput has no field named stderr`.

- [ ] **Step 3: Write the failing integration tests**

Append to `crates/balerix-runtime/tests/workspace_it.rs`:

```rust
/// `tools` with `nono` replaced by a shim that runs the real nono for
/// `profile validate` and for the canary (an argv ending in `version`),
/// and on every other call fails the way nono itself does: a `nono: …`
/// line on stderr and exit 1, which a yes/no probe accepts as an exit
/// code (#109).
fn nono_failing_after_the_canary(
    root: &Path,
    tools: &balerix_runtime::ToolPaths,
) -> balerix_runtime::ToolPaths {
    use std::os::unix::fs::PermissionsExt;

    let shim = root.join("nono-fails-after-canary.sh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nlast=\nfor a in \"$@\"; do\n  if [ \"$a\" = validate ]; then exec {nono} \"$@\"; fi\n  last=$a\ndone\nif [ \"$last\" = version ]; then exec {nono} \"$@\"; fi\necho 'nono: sandbox initialization failed' >&2\nexit 1\n",
            nono = tools.nono.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    balerix_runtime::ToolPaths {
        nono: shim,
        ..tools.clone()
    }
}

/// `tools` with `git` replaced by a shim that prints a warning on stderr
/// on any call whose argv holds `subcommand` as a word, then runs the real
/// git either way.
fn noisy_git(
    root: &Path,
    tools: &balerix_runtime::ToolPaths,
    subcommand: &str,
) -> balerix_runtime::ToolPaths {
    use std::os::unix::fs::PermissionsExt;

    let shim = root.join(format!("git-noisy-{subcommand}.sh"));
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = \"{subcommand}\" ]; then\n    echo 'warning: shim noise' >&2\n  fi\ndone\nexec {} \"$@\"\n",
            tools.git.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    balerix_runtime::ToolPaths {
        git: shim,
        ..tools.clone()
    }
}

/// A cache, and `f/c/a`'s clone on `balerix/f/c/a` holding one commit
/// origin lacks.
fn clone_with_unpushed_work(
    tools: &balerix_runtime::ToolPaths,
    root: &Path,
) -> (
    RepoRef,
    balerix_runtime::CrewPaths,
    balerix_runtime::AgentPaths,
) {
    let layout = support::layout(root);
    let repo = bare_repo(root);
    let id: balerix_core::AgentId = "f/c/a".parse().unwrap();
    let crew = layout.crew(&id.crew_ref());
    let paths = layout.agent(&id);
    let ws = Workspace {
        tools,
        gh_config_dir: None,
    };
    ws.ensure_repo("f/c", &crew, &repo, "main").unwrap();
    ws.ensure_clone("f/c/a", &crew, &paths, &repo, "balerix/f/c/a", "main")
        .unwrap();
    std::fs::write(paths.workspace.join("work.txt"), "unpushed\n").unwrap();
    git(&paths.workspace, &["add", "."]);
    git(&paths.workspace, &["commit", "-q", "-m", "agent work"]);
    (repo, crew, paths)
}

/// #109: nono proved it starts (the canary), then fails on a probe. Its
/// exit 1 is not "no promisor keys", "branch absent" or "detached": the
/// removal fails and the clone, with its unharvested commit, stays.
#[test]
fn a_nono_failure_after_the_canary_fails_the_removal_and_keeps_the_clone() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-nono-after-canary");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (_repo, crew, paths) = clone_with_unpushed_work(&tools, &root);

    let broken = nono_failing_after_the_canary(&root, &tools);
    let e = Workspace {
        tools: &broken,
        gh_config_dir: None,
    }
    .harvest_and_remove("f/c/a", &crew, &paths)
    .expect_err("a nono failure must not read as git's no")
    .to_string();
    assert_eq!(e, "f/c/a: git config: nono: sandbox initialization failed");
    assert!(
        paths.workspace.join("work.txt").exists(),
        "a failed removal deletes nothing"
    );
    assert!(!git_ok(
        &crew.repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/balerix/f/c/a"
        ]
    ));
}

/// #109: the same failure on a branch change is not a detached HEAD to
/// reuse; the pass fails and the clone is left as it was.
#[test]
fn a_nono_failure_after_the_canary_fails_a_branch_change() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-nono-after-canary-change");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (repo, crew, paths) = clone_with_unpushed_work(&tools, &root);
    push_branch(&root, "feature/issue-109");

    let broken = nono_failing_after_the_canary(&root, &tools);
    let e = Workspace {
        tools: &broken,
        gh_config_dir: None,
    }
    .ensure_clone(
        "f/c/a",
        &crew,
        &paths,
        &repo,
        "feature/issue-109",
        "feature/issue-109",
    )
    .expect_err("a nono failure must not read as a detached HEAD")
    .to_string();
    assert_eq!(e, "f/c/a: git config: nono: sandbox initialization failed");
    assert_eq!(
        git(&paths.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "balerix/f/c/a",
        "the clone is left as it was"
    );
    assert_eq!(
        std::fs::read_to_string(paths.branch_marker()).unwrap().trim(),
        "balerix/f/c/a"
    );
}

/// §12.2: exit 0 is never inspected. A git that warns on a probe that
/// answers "yes" still harvests.
#[test]
fn a_git_warning_on_a_successful_probe_does_not_fail_the_harvest() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-noisy-yes");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (_repo, crew, paths) = clone_with_unpushed_work(&tools, &root);

    // the harvest's `rev-parse --verify` answers "yes" (exit 0) here
    let noisy = noisy_git(&root, &tools, "rev-parse");
    Workspace {
        tools: &noisy,
        gh_config_dir: None,
    }
    .harvest_and_remove("f/c/a", &crew, &paths)
    .unwrap();
    assert!(!paths.workspace.exists());
    assert!(git_ok(
        &crew.repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/balerix/f/c/a"
        ]
    ));
}

/// §12.2: the rule fails closed both ways. A warning beside a real "no"
/// (the promisor probe finds no key and exits 1) fails the step, as any
/// git error does (#74); nothing is deleted.
#[test]
fn a_git_warning_beside_a_no_fails_the_removal_and_keeps_the_clone() {
    let Some(tools) = support::tools() else {
        assert!(!support::require_or_skip("git", false));
        return;
    };
    let root = support::temp_root("workspace-noisy-no");
    if !support::require_or_skip("landlock", support::landlock_works(&tools, &root)) {
        return;
    }
    let (_repo, crew, paths) = clone_with_unpushed_work(&tools, &root);

    let noisy = noisy_git(&root, &tools, "config");
    let e = Workspace {
        tools: &noisy,
        gh_config_dir: None,
    }
    .harvest_and_remove("f/c/a", &crew, &paths)
    .unwrap_err()
    .to_string();
    assert_eq!(e, "f/c/a: git config: warning: shim noise");
    assert!(paths.workspace.join("work.txt").exists());
}
```

In the existing `a_nono_that_cannot_run_fails_the_removal_and_keeps_the_clone`, after

```rust
    assert!(e.starts_with("f/c/a: git "), "{e}");
```

add

```rust
    assert!(
        e.starts_with("f/c/a: git version: the sandbox did not start; see ")
            && e.contains("/logs/nono-git.log: "),
        "{e}"
    );
```

- [ ] **Step 4: Run them to verify they fail**

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test workspace_it nono_failure_after_the_canary git_warning a_nono_that_cannot_run`
Expected (the lib test from Step 1 lives in another target, so this compiles): FAIL on four tests.
- `a_nono_failure_after_the_canary_fails_the_removal_and_keeps_the_clone`: panics with `a nono failure must not read as git's no` (the removal returned `Ok` and deleted the clone).
- `a_nono_failure_after_the_canary_fails_a_branch_change`: panics with `a nono failure must not read as a detached HEAD`.
- `a_git_warning_beside_a_no_fails_the_removal_and_keeps_the_clone`: panics on `unwrap_err`.
- `a_nono_that_cannot_run_fails_the_removal_and_keeps_the_clone`: fails the new hint assertion.

`a_git_warning_on_a_successful_probe_does_not_fail_the_harvest` passes already; it guards the rule against going too far.

- [ ] **Step 5: Write the implementation**

`crates/balerix-runtime/src/tools.rs`: replace

```rust
#[derive(Debug)]
pub(crate) struct CmdOutput {
    pub stdout: String,
```

with

```rust
#[derive(Debug)]
pub(crate) struct CmdOutput {
    pub stdout: String,
    /// What the child printed on stderr. A caller that accepts a non-zero
    /// exit as an answer reads it to tell the tool's silent "no" from a
    /// wrapper's failure with the same code (`Workspace::agent_git`).
    pub stderr: String,
```

and at the end of `exec` replace

```rust
        Ok(CmdOutput { stdout, code })
```

with

```rust
        Ok(CmdOutput {
            stdout,
            stderr,
            code,
        })
```

`crates/balerix-runtime/src/workspace.rs`: add above `impl`'s `prepare_sandbox` doc, as a free function next to `remove_tree`:

```rust
/// Whether an exit code `agent_git` accepted is git's own answer. The
/// yes/no probes (`config --get-regexp`, `rev-parse --verify --quiet`,
/// `symbolic-ref -q`) print nothing when they exit 1. nono's own failure
/// also exits 1, after the canary as well as before it, and prints
/// `nono: …`; read as "no", that skips the harvest and the clone is
/// deleted unharvested (#109). So a non-zero exit that printed anything
/// is a failure. That fails closed for a git warning beside a real "no"
/// too, as any git error does (#74). Exit 0 is not inspected.
fn is_gits_answer(out: &CmdOutput) -> bool {
    out.code == 0 || out.stderr.trim().is_empty()
}
```

Replace `prepare_sandbox`'s doc comment and body

```rust
    /// Writes the git profile and proves the sandbox starts under it,
    /// before any probe's exit code is trusted. The yes/no probes after
    /// this (`config --get-regexp`, `rev-parse --verify --quiet`) accept
    /// exit 1 as git's answer, and nono's own failure to run also exits 1;
    /// a `version` that must exit 0 tells the two apart, so a nono that
    /// validates but cannot run fails the step instead of reading as "no
    /// promisor keys" or "branch absent".
    fn prepare_sandbox(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<(), MaterializeError> {
        write_git_profile(self.tools, id, agent, crew)?;
        self.agent_git(id, crew, agent, &["version"], &[0])
            .map(|_| ())
    }
```

with

```rust
    /// Writes the git profile and proves the sandbox starts under it,
    /// before any probe's exit code is trusted. The yes/no probes after
    /// this (`config --get-regexp`, `rev-parse --verify --quiet`) accept
    /// exit 1 as git's answer, and nono's own failure to run also exits 1;
    /// a `version` that must exit 0 tells the two apart for a nono that
    /// validates but cannot run at all, and names the log to read. A
    /// failure on a later call is caught by `is_gits_answer`.
    fn prepare_sandbox(
        &self,
        id: &str,
        crew: &CrewPaths,
        agent: &AgentPaths,
    ) -> Result<(), MaterializeError> {
        write_git_profile(self.tools, id, agent, crew)?;
        self.agent_git(id, crew, agent, &["version"], &[0])
            .map(|_| ())
            .map_err(|e| match e {
                // a prefix, not a line: the error displays the first line
                MaterializeError::Tool {
                    id,
                    tool,
                    subcommand,
                    args,
                    stderr,
                } => MaterializeError::Tool {
                    id,
                    tool,
                    subcommand,
                    args,
                    stderr: format!(
                        "the sandbox did not start; see {}: {stderr}",
                        agent.logs.join("nono-git.log").display()
                    ),
                },
                other => other,
            })
    }
```

In `agent_git`, replace the tail

```rust
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

with

```rust
        let tool_error = |argv: Vec<String>, stderr: String| MaterializeError::Tool {
            id: id.to_string(),
            tool: "git".into(),
            subcommand: args.first().copied().unwrap_or_default().to_string(),
            args: argv,
            stderr,
        };
        let out = cmd
            .run_with_exit_codes(accepted)
            .map_err(|f| tool_error(f.args, f.stderr))?;
        if !is_gits_answer(&out) {
            let argv = args.iter().map(|a| (*a).to_string()).collect();
            return Err(tool_error(argv, out.stderr));
        }
        Ok(out.stdout)
    }
```

and append to `agent_git`'s doc comment, after "A failure is reported as git's, with git's subcommand, not nono's.":

```rust
    /// An accepted non-zero exit that printed anything on stderr is a
    /// failure too (`is_gits_answer`).
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `mise x -- cargo nextest run -p balerix-runtime --lib an_accepted_exit_is_gits_answer`
Expected: PASS.

Run: `BALERIX_REQUIRE_TOOLS=1 mise x -- cargo nextest run -p balerix-runtime --test workspace_it`
Expected: PASS, all of it. The existing cases that rest on a silent exit 1 (`removal_harvests_head_without_a_marker_and_skips_what_is_not_there`, `a_changed_branch_moves_a_clean_clone_and_keeps_the_old_branch`, `an_agent_switching_branches_itself_is_left_alone_on_an_unchanged_setting`) passing is the proof that git's own "no" is still a "no".

- [ ] **Step 7: Update `AGENTS.md`**

In the same gotcha as Task 2, replace

```
  and the clone is deleted unharvested
  (`a_nono_that_cannot_run_fails_the_removal_and_keeps_the_clone`).
```

with

```
  and the clone is deleted unharvested
  (`a_nono_that_cannot_run_fails_the_removal_and_keeps_the_clone`).
  The canary covers a nono that cannot run at all; for one that fails
  later, `agent_git` treats an accepted non-zero exit that printed
  anything on stderr as a failure (`is_gits_answer`, #109): git's three
  probes are silent on a "no", nono prints `nono: …`. A git warning
  beside a real "no" fails the step too, on purpose.
```

- [ ] **Step 8: Run the gate and commit**

Run: `mise run check`
Expected: PASS.

```bash
git add crates/balerix-runtime/src/tools.rs crates/balerix-runtime/src/workspace.rs crates/balerix-runtime/tests/workspace_it.rs AGENTS.md
git commit -m "fix(runtime): a sandbox failure after the canary is never git's \"no\" (#109)"
```

---

### Task 4: Full verification, follow-up issue, pull request

**Files:**
- No source changes expected. Fixes found here go back to the task that owns the code.

**Interfaces:**
- Consumes: the three commits above.
- Produces: a PR that closes #109, and the follow-up issue NS-6 names.

- [ ] **Step 1: Integration and end-to-end tiers**

Run: `mise run test-it`
Expected: PASS, nothing skipped.

Run: `mise run e2e`
Expected: PASS.

- [ ] **Step 2: The claude bump's checks**

claude changed from 2.1.286 to 2.1.287, and the embedded tool table hands that to agents. Both checks need a logged-in `claude` and are driven from a tmux session (memory: `verify-claude-drivable-from-tmux`; expect the start-up Enter delay of `claude-startup-enter-timing`).

Run: `mise run verify-claude`
Expected: every section passes, including G (`/exit` ends the session) and H (Enters before `UserPromptSubmit`). A first-start dialog that appears means a `.claude.json` key in `home.rs::render_claude_json` was renamed or re-armed: stop and report it; that is its own change.

Run: `mise run verify-questions`
Expected: every recorded answer matches the plan `plugins/common/src/question.rs::plan` produced.

Record both outcomes, with the claude version printed, for the PR body.

- [ ] **Step 3: Open the follow-up issue**

```bash
gh issue create \
  --title "Git profile: a daemon-level setting for read-only library prefixes" \
  --label enhancement \
  --body "Split from #109 (Spec N amendment 2026-10-01 §12.1, NS-6).

The git profile grants the system prefixes, the git binary and its \`--exec-path\` directory. A git that loads its libraries from its own prefix (nix, Linuxbrew), or a mise shim, still cannot run under it: removal, \`down\` without \`--purge\` and a branch change fail closed on such a host. Not reproduced on a real host.

- [ ] Reproduce on a nix or Linuxbrew git.
- [ ] A daemon-level setting (not the agent's \`sandbox\` block) naming read-only prefixes for the git profile, with a threat-model clause.
- [ ] A test with a git whose libraries sit outside the system prefixes."
```

Expected: prints the new issue's URL. Note its number.

- [ ] **Step 4: Push and open the pull request**

```bash
git push -u origin fix-109-git-profile-limits
gh pr create \
  --title "fix(runtime): grant git's exec-path in the git profile; a sandbox failure is never git's \"no\" (#109)" \
  --body "Closes #109. Spec: \`docs/superpowers/specs/2026-10-01-balerix-n-sandboxed-daemon-git-design.md\` §12.

- The git profile grants the canonical \`git --exec-path\` directory, asked of the host git from an empty environment; no profile is written without it.
- An accepted non-zero exit that printed on stderr is a failure in \`agent_git\`, so a nono failure after the canary no longer reads as \"no\" and deletes a clone unharvested. A failed canary names \`logs/nono-git.log\`.
- Library prefixes are not granted (NS-6); follow-up: #<issue from step 3>.
- Tool bumps: rust 1.99.0, claude 2.1.287, trivy 0.75.0. \`verify-claude\` and \`verify-questions\` on claude 2.1.287: <outcomes from step 2>.

Tests: \`sandbox_it\` (exec-path grant, refusal, empty environment), \`workspace_it\` (nono failing after the canary at removal and at a branch change; a git warning on a yes and beside a no)."
```

Replace the two `<…>` with the issue number and the recorded outcomes before running. Expected: prints the PR URL; CI starts.

- [ ] **Step 5: Watch CI**

Run: `gh pr checks --watch`
Expected: all green. `pr-title.yml` accepts the title; the trivy job runs on 0.75.0.
