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
  (`mise run plugin matrix`). `plugins` does all four (common is the shared
  library, Spec K). Neither is part of `check`; CI runs them as their own
  concurrent jobs.
- `mutants` — nightly tier: mutation-tests `balerix-core` (the reconciler).
  `.cargo/mutants.toml` excludes `fakes.rs`: the fakes are exercised by
  `balerix-server`'s tests, which that run never executes.
- `e2e` — the Phase 3 journey against a real daemon; needs the same tools as `test-it`.
- `package-plugins [names…]` — builds the named in-tree plugins inside
  their own projects and assembles each as a directory source under
  `target/plugins/<name>/` (under `CARGO_TARGET_DIR` when set). No names
  means all three. `test` and `e2e` ask it for `flow web`, the two the
  journey needs; the flow e2e skips (fails under `BALERIX_REQUIRE_TOOLS`)
  without it.
- `serve` — a foreground daemon under `target/tmp/serve` for poking by hand
  (`HOME` is overridden, so it never touches your real state).
- `verify-claude` — the interactive spec §8.1 check with a real `claude`
  (`scripts/verify-claude.sh`); `BALERIX_VERIFY_FAKE=1` self-tests it with
  `dev fake-claude`. Its data root (`target/tmp/verify-data`, the daemon
  pool) is kept across runs so claude downloads once per version; config and
  state under `target/tmp/verify-claude` are wiped.
- `verify-matrix` — Spec G's manual check against a real Matrix homeserver
  (`scripts/verify-matrix.sh`); needs `MATRIX_HOMESERVER`, `MATRIX_USER_ID`,
  `MATRIX_PASSWORD` and `MATRIX_INVITE`. Not part of any CI tier.
- `verify-questions` — Spec J's manual check (`scripts/verify-questions.sh`):
  the real pinned `claude` in tmux, answered with the key plans common's
  `question.rs` (`plugins/common`) produces, the recorded answers read
  back from the `PostToolUse` hook. Needs a logged-in `claude`. Not part
  of any CI tier.
- `lint`, `test`, `fmt`, `precommit`, `audit` — defined in `mise.toml`.
- `vendor-xterm` is a script, not a task: `scripts/vendor-xterm.sh` re-fetches
  and verifies the web plugin's assets against
  `plugins/web/assets/VENDOR.md` (installing them only when
  every digest matches); `--check` verifies the committed files offline.
- `release-prepare <unit> [version]` — what `release-pr.yml` runs: works
  out one release unit's next version (core, common, flow, web, matrix) and
  writes it into the manifests, lockfiles, plugin manifest and changelog.
  Commits nothing. `docs/RELEASING.md` is the release process.
- `release-test` — scenario tests for `scripts/release/` against throwaway
  clones under `target/tmp`; CI runs it when the scripts change.

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

## Gotchas
- Run cargo through mise (`mise x -- cargo …`) or via a `mise run` task.
- `mise run check` covers the core workspace only. Cargo unifies features
  across every member one invocation selects, so while the plugins were
  members, a `--workspace` build handed `balerix-server` a `reqwest` with
  rustls, HTTP/2, gzip and stream that it never asked for, and tripled the
  gate's cold build. `scripts/check-core-deps.sh` fails if that ever comes
  back: it asserts both the seven-crate member list and that the core
  `reqwest` carries `json` alone, so a readmitted plugin is caught whether or
  not its tree pulls a `reqwest` feature. A plugin change is caught by
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
  changes what agents get.
- The matrix plugin answers `AskUserQuestion` by counting rows and pressing
  Down and Enter (`plugins/common/src/question.rs::plan`). That encodes
  Claude Code's dialog layout, which no API promises. The property test
  there proves `plan` against a *model* of the dialog; only
  `mise run verify-questions` proves the model. Bump `claude` in `mise.toml`
  and you run it. Keys sent with no pause are dropped at a question
  transition, which is why `send_keys` has a 20 ms floor; and never use Tab
  in a plan: it goes to different places from different rows.
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
  from every git call; `harden_agent_git` adds the rest of the hardening for a
  call in an agent's clone. The integration-test `git`
  fixtures do the same — the pre-commit hook exports them, and a git
  subprocess that inherits them operates on this repository instead of the
  test's.
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
- `balerix` is a reserved fleet name (the plugin fleet). `FleetName` still
  parses it — the reservation lives in `Daemon::apply`/`down` and
  `balerix_config::resolve`.
- A plugin's token is the hook secret the fleet actor mints for
  `balerix/plugins/<name>`; it lives in `nono-profile.json` as
  `BALERIX_PLUGIN_TOKEN` and nowhere else. It is minted when the plugin is
  added and rotates on remove + re-add, not on every restart: `Actor::apply`
  reuses an existing secret and only drops the ones the spec no longer wants.
- Plugin packages: `plugins.yaml` directory sources are used in place with no
  digest (development and the e2e); tarballs and URLs need `sha256` and
  unpack read-only under `$XDG_DATA_HOME/balerix/plugins/<name>/<digest12>/`.
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
  sides — `PluginHost::purge` waits (30 s) for the actor to take the plugin
  out of the record before deleting anything, and `Runtime::rm_rf` retries
  `remove_dir_all` for 5 s while the error is `DirectoryNotEmpty`.
- `up` waits for plugin activations as well as `Ready`; a `fake=pending` in
  the timeout table means the plugin never said `hello` (look at
  `plugins/<name>/logs/`), a `rejected` row fails `up` at once with the
  plugin's message. Re-running `update` after fixing the plugin does
  re-attempt it: `Daemon::apply` diffs against the fleet's *active* rows
  only, so a pending or rejected pair is offered again even though its
  config did not change (R24).
- The activation table is not persisted: after a daemon restart every pair
  is `pending` until the plugin's next `hello`, which re-activates all of
  them.
- `plugin remove` prunes the activation rows of the removed plugin; `plugin
  remove --purge` also deletes `plugins/<name>/kv/` — a plugin's KV state
  survives a plain remove.
- A plugin's `stop` action leaves the agent `Stopped` until a `restart`
  action or the next `up`/`update`; the reconciler will not restart it and
  `status` shows `stopped`.
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
- Workspace git calls (`balerix-runtime/src/inspect.rs`) set
  `GIT_OPTIONAL_LOCKS=0` and `-c core.fsmonitor=false -c core.hooksPath=<empty>`
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
- A workspace `diff` refuses with `repository config sets <key>; workspace
  diff refused` when the clone's `.git/config` declares a
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
  The review page applies a changed diff automatically (3 s rate limit),
  but never while a comment box is open.
- `web`'s event buffer is memory only: after a plugin restart the review
  page's column is empty until new events arrive (no observer catch-up).
- The matrix plugin's `matrix-sdk` state and crypto store lives in the
  plugin's `scratch/`, so `plugin remove --purge` discards the device keys:
  the bot rejoins as a new device and previously encrypted history stops
  being readable by it. Purge only when you mean that.
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
  admin wrappers of `apply_as`/`down_as`.
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
  purge. A `workspace/.git` that is a *file* is a 0.1.x worktree and is
  refused with the purge message; `down --keep-repos` + `up` also works
  (the old crew clone becomes the cache and its branches seed the new
  clones).
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
