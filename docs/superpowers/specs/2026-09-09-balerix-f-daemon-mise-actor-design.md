# Balerix — Spec F: a daemon-level mise actor

**Date:** 2026-09-09
**Status:** Approved in brainstorm 2026-09-09
**Scope:** who owns the daemon pool and when it is installed. Touches
`balerix-core` (a new port), `balerix-runtime` (the system level moves out of
`install_pools`; `Cmd` gains a timeout), `balerix-server` (a new actor, the
readiness gate), the binary's wiring, and two documents.

Where this document and Spec E
(`2026-09-09-balerix-e-mise-pool-hierarchy-design.md`) disagree, this document
wins; §8 lists the corrections.

---

## 1. Problem

Spec E made every fleet's `ensure_crew` install the system level into the
daemon pool. The daemon pool is daemon-global state — one config file, one
marker, one install directory — and there is one actor per fleet, each its own
`tokio::spawn`, with passes running in `spawn_blocking`. So N fleet actors
write daemon-global state on parallel OS threads with no lock between them.

Spec E recorded the resulting race as an accepted risk. Two threads that both
find a stale marker proceed together: `write_atomic` names its temp file after
the pid, which both threads share, so one can unlink the other's in-flight file
or fail `create_new`; and both then run `mise install` into the same pool with
`MISE_STATE_DIR`/`MISE_CACHE_DIR` derived from it. Measured during Spec E: a
concurrent `mise where` failed 39 times out of 40 under that contention.

The codebase's own discipline is one actor per piece of state, single writer.
Daemon-global state written by N fleet actors breaks it. This spec restores the
discipline rather than guarding the violation with a lock.

## 2. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| F-1 | A dedicated `SystemToolchain` port in `balerix-core`, not a new method on `Materializer`. | `Materializer` is about materializing an agent or a crew. A daemon-level capability on it would grow `FakeMaterializer` and the plugin no-op wrapper a method neither has any use for. |
| F-2 | The daemon pool is installed by one `SystemPool` actor started in `Daemon::start`, mirroring `PluginHost`. | Single writer restores the codebase's actor discipline. It also moves the install log out of whichever crew happened to trigger it. |
| F-3 | The API serves immediately; fleet actors gate on readiness. | Daemon availability must not depend on a download. Gating the passes rather than the listener keeps `balerix status` usable while the pool fills. |
| F-4 | Readiness is a **live condition**, re-evaluated at the top of every pass — not a latch. | Fail-closed means no agent runs against a pool that does not match the declared table. A latch reintroduces fail-open: a failed pin bump would silently hand every new agent the old version while the daemon reported itself healthy. A latch also keeps asserting ready over a pool deleted underneath it. |
| F-5 | Readiness requires the marker to match **and** the pool to exist. | The marker records only what was rendered. Marker present plus pool wiped would otherwise read as ready, which F-4 makes load-bearing. |
| F-6 | Every attempt gets a timeout. `Cmd::run` has none today. | Without one the first attempt can hang forever and no retry ever fires — the actor would sit in a single stalled install indefinitely. The timeout exists to guarantee the loop keeps turning, not to bound a countdown to failure. |
| F-7 | A failed install is **never fatal**, at startup or later. The daemon stays up, publishes unready with the reason, and retries on the next tick. There is no startup/mid-flight distinction. | This is meant to run under Kubernetes, where exiting is the wrong idiom: the kubelet restarts the container and a permanently bad table becomes CrashLoopBackOff, which churns the daemon and reports worse than a pod that stays up and fails its readiness probe. Staying up also keeps the API available to diagnose with, and agents in tmux are untouched either way since the tmux server is a separate process. |
| F-8 | Plugin installs into the daemon pool are **out of scope** and stay unserialized. | Spec E's E-7 left plugins unchanged deliberately. Routing them through this actor is a larger change touching the plugin subsystem; the narrower residual is documented instead (§8). |

## 3. The port and what moves

```rust
// balerix-core, alongside the existing five ports. Synchronous, like the rest.
pub trait SystemToolchain: Send + Sync {
    /// Installs the system table into the daemon pool if it is not already
    /// there. Cheap and idempotent when nothing has changed.
    fn ensure_system_pool(&self) -> Result<(), MaterializeError>;
}
```

`Runtime` implements it in `balerix-runtime`; the binary wires it like the
other ports; `balerix-server` receives it and never imports the runtime.

The implementation is Spec E's system level lifted out verbatim: render the
system table, `install_level` into `mise_data_dir()` with no parents, guarded
by `system_installed_marker()`. Two changes:

- The log moves from the triggering crew's `logs/mise.pools.log` to the
  daemon's own directory, `server_dir()/logs/mise.system.log`.
- `Ok(())` means the pool **is** ready, so the implementation verifies the pool
  directory exists rather than trusting the marker alone (F-5). A marker that
  matches a missing pool is treated as stale and reinstalled.

`Runtime::install_pools` loses its first `install_level` call and becomes fleet
and crew only. `system_tools` and `system_mise_toml_generated` move under this
port's ownership. `CrewTools`, `Materializer::ensure_crew` and the reconciler
are untouched.

## 4. The actor

`Daemon::start` creates the readiness channel, spawns the `SystemPool` actor,
spawns the fleet actors, and returns. The API serves immediately.

The actor's loop is deliberately thin, because the marker already does change
detection:

- At startup, and again on every resync tick, call `ensure_system_pool` on a
  blocking thread.
- Publish the outcome on a `watch` channel carried in `Shared`.

`Shared`, not `Ports`. `Ports` is built at five separate call sites and
`PluginHost::start` rebuilds it field by field, so a field there means editing
all of them. `Shared` is created by `actor::shared()` inside `Daemon::start` —
exactly where the actor is spawned — and the plugin host forwards it untouched,
so the channel is born where it is needed and no construction site changes.

No filesystem watcher and no new dependency. When the rendered table hashes to
what the marker holds and the pool exists, the call returns before touching
anything, so a tick costs one small file read and one hash. Staleness is
bounded by one resync interval, which matters only for a hand-edited
`$XDG_CONFIG_HOME/balerix/mise.toml` — the embedded default can only change
across an upgrade, which is a restart.

```rust
// balerix-server, beside `Shared` in actor.rs. The receiver goes in `Shared`.
pub enum SystemPoolState {
    Pending,
    Ready,
    Unready { reason: String },
}
```

This state is the actor's own and the daemon log's. Exposing it through the API
is out of scope here but has a known future consumer: a Kubernetes readiness
probe needs exactly this, read over HTTP. When that arrives it should read this
channel rather than introduce a second notion of readiness (§9).

## 5. Readiness and the fleet gate

Fleet actors read the receiver at the top of **every** pass (F-4).

- `Ready` — the pass proceeds.
- `Pending` or `Unready` — the pass is **skipped, not failed**. A fleet is not
  at fault for a daemon-level condition, so this must not mark crews failed or
  touch restart counters.

A skipped pass waits on the readiness channel changing, not only on the next
resync tick. Otherwise a pool that becomes ready two seconds after startup
would still leave every fleet idle for a full resync interval. The wait is
whichever comes first: readiness changes, or the tick arrives.

The plugin fleet gates identically, with no special case: it is an ordinary
actor, and plugins install into this same pool.

Gating the passes is what makes the live check meaningful. If fleets proceeded
against a stale pool, re-checking would change nothing and readiness would be a
latch by another name.

## 6. Timeout and retries

**Timeout.** `Cmd` gains an optional timeout, set initially only by the system
install. The implementation must be pipe-safe: today `output()` drains the
child's stdout and stderr, and a naive `try_wait` poll would deadlock on a
child that fills a pipe buffer. Run `wait_with_output` on its own thread, wait
on a channel with a deadline, and kill the child by pid on expiry. Dependency
free, and capture behaviour is unchanged.

Default per-attempt timeout: ten minutes, tunable in the actor's config. A cold
`claude` download is slow, and the timeout only needs to be shorter than
"forever" for the loop to keep turning.

**Retries are the tick.** One attempt per tick, retried indefinitely; there is
no separate retry budget and no backoff to tune. Retries are already
idempotent, since the marker is written only on success. The actor's loop is
sequential, so an attempt runs to completion or timeout before the next tick is
considered — attempts never overlap, and a slow attempt simply delays the next
one rather than stacking.

**Failure is never fatal** (F-7). On any failure, at startup or later: log,
publish `Unready { reason }`, keep serving, try again next tick. There is no
fatal channel, `Daemon::start` keeps its signature, and nothing touches the
shutdown future.

The cost is explicit and accepted: a system table that never installs — a
typo'd pin, a dead network — leaves the daemon running with every fleet pass
gated and nothing reconciling, indefinitely. That is fail-closed working as
intended. It is visible in the log rather than silent, the API stays up to
diagnose with, and agents already running in tmux are untouched. Under
Kubernetes it is also the correct shape: the pod stays up and fails readiness
instead of crash-looping.

## 7. Errors and testing

**Errors.** A system-level failure is no longer a crew failure. It is the
actor's, surfaced as `Unready { reason }` and in the daemon log. Fleet and crew
pool failures keep Spec E's semantics exactly.

**Unit.**
- The actor's state machine against a fake `SystemToolchain`: succeeds; fails
  twice then succeeds; always fails. Assert the readiness transitions, that one
  attempt runs per tick, and that a persistently failing install keeps the
  actor retrying and the daemon alive rather than terminating anything.
- A failure at the very first attempt is treated exactly like a later one —
  unready plus a reason, then a retry — with no startup special case (F-7).
- Readiness drops to `Unready` on a mid-flight failure and recovers on a later
  success (F-4) — the test that would have caught the latch.
- `ensure_system_pool` treats a matching marker with a missing pool as stale
  (F-5).
- A gated fleet pass is skipped, not failed: no crew marked failed, no restart
  counter touched.
- `Cmd`'s timeout kills a long-running child and returns a timeout error, and
  does not deadlock on a child that writes more than a pipe buffer.

**Integration** (`balerix-runtime`, real tools, gated by
`BALERIX_REQUIRE_TOOLS`):
- `ensure_crew` no longer writes the daemon pool: snapshot it before and after
  and assert it is unchanged.
- Spec E's existing pool tests still pass with the system level removed from
  `install_pools`.

**End to end.** The daemon starts, the pool becomes ready, fleets reconcile.
The existing journey covers the happy path once the gate is in place.

**Docs.** ARCHITECTURE.md: the pool-hierarchy bullet names the daemon-level
owner and the gate. THREAT-MODEL.md: retire the accepted-risk line Spec E added
for the cross-fleet race and replace it with the narrower plugin-versus-system
residual (§8).

## 8. Corrections to Spec E

- §5's install table: the system level no longer "runs in `ensure_crew`,
  first". It runs in the `SystemPool` actor, at startup and on each resync tick.
- §7's error semantics — "a failed pool install is a `MaterializeError::Tool`
  from `ensure_crew` … retried by the next crew" — still describes the fleet
  and crew levels. The system level is no longer in that path.
- The accepted-risk line for the cross-fleet system-pool race is retired: with
  a single writer the race cannot occur. It is replaced by a narrower one —
  **a plugin install and a system install can still contend on the daemon
  pool**, since plugin installs run `mise install` with the pool as their
  `MISE_DATA_DIR` and remain outside this actor (F-8). They do not share a
  config path, so only the install contention applies, not the `write_atomic`
  temp-file collision.

Closes #43. Closes the system-level half of #45.

## 9. Deliberately deferred

- Routing plugin installs through the actor (F-8).
- A readiness endpoint over HTTP. Wanted for a Kubernetes readiness probe, and
  the reason this spec keeps `SystemPoolState` in a channel rather than a local
  variable: the probe should read that channel, not invent a second notion of
  readiness. Note the probe would report the *pool*, which is only one input to
  whether the daemon is serving usefully.
- Applying `Cmd`'s new timeout to any other tool call. The primitive gains the
  capability; only the system install sets it. Widening it is its own decision.
- Pruning the daemon pool, still, as in Spec E §9.

## 10. Done when

- `mise run check` and `mise run test-it` pass, including the new unit tests
  and the "`ensure_crew` does not write the daemon pool" integration test.
- A daemon with two fleets starts, installs the daemon pool exactly once, and
  both fleets reconcile after readiness flips.
- Deleting the daemon pool under a running daemon drops it to `Unready` at the
  next tick and it reinstalls, rather than reporting ready over an empty
  directory.
- An unsatisfiable system table leaves the daemon running and unready with the
  install error in its log, whether it is present at startup or introduced
  later, and the actor keeps retrying. Nothing exits.
- The two documents reflect §3, §6 and §8.
