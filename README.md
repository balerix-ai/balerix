# balerix

A control plane and orchestrator for fleets of coding agents (Claude Code
first), driven over a loopback HTTP API from a thin CLI. Agents run isolated — own
`$HOME`, own tools, own sandbox — as tmux windows grouped into crews that share
a repository.

## Quickstart
1. `mise trust && mise install` — pinned toolchain (Rust and every tool balerix shells out to).
2. `git config core.hooksPath .githooks` — enables the pre-commit tier.
3. `mise run check` — lint + tests, e2e included; the same gate CI runs.
4. `mise x -- cargo run -q -p balerix -- config resolve examples/payments.yaml --no-host-defaults`
   — resolves the example fleet and prints every agent's merged settings.
5. `mise x -- cargo run -q -p balerix -- serve -d` — starts the daemon on `127.0.0.1:7643`
   (token, vault key and log under `$XDG_STATE_HOME/balerix/server/`).
6. `mise x -- cargo run -q -p balerix -- up my-fleet.yaml` — point `repo:` at a repository you
   can clone; waits until every agent's Claude has started. Then `status <fleet>`, `list`,
   and `down <fleet> --keep` (repos and homes survive; `--purge` removes everything).
7. `mise x -- cargo run -q -p balerix -- plugin install ./my-plugin` — declares a
   plugin package (a directory with `mise.toml` and `balerix-plugin.yaml`) in
   `$XDG_CONFIG_HOME/balerix/plugins.yaml` and syncs the daemon; `plugin list`
   shows its phase, `plugin remove <name> [--purge]` takes it out. `plugin
   package <dir>` builds the tarball and prints the `sha256` a `plugins.yaml`
   entry needs. `https://` sources are part of the file format but rejected
   until a TLS-enabled build — package the plugin and point at the tarball.
   `plugin list` shows each plugin's phase and how many agents it is active
   for; an agent opts into a plugin with `plugins: { <name>: { …config… } }`
   in its settings block and `up` waits until the plugin has accepted it.
8. `mise run package-plugins` — assembles the in-tree `flow` plugin under
   `target/plugins/flow/` (and `web` under `target/plugins/web/`); point a
   `plugins.yaml` entry's `source` at that directory and give an agent a
   `plugins: { flow: … }` block to drive it by rule: block a tool call,
   send text on `Stop`, move between states. `examples/payments.yaml`
   shows one.
9. `mise x -- cargo run -q -p balerix -- dev materialize examples/payments.yaml backend/bob --no-host-defaults`
   — renders bob's generated files into a temp dir without launching anything.
10. `mise x -- cargo run -q -p balerix -- plugin open web` — prints a
    single-use login URL (60 s); open it in a browser to reach the web
    plugin's index and click an agent for a live terminal.

## Where to look
- `ARCHITECTURE.md` — the map: pieces, flow, non-obvious decisions.
- `AGENTS.md` — conventions and gotchas for contributors (human or agent).
- `docs/superpowers/specs/` — the design; `docs/superpowers/plans/` — how it is being built.
- `docs/THREAT-MODEL.md` — what is protected, from whom, and what is out of scope.
- `docs/plugin-protocol.md` — the wire contract for plugins in any language.

## Status
Spec A, Spec B (plugins) and Spec C (workspace reads and browser code
review) are complete: plugin workloads, the event protocol, the `flow` and
`web` plugins, the proxied plugin mount with browser sessions, attach and
`fleets/watch`, and the web plugin's review page.

### Upgrading to Spec B phase 1
- Fleet files rename the reserved `flow: {}` settings block to `plugins: {}`
  (a map of plugin name → that plugin's config). Stored `fleet.json` records
  still load with the old name.
- On the first start after the upgrade every running agent restarts once: its
  spec hash changed with the block.
- A stored fleet named `balerix` is ignored, with an error in the daemon log —
  the name is now reserved for the plugin fleet.

### Upgrading to Spec B phase 2a
- `status` gains a `PLUGINS` column and `plugin list` an `ACTIVE` column.
- `fleet.json` gains an empty `stopped` list.
- An agent naming a plugin that is not in `plugins.yaml` now fails `up` with
  `crews.<c>.agents.<a>.plugins.<p>: no plugin "<p>" is installed` (phase 1
  ignored the block).

### Upgrading to Spec B phase 2b
- `Plugin::metrics` in the SDK returns `Option<&Metrics>` instead of text;
  register families through `Metrics` and the prefix is applied for you.
- `mise run test` and `mise run e2e` now run `package-plugins` first.

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

### Upgrading to Spec C
- `balerix-plugin.yaml` may declare `needs: [workspace]` for the three
  read-only worktree routes (plugin-protocol §3 "Workspace"). The web
  plugin's manifest now declares `actions` and `workspace` and observes
  every hook event; re-run `mise run package-plugins`.
- `balerix-plugin-sdk`: `Host::{workspace_diff, workspace_file,
  workspace_tree}`, `FakeHost::{set_workspace, fail_actions}`,
  `Harness::post_route` are new; nothing existing changed.
- `send_text` with a newline is now a bracketed paste on tmux.

### Upgrading to Spec D
- `GET agents/…/workspace/version` is a fourth `workspace` route
  (plugin-protocol §3); `Host::workspace_version` is new; nothing existing
  changed. The web plugin's `events.json` gains a `workspace` field; re-run
  `mise run package-plugins`.
