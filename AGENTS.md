# Balerix — agent instructions

Read `ARCHITECTURE.md` first. The design is in
`docs/superpowers/specs/2026-09-05-balerix-architecture-design.md`; the threat
model in `docs/THREAT-MODEL.md` — load it before touching anything that handles
credentials, hook input, or sandbox rules.

## Tasks (`mise run <task>`)
- `check` — lint + test; run before every commit.
- `test-it` — the `balerix-runtime` integration tests against real
  git/mise/nono/tmux, with `BALERIX_REQUIRE_TOOLS=1` so a missing tool fails
  instead of skipping.
- `plugin <name>` — lint and test one standalone plugin project
  (`mise run plugin matrix`). `plugins` does all five (common is the shared
  library, Spec K). Neither is part of `check`; CI runs them as their own
  concurrent jobs. web's review-page test runs the page's script under the
  pinned `node`, skipping without it unless `BALERIX_REQUIRE_TOOLS=1` (CI).
- `agent` — lint and test the standalone `agent/` project (`balerix-agent`,
  Spec O §12); builds `balerix` first, since its two-process tests run
  `balerix serve --mode kubernetes` and `launch.sh`. Its own CI job; not part
  of `check`.
- `operator` — lint and test the standalone `operator/` project
  (`balerix-operator`, Spec O §12); builds `balerix` first, since its
  client test runs `balerix serve --mode kubernetes`. Fails when
  `charts/balerix-operator/templates/crds/` differs from the Rust types. The controller tests run
  on the envtest binaries the task pins (a `kube-apiserver` and `etcd`, no
  cluster, no kubelet: the test stands in for it). `scripts/operator.sh
  check` leaves the journey out (`-E 'not binary(e2e_k8s)'`);
  `scripts/operator.sh e2e` (the `e2e-k8s` task) runs it alone.
  `operator/.config/nextest.toml` holds the `envtest` test group (four API
  servers at once) and the `e2e-k8s` profile. Its own CI job; not part of
  `check`.
- `crds` — regenerates `charts/balerix-operator/templates/crds/` from `operator/src/api/`.
- `charts` — `helm lint` both charts under `charts/`, then
  `operator/tests/charts_it.rs`: renderings asserted, a server-side dry run
  of each against the envtest API server, and the controllers run as the
  operator chart's service account (impersonated), so a verb its RBAC
  lacks fails. Its own CI job; not part of `check` or `operator`.
- `mutants` — nightly tier: mutation-tests `balerix-core` (the reconciler).
  `.cargo/mutants.toml` excludes `fakes.rs`: the fakes are exercised by
  `balerix-server`'s tests, which that run never executes.
- `e2e` — the Phase 3 journey against a real daemon; needs the same tools as `test-it`.
- `kind-up` — a kind cluster `balerix-e2e` for `e2e-k8s`: the shared
  local-path class (§19.3), and the daemon, agent, operator and plugin
  images built from this tree and loaded (`balerix:e2e`,
  `balerix-agent:e2e`, `balerix-operator:e2e`, …); no CRDs. Needs
  docker; `mise run kind-up -- down` deletes it. `mise run kind-up cluster`
  makes the cluster and class only, no images (what the charts release
  check uses). This host has no docker: CI only.
- `e2e-k8s` — the Phase 3 and plugin journeys on that cluster
  (`operator/tests/e2e_k8s.rs`): `scripts/operator.sh e2e` installs the
  operator chart (with `--take-ownership`, so definitions an earlier `kubectl apply` made are adopted; the CRDs, and the operator in the cluster under its own
  RBAC); each journey installs the daemon chart; `dev fake-claude` in the pods. Fails, not skips, without the cluster. Its own CI job,
  path-filtered on pull requests.
- `package-plugins [names…]` — builds the named in-tree plugins inside
  their own projects and assembles each as a directory source under
  `target/plugins/<name>/` (under `CARGO_TARGET_DIR` when set). No names
  means all four. `test` and `e2e` ask it for `flow web`, the two the
  journey needs; the flow e2e skips (fails under `BALERIX_REQUIRE_TOOLS`)
  without it.
- `serve` — a foreground daemon under `target/tmp/serve` for poking by hand
  (`HOME` is overridden, so it never touches your real state).
- `verify-claude` — the interactive spec §8.1 check with a real `claude`
  (`scripts/verify-claude.sh`); `BALERIX_VERIFY_FAKE=1` self-tests it with
  `dev fake-claude`. Section G sends `/exit` the way `send_text`
  does and reports whether the session ended (#41); section H adds a
  second agent, sends a text the moment its `SessionStart` arrives and
  reports how many Enters it took before `UserPromptSubmit` (#99). Its data root (`target/tmp/verify-data`, the daemon
  pool) is kept across runs so claude downloads once per version; config and
  state under `target/tmp/verify-claude` are wiped.
- `verify-matrix` — Spec G's manual check against a real Matrix homeserver
  (`scripts/verify-matrix.sh`); needs `MATRIX_HOMESERVER`, `MATRIX_USER_ID`,
  `MATRIX_PASSWORD` and `MATRIX_INVITE`. Not part of any CI tier.
- `verify-matrix-local` — the same check with no real homeserver: starts a
  pinned tuwunel (a task-level mise tool) under `target/tmp/verify-matrix-local`,
  registers a bot and an operator, pins an unencrypted room so reactions are
  readable over the client API (`scripts/verify-matrix-room.sh`), then runs
  `verify-matrix.sh`, a daemon and a two-agent fleet the way `verify-claude`
  does. `mise run verify-matrix-local -- down` stops it. Needs a logged-in
  `claude`. Not part of any CI tier.
- `verify-questions` — Spec J's manual check (`scripts/verify-questions.sh`):
  the real pinned `claude` in tmux, answered with the key plans common's
  `question.rs` (`plugins/common`) produces, the recorded answers read
  back from the `PostToolUse` hook. Needs a logged-in `claude`. Not part
  of any CI tier.
- `verify-github` — Spec M's manual check against a real GitHub App on a
  scratch repository (`scripts/verify-github.sh`); needs `GITHUB_APP_ID`,
  `GITHUB_APP_KEY`, `GITHUB_WEBHOOK_SECRET`, `GITHUB_REPO` and a listener
  GitHub can reach. Not part of any CI tier.
- `verify-k8s` — Spec O §24.6's manual check with the real `claude` on the
  cluster `KUBECONFIG` names (`scripts/verify-k8s.sh`): both charts from
  the tree (`-- --from tree`, the default) or the published index
  (`-- --from index`, with `BALERIX_VERIFY_OWNER` for a fork), a
  payments-shaped Fleet on pod runners, the e2e journey's checks; prints a
  report. Needs a ReadWriteMany class (`BALERIX_VERIFY_SHARED_CLASS`) and a
  logged-in `claude`; `-- down` removes it. Not part of any CI tier.
- `lint`, `test`, `fmt`, `precommit`, `audit` — defined in `mise.toml`.
- `vendor-xterm` is a script, not a task: `scripts/vendor-xterm.sh` re-fetches
  and verifies the web plugin's assets against
  `plugins/web/assets/VENDOR.md` (installing them only when
  every digest matches); `--check` verifies the committed files offline.
- `release-prepare <unit> [version]` — what `release-pr.yml` runs: works
  out one release unit's next version (core, common, flow, web, matrix,
  github, charts) and writes it into the manifests, lockfiles, plugin manifest and
  changelog. Commits nothing. `docs/RELEASING.md` is the release process.
- `release-test` — scenario tests for `scripts/release/` against throwaway
  clones under `target/tmp`; CI runs it when the scripts change. It covers
  the charts unit's gates (`appVersion`, plugin tags, a pin-only change,
  a core minor), staging and the fork rewrite.

## Conventions
- Ports (`Materializer`, `AgentRunner`, `Clock`, `FleetStore`, `EventHandler`)
  live in `balerix-core`; adapter crates implement them and never depend on
  each other. Only the `balerix` binary wires adapters to ports;
  `balerix-server` receives `Ports` and never imports `balerix-runtime`.
  `balerix-plugin-sdk` depends on `balerix-api` only. `balerix-server`'s
  *dev*-dependencies may include `balerix-plugin-sdk` (in-process plugin
  tests). Plugin crates (`balerix-plugin-flow`) depend on `balerix-plugin-sdk`
  and `balerix-api` only. Plugins are not workspace members: each is a
  standalone project under `plugins/<name>/` with its own `Cargo.lock`,
  dependency table, lints, `clippy.toml` and `deny.toml`, reaching the SDK by
  relative path. That is what keeps a plugin's dependency tree out of the
  daemon's feature resolution.
- Library crates return `thiserror` errors whose messages start with the config
  path (`crews.backend.agents.bob.tools.node: …`); only the binary uses `anyhow`.
- Every tool version — `mise.toml` and fleet `tools:` — is exact.
- Types holding secrets hand-implement `Debug` and print `<redacted>`. Secrets
  never go in argv, env, or logs; `config resolve` withholds the credential
  bundle but prints the host `settings.json` verbatim unless
  `--no-host-defaults` is given.
- New Cargo dependencies are a deliberate decision, and they land in the
  project that uses them: a core dependency in the root
  `[workspace.dependencies]`, a plugin dependency in that plugin's own
  manifest. Exact version either way, and say why in the commit.
- An agent's `launch.sh` starts under `balerix agent-supervise` (Spec N
  amendment §13). `balerix-runtime`'s test support fills the `balerix`
  tool slot with the test executable, so a test there must not execute a
  rendered `launch.sh`; tests that need the wrapper live in
  `crates/balerix/tests/cli_supervise.rs`. Never call
  `balerix_runtime::supervise::supervise` inside a test process: it makes
  the process a subreaper and kills every descendant.
- `agent/` is standalone like a plugin (own `Cargo.lock`, `deny.toml`,
  lints), depending on `balerix-api`, `balerix-core` and `balerix-runtime` by
  path; its TLS and WebSocket stack never reaches the core resolution.
- `operator/` is standalone like `agent/`, depending on `balerix-api`,
  `balerix-core` and `balerix-config` by path; `kube` and `k8s-openapi`
  never reach the core workspace or `agent/`, which is why a `pod`
  runner's Kubernetes shapes are opaque JSON in `balerix-api`.
  `operator/src/desired/` is pure: no clock, no random value, no I/O.
  Tokens and certificates are made by `pki` and passed in.
- The controllers (`operator/src/controllers/`) are the dumb executor:
  observe, call `desired`, apply with server-side apply under the field
  manager `balerix-operator`, patch status. Every Job goes through
  `controllers::jobs::ensure_job`.

## Gotchas
- A `desired` function writes its manifest as JSON and returns the
  `k8s-openapi` type (`common::typed`). Those types ignore unknown
  fields, so a misspelt key is dropped without an error: the insta
  snapshot is the check. Read a `.snap.new` for what is missing, not
  only for what is there.
- `ResolveOptions::runner` says which runner a resolution is for; the
  CLI's is `Tmux`, the operator's `Pod`, and an agent of the other kind
  fails with its config path. A Kubernetes-mode daemon does not check
  the runner; a tmux-mode daemon refuses `pod`.
- The Jobs (`balerix-agent crew-sync`, `pool-sync`, `harvest`) see the
  shared slice at the pod's paths (`SharedSlice`, `/balerix/shared`):
  `check_clone` compares a clone's `alternates` with the cache's path,
  so a harvest that saw the cache anywhere else would refuse every
  clone. `harvest` writes nothing under the claim; its git profile,
  nono's home and its logs are in the scratch directory, which is why
  `AgentPaths::git_profile` is a field.
- In a pod `CrewPaths::logs` is on the agent claim
  (`.balerix/state/crew-logs`): the crew root is a read-only mount.
  Never write `crew.root.join("logs")`.
- The cache directory can be a mount point (a sync Job): it can be
  emptied, never removed. `discard_half_made` accepts an emptied
  directory for that reason.
- Envtest has an API server and etcd and nothing else: no scheduler, no
  kubelet, no controller-manager. An unscheduled Pod is deleted at once, but
  Jobs never run and claims never go. The tests play the missing parts:
  `support::finish_job` writes a Job's stand-in pod (its termination
  message first, then the Job status, since the Job patch wakes the
  operator), `support::reap_pod` deletes a Pod by uid,
  `support::reap_claim` removes a claim's `pvc-protection` finalizer, and
  `support::reap_job` the `foregroundDeletion` finalizer of a stale Job
  `ensure_job` deleted (there is no garbage collector either).
- Run cargo through mise (`mise x -- cargo …`) or via a `mise run` task.
- `mise run check` covers the core workspace only. Cargo unifies features
  across every member one invocation selects, so while the plugins were
  members, a `--workspace` build handed `balerix-server` a `reqwest` with
  rustls, HTTP/2, gzip and stream that it never asked for, and tripled the
  gate's cold build. `scripts/check-core-deps.sh` fails if that ever comes
  back: it asserts both the seven-crate member list and that the core
  `reqwest` carries exactly `__rustls,__tls,json,rustls-no-provider` (TLS on
  one authority, no webpki or native roots, Spec O §23.1), so a readmitted
  plugin is caught whether or not its tree pulls a `reqwest` feature. A plugin change is caught by
  `mise run plugins`, not by `check`.
- insta snapshots: read the `.snap.new`, compare against the plan's expected
  values, then `mise x -- cargo insta accept`. Never blind-accept.
- Edition 2024 makes `std::env::set_var` unsafe and the workspace forbids
  `unsafe`; inject environment through parameters (see `HostPaths::from_env`).
- The merge is a left fold — don't "fix" its non-associativity.
- `balerix-runtime` integration tests skip with a printed reason when a tool or
  Landlock is missing; `mise run test-it` (and CI) sets `BALERIX_REQUIRE_TOOLS=1`
  so they fail instead. Their temp roots live under `target/tmp`, never `/tmp`
  (nono grants `/tmp` by default, which would make escape assertions vacuous).
- The embedded default tool table is `include_str!("../../../mise.toml")` in
  `balerix-runtime/src/toolchain.rs`; bumping `claude` or `gh` in `mise.toml`
  changes what agents get. Bump `claude` and run `mise run verify-claude`:
  every first-start dialog it suppresses (`render_claude_json` in
  `home.rs`: onboarding, folder trust, the auto-mode default offer) is an
  undocumented `.claude.json` key that a new version can rename or re-arm,
  and `dev fake-claude` models none of them, so the e2e cannot catch it.
- The matrix plugin answers `AskUserQuestion` by counting rows and pressing
  Down and Enter (`plugins/common/src/question.rs::plan`). That encodes
  Claude Code's dialog layout, which no API promises. The property test
  there proves `plan` against a *model* of the dialog; only
  `mise run verify-questions` proves the model. Bump `claude` in `mise.toml`
  and you run it. Keys sent with no pause are dropped at a question
  transition, which is why `send_keys` has a 20 ms floor; and never use Tab
  in a plan: it goes to different places from different rows.
- `SessionStart` is not "ready for keys": Claude Code's TUI reads the
  keyboard a second or more after the hook fires, so a text sent on
  `SessionStart` lands in the composer and its Enter is lost (#99).
  `send_text` does not wait; the plugins that track deliveries (github,
  matrix) press Enter again every 5 s, for up to 90 s, while a prompt is
  unconfirmed and the agent is not mid-turn
  (`plugins/common/src/delivery.rs::nudge`). How long Claude swallows
  Enter varies from 1 s to over 20 s between cold starts, so github
  reports its first prompt only after `confirmWindow` plus 60 s
  (`START_UP_ALLOWANCE`).
  Flow tracks nothing, so a flow `send` on `SessionStart` is still lost.
  `dev fake-claude` reads stdin from the start and cannot show any of
  this; `mise run verify-claude` section H does.
- nono's state root follows nono's own `$HOME`; never point that at the agent's
  `home/` (see ARCHITECTURE.md).
- The sandbox grants read on exactly two binaries outside `/usr`, `/bin`,
  `/lib`: this `balerix` (the relay) and the discovered `mise` (`launch.sh`
  execs it). GitHub's mise-action installs `mise` under `$HOME`, so without
  that grant every agent in CI died with exit 127 while local runs, where
  `mise` sits in `/usr/local/bin`, were fine.
- Never put the daemon port in the profile's `network.connect_port`: on
  Landlock that list is an outbound allowlist and the agent loses DNS and the
  API. The daemon port is an `open_port` (localhost only), and claude's temp
  dir is `home/tmp` via `TMPDIR`/`CLAUDE_CODE_TMPDIR` — nothing under `/tmp`
  is granted. Both were found by `mise run verify-claude` against the real
  `claude`, not by the e2e (the fake needs neither).
- tmux answers `has-session -t =<crew>` with "no current target" while its
  server is up with no session at all — the moment another crew's
  `new-session` on the same socket is starting it. `TmuxRunner` reads that
  as absent like "can't find session"; before it did, every daemon start
  with two crews (verify-claude's plugins fleet plus the fleet under test)
  failed its first pass and waited out the 30 s resync before `up` could
  proceed. The actor logs `agent ready (SessionStart received)`, the only
  line that dates readiness; the tick after it logs the phase.
- `Workspace::git` (`crates/balerix-runtime/src/workspace.rs`) (`scrub_git_env`)
  scrubs `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`/`GIT_PREFIX`/`GIT_COMMON_DIR`
  from every git call; a call in an agent's clone runs under the git
  profile instead (`workspace::sandboxed_git`, next item). The integration-test `git`
  fixtures do the same — the pre-commit hook exports them, and a git
  subprocess that inherits them operates on this repository instead of the
  test's.
- Every daemon git call in an agent's existing clone — the clone step's
  (`Workspace::agent_git`, the harvest's `upload-pack`) and the workspace
  reader's (`inspect.rs`, #108) — goes through `workspace::sandboxed_git`
  and runs inside `nono run --profile nono-git-profile.json` from an empty
  environment. Its hardening variables live in that profile's `set_vars`
  (`sandbox::render_git_profile`), not on the command: nono drops every
  other variable. Each call runs `--no-audit` with `HOME` a fresh
  `.nono-git-*` directory beside the agent's `nono/`, removed when the
  call ends (`workspace::ScratchHome`): nono writes a session record per
  `run` under `$HOME`, and the agent's own `nono/` holds its live
  session's, so those could never be swept there. Each call is a sandbox start (about 55 ms idle, far
  more on a loaded host), so the reader's `version` is one call (#174: its
  four git commands run in one `/bin/sh` under the profile,
  `workspace::sandboxed_git_script`; the script is a constant and every
  value, git's path and the base ref included, is a positional argument)
  and its `diff` one combined `git diff -U3` split per file
  (`split_patch`), plus one `--no-index` per untracked file; keep it that
  way. Tests that run git in an existing
  clone need Landlock and gate on `support::landlock_works`. Go through
  `Workspace::prepare_sandbox` before the first sandboxed call: it writes
  the profile, then runs one `git version` that must exit 0. The yes/no
  probes accept exit 1 as git's "no", and nono's own start-up failure also
  exits 1, so without it a nono that cannot run reads as "branch absent"
  and the clone is deleted unharvested
  (`a_nono_that_cannot_run_fails_the_removal_and_keeps_the_clone`).
  The canary covers a nono that cannot run at all; for one that fails
  later, `agent_git` treats an accepted non-zero exit that printed
  anything on stderr as a failure (`is_gits_answer`, #109): git's three
  probes are silent on a "no", nono prints `nono: …`. A git warning
  beside a real "no" fails the step too, on purpose.
  The profile grants the system prefixes (`/usr`, `/lib`, `/lib64`,
  `/bin`), the `git` binary as a single file and the directory `git
  --exec-path` names (asked from an empty environment by
  `sandbox::git_exec_path`; a git that cannot answer fails the step),
  not its shared libraries, so a `git` that loads them from elsewhere
  (nix, Linuxbrew) or is a mise shim cannot run under it (#109, Spec N
  amendment NS-6) unless the daemon's `config.toml` names the prefix in
  `[sandbox] git_read` (#111; `Runtime::with_git_read`, validated in
  `serve.rs`; pods get none). When the canary's `git version` fails,
  `/bin/true` under the same profile tells "git could not run" (the
  error names `sandbox.git_read`) from "the sandbox did not start". That fails closed: `down` without
  `--purge`, `remove` and a branch change fail on such a host, `--purge`
  is the way past, and the user `sandbox` block does not reach this
  profile.
- Unix sockets in a sandbox (`socket_policy.rs`, `seccomp.rs`; `[sandbox]
  unix_sockets`, default `auto`, resolved once in `serve.rs::socket_policy`):
  `linux.af_unix_mediation` is balerix-owned and refused in every `sandbox`
  block, and `SocketPolicy` is applied to a profile after the fleet or
  manifest block is merged (`apply_to_profile`). `Mediate` is nono's
  pathname mediation, which needs ptrace, so on a host where it does not
  work (`kernel.yama.ptrace_scope = 2`, restricted containers) `auto`
  resolves to `Deny`; the probe (10 s deadline) passes only if its JSON line
  is exactly `{"tcp":"ok","inside":"ok","outside":"refused"}` (its profile
  cannot set `environment.set_vars`: nono reserves `PATH`). `Deny` runs
  every nono command (`launch.sh`, the daemon's sandboxed git, upload-pack,
  the sandbox start check) through `balerix sandbox-exec`, which installs
  two stacked seccomp filters (`socket(AF_UNIX)` -> `EAFNOSUPPORT`;
  io_uring -> `ENOSYS`). The pod sidecar always resolves `auto` (no bundle
  field: `AgentBundle` is in published `balerix-api` with
  `deny_unknown_fields`). Under `Deny`, `serve` and the sidecar both
  run `balerix sandbox-exec` once at start-up
  (`socket_policy::check_sandbox_exec`) and refuse to start with its cause
  when it fails (in a pod: an image `balerix` without it). Tests: `cli_sandbox_sockets`
  (a real tmux server) and `cli_sandbox_*`; the Mediate ones skip where
  mediation does not work unless `BALERIX_REQUIRE_MEDIATION=1` (CI sets it;
  not set locally at `ptrace_scope` 2).
- `TmuxRunner::at_socket` is the pod runner: `-S <path> -u -N` on every
  call, and stops wait on `pane_dead` through a waiter the pane is
  respawned into, never on `/proc` (the pid is the agent container's).
  `-N` because a tmux client starts a server when none listens: the
  server, and so Claude, must only ever run in the agent container
  (`balerix-agent run` creates it with the crew session), so a pod
  `ensure_crew` that finds no session fails the pass instead of creating
  one. The
  one-machine runner (`TmuxRunner::new`) is unchanged; `tmux_pod_it`
  covers the other.
- A Kubernetes-mode daemon (`serve --mode kubernetes`) runs no planner
  for its fleets and reads no `plugins.yaml`; `Harness::kube()` is the
  test harness for it, `kube_*_it.rs` the tests.
- The e2e overrides the system tool table with an empty `[tools]` in its scratch
  `$XDG_CONFIG_HOME/balerix/mise.toml` so nothing downloads; the real embedded
  table pins `claude`, and a fresh `up` on a real host installs it.
- `balerix dev fake-claude` is what the e2e runs as `claude.binary`; it reads
  `$CLAUDE_CONFIG_DIR/settings.json` and fires the hooks itself. Change the hooks
  block in `home.rs` and the fake together.
- `serve` writes `server/endpoint` after binding; clients resolve `--api-url`,
  then `BALERIX_API_URL`, then that file. Tests bind port 0 and read it.
- `DELETE …?keep_repos=true`: axum's `Query` rejects bare flags, so every flag is
  `key=true|false` (`DownQuery::to_query_string`).
- Hook secrets live in `secrets.enc` (vault), the agent's `settings.json` (HTTP
  header) and `nono-profile.json` (`BALERIX_HOOK_SECRET` for the relay), all 0600.
- `balerix-server` and `balerix` integration tests (`api_it`, `cli_serve`,
  `cli_fleet`, `e2e`) bind port 0 and use private tmux sockets named after
  the test's pid, and every test root under `target/tmp` is
  `<prefix>-<pid>` (`balerix_runtime::testing::TempRoot`). A root removes
  itself when the test passes and stays for reading when it fails; a run
  that nextest or Ctrl-C kills (the `.config/nextest.toml` slow-timeout
  terminates a hung test after three minutes) leaves its detached daemon,
  tmux server and socket alive, and the next e2e run reaps them by the dead
  pid in the socket name (`reap_earlier_runs`). To reap by hand:
  `tmux -L balerix-e2e-<pid> kill-server` and `kill` the `balerix serve`
  whose argv carries that `--tmux-socket` (the plugin e2e uses socket
  `balerix-e2e-plugins-<pid>`, the flow e2e `balerix-e2e-flow-<pid>`).
- A body over a route's limit is read to its end, up to four times the
  limit and for at most 10 s, before the route answers it
  (`body_limit::drain_over_limit`, #116; the SDK and the agent
  sidecar's hook ingress keep copies, #168): axum alone
  answers 413 the moment it has read past the limit and the connection
  closes with the rest unread, so a client still writing got EPIPE in
  place of the answer. Past four times the limit the answer (413), and
  after the time a 408, comes at once with `Connection: close`, and a
  client still writing can miss it. Authentication sits *outside* the drain (the auth
  `route_layer` goes on after `limited`; the events route has
  `hooks::require_secret`, the plugin routes `require_plugin`): a caller
  that fails it is answered 401 having had at most 64 KiB of its body read
  (`body_limit::refuse`; more than that and the connection is closed
  unread). A limited router goes through `limited`, never a bare
  `DefaultBodyLimit`. The answer is the route's own: the middleware hands
  the handler a stand-in body one byte over the limit, so a route's own 413 (and
  its JSON body) is what the client reads. The plugin mount
  (`proxy.rs::read_body`) is not a limited router, since it
  authenticates inside its handler, and drains the same way itself
  (#168); a refused or not-ready mount goes through `refuse`. `api_it.rs::one_byte_over` sends exactly one byte over, on
  a connection of its own (#79, #113), and
  `events_it.rs::every_limited_route_answers_a_client_that_sends_the_whole_body`
  sends it all.
- `balerix hook-relay` refuses a hook event over 1 MiB itself (stderr
  names the limit, stdout `{}`), rather than posting it truncated (#116).
- `balerix` is a reserved fleet name (the plugin fleet). `FleetName` still
  parses it — the reservation lives in `Daemon::apply`/`down` and
  `balerix_config::resolve`.
- A plugin's token is the hook secret the fleet actor mints for
  `balerix/plugins/<name>`; it lives in `nono-profile.json` as
  `BALERIX_PLUGIN_TOKEN` and nowhere else. It is minted when the plugin is
  added and rotates on remove + re-add, not on every restart: `Actor::apply`
  reuses an existing secret and only drops the ones the spec no longer wants.
- Plugin packages: `plugins.yaml` directory sources are used in place with no
  `sha256` (development and the e2e); the daemon hashes their `mise.toml` and
  `balerix-plugin.yaml` into the plugin's digest, so editing either and
  running `plugin sync` reinstalls (#4). Tarballs and URLs need `sha256` and
  unpack read-only under `$XDG_DATA_HOME/balerix/plugins/<name>/<digest12>/`.
- A plugin manifest's `sandbox` block is an allowlist, unlike a fleet's
  (`sandbox::check_plugin_sandbox`, #2): `filesystem` (the six grant lists
  and `deny`), `network` minus its credential keys,
  `security.ipc_mode`, and those inside `platform_overrides.<os>`. Paths are
  absolute and literal (no `~`, `$`, `..`, globs), and no grant may reach the
  state, data or config root outside the plugin's own grants. Anything else
  fails the render with `sandbox.<key path>: not accepted`. Widening the list
  means checking what the key lets nono's supervisor do outside the sandbox.
- The daemon runs `mise trust` + `mise install` on the package's own
  `mise.toml` with `MISE_STATE_DIR` under the plugin's home; the sandbox uses
  the same dir, so if `mise run` says the config is untrusted, the two
  `MISE_STATE_DIR`s diverged (`plugin.rs`: `install_plugin_tools` vs
  `plugin_env`). Do not set `MISE_GLOBAL_CONFIG_FILE` inside the sandbox —
  mise would run the start task in `$HOME` and walk `$HOME`'s ancestors out
  of the sandbox (`plugin.rs::plugin_env` explains).
- `balerix dev fake-plugin` is what the plugin e2e runs; it binds a loopback
  listener under nono and says hello through the SDK.
- `plugin remove --purge` (and `down --purge`) used to answer 500 `Directory
  not empty` about one run in twenty: `tmux kill-window` returns before nono
  finishes writing its ledger under `plugins/<name>/nono/`. Fixed on both
  sides — `PluginHost::purge` waits (20 s in all, under the CLI's 30 s
  request timeout) for the daemon's tool pool to be ready (no pass runs
  before it; a pool whose last install failed is a 503 at once, with its
  reason, #178), then for a pass the actor runs after the request
  (`Msg::Barrier`; a window that outlived a restart is in no record) and
  for the plugin to be out of the record before deleting anything (#1);
  `plugin remove --purge` retries a 503 until its `--timeout` (#178); and
  `Runtime::rm_rf` retries
  `remove_dir_all` for 5 s while the error is `DirectoryNotEmpty`.
- `up` waits for plugin activations as well as `Ready`; a `fake=pending` in
  the timeout table means the plugin never said `hello` (look at
  `plugins/<name>/logs/`), a `rejected` row fails `up` at once with the
  plugin's message. Re-running `update` after fixing the plugin does
  re-attempt it: `Daemon::apply` diffs against the fleet's *active* rows
  only, so a pending or rejected pair is offered again even though its
  config did not change (R24). Only to a ready plugin, though: while it
  is not ready an unchanged rejected row stays rejected, and `update`'s
  wait (`fleet.rs::ready_check`) fails at once with the old
  `crews.<c>.agents.<a>.plugins.<p>: <message>` (the daemon accepted the
  update; `--no-wait` returns the record) until the plugin's next `hello`
  re-offers the pair. A changed config is `pending` with the old message
  kept on the row (`GET /v1/fleets/{f}`'s `plugins.<p>.message`; the
  CLI's status table shows only the state), and the wait waits for it like any
  pending pair (#11).
- The activation table is not persisted: after a daemon restart every pair
  is `pending` until the plugin's next `hello`, which re-activates all of
  them.
- `plugin remove` prunes the activation rows of the removed plugin; `plugin
  remove --purge` also deletes `plugins/<name>/kv/` — a plugin's KV state
  survives a plain remove.
- A plugin's `stop` action leaves the agent `Stopped` until a `restart`
  action or the next `up`/`update`; the reconciler will not restart it and
  `status` shows `stopped`. In Kubernetes mode the operator's apply keeps
  the stop (it re-sends on every reconcile); only a `restart` ends it.
- The `PluginClient` is built with `.no_proxy()`; do not remove it — a
  `HTTP_PROXY` in the daemon's environment would otherwise capture loopback
  calls.
- Plugin `/v1/metrics` bodies must carry only `balerix_plugin_<name>_`
  families or the whole body is dropped (counted in
  `balerix_plugin_metrics_scrape_failures_total`).
- The SDK's `Plugin` trait uses return-position `impl Future + Send`;
  implement methods as `async fn` in the impl block (the compiler accepts
  that), and keep `Send` state (`Mutex`, not `RefCell`).
- The e2e waits for `plugin list … ready` before `up` when a fleet names a
  plugin: `Daemon::apply` only activates a pair inline against a plugin that
  is already listening, and the agent's first `PreToolUse` can fire before a
  pending pair's next `hello`.
- Flow `match` regexes are full-match (`^(?:…)$`); `"rm -rf"` does not match
  `rm -rf /x`, `"rm -rf.*"` does. Compiled at `activate` with a 10 KiB size
  limit; errors carry the path
  `states.<s>.on[<i>].match.<pointer>: …`.
- Flow's state is KV `state/<agent>` (`plugins/flow/kv/state/<fleet>/<crew>/<agent>`
  on disk). A plugin or daemon restart resumes it; `down`, a config change
  or dropping the block resets it (`deactivate` deletes the key). To reset
  by hand: `down` and `up`. A changed config arrives as an `activate` in
  place, with no `deactivate` before it, and resets through the hash check;
  a rejected `update` therefore changes nothing (§17.9).
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
- Cookie-authenticated proxy requests need `Sec-Fetch-Site: same-origin`,
  or an `Origin` equal to the exact `http://127.0.0.1:<port>` of the login
  URL (`localhost` is another origin) or whose authority is the request's
  `Host` (a WebSocket handshake through a reverse proxy: no
  `Sec-Fetch-Site`, `Origin` the proxy's hostname — found by
  `verify-claude` behind one, as `[disconnected]` on the terminal page). A
  test client that sends none of them passes (a navigation sends none).
- `/v1/plugins/<name>` without the trailing slash is the admin purge
  route, so a browser there got 401 `missing or invalid admin token`
  even with a good cookie; a GET there is now a 308 to the mount, and a
  login `to` of a bare mount root gains its slash. Found by
  `verify-claude` through a reverse proxy, where the URL's final `/`
  went missing in the copy.
- `balerix plugin open <name>` prints a URL valid for 60 s, once. Opening
  it twice is a 404 by design; the body says `already used Ns ago` or
  `expired Ns ago` (remembered 10 min), and server.log records every
  attempt with its `Host`, `Sec-Fetch-Site` and `User-Agent` — read those
  before blaming a reverse proxy or a prefetching browser.
- The web plugin's assets are `include_bytes!` of `assets/`; the crate
  does not build without them. Run `scripts/vendor-xterm.sh` after a
  fresh clone only if the files are missing — they are committed.
- `FleetWatch::next` never returns: a plugin that stops wanting frames
  drops the watch (the web plugin aborts its task at exit).
- A tmux command sequence inherits the previous command's target, so the
  attach sequence must never name the crew session; a `select-window -t
  =f/c:…` in it would make `set-option destroy-unattached on` land on
  the crew session and destroy it.
- Enter on the attach PTY is `\r` (what a terminal and xterm.js send);
  tmux treats `\n` as `C-j`. The `tmux_it` attach test writes `\r`.
- The vendored minified `xterm.js` trips gitleaks' `generic-api-key` rule
  on `…Key=void 0`; `.gitleaks.toml` keeps the default ruleset and
  allowlists exactly the three vendored files by anchored path. Anything
  else under `assets/` is still scanned.
- `send_text` with a newline, or longer than 4 KiB, goes through
  `load-buffer -b <name> -` + `paste-buffer -p -d` (a bracketed paste);
  shorter single lines go through `send-keys -l`. The text rides in on the
  tmux client's *stdin*, not in the argv: the client refuses a command
  whose packed argv is over 16 KiB ("command too long"), and a review may
  be 64 KiB. The e2e's
  `fake-claude` records a paste line by line; the real `claude` is expected
  to arrive as one message — verify with `mise run verify-claude`
  (Spec C §8, pending).
- Workspace git calls (`balerix-runtime/src/inspect.rs`) run under the git
  profile (`GIT_OPTIONAL_LOCKS=0` and the rest in its `set_vars`), with
  `-c core.fsmonitor=false -c core.hooksPath=<empty>`
  and pass `--no-ext-diff --no-textconv --no-color --submodule=short
  --ignore-submodules=dirty` to every `diff`: the clone's `.git/config` is
  agent-writable, and the last two keep git out of a nested repository the
  agent committed as a gitlink, whose own config (its `diff.external`) the
  `-c` overrides reach but the argv flags do not. They also set
  `GIT_CEILING_DIRECTORIES` to the agent's own root (canonical — git ignores
  a ceiling that does not match the resolved path), so that deleting the
  clone's `.git` directory makes git refuse instead of discovering the
  repository that holds the state root. Keep those when adding a git call
  there.
- Two plugin-visible changes came with the sandboxed reader (#108): git
  inside the git profile sees no `HOME` and no system config, so the
  daemon user's global `core.excludesFile` no longer hides untracked
  files (only the clone's `.gitignore` and `.git/info/exclude` do); and
  `uncommitted` (and the `version` fingerprint) now come from `git status`,
  so an index-only change (staged, with the worktree back at `HEAD`'s
  content) counts as uncommitted, where `diff HEAD` used to miss it.
- A workspace `diff` or `version` refuses with `repository config sets
  <key>; workspace read refused` when the clone's `.git/config` declares a
  `filter.<x>.<clean|smudge|process>`, or when it sets
  `extensions.worktreeconfig` (a `config.worktree` file could then hold a
  filter the check's `--local` read cannot see, so the extension alone is
  refused; `git sparse-checkout init/set` and `scalar` set it too, so it is
  not always tampering) — an agent can write that config; the fix is to unset
  the filter, or `git sparse-checkout disable` where sparse checkout set the
  extension, rather than unsetting that key by hand.
  `read_file`/`list_dir` run no git and are unaffected.
- A workspace route's two 404s differ on purpose: `no workspace for agent`
  (no worktree yet) and `no such path`; the SDK maps only the second to
  `None`.
- The workspace `version` fingerprint covers `HEAD`, the merge-base and the
  size and mtime of every changed or untracked path — content, not index
  state: a byte-identical `git add` is invisible by design, and a same-size
  rewrite within the filesystem's mtime resolution is the theoretical miss.
  At most `VERSION_PATH_CAP` (2000) paths are stat'ed, tracked changes
  first and untracked paths after them (each sorted), so a large untracked
  tree cannot hide an edit to a tracked file; past the cap a marker and
  the total count are hashed too, so a path coming or going still shows
  but an edit to one left out does not (#18). A path that fails
  `check_path` — a valid Linux name with a `\` in it, say — is left out
  of the fingerprint altogether (#19). A path whose `stat` fails for any
  reason hashes as `missing`.
  The web plugin's `events.json` waits 1.5 s for it, then answers
  `workspace: null` and counts a `version_failures_total`.
  The review page applies a changed diff automatically (3 s rate limit),
  but never while a comment box is open.
- `web`'s event buffer is memory only: after a plugin restart the review
  page's column is empty until new events arrive (no observer catch-up).
- The matrix plugin's `matrix-sdk` state and crypto store lives in the
  plugin's `scratch/`, so `plugin remove --purge` discards the device keys:
  the bot rejoins as a new device and previously encrypted history stops
  being readable by it. Purge only when you mean that.
- The GitHub plugin refuses a repository whose sanitised fleet name
  (`gh-<owner>-<name>`) already stands for another repository
  (`repo/<fleet>` in its KV, `plugins/github/kv/repo/<fleet>` on disk);
  after a repository rename, delete that key. A row rebuilt from an
  activation after a lost KV carries `installation: 0` until the next
  webhook on it refreshes it.
- `plugins.yaml` entries may carry `secrets: { <config key>: <path> }`. The
  daemon reads each file at load, trims one trailing newline and injects the
  value into the entry's `config` before `hello`. The file must not be
  readable by group or others. The resolved config lives only in memory;
  `ResolvedPlugin` hand-implements `Debug` and prints `config: <redacted>`,
  so never go back to deriving it.
- Rotating a file named in `secrets` changes `ResolvedPlugin::hash` and so
  restarts the plugin on the next sync. That is intended.
- A fleet record's `owner` (Spec L) is set by the first `PUT
  /v1/plugin-host/fleets/{name}` and never transferred: `up`/`update`
  on it are 409 `fleet <f> is managed by plugin <p>`, `down` needs
  `--force`, and a forced down keeps the owner so the plugin's next
  apply resumes it. Every sync downs the up fleets whose owner
  `plugins.yaml` does not declare (`SyncReport.downed`): `plugin remove`'s
  sync, and the first sync at `serve` for a plugin removed while the
  daemon was stopped. The
  daemon-side rule is `Daemon::check_owner`; `apply`/`down` are the
  admin wrappers of `apply_as`/`down_as`. `apply_as` also refuses a
  plugin caller the registry no longer lists (409 `plugin <p> is not
  installed`): a `PUT` in flight across `plugin remove` must not
  re-raise a fleet the sync downed (#62). A test that seeds a
  plugin-owned fleet through `apply_as` therefore installs that plugin
  first (`cli_fleet.rs::install_gh`). `plugin remove --purge` lists the
  plugin's fleets after the sync, not before.
- The resolver a plugin's fleet file goes through lives in the binary
  (`crates/balerix/src/wiring.rs::HostResolver`, both ports): it reads
  `HostPaths::discover()` at call time, so the daemon's `HOME` is the
  operator whose credentials every managed fleet gets. `balerix-server`
  sees only the `FleetResolver`/`CredentialSource` ports;
  `Harness` answers them with `FakeResolver` (no answer → `name: no
  resolver answer configured`; set it per test) and `FakeCredentials`.
- `AgentSettings.branch` makes the *remote* branch the clone's branch
  and its start point (`ResolvedAgent::start_ref`); a branch the remote
  lacks fails the materialize step with git's message and retries at the
  resync cadence. The workspace diff base is still `origin/<crew ref>`.
  `branch: <default branch>` works: a fresh `--no-checkout` clone already
  has that branch with HEAD on it, hence `checkout -B` and the seed
  fetch's `--update-head-ok` and `+` in `create_clone`. The cache keeps
  no copy of its own: `ensure_repo` detaches the cache's HEAD and deletes
  the clone-time default branch, so every branch in the cache is a
  harvest (#69), and a harvest seeds only while it holds commits
  `origin/<branch>` lacks.
- Every agent's `workspace/` is a private clone (Spec N); the crew's
  `repo/` is an object cache the daemon alone writes (`gc.auto=0`, every
  daemon fetch `--no-auto-gc`, never pruned) and agents read through
  `objects/info/alternates`. The sandbox grants `repo/.git/objects`
  read-only and nothing else under `repo/`. Before a clone is deleted
  (`remove_agent`, `down --keep-repos`, a changed `branch`) its marker's
  branch is fetched into the cache from inside the cache — never pushed
  from the clone — and a new clone for a branch the cache holds seeds
  from it. Only that branch survives: other local branches and every
  file in the tree go with the clone. Plain `down` and `--purge` delete
  the cache too and harvest nothing, so a broken clone cannot wedge a
  purge. A git error is never a "no": a yes/no question (`rev-parse
  --verify`, `merge-base --is-ancestor`, `symbolic-ref -q`) goes through
  `git_probe`/`head_branch`, where exit 1 is the answer and any other
  exit fails the step, and a marker or HEAD the daemon cannot read fails
  the harvest rather than skipping it (#74). A cache or clone whose
  making failed half-way is removed with the error, so the next pass
  starts over instead of judging the remains. A `workspace/.git` that is a *file* is a 0.1.x worktree and is
  refused with the purge message; `down --keep-repos` + `up` also works
  (the old crew clone becomes the cache and its branches seed the new
  clones).
- The daemon can read every repository under its uid, so the harvest is a
  confused deputy unless the clone's objects are the clone's own. Before
  any git runs in an existing clone (`ensure_clone`, `harvest_and_remove`)
  `check_clone` refuses, with the `--purge` remedy, a `.git` that is not a
  real directory, any symlink under `.git/objects`, a `.git/commondir`, and
  an `objects/info/alternates` other than the one line naming the crew
  cache. `agent_git` passes `--git-dir=<ws>/.git` and the harvest fetches
  `<ws>/.git` with `upload-pack --strict`: without them a `.git` git
  rejects (no `HEAD`) makes git serve `workspace/` itself as a bare
  repository. Keep all three when touching either call, and keep the
  harvest's remote a percent-encoded `file://` URL (`file_url`): given a
  plain path that is a bundle file, git reads it as the daemon and skips
  `upload-pack`. It also refuses
  an `alternates` file in the *cache's* `objects/info/` (never legitimate).
  A promisor remote the agent writes into its clone's config
  (`extensions.partialClone`, or any `remote.<x>.promisor`) would make
  `status`/`diff` fetch foreign objects into the clone as the daemon, and
  run `remote.<x>.uploadpack`: `check_clone_config` (before the first
  object-reading call on a branch change and before a harvest; the
  marker's reuse path makes no git call) and the reader's `refuse_filters`
  refuse those keys, read with `config --local --includes` so includes
  count as git counts them. `scrub_git_env` sets `GIT_NO_LAZY_FETCH=1` on
  every git call as the second layer; git honours it from 2.45.1 (and
  the patched maintenance releases from 2.39.4), so for the workspace
  reader the refusal, not the variable, is what the guard rests on (#67);
  for the clone step the git profile (network blocked) is what holds, and
  the refusal is the operator's message.
- `dev fake-plugin` applies a fleet when its `plugins.yaml` entry has
  `config: { manage: { fleet, file } }` (the e2e's managed journey) and
  writes the outcome to `scratch/fake-plugin.manage`. The SDK's
  `FakeHost` answers `PUT fleets/{name}` with the record it already holds
  under that name (`set_fleets`), else a fresh one owned by `plugin`.
- `matrix-sdk` is a large tree, isolated in the standalone `plugins/matrix`
  project and out of the core workspace's resolution entirely; run
  `cargo deny` there when bumping it.
- A plugin that needs daemon-level config implements `Plugin::configure`.
  The daemon may deliver an `activate` before `configure` returns, so such a
  plugin must buffer, as the matrix actor does.
- `matrix-sdk` changed two things for crates that never asked for it,
  because cargo unifies features across one build: every HTTP client in the
  workspace now requests compressed responses and decodes them
  transparently (nothing depends on that, but it is a wire change — see
  `async-compression` in `Cargo.lock`), and building an HTTP client now
  reads and parses the system certificate store eagerly, failing outright
  if that store is empty, even for a client that only ever speaks plaintext
  to localhost (`reqwest`'s `rustls-platform-verifier`). That hits the
  daemon's plugin client (`crates/balerix-server/src/plugins/client.rs`)
  and the SDK's host client (`crates/balerix-plugin-sdk/src/host.rs`): on a
  host with no CA bundle, constructing either now fails where it used to
  succeed. The sandbox profile's default `/etc` read grant
  (`SYSTEM_READ` in `crates/balerix-runtime/src/sandbox.rs`) covers it for
  agents; a bare container without `/etc/ssl` populated is not covered.
  Also: the workspace test suite went from roughly 5.9-6.3 s to 8.2-8.7 s
  on an idle machine after this dependency landed, most plausibly the
  eager certificate read paid once per client built in-process across the
  suite. Several tests elsewhere in this workspace already fail under a
  busy host and pass alone (`balerix-runtime`'s `processes_with_arg_pair`
  and `reap_kills` among them); a slower suite makes those flakes more
  likely, not less — rerun once with `--no-fail-fast` before assuming a
  real regression.
- In `balerix-plugin-matrix`, `routing::Maps.threads` is the single source
  of truth (mirrored to the daemon's KV) and `.routes` is derived from it
  and rebuilt at startup (`Maps::load`), never persisted itself — don't add
  a second place that writes routes.
- The actor's inbound `Queue` (`plugins/common/src/queue.rs`, used by
  `plugins/matrix/src/actor.rs`) is bounded and drops its *oldest* entry
  rather than blocking its producer: `observe` is a daemon-to-plugin HTTP
  call and must return, so a slow or wedged homeserver can never stall
  hook delivery to Claude. It can silently lose old, stale commands under
  sustained overload instead — that's the intended trade.
- `matrix::fake::FakePort` (always compiled, like the SDK's `FakeHost`) is
  what makes the actor's ordering rules — thread creation before the first
  event, refusing a room-level reply, refusing a reply after `SessionEnd`
  — unit-testable without a real homeserver. When changing an ordering
  rule, extend `FakePort`'s recorded calls rather than reaching for an
  integration test against a live server.
- PR titles are Conventional Commits (`pr-title.yml`) and PRs are
  squash-merged, so the title is the commit the release scripts read.
  `feat`/`fix`/`perf`/`refactor`/`build` release the units whose paths the
  change touches; `docs`/`test`/`ci`/`chore`/`style`/`revert` never do. A
  change under `crates/balerix-api/` or `crates/balerix-plugin-sdk/` counts
  for core and for every plugin.
- Don't bump a version by hand. A plugin's `package/balerix-plugin.yaml`
  `version` is written from its `Cargo.toml` by
  `scripts/release/prepare.sh`, and `mise run plugin <name>` fails when the
  two differ. A version on `main` with no matching tag
  (`<crate>-v<version>`) whose changelog has that version's section means
  "release pending" (its release PR was merged): `release.yml` releases it
  on the next push and `prepare.sh` answers `in-progress` for it. A version
  with neither is not proposed yet, and nothing releases it. If an older
  tag exists too, the version was changed outside a release PR:
  `prepare.sh` dies naming it; recover by reverting the edit or, for a
  release version above the last one, forcing it.
- `plugins/common` is published to crates.io, so its `balerix-api` and
  `balerix-plugin-sdk` dependencies carry a version beside their path.
  `release-prepare core` moves them; `release-prepare common` answers
  `status=none` (the reason on stderr) until that core version is tagged,
  and while `crates/balerix-api` or `crates/balerix-plugin-sdk` has changed
  since that tag. In-tree plugins depend on common by
  path only (they are `publish = false`).
- Released `CHANGELOG.md` sections are read back by
  `scripts/release/notes.sh` for the GitHub Release; don't edit them by
  hand.
- `scripts/release/test.sh` clones the repository instead of using `git
  worktree`: a worktree shares this repository's tags, and the scenarios
  create and delete tags.
- The release tools in `mise.toml` (git-cliff, cargo-edit, cosign, trivy,
  hadolint, zizmor, actionlint, shellcheck, jq) never reach agents: the
  embedded table is filtered to `claude` and `gh`
  (`balerix-runtime/src/toolchain.rs` `INHERITED`).
- `docker/balerix/Dockerfile` must not leave a mise config in the final
  image: a system `/etc/mise/config.toml` would enter every agent's and
  plugin's tool resolution. The `mise.toml` it installs from stays in the
  `tools` build stage.
- `release.yml` builds and tests each binary on a runner of its own
  architecture (`ubuntu-24.04-arm` for aarch64) because the smoke tests run
  the binary. On an arm64 host cc-rs wants `aarch64-linux-musl-gcc`, which
  `musl-tools` lacks; `scripts/release/build.sh` sets
  `CC_aarch64_unknown_linux_musl=musl-gcc` for matrix's C dependencies.
- zizmor runs with `--min-severity medium`; release workflows keep
  `cache: false` on mise-action (a cache restored into a job that publishes
  is a poisoning path).
