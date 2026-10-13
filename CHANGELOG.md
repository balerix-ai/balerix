# Changelog

### Upgrading

- A fleet's `sandbox` block (fleet, crew and agent `defaults`, a plugin
  entry's `fleetDefaults`, and a Fleet's or Daemon's `defaults` in
  Kubernetes) is now an allowlist, like a plugin manifest's (#187):
  `filesystem` (`read`, `allow`, `write`, their `_file` forms, `deny` and
  the `unix_socket*` lists), `network` without `credentials`,
  `custom_credentials` or `tls_intercept`, `security.ipc_mode`, and those
  inside `platform_overrides.<os>`. Refused, naming the path:
  - every other key (`extends`, `command_policies`, `hooks`,
    `session_hooks`, `bypass_protection`, `process_info_mode`,
    `capability_elevation`, `linux`, …), and `environment`, `meta` and
    `security.signal_mode` (balerix-owned; until now refused only when
    the agent was rendered);
  - `security.ipc_mode: full` (only `shared_memory_only` is accepted)
    and `network.allow_domain` entries that are objects (endpoint rules);
  - a path with `~`, `$`, `..` or a glob, or under `/proc` or `/dev/fd`;
  - at render, a grant under balerix's state, data or config root
    outside the agent's own `home/` and `workspace/` (or, for reading,
    its tool pools, `mise.toml` and its crew's object cache), or one that
    contains a root (your home directory, `/`);
  - a grant naming balerix's tmux socket directory
    (`<$TMUX_TMPDIR or /tmp>/tmux-<uid>`; in a pod the run directory),
    anything under it, or anything containing it (`/tmp`).

  The same `ipc_mode`, `allow_domain`, `/proc` and tmux socket rules now
  apply to a plugin manifest's block, whose key and path rules are also
  checked at `plugin install`.

  `balerix up` refuses such a file on the client, naming the layer; the
  daemon's `POST`/`PUT` answer 400 with the path. A stored fleet that now
  fails is not converted any more: the daemon logs `pass failed …
  invalid stored spec: <path>: …` on every pass, `balerix status` stays
  as it was, and the agents keep running under their old profile with
  nothing supervising them (no restarts, no spec changes; stopping or
  starting one agent does nothing). `balerix down` still works. Fix the
  fleet's `sandbox` block and re-apply it, or `down` it. In Kubernetes
  mode the Fleet goes `Resolved=False` (`InvalidFleet`, naming the path)
  and its status stops updating; its pods keep running until they
  restart, and then the sidecar refuses to render the agent. Fix the
  Fleet's (or the Daemon's `spec.defaults`) `sandbox` block.

- The workspace reader's `diff` and `version` now have a time budget
  (#213): 6 s for a `version` and 25 s for a whole `diff`, both under
  the plugin SDK's own request timeouts (10 s and 30 s). A read that runs
  out ends its sandboxed git, and everything it started, and answers 504
  `<agent>: workspace <diff|version>: timed out after <n>s`, where it
  used to hold the request (and the review page's poll) until the
  client gave up. In Kubernetes mode the same read answers 500
  (`<agent>: git link: workspace <diff|version>: timed out after <n>s`),
  and the daemon now waits 28 s for a sidecar's diff (15 s for
  every other call, as before).
- An untracked symlink that git would follow (to a FIFO, which it reads
  and blocks on, or to a directory) or that points out of the worktree
  is no longer given to git: its patch is the link's target text, as git
  prints it for a new symlink, built without looking at the target. An
  untracked directory, nested repository or other file that is not a
  regular file shows as `not a regular file, skipped`. When git cannot
  run under the daemon's git profile (it exits 126 or 127), `diff` and
  `version` now name `sandbox.git_read` and both logs, as a removal
  already did (#214).
- A plugin now starts under `balerix agent-supervise`, as an agent has
  since 0.2.1 (#119). Stopping, restarting or purging a plugin ends
  everything it started, including processes that ignore the hangup or
  detach (`setsid`, a double fork), and waits for them: such a plugin
  now takes the 2 s grace to stop, and a stop still failing after 5 s
  reports `agent processes still running after stop`. Nothing to do: the
  upgraded daemon restarts every running plugin on its first pass, and
  that restart's stop is the last one without the wrapper.
- A workspace `diff` is now at most 8 MiB as JSON (its `files`), and a
  `tree` at most 5000 entries (#229), in both modes. Past 8 MiB the
  entries come first: the list stops, with the diff's `truncated` set,
  at the first file that does not fit even without a patch; the patches
  then fill what is left in path order, the one that crosses the limit
  cut at a line and those after it empty, each with its file's
  `truncated` set (the review page shows `patch truncated`). A `tree`
  answers the first 5000 entries by name and nothing in it says that
  more were left out. A diff under 8 MiB, and a directory of up to 5000
  entries, answer as before. In Kubernetes mode the link carries
  messages of up to 16 MiB, and a sidecar reply that would still be
  larger fails that call with a 500 (`<agent>: git link: the reply is
  <n> bytes, over the link's limit of 16777216`) where it used to drop
  the link.
- The operator now checks that an object it finds by name carries the
  labels it gives its own (`balerix.ai/component` and the fleet, crew,
  agent or plugin it is for) before it reads, applies over or deletes it
  (Spec O §26). Another object by that name is left alone, nothing of
  the owner's is written while it is there, and the owner's readiness
  condition goes `False`, reason `Conflict`, naming it. Rename the Fleet,
  crew or agent, or remove the other object. Objects earlier operators
  made already carry these labels; one without them whose controller
  owner reference is its owner's is labelled and kept.
- A Fleet with two agents whose names join to one object name (crew `b`
  with agent `c-d` and crew `b-c` with agent `d` both make `f-b-c-d`) is
  now `Resolved=False` (`InvalidFleet`), naming both (#232). Such a Fleet
  used to run one pod for the two, with one token. Rename one of them.
  Of two Fleets whose objects take the same names, the second is now
  `Ready=False`, `Conflict`, and the first keeps its objects; the two
  used to take the objects from each other on every reconcile.
- Fleet names starting with `balerix-` are now refused; the prefix is
  reserved for the operator's own objects. The Fleet definition refuses
  new ones (and `balerix`) at admission, and the operator at resolution
  (`Resolved=False`, `InvalidFleet`). An existing `balerix-*` Fleet is
  marked `InvalidFleet`: recreate it under another name. It can still be
  deleted. Host fleets are unchanged.
- A Fleet deleted in the foreground (`kubectl delete
  --cascade=foreground`, Argo CD's default prune) now harvests with
  `retain: Branches` and runs the crew cleanup with `retain: None`
  (#233). The agent claim no longer has an owner reference: the next
  reconcile removes it from existing claims, and a claim whose Agent's
  finalizer is removed by hand now stays, to delete by hand. Harvest and
  cleanup Jobs no longer have one either; they are deleted an hour after
  they finish (`ttlSecondsAfterFinished: 3600`). A harvest or cleanup
  still under way at the upgrade starts again once.
- An Agent whose branch changes (its `branch` in the Fleet set, changed
  or removed) now gets the host's behaviour (Spec O §8.5, §27, #128): the
  pod is deleted, a harvest Job (`harvest --branch <new>`, with only the
  clone's directory of the claim writable) fetches the old branch into
  the crew cache and empties the clone, and only then does the new pod
  start, on the same claim, and clone the new branch. It used to leave
  the agent down with a `Materialized=False` git error. A clone with
  uncommitted changes, or any harvest that fails, keeps the clone and the
  pod away, with `Ready=False`, reason `BranchChangeBlocked`. Set the
  branch back, commit or discard the changes in the pod, then change it
  again; or annotate the Agent `balerix.ai/purge=true`, which discards
  the clone's uncommitted changes and unpushed commits. The operator
  never removes that annotation: remove it yourself; until you do, every
  branch change and the Agent's removal discard unpushed work (a
  `PurgeAnnotationKept` Warning Event on the Agent says so). The agent
  claim now records its clone's branch in the annotation
  `balerix.ai/branch`. Existing claims get the Agent's current branch on
  the first reconcile, with no harvest. An Agent already stuck on a
  branch change at the upgrade is recorded under its new branch and
  stays stuck: set its branch back (the pod starts on the old clone),
  then change it again.
- An Agent's deletion now waits for its own harvest Job while it is still
  running, up to the Job's deadline (an hour), instead of replacing it
  when the Job's input changed (after an image bump at the upgrade, for
  instance); the Agent's `Ready` says `Deleting`, naming the Job. A
  branch change likewise waits for a running harvest Job, and no pod
  starts while one runs.
- A plugin whose name contains `-` now has its metric families under
  `balerix_plugin_<name>_`, with `-` in the name written as `_` (#224):
  `my-plugin`'s are `balerix_plugin_my_plugin_…`. Registering any family
  used to fail for such a plugin. Upgrade the daemon and the plugin's
  SDK together, to this release: an older daemon expects the hyphenated
  prefix and drops the whole scrape of a hyphenated plugin built on the
  new SDK. A plugin that formats its own text must use the new prefix.
  The daemon now also drops a plugin's whole scrape when a family name
  is outside Prometheus's `[a-zA-Z_:][a-zA-Z0-9_:]*`, or when it shares
  a name with one of the daemon's own families (several sit under
  `balerix_plugin_`, such as `balerix_plugin_events_total`) or with an
  earlier plugin's (`a`'s `b_x` and `a-b`'s `x` are both
  `balerix_plugin_a_b_x`); the log names the clash and
  `balerix_plugin_metrics_scrape_failures_total` counts it.
- A new or re-created clone now takes about a second longer (#253): the
  clone step waits for the clock to leave the second the checkout wrote
  the index in, then refreshes the index under the git profile, so the
  workspace reader's `version` and `diff` no longer re-hash every file
  the checkout wrote, on every poll, until the agent's own git rewrote
  the index (they were about twice as slow on a fresh clone). The wait
  is per clone, and a fleet pass makes its clones one after another, so
  `up` on a fleet of N new agents takes about N seconds longer; in
  Kubernetes each pod's sidecar clones its own agent, so each pod's start
  is about a second longer. A failed refresh only warns in the crew's
  `git.log`.

## 0.2.4 - 2026-10-10

### Bug fixes

- Plugin host leftovers batch (#16, #176, #13, #178, #175) (#201)

### Upgrading

- `balerix_plugin_events_dropped_total` has a second label, `reason`
  (`overflow`, `not_ready` or `unacknowledged`), so its label set is now
  `{plugin, reason}` (#13). A query or alert that matched the series on
  `{plugin}` alone still matches; one that compares series by their full
  label set (`on(plugin)` is fine, a bare `/` or `==` against another
  `{plugin}` metric is not) needs `sum by (plugin)` first.

## 0.2.3 - 2026-10-09

### Bug fixes

- **runtime:** Bind the socket probe's sockets in a short directory (#196)

## 0.2.2 - 2026-10-09

### Features

- Git and sandbox batch (#118, #108, #111) (#173)

### Bug fixes

- Supervisor batch (#148, #120, #123, #122, #121) (#158)
- Plugin batch (#17, #42, #20, #49, #48) (#159)
- Daemon batch (#9, #11, #10, #12, #116) (#166)
- Plugin host batch (#170, #15, #14, #168, #6, #1, #169) (#172)
- Toolchain batch (#24, #23, #22, #25) (#180)
- Workspace reader batch (#174, #18, #19, #20) (#181)
- Plugin hardening batch (#2, #3, #4, #5, #7) (#188)
- **runtime:** Keep sandboxed processes off Unix sockets outside their grants (#189)

### Upgrading

- New `[sandbox] unix_sockets` setting in the daemon's `config.toml`
  (`auto` | `mediate` | `deny` | `open`, default `auto`) controls which
  Unix sockets agents, plugins and the daemon's sandboxed git can reach.
  On a host where nono's pathname mediation does not work
  (`kernel.yama.ptrace_scope = 2`, restricted containers) `auto` resolves
  to `deny`: sandboxed processes can no longer create Unix sockets (a
  local postgres, docker or `git fsmonitor` in an agent's workspace fails
  with "address family not supported") unless you set `open`. Agents
  already running keep their old profile until their next launch: restart
  them with `balerix down` / `balerix up`, or restart the daemon, which
  re-renders on its first pass. In Kubernetes mode the pod sidecar always
  resolves `auto` and, when that is `deny`, refuses to start unless the
  image's `balerix` supports `sandbox-exec`: build agent images on a
  balerix base of this release or later.
- The daemon now refuses an inexact tool version (`latest`, `22`, `22.x`,
  …) in a fleet's, crew's or agent's `tools` on every fleet it is posted,
  not only in `balerix up`'s client-side check (#24). A stored fleet
  record written by a client that bypassed that check and holding such a
  version fails every pass after the upgrade with `<fleet>: invalid
  stored spec: <path>: expected an exact version, got "…"`; re-apply it
  with exact versions (`mise latest <tool>@<version>` names one), or
  `balerix down` it. `down` is enough on one machine only: in Kubernetes
  mode a record whose spec no longer converts sends its agents no Stop,
  so re-apply it there instead.

## 0.2.1 - 2026-10-07

### Features

- **core:** Restricted settings surface for plugin-applied fleet files, the operator's fleetDefaults layer, and the fleets-gated PUT answer (Spec M §12) (#86)
- **github:** The GitHub plugin, an agent per issue or pull request (Spec M) (#93)
- **server:** Kubernetes mode and the balerix-agent sidecar over a TLS link (Spec O §6, §7) (#125)
- **operator:** The operator project, its five kinds and pure desired state; the sync and harvest Jobs (Spec O §20) (#126)
- **operator:** The controllers, envtest tests, the images, kind and e2e-k8s (Spec O §21) (#127)
- Plugins without a cluster (Spec O 4a, §23.1–§23.3) (#143)
- Plugins on a cluster (Spec O 4b, §23.4–§23.9) (#144)
- The Helm charts (Spec O 5a, §24.1–§24.3) (#145)
- Release the operator, agent and charts (Spec O 5b, §24.4–§24.6) (#146)

### Bug fixes

- **common:** Press Enter again while a prompt is unconfirmed, adopted by github and matrix (#99) (#101)
- **flow:** Press Enter again while a submitted send is unconfirmed (#100) (#102)
- **github:** Ignore a redelivered opening on a live row; deliver reviews in arrival order (#96) (#103)
- **github:** Write the closed row in end at once; prune closed rows after seven days (#97) (#105)
- **runtime:** Run daemon git in an agent's clone under a read-only nono profile (#68, #70) (#110)
- **runtime:** Grant git's exec-path in the git profile; a sandbox failure is never git's "no" (#109) (#112)
- **server:** Write the activation rows before the actor's first snapshot (#113) (#114)
- **runtime:** Stop ends the sandbox's whole process tree and waits for it to be empty (#107) (#117)
- **operator:** Serve the controllers' watches from an unpooled client (#129) (#139)
- **operator:** Hardening — request bound, timeout back-off, Job failure, crew lock, CA check (Spec O §22) (#141)
- Flake batch (#136, #137, #142), release-PR CI, claude 2.1.292 (#147)

## 0.2.0 - 2026-09-27

### Features

- **core:** Plugin-managed fleets, owned records and a per-agent branch (Spec L)
- **runtime:** [**breaking**] A private clone per agent (Spec N) (#66)

### Bug fixes

- **cli_serve:** Poll for the pid file as well as the endpoint on SIGTERM (#55)
- **runtime:** Detect a changed agent branch against a marker, not HEAD (#65)
- **runtime:** Decline claude's auto-mode default offer in the seeded .claude.json (#75)
- **core:** Stop and refuse a 0.1.x agent at the first pass; a failed step gets the phase `failed` (#76)
- **runtime:** Keep no clone-time default branch in the crew cache (#78)
- **runtime:** Refuse a promisor remote in the clone's config on any git version (#80)
- **runtime:** A git error is never a "no" in the clone and cache paths; trust only the workspace (#81)
- **server:** Refuse a removed plugin's apply; purge the fleets listed after the sync; threat-model example (#82)

### Upgrading

- 0.2.0 gives every agent a private clone (Spec N). Before upgrading,
  `balerix down <fleet>` every fleet (`--keep-repos` keeps its branches;
  they seed the new clones), then `up` on 0.2.0. A 0.1.x agent left
  running is stopped at the 0.2.0 daemon's first pass (its start, or the
  next `up`), never adopted under its old sandbox, and refused with
  `created by balerix 0.1 as a worktree`; `balerix status` shows it as
  `failed` with that message, and its worktree is left in place — push
  unpushed work from it, then `balerix down <fleet> --purge` (or
  `--keep-repos`) and `up`. From 0.2.0 only an agent's assigned branch
  survives `down --keep-repos`, a removal or a `branch` change; other local
  branches and the working tree go with the clone.
- An agent whose materialize or start step failed now has the phase
  `failed` (previously it kept its last phase and only the message said
  so); it is retried every pass and returns to the usual phases on the
  next success. Anything matching on phase strings should expect the new
  value.
- The pinned `claude` is now 2.1.283. From that version an interactive
  session starts in auto mode unless a settings file sets
  `permissions.defaultMode`, so an agent whose fleet and host settings set
  none now runs under the auto-mode classifier rather than in manual mode.
  Set `permissions.defaultMode` in the fleet's `settings` (or your host
  `~/.claude/settings.json`, which every agent inherits) to choose. The
  one-time "Make auto mode your default permission mode?" offer that
  2.1.282 added is declined in the agent's seeded `.claude.json` (#73).

## 0.1.1 - 2026-09-23

### Features

- **matrix:** Show and answer Claude's questions from the thread (Spec J) (#47)
- **common:** Extract balerix-plugin-common from matrix and web (Spec K)

### Bug fixes

- **examples:** Sandbox.network.mode is not a nono key (#35)

## 0.1.0 - 2026-09-11

- Initial release.
