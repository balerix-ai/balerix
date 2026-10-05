# Spike: the lost kube request (balerix#129)

Date: 2026-10-05. Branch `spike/hyper-lost-request` (from `main` at f4a96d6).
Nothing here is meant to merge. The branch holds the instrumentation, the run
and analysis scripts, a standalone reproducer, and this write-up.

## TL;DR

- **Reproduced.** With hyper's internal tracing on, 8 of 10 full `controllers_it` runs
  lost at least one request (29 lost requests in total). The first run lost one.
- **Mechanism (high confidence).** The HTTP/1 connection's `want` flag goes stale:
  1. The idle connection task finds its request queue empty.
  2. Before it calls `taker.want()`, a caller on another thread runs `give()` and queues a request.
  3. The task's `want()` lands after the `give()`, so the task takes the request with the flag still `Want`.
  4. Nothing clears the flag while that request is in flight, so `SendRequest::is_ready()` stays `true`.
  5. hyper-util's legacy client returns a connection to the idle pool when a response head arrives, if `is_ready()` is true. So it pools the connection while the response body is still streaming.
  6. When the response is a kube **watch** (`timeoutSeconds=290`), the next request checked out onto that connection waits behind the watch. It is not written for up to 290 s, so to the caller it is lost.
- **Upstream (known, not fixed).** This is hyperium/hyper#4207 (open, 2026-09-28). The fix
  is in hyperium/hyper#4208 (a draft) and seanmonstar/want#6 (`Taker::unwant`, open). None of it is
  released.
  - hyper 1.11.1 and hyper-util 0.1.21 are the latest releases, so there is no version to bump to.
  - hyper `master` has no change to this code.
  - The fix those PRs propose is the same one-liner this spike derived on its own: withdraw the `want` when the dispatcher takes a request.
- **The fix works here.** It is applied in this spike as a vendored patch.
  - In the operator: 10 runs lost 0 requests, against 29 lost in 8 of 10 runs without it. Load was comparable.
  - In the standalone reproducer: stock crates lost 30 requests in 3 × 60 s, and the fixed crates lost 0.
- **New relative to #4207.** #4207 describes latency (a probe waits behind a stream). With
  kube, the earlier response is a watch, so the wait is up to the watch timeout. Watches can
  themselves queue behind watches, and a controller's watch that never starts means that
  kind is never reconciled. This happened in base run 5: a test failed even with the 30 s
  reconcile bound. That is worth a comment on #4207. A draft is at the end.

## Versions

| Component | Version |
|---|---|
| kube / kube-client | 4.2.0 (`client`, `runtime`, `rustls-tls`, `ring`) |
| hyper | 1.11.1 (latest release; vendored copy with instrumentation) |
| hyper-util | 0.1.21 (latest release), legacy `Client`, pool enabled |
| want | 0.3.1 (latest release) |
| tokio / tower | 1.53.2 / 0.5.3 |
| hyper-rustls / tokio-rustls / rustls | 0.27.10 / 0.26.6 / 0.23.45 |
| rustc | 1.99.0 |
| kube-apiserver / etcd | envtest-v1.34.1 |

Host: shared 24-core machine, load average 15–95 during the runs (other tenants).

## Method

### Instrumentation (`spike/hyper-lost-request/`)

- `setup.sh [--fix]` copies hyper 1.11.1 and want 0.3.1 from the cargo registry into `vendor/`
  (gitignored) and applies the patches in `patches/`:
  - `hyper-1.11.1-instrumentation.patch`
    - An id per client HTTP/1 connection, shared by its `SendRequest` and its dispatcher.
    - A `h1conn{conn=N}` span around every poll of the dispatcher.
    - A trace of the dispatcher's full state at the start and end of every poll: `Conn` state, write and read buffers, `callback`, `rx_len`, and the `want::Taker` state.
    - Traces of each `can_send`/`give()`, `try_send_request` (queued, answered), `poll_msg` (took, Pending) and `recv_msg`.
    - A global registry with `hyper::spike::dump()`. It returns one line per live connection: its age, the time since its last poll, its last request and its state after that poll.
  - `want-0.3.1-unwant.patch` adds `Taker::unwant()` (CAS `Want`→`Idle`), the same as want#6.
  - `hyper-1.11.1-unwant.patch` (`--fix` only) calls it in `dispatch::Receiver::poll_recv`
    when a request is taken, the same as hyper#4208.
- `operator/Cargo.toml` makes hyper (with its `tracing` feature) and hyper-util (with `tracing`) direct
  dependencies, and patches in `vendor/hyper` and `vendor/want`.
- `env.sh` sets `RUSTFLAGS="--cfg hyper_unstable_tracing"`, a worktree-local `CARGO_TARGET_DIR`
  and `TMPDIR`, and `ENVTEST_DIR`.
- The test harness had no subscriber. `support::spike_tracing()` is called from the envtest
  start. With `SPIKE_LOG_DIR` set, it logs to `<dir>/<pid>.log` under `RUST_LOG`
  (`info,spike=trace,balerix_operator=debug,kube_client=debug,tower_http=debug,hyper=trace,hyper_util=trace`).
- Detection keeps main's bounds but dumps before giving up:
  - `controllers::within` now `select!`s the reconcile against `sleep(30 s)`. On expiry, it logs
    `SPIKE lost request` with `hyper::spike::dump()` while the stuck future is still alive, and
    only then drops it.
  - The harness's 5 s probe timeout (`probe_once`) does the same as `SPIKE lost probe`.
- `runs.sh <label> <n>` runs the full `controllers_it` suite (nextest, envtest group of 4) n
  times. It keeps the logs of runs that logged a `SPIKE lost` dump and deletes the envtest roots after each run.
- `analyze.py [-v] <logs>` counts, per test process:
  - `race`: dispatcher polls that ended with a request queued and `Taker` in `Want` (`want()` landed after `give()`).
  - `stale_want`: dispatcher polls that ended with a request in flight or a body being read and `Taker` still in `Want`.
  - `behind_body`: requests queued onto a connection still reading an earlier response body.
  - `behind_watch`: the same, where that earlier request is a watch.
  - `spike_dumps`: the `SPIKE lost` lines.

### Reproducer (`spike/hyper-lost-request/repro/`)

The reproducer is a standalone crate on stock hyper 1.11.1 and hyper-util 0.1.21, over plain HTTP on localhost.

- **The server.** It answers `/stream` with one chunk and then holds the body open, like a watch. It answers `/quick` at once.
- **The streamers.** 256 tasks each loop: `/quick`, then at once `/stream` on the just-released pooled
  connection, hold the body for 5 s, drop it.
- **The quick tasks.** 16 tasks send `/quick` with a 2 s deadline. A miss counts as lost.

```
cd spike/hyper-lost-request/repro
cargo run --release -- 60 256 16                       # stock
../setup.sh --fix && cargo run --release \
  --config 'patch.crates-io.hyper.path="../vendor/hyper"' \
  --config 'patch.crates-io.want.path="../vendor/want"' -- 60 256 16   # fixed
```

## Results

### Operator, full `controllers_it` runs

| Build | Runs | Runs with a lost request | Lost requests (dumps) | `behind_watch` | `race` / `stale_want` polls per run (logs kept) | Test failures | Load avg (1 min) |
|---|---|---|---|---|---|---|---|
| hyper 1.11.1 + instrumentation | 10 | **8** | **29** | 51 | 21–232 / 65–363 | 1 (run 5, see below) | 15–96 |
| same + `unwant` fix (`setup.sh --fix`) | 10 (+1 with logs kept) | **0** | **0** | 0 | 57 / **0** (the kept run) | 0 | 28–70 |

With the fix, the race itself still happens (`race` = 57 in the kept run: `want()` lands after
`give()` with a request queued). But the stale `want` never survives the take (`stale_want` = 0), so no
request queues behind a watch. The `behind_body` metric (51 in base run 1, 4 in the kept fix
run) is noisy. A request can be logged as queued just after the dispatcher has, in the same poll,
finished a short body and signalled want, and the per-thread log lines interleave. Only `behind_watch` and the dumps count as losses.

Per base run, the lost requests were 2, 0, 0, 1, 4, 3, 4, 5, 8 and 2.
- Runs 4–8 overlapped with the standalone reproducer running on the same host, so their rate is inflated by the extra load.
- The two runs without a loss (2 and 3) had their logs deleted by `runs.sh`, so their `stale_want` counts are unknown.
- Main's 30 s reconcile bound and 5 s probe bound recovered all but one of the lost requests. Without the spike's dumps, 9 of 10 runs would have passed.

**Run 5's failure.** `a_terminating_namespace_skips_the_harvest` failed with "timed out waiting for the Jobs".
- The operator's own **Fleet watch** (`GET …/fleets?watch=true`) was queued onto a connection that was streaming the **Jobs watch**.
- kube-runtime's watcher has no timeout, so the Fleet controller never received an event, and no reconcile ran for the bound to catch.
- The same `watch behind watch` pattern appears in base run 1, test 4031121 (a `daemons` watch behind a `jobs` watch). That test happened to pass.
- This is the "watches never delivered" stall in the #129 diagnosis (diag2-3).

### Standalone reproducer, 3 × 60 s each, interleaved

| Build | Quick requests lost (> 2 s) | Quick requests sent | Streams opened |
|---|---|---|---|
| stock hyper 1.11.1 / hyper-util 0.1.21 | 13, 14, 3 = **30** | 16.8 M | 9 199 |
| + `Taker::unwant` fix | 0, 0, 0 = **0** | 18.0 M | 9 216 |

A lost quick request in the reproducer waits for the stream it is queued behind to be
dropped (5 s). The connection then closes and hyper-util retries the request. With a
kube watch, nothing drops the stream for up to 290 s.

## Key trace: the stuck connection's lifecycle

This is base run 1, test `the_crew_lock_holds_the_harvest_while_a_sync_runs`. The full excerpt is in
`spike/hyper-lost-request/excerpts/conn4-lifecycle.log`. Below, it is trimmed to the fields that matter (`T` is the tokio worker thread).

**1. The race (µs apart).** Connection 4 is idle and its flag is `Want`. Thread T4 checks it out for the Pods
watch, while thread T10 is polling connection 4's dispatcher:

```
45.050872 T4  HTTP{GET …/pods?watch=true&timeoutSeconds=290}: pool: reuse idle connection
45.050886 T10 h1conn{conn=4}: poll begin: keep_alive: Idle … rx_len=0 taker=Want
45.050908 T4  HTTP{GET …/pods?watch…}: can_send give=true … giver=Giver { state: Idle }   ← give(): Want→Idle
45.050927 T10 h1conn{conn=4}: poll_msg Pending (rx empty, want signalled)              ← want(): Idle→Want (stale)
45.050932 T4  HTTP{GET …/pods?watch…}: try_send_request conn=4 … queued=true
45.050967 T10 h1conn{conn=4}: poll end Pending: keep_alive: Idle … rx_len=1 taker=Want  ← request queued AND Want
```

**2. The request is taken and written, and the flag stays `Want` while it is in flight:**

```
45.051311 T10 h1conn{conn=4}: poll_msg took GET …/pods?watch=true&timeoutSeconds=290…
45.051413 T10 h1conn{conn=4}: flushed 297 bytes
45.051459 T10 h1conn{conn=4}: poll end Pending: writing: KeepAlive, keep_alive: Busy … callback=true … taker=Want
```

**3. The watch's response head arrives; its body is chunked and open-ended. hyper-util pools the connection at once:**

```
45.071272 T3  h1conn{conn=4}: incoming body is chunked encoding
45.071457 T3  h1conn{conn=4}: poll end Pending: reading: Body(Chunked { state: Start }) … keep_alive: Busy … taker=Want
45.073045 T5  HTTP{GET …/pods?watch…}: try_send_request conn=4 … answered 200 OK
45.073081 T5  HTTP{GET …/pods?watch…}: pool: put; add idle connection
```

**4. 106 ms later, a Daemon reconcile's PATCH is checked out onto connection 4. `give()` consumes the stale `Want`:**

```
45.179745 T8  reconcile{Daemon}:HTTP{PATCH …/secrets/balerix-default-admin}: pool: reuse idle connection
45.179820 T8  … can_send give=true buffered_once=true giver=Giver { state: Idle }
45.179864 T8  … try_send_request conn=4 PATCH …/secrets/balerix-default-admin … queued=true
```

**5. 30 s later, the reconcile bound fires.**
- The dump shows connection 4 last polled 29.9 s earlier, still reading the watch body.
- Its last request is the PATCH: `method=Some(GET)` is the watch's.
- It is not closing, has no EOF and nothing buffered to write.
- The PATCH was never written: socket queues 0, no audit `RequestReceived`.

```
conn=4 age=30.070s polls=9 since_poll=29.897s sends=4 since_send=29.789s
  last_send="PATCH /api/v1/namespaces/t-lock-4044860/secrets/balerix-default-admin?…"
  state=State { reading: Body(Chunked { state: Start }), writing: KeepAlive, keep_alive: Busy }
  method=Some(GET) wbuf=false rbuf=0 read_blocked=true closing=false body_rx=false body_tx=true
  callback=false rx_closed=false rx_len=0 taker=Taker { state: Want }
```

`rx_len=0` in this dump is the snapshot from the last poll, taken before the PATCH arrived.

**6. On the dispatcher's next poll (the watch delivers an event), the PATCH is still queued, and its caller is gone:**

```
15.379608 T6 h1conn{conn=4}: poll begin: reading: Body(Chunked …) … rx_len=1 rx_closed_chan=true taker=Idle
```

**The answers to the questions in #129:**
- The connection is not closing and has not seen EOF.
- It *is* mid-response from a previous request: a 290 s watch whose body it keeps reading.
- The `want` signalling is the defect: the flag was set after the request was queued, and never cleared while the watch was in flight.
- The request goes into the channel, but the dispatcher polls the channel only once the connection is idle again (`can_write_head`).

The other lost requests show the same shape. `analyze.py -v` lists each `behind_watch` request
with the watch it waited behind. Every `SPIKE lost` dump had a connection whose
`last_send` was a non-watch request while its state was reading a chunked body.

## Mechanism and confidence

**Confidence: high.**
- The traces show every step on the stuck connection, including the `give()`/`want()` interleaving.
- The one-line `unwant` fix removes the loss in both the operator and the reproducer.
- An independent upstream report (#4207) describes the same chain, with a deterministic hyper test on its second path.

The flag can also go stale through tokio's coop budget, which #4207 describes:
- `UnboundedReceiver::poll_recv` returns `Pending` with a message queued once the task's budget is spent.
- The task then signals want anyway.

The traces here show the cross-thread race. The spike did not separate how often each path happens. The fix covers both.

**Why load raises the rate.** The race window is between `inner.poll_recv()` returning `Pending`
and `taker.want()`, a few instructions. Preemption and multi-thread contention widen it.
Loss also needs the next checkout to land on that connection while it streams a long body.
The operator's many watches on one shared `Client` make that likely.

**Why 30 s bounds mask it but do not cure it.** The bounds turn a lost request into a requeue.
They do nothing for a lost *watch* request, as run 5 showed. Watch requests go through the
same pool. Until a fixed hyper is released, there are two mitigations:
- Give watches a separate `Client` (pool), so no request can queue behind a watch.
- Disable pooling for the operator's client (`pool_max_idle_per_host(0)`), at a connection-churn cost.

## Newer versions

- hyper 1.11.1 (2026-08-28), hyper-util 0.1.21 (2026-09-24) and want 0.3.1 (2023) are the newest releases on crates.io as of 2026-10-05.
- `cargo update -p hyper -p hyper-util` has nothing to move to, so the bump experiment could not run.
- On `master`, since 1.11.1, hyper's `src/client/dispatch.rs`, `src/proto/h1/dispatch.rs` and `src/client/conn/http1.rs` have only style or lint commits.
- hyper-util's `client/legacy/{client,pool}.rs` have only a `SyncWrapper` refactor (#335) and a pool idle-interval fix (#292).
- The fix is not on any branch that is released.

## Known upstream issues

- **hyperium/hyper#4207**, "HTTP/1 client: `SendRequest::is_ready()` can stay true while a request
  is in flight" (open, 2026-09-28, no comments or labels yet). It is this bug.
  - It covers hyper 1.10.1, 1.11.1 and master with hyper-util 0.1.20 and 0.1.21.
  - It describes both stale-want paths, the pool consequence, a deterministic test (the coop path) and the `unwant` fix.
  - It was found in a reverse proxy, as probes delayed behind streaming POSTs.
- **hyperium/hyper#4208** (draft PR, open): it calls `taker.unwant()` in `Receiver::poll_recv` and adds the
  test. It is blocked on want#6.
- **seanmonstar/want#6** (PR, open): it adds `Taker::unwant`.
- **Related, but a different mechanism: hyperium/hyper#4202.** An HTTP/1 request can hang forever when the
  connection closes while the request is being enqueued (a tokio mpsc send/close race). That needs a closing connection.
  Here, the connection stays open and healthy.

## Drafted upstream report

#4207 already covers this bug, so a new issue would be a duplicate. The draft below is a comment on
#4207, adding the Kubernetes impact and independent confirmation of the fix.

**Title:** (comment on hyperium/hyper#4207)

**Body:**

> We hit this independently through kube-rs, where it doesn't show as tail latency. Requests are
> lost outright.
>
> **Setup.** kube 4.2.0 → hyper-util 0.1.21 legacy `Client` → hyper 1.11.1, want 0.3.1, tokio 1.53.2,
> HTTP/1 over rustls to a local kube-apiserver. One `Client` serves a controller's watches
> (`GET …?watch=true&timeoutSeconds=290`) and its ordinary GETs and PATCHes.
>
> **What happens.**
> - A watch request takes the race described in this issue: `give()` and then the task's late `want()`,
>   microseconds apart in our traces.
> - When the watch's response head arrives, the pool gets the connection back, though its body
>   streams for up to 290 s.
> - The next request checked out onto it (a PATCH in our trace) sits in the dispatch queue for the
>   whole watch. It is never written: the apiserver audit log has no `RequestReceived`, and the
>   socket queues are empty.
> - kube's client has no read timeout by default, so the caller waits until the watch ends.
> - Watches also queue behind watches. A controller whose own watch request queues behind another
>   watch never receives an event.
>
> We traced it with per-connection instrumentation in hyper (`--cfg hyper_unstable_tracing` plus a
> connection id and a state dump). Every stuck request was queued onto a connection in
> `reading: Body(Chunked)`, `keep_alive: Busy`, `Taker { state: Want }`.
>
> **The fix works for us.** We applied the fix from #4208 and want#6 (`Taker::unwant()` when `Receiver::poll_recv` takes a request):
> - Our Kubernetes operator's envtest suite (24 tests, run 10 times each) went from 29 lost requests in 8 of 10 runs to 0 in 10 runs.
> - A standalone reproducer with stock crates (an open-ended `/stream`, 256 streamers re-using
>   just-released connections, short requests with a 2 s deadline) went from 30 lost requests per
>   3 × 60 s to 0. We can share it.
>
> It would help a lot to get want#6 released and #4208 landed. For kube users, a watch-heavy client
> is the common case, and the symptom is a controller that silently stops reconciling an object.

## Cleanup

The worktree's `target/` directory (build, logs, envtest roots) was removed at the end of the spike.
The trimmed trace is kept in `excerpts/`. The full logs are not.
