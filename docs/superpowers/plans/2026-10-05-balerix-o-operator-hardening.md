# Operator Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close #129, #130, #131, #132, #133, #134 and #138. Every kube request is bounded, repeated reconcile timeouts back off, a Job failed by its condition alone counts as failed, the crew lock covers pods and dropped reconciles and prunes itself, and a mismatched CA is re-minted once.

**Architecture:** Every change stays inside the standalone `operator/` project.
- One new module, `request_client`, builds the reconcile client with a tower timeout.
- `controllers/mod.rs` gains three things: an Event `Recorder` on `Context`, timeout counting in `within`, and a `CrewGuard` that can outlive its caller.
- `controllers/jobs.rs` runs the busy check and the create under that guard, and the busy check also lists the crew's Job pods.
- `desired/jobs.rs` gets a single `job_failed` predicate.
- `pki.rs` gets `authority_works`, which `controllers/daemon.rs` uses when it reads the CA back.

**Tech Stack:** Rust 2024, kube 4.2.0 (`client`, `runtime`; `kube::runtime::events::Recorder`), k8s-openapi 0.28.0, tower 0.5.3 (adds the `timeout` feature), tokio 1.53.1, rcgen/rustls through the existing `pki`, envtest (`kube-apiserver` + `etcd`, envtest-v1.34.1), and cargo-nextest.

**Spec:** `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` §22 (with §21.6, §5.3). Read §22 before any task.

## Global Constraints

- **Branch:** `feat/operator-hardening`, cut from `docs/spec-o-hardening`. Make one commit per task, with a conventional message (`fix(operator): …`, `feat(operator): …`) that names the issue.
- **No new crate.** The only manifest change is tower's `timeout` feature, and the commit must say why: `tower = { version = "0.5.3", default-features = false, features = ["retry", "timeout", "util"] }`.
- **Request bound:** `REQUEST_TIMEOUT = 10 s`. The reconcile bound stays at `RECONCILE_TIMEOUT = 30 s`. The probe bound stays at `PROBE_TIMEOUT = 5 s`.
- **Timeout back-off:** `QUICK_TIMEOUTS = 2`. Timeouts 1 and 2 requeue in 2 s. From timeout 3 on, the delay is `5 s × 2^(n−1)`, capped at 300 s (n = 3 gives 20 s, n = 4 gives 40 s).
- **Event wording.** All Events are type Warning, reporter controller `balerix-operator`, action `Reconcile`.
  - `ReconcileTimedOut`: note `the reconcile did not finish in 30s, 3 or more times in a row: backing off`. The note is fixed so repeats fold into one series.
  - `CrewLocked`: note `crew <fleet>/<crew> waits on <Job|Pod> <name>, being deleted since <RFC 3339>: the crew stays locked until it is gone`.
- **`RunConfig::stuck_after`:** defaults to 300 s.
- **Code rules.**
  - Non-test code uses no `unwrap`/`expect`; clippy runs with `-D warnings`.
  - Library errors are `thiserror`.
  - Comments are sparse and say *why*, in the surrounding style: short `///` docs that cite the spec section (`§22.3`).
- **Gate before every commit:** `mise run operator` (that is `scripts/operator.sh check`: fmt check, clippy, `crds` drift and the whole nextest suite except `e2e_k8s`). It takes several minutes; run it at least at each task's last step.
- **Focused runs** (run from `/workspace/operator`):
  - Unit: `cargo nextest run --config-file .config/nextest.toml --lib -E 'test(<name>)'`
  - envtest: `ENVTEST_DIR=$HOME/.local/share/mise/installs/github-kubernetes-sigs-controller-tools/envtest-v1.34.1/envtest cargo nextest run --config-file .config/nextest.toml --test controllers_it -E 'test(<name>)'`. The mise shim for `kube-apiserver` fails outside the `operator` task, so set `ENVTEST_DIR` explicitly. Without it the harness *skips*: a skipped test reports PASS, so check the output for `skip:`.
- **Host:** there is no docker here, so `e2e_k8s` cannot run locally. CI runs it.

## Review Focus

1. **An agent's own pod must never hold its crew.** It carries `balerix.ai/fleet` and `balerix.ai/crew` but no `batch.kubernetes.io/job-name`. If it held the crew, every harvest would wait forever. Pinned in Task 5's pod test.
2. **A finished Job pod (`Succeeded`/`Failed`) left behind by an old Job must not hold the crew.** Pinned in Task 5's pod test.
3. **An Event the API server refuses (no RBAC, nothing listening) must not fail or slow the reconcile.** Pinned in Task 2's unit test, which publishes against a closed port.
4. **A crew lock that another caller is waiting on must not be pruned.** If it were, two callers would hold two different mutexes for one crew. Pinned in Task 4's unit test.
5. **A watch must not get the 10 s bound.** If it did, an idle watch would be cut every 10 s. Only the reconcile client goes through `request_client`; `watch_client` is untouched. Task 1 says which call sites change. The existing controller tests (each runs well over 10 s on watches) are the regression check.

---

### Task 1: A bound on every kube request (#129, #138)

**Files:**
- Create: `operator/src/request_client.rs`
- Modify: `operator/src/lib.rs` (add `pub mod request_client;`)
- Modify: `operator/Cargo.toml` (tower features)
- Modify: `operator/src/main.rs:85`
- Modify: `operator/tests/support/envtest.rs:287`
- Modify: `operator/tests/e2e_k8s.rs:120`
- Modify: `operator/tests/support/mod.rs` (the `PROBE_TIMEOUT` doc comment)
- Modify: `operator/src/controllers/mod.rs` (the `RECONCILE_TIMEOUT` doc comment)
- Modify: `operator/src/watch_client.rs:11` (doc comment)
- Modify: `docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md` (§21.6 bullets)

**Interfaces:**
- Produces: `balerix_operator::request_client::{request_client, REQUEST_TIMEOUT}`, with `pub fn request_client(config: kube::Config) -> kube::Result<kube::Client>` and `pub const REQUEST_TIMEOUT: Duration` (10 s).

- [ ] **Step 1: Cut the branch**

```bash
cd /workspace && git switch docs/spec-o-hardening && git switch -c feat/operator-hardening
```

- [ ] **Step 2: Write the failing test.** Create `operator/src/request_client.rs` with the test only and a stub, then add `pub mod request_client;` to `operator/src/lib.rs` (alphabetical, after `pub mod pki;`).

```rust
//! The kube `Client` the reconciles run on (Spec O §22.1): kube's own
//! stack with a bound on every request, so a request lost on a pooled
//! connection (hyperium/hyper#4207, #129) fails after `REQUEST_TIMEOUT`
//! instead of waiting forever. The bound covers the response head; kube
//! reads the body after it. Watches never run on this client: they get
//! `watch_client`'s, where a bound would cut an idle watch.

use std::time::Duration;

use kube::{Client, Config};

/// Well inside `controllers::RECONCILE_TIMEOUT` (30 s): a lost request
/// fails its reconcile before the reconcile bound has to cut it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn request_client(config: Config) -> kube::Result<Client> {
    bounded_client(config, REQUEST_TIMEOUT)
}

fn bounded_client(config: Config, _limit: Duration) -> kube::Result<Client> {
    Client::try_from(config)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use k8s_openapi::api::core::v1::ConfigMap;

    #[tokio::test]
    async fn a_request_that_gets_no_answer_fails_within_the_bound() {
        // accepts and holds every connection, reads nothing, answers nothing
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let held = tokio::spawn(async move {
            let mut sockets = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                sockets.push(socket);
            }
        });
        let client =
            bounded_client(Config::new(url.parse().unwrap()), Duration::from_millis(300)).unwrap();
        let api: kube::Api<ConfigMap> = kube::Api::namespaced(client, "ns");
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(10), api.get("c"))
            .await
            .expect("the request was not bounded")
            .unwrap_err();
        assert!(
            matches!(&error, kube::Error::Service(e) if e.is::<tower::timeout::error::Elapsed>()),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        held.abort();
    }
}
```

- [ ] **Step 3: Run it and check that it fails**

Run: `cd /workspace/operator && cargo nextest run --config-file .config/nextest.toml --lib -E 'test(a_request_that_gets_no_answer)'`
Expected: FAIL. Either it does not compile because `tower::timeout` is missing (the feature is off), or it panics with "the request was not bounded".

- [ ] **Step 4: Implement.** In `operator/Cargo.toml`, set `tower = { version = "0.5.3", default-features = false, features = ["retry", "timeout", "util"] }`. Replace `bounded_client`:

```rust
fn bounded_client(config: Config, limit: Duration) -> kube::Result<Client> {
    Ok(kube::client::ClientBuilder::try_from(config)?
        .with_layer(&tower::timeout::TimeoutLayer::new(limit))
        .build())
}
```

If kube wraps the elapsed error in a variant other than `kube::Error::Service`, keep the test's intent: assert that the error's source chain contains `tower::timeout::error::Elapsed`, and note the variant in the commit message.

- [ ] **Step 5: Run the test and check that it passes.** Same command. Expected: PASS.

- [ ] **Step 6: Route the reconcile and test clients through it.** Leave every watch client alone.

`operator/src/main.rs:85`:
```rust
    let client = balerix_operator::request_client::request_client(config)
        .context("cannot build the client")?;
```

`operator/tests/support/envtest.rs:287`:
```rust
    // every request bounded (§22.1): a lost one costs the test a retry, not 180 s
    let client = balerix_operator::request_client::request_client(config).unwrap();
```

`operator/tests/e2e_k8s.rs:120`:
```rust
    let client = balerix_operator::request_client::request_client(
        kube::Config::infer().await.unwrap(),
    )
    .unwrap();
```
Afterwards, remove any import made unused (e.g. `Client` if only `try_default` used it).

Check: `grep -rn "Client::try_from\|try_default" operator/src operator/tests` must show only `watch_client.rs` (its own test and its doc) and `controllers/mod.rs`'s test helper.

- [ ] **Step 7: Refresh the doc comments.**
- `operator/tests/support/mod.rs`, the `PROBE_TIMEOUT` doc. Replace "A kube request is now and then lost on a pooled connection and never answered (the client has no read timeout); the probe is dropped and polled again." with:

```rust
/// How long one probe of `wait_for` or `hold_for` may take. Every request
/// is bounded at 10 s (`request_client`, §22.1); a probe gives up sooner,
/// so a lost one is polled again within the wait.
```

- `operator/src/controllers/mod.rs`, the `RECONCILE_TIMEOUT` doc:

```rust
/// How long one reconcile may run. Each request is bounded
/// (`request_client`, 10 s), but a reconcile makes many, and the runtime
/// never starts a second reconcile of an object whose first still runs: a
/// reconcile that never ends would hold its object for good.
```

- `operator/src/watch_client.rs:11`: change "Remove this, and use one `Client::try_from(config)`, once a hyper with the fix (hyperium/hyper#4208) ships." to "Remove this, and run the watches on `Client::try_from(config)`, once a hyper with the fix (hyperium/hyper#4208) ships; the reconciles keep `request_client`."

- [ ] **Step 8: Amend spec §21.6** (the bullets §22.1 says it amends). Run from `/workspace`:

```bash
python3 - <<'EOF'
p='docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md'
s=open(p).read()
pairs=[
("""- The envtest suite has load-sensitive waits (60 s `wait_for`, an
  occasional 180 s hang under four API servers on one host).""",
"""- The envtest suite has load-sensitive waits (60 s `wait_for`). A lost
  request no longer hangs a test: probes are bounded at 5 s and every
  other request at 10 s (§22.1)."""),
("""- A per-request client timeout for production (`Config::read_timeout` cuts
  idle watches too, so a tower timeout layer on non-watch requests). The lost
  requests are hyper#4207 (a stale HTTP/1 want pools a connection mid-watch);
  until a fixed hyper ships, the watches run on an unpooled client (#129).""",
"""- The lost requests are hyper#4207 (a stale HTTP/1 want pools a connection
  mid-watch); until a fixed hyper ships, the watches run on an unpooled
  client (#129). The per-request timeout is §22.1."""),
]
for old,new in pairs:
    assert old in s, old[:60]
    s=s.replace(old,new)
open(p,'w').write(s)
EOF
```

- [ ] **Step 9: Gate and commit**

Run: `cd /workspace && mise run operator`. Expected: all pass, with no `skip:` lines for the controller tests.

```bash
git add operator docs/superpowers/specs/2026-10-02-balerix-o-kubernetes-design.md
git commit -m "fix(operator): bound every reconcile and test request at 10 s (#129, #138)

tower's timeout feature (no new crate) wraps kube's client stack: a request
lost on a pooled connection fails instead of waiting forever. Watches keep
their own unpooled client, which a bound would cut."
```

---

### Task 2: Events, and back-off after repeated reconcile timeouts (#130)

**Files:**
- Modify: `operator/src/controllers/mod.rs` (Context, `within`, `bounded`, `error_policy`, tests)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces:
  - `Context::warn<K>(&self, object: &K, reason: &str, note: String)`, async, where `K: Resource` and `K::DynamicType: Default`. It publishes a Warning Event, best effort.
  - `pub const QUICK_TIMEOUTS: u32 = 2;`
  - `within(limit: Duration, object: Arc<K>, ctx: &Context, reconcile) -> Result<Action, Error>` (gains `ctx`).
  - `fn backoff(attempt: u32) -> Duration`.

- [ ] **Step 1: Write the failing tests.** In `controllers/mod.rs`'s `tests` module, replace `a_reconcile_inside_the_bound_keeps_its_result` and `a_reconcile_past_the_bound_is_dropped_and_requeued_in_two_seconds` with:

```rust
    /// Nothing listens on port 9: building the client opens no connection,
    /// and an Event published through it fails at once.
    fn context() -> Arc<Context> {
        let client =
            Client::try_from(kube::Config::new("http://127.0.0.1:9".parse().unwrap())).unwrap();
        Arc::new(Context::new(
            client,
            RunConfig::new(
                "0.2.0",
                crate::desired::common::Images::for_version("0.2.0"),
                "ns",
            ),
        ))
    }

    #[tokio::test]
    async fn a_reconcile_inside_the_bound_keeps_its_result() {
        let done = within(Duration::from_secs(30), object(), &context(), async {
            Ok(Action::requeue(Duration::from_secs(7)))
        })
        .await;
        assert_eq!(done.unwrap(), Action::requeue(Duration::from_secs(7)));
    }

    #[tokio::test]
    async fn two_timeouts_requeue_in_two_seconds_and_the_third_backs_off() {
        let ctx = context();
        let limit = Duration::from_millis(20);
        let mut delays = Vec::new();
        for _ in 0..4 {
            // a request that is never answered
            let lost = std::future::pending::<Result<Action, Error>>();
            let error = within(limit, object(), &ctx, lost).await.unwrap_err();
            assert!(matches!(error, Error::TimedOut(d) if d == limit), "{error}");
            delays.push(error_policy(object(), &error, ctx.clone()));
        }
        // the third and fourth also published the Event; that it could not
        // be written cost nothing
        let secs = |s| Action::requeue(Duration::from_secs(s));
        assert_eq!(delays, vec![secs(2), secs(2), secs(20), secs(40)]);
        // a reconcile that ends well starts the count again
        reconciled(&ctx, object().as_ref());
        let lost = std::future::pending::<Result<Action, Error>>();
        let error = within(limit, object(), &ctx, lost).await.unwrap_err();
        assert_eq!(error_policy(object(), &error, ctx), secs(2));
    }

    #[test]
    fn the_backoff_doubles_from_five_seconds_to_five_minutes() {
        assert_eq!(backoff(1), Duration::from_secs(5));
        assert_eq!(backoff(3), Duration::from_secs(20));
        assert_eq!(backoff(7), Duration::from_secs(300));
        assert_eq!(backoff(60), Duration::from_secs(300));
    }
```

- [ ] **Step 2: Run them and check that they fail**

Run: `cd /workspace/operator && cargo nextest run --config-file .config/nextest.toml --lib -E 'test(/timeout|backoff|inside_the_bound/)'`
Expected: does not compile (`within` takes three arguments; `backoff` is not defined).

- [ ] **Step 3: Implement.** In `controllers/mod.rs`:

Imports: add `use kube::runtime::events::{Event, EventType, Recorder, Reporter};`.

Context field, after `errors`:
```rust
    /// Warning Events on the objects (§22.5).
    recorder: Recorder,
```
In `Context::new`, before `Self {`:
```rust
        let recorder = Recorder::new(
            client.clone(),
            Reporter {
                controller: "balerix-operator".into(),
                instance: None,
            },
        );
```
and add the field `recorder,` to the struct literal.

Methods on `Context`:
```rust
    /// A Warning Event on `object` (§22.5). Best effort: an Event the API
    /// server refuses (no RBAC, a lost request) is logged and dropped.
    pub async fn warn<K>(&self, object: &K, reason: &str, note: String)
    where
        K: Resource,
        K::DynamicType: Default,
    {
        let event = Event {
            type_: EventType::Warning,
            reason: reason.into(),
            note: Some(note),
            action: "Reconcile".into(),
            secondary: None,
        };
        let reference = object.object_ref(&Default::default());
        if let Err(e) = self.recorder.publish(&event, &reference).await {
            tracing::warn!(
                namespace = %object.namespace().unwrap_or_default(),
                name = %object.name_any(),
                reason,
                "cannot publish the Event: {e}"
            );
        }
    }

    /// One more consecutive failure of the object; the count.
    fn count_error(&self, key: &str) -> u32 {
        let mut errors = self.errors.lock().unwrap_or_else(|e| e.into_inner());
        let n = errors.entry(key.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        *n
    }

    fn errors_of(&self, key: &str) -> u32 {
        self.errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .copied()
            .unwrap_or(0)
    }
```

Replace `bounded` and `within`:
```rust
/// Timeouts in a row that still requeue in 2 s: one most likely means one
/// lost request (§22.2).
pub const QUICK_TIMEOUTS: u32 = 2;

/// A controller's reconcile under `RECONCILE_TIMEOUT`, for `.run(...)`.
pub fn bounded<K, F, Fut>(
    reconcile: F,
) -> impl FnMut(Arc<K>, Arc<Context>) -> BoxFuture<'static, Result<Action, Error>>
where
    K: Resource + Send + Sync + 'static,
    K::DynamicType: Default,
    F: Fn(Arc<K>, Arc<Context>) -> Fut,
    Fut: Future<Output = Result<Action, Error>> + Send + 'static,
{
    move |object, ctx| {
        let reconcile = reconcile(object.clone(), ctx.clone());
        Box::pin(async move { within(RECONCILE_TIMEOUT, object, &ctx, reconcile).await })
    }
}

/// `reconcile`, or `TimedOut` once it has run for `limit`: the future is
/// dropped, and whatever it held with it. Counts the timeout, which
/// `error_policy` reads, and from the third in a row says so on the
/// object (§22.2).
pub async fn within<K>(
    limit: Duration,
    object: Arc<K>,
    ctx: &Context,
    reconcile: impl Future<Output = Result<Action, Error>>,
) -> Result<Action, Error>
where
    K: Resource,
    K::DynamicType: Default,
{
    match tokio::time::timeout(limit, reconcile).await {
        Ok(result) => result,
        Err(_) => {
            let n = ctx.count_error(&object_key(object.as_ref()));
            tracing::warn!(
                namespace = %object.namespace().unwrap_or_default(),
                name = %object.name_any(),
                timeouts = n,
                "the reconcile did not finish in {limit:?}: dropped and requeued"
            );
            if n > QUICK_TIMEOUTS {
                // fixed, so the Recorder folds the repeats into one series
                let note = format!(
                    "the reconcile did not finish in {limit:?}, {} or more times in a row: backing off",
                    QUICK_TIMEOUTS + 1
                );
                ctx.warn(object.as_ref(), "ReconcileTimedOut", note).await;
            }
            Err(Error::TimedOut(limit))
        }
    }
}
```

Replace `error_policy`'s body after the `Waiting` arm:
```rust
    // `within` counted it: the first ones are lost requests, not the object
    if let Error::TimedOut(_) = error {
        let n = ctx.errors_of(&key);
        return Action::requeue(if n <= QUICK_TIMEOUTS {
            Duration::from_secs(2)
        } else {
            backoff(n)
        });
    }
    let attempt = ctx.count_error(&key);
    let delay = backoff(attempt);
    tracing::warn!(namespace = %namespace, name = %name, attempt, "reconcile failed: {error}");
    Action::requeue(delay)
}

/// 5 s, doubling per consecutive failure of the same object, at most 5 min.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((5u64 << attempt.saturating_sub(1).min(6)).min(300))
}
```
Move the doc line "5 s, doubling…" off `error_policy` and give it: `/// The requeue after a failed reconcile: `Waiting` in 2 s, `TimedOut` per §22.2, anything else by `backoff`.`

Also update the `TimedOut` variant's doc: `/// The reconcile ran past `RECONCILE_TIMEOUT` and was dropped: requeued in 2 s twice in a row, then backed off with an Event (§22.2).`

- [ ] **Step 4: Run the tests and check that they pass.** Same command. Expected: PASS. Also run `cargo clippy --all-targets -- -D warnings` from `operator/`: the four controllers' `.run(bounded(reconcile), …)` must still compile, since all four kinds have `DynamicType = ()`.

- [ ] **Step 5: Gate and commit**

Run: `cd /workspace && mise run operator`. Expected: PASS.

```bash
git add operator/src/controllers/mod.rs
git commit -m "fix(operator): back off after repeated reconcile timeouts and say so in an Event (#130)"
```

---

### Task 3: A Job failed by its condition alone is failed (#131)

**Files:**
- Modify: `operator/src/desired/jobs.rs` (`job_failed`, `job_outcome`, tests)
- Modify: `operator/src/controllers/jobs.rs` (`unfinished`, tests)
- Modify: `operator/tests/support/mod.rs` (`expire_job`)
- Test: `operator/tests/controllers_it.rs`

**Interfaces:**
- Produces: `pub fn job_failed(job: &Job) -> bool` in `balerix_operator::desired::jobs`, and the support helper `pub async fn expire_job(client: &Client, namespace: &str, name: &str)`.

- [ ] **Step 1: Write the failing unit tests.** In `desired/jobs.rs`'s `tests` module, after `an_outcome_is_read_from_the_job_and_its_pods_termination_message`:

```rust
    #[test]
    fn a_job_whose_pod_was_never_created_fails_by_its_condition() {
        let images = images();
        let wanted = daemon_pool_job(&ctx(&images)).unwrap();
        // a quota refused the pod; the deadline failed the Job: `failed` is 0
        let refused = observed(
            &wanted,
            json!({ "conditions": [{ "type": "Failed", "status": "True",
                "reason": "DeadlineExceeded", "message": "Job was active longer than specified deadline" }] }),
        );
        assert!(job_failed(&refused));
        assert_eq!(
            job_outcome(Some(&refused), &[], &wanted),
            JobOutcome::Failed(
                "the job failed: DeadlineExceeded: Job was active longer than specified deadline"
                    .into()
            )
        );
        // a condition that is not True decides nothing
        let not_yet = observed(
            &wanted,
            json!({ "conditions": [{ "type": "Failed", "status": "False" }] }),
        );
        assert!(!job_failed(&not_yet));
        assert_eq!(job_outcome(Some(&not_yet), &[], &wanted), JobOutcome::Running);
    }
```

In `controllers/jobs.rs`'s `tests` module, add:
```rust
    #[test]
    fn a_job_failed_by_its_condition_alone_is_finished() {
        let job: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1", "kind": "Job", "metadata": { "name": "j" },
            "status": { "conditions": [{ "type": "Failed", "status": "True", "reason": "DeadlineExceeded" }] }
        })).unwrap();
        assert!(!unfinished(&job));
    }
```

- [ ] **Step 2: Run them and check that they fail**

Run: `cd /workspace/operator && cargo nextest run --config-file .config/nextest.toml --lib -E 'test(/condition_alone|never_created/)'`
Expected: does not compile (`job_failed` is not defined).

- [ ] **Step 3: Implement.** In `desired/jobs.rs`, above `job_outcome`:

```rust
/// Whether a Job failed: a pod counted as failed, or the Job controller's
/// `Failed` condition. A Job whose pod was never created (a ResourceQuota,
/// an admission webhook) reaches `Failed=True/DeadlineExceeded` under
/// `activeDeadlineSeconds` with `failed: 0` (§22.3).
pub fn job_failed(job: &Job) -> bool {
    let status = job.status.as_ref();
    status.and_then(|s| s.failed).unwrap_or(0) >= 1
        || status
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|c| c.iter().any(|c| c.type_ == "Failed" && c.status == "True"))
}
```
In `job_outcome`, replace `if status.and_then(|s| s.failed).unwrap_or(0) >= 1 {` with `if job_failed(job) {`.

In `controllers/jobs.rs`, import `use crate::desired::jobs::{job_failed, job_outcome};` and replace `unfinished`'s body:
```rust
/// Whether a Job is still running: created and not yet succeeded or failed.
fn unfinished(job: &Job) -> bool {
    job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) == 0 && !job_failed(job)
}
```

- [ ] **Step 4: Run the unit tests and check that they pass.** Same command. Expected: PASS.

- [ ] **Step 5: Write the failing envtest test.** In `operator/tests/support/mod.rs`, after `finish_job`:

```rust
/// The Job controller's part for a Job whose pod was never created (a
/// quota or a webhook refused it) and whose deadline passed:
/// `Failed=True/DeadlineExceeded` with `failed: 0` and no pod. 1.34 wants
/// `startTime` and `FailureTarget=True` before `Failed=True`.
pub async fn expire_job(client: &Client, namespace: &str, name: &str) {
    use k8s_openapi::api::batch::v1::Job;
    use kube::api::{Patch, PatchParams};
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let now = k8s_openapi::jiff::Timestamp::now().to_string();
    let why = "Job was active longer than specified deadline";
    let status = serde_json::json!({ "status": { "startTime": now, "conditions": [
        { "type": "FailureTarget", "status": "True", "reason": "DeadlineExceeded", "message": why, "lastTransitionTime": now },
        { "type": "Failed", "status": "True", "reason": "DeadlineExceeded", "message": why, "lastTransitionTime": now }
    ] } });
    jobs.patch_status(name, &PatchParams::default(), &Patch::Merge(&status))
        .await
        .unwrap();
}
```
If the API server refuses this patch, read its message and add exactly the field it names (e.g. a `failed: 0` or `active: 0`). Keep `failed` absent or 0: that is the case under test.

In `controllers_it.rs`, add `expire_job` to the `support::{…}` import. Then add this test after `a_failed_pool_job_is_reported_and_retried_after_the_delay`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_job_whose_pod_was_never_created_fails_at_its_deadline_and_is_retried() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "expired").await;
    let client = env.client.clone();
    let daemons: Api<Daemon> = Api::namespaced(client.clone(), &ns);
    daemons
        .create(&PostParams::default(), &Daemon::new("default", daemon_spec()))
        .await
        .unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let first = wait_for("the pool Job", Duration::from_secs(60), || async {
        jobs.get_opt("balerix-default-pool").await.unwrap()
    })
    .await;

    // a quota refused its pod; the deadline failed it with `failed: 0`
    expire_job(&client, &ns, "balerix-default-pool").await;
    let status = wait_for(
        "SystemToolsReady=False for the expired Job",
        Duration::from_secs(10),
        || async {
            let s = daemons.get("default").await.unwrap().status?;
            let tools = condition(&s.conditions, "SystemToolsReady");
            (tools.status == "False" && tools.reason == "PoolSyncFailed").then_some(s)
        },
    )
    .await;
    assert_eq!(
        condition(&status.conditions, "SystemToolsReady").message,
        "the job failed: DeadlineExceeded: Job was active longer than specified deadline"
    );

    clock.advance(31);
    wait_for("the retry", Duration::from_secs(60), || async {
        let j = jobs.get_opt("balerix-default-pool").await.unwrap()?;
        (j.metadata.uid != first.metadata.uid).then_some(j)
    })
    .await;
    let attempts = daemons
        .get("default")
        .await
        .unwrap()
        .metadata
        .annotations
        .unwrap()["balerix.ai/attempts"]
        .clone();
    assert_eq!(attempts, "{\"balerix-default-pool\":2}");
    operator.abort();
}
```

- [ ] **Step 6: Confirm the envtest test fails without the fix, then passes with it.** Run `git stash push operator/src` and run the test: expected FAIL ("timed out waiting for SystemToolsReady=False for the expired Job"). Then run `git stash pop` and run it again: expected PASS. Use the envtest command in Global Constraints, with `-E 'test(a_job_whose_pod_was_never_created)'`.

- [ ] **Step 7: Gate and commit**

Run: `cd /workspace && mise run operator`. Expected: PASS.

```bash
git add operator
git commit -m "fix(operator): a Job failed by its Failed condition alone is failed (#131)"
```

---

### Task 4: The crew lock outlives a dropped reconcile and prunes itself (#132 gap 3, #133)

**Files:**
- Modify: `operator/src/controllers/mod.rs` (`CrewGuard`, `Context::lock_crew`, the `crew_locks` field, `Error::Join`, tests)
- Modify: `operator/src/controllers/jobs.rs` (`ensure_job`'s `Absent` arm, `check_and_create`, `crew_busy` signature)

**Interfaces:**
- Consumes: `context()` test helper from Task 2.
- Produces:
  - `pub async fn Context::lock_crew(&self, namespace: &str, fleet: &str, crew: &str) -> CrewGuard`
  - `pub struct CrewGuard` with `pub fn hold_through<T: Send + 'static>(self, work: impl Future<Output = T> + Send + 'static) -> tokio::task::JoinHandle<T>`
  - `Error::Join(tokio::task::JoinError)`
  - `async fn check_and_create(client: Client, namespace: String, crew: Option<(String, String)>, wanted: Job) -> Result<Created, Error>`, where `enum Created { Made, Raced, Busy, NamespaceTerminating }`. Task 5 changes `Busy` to `Busy(Vec<Holder>)`.
  - `crew_busy(client: &Client, namespace, fleet, crew, except)` (takes a `Client`, not the `Context`).
- Removes: `Context::crew_lock`.

- [ ] **Step 1: Write the failing tests** in `controllers/mod.rs`'s `tests` module:

```rust
    #[tokio::test]
    async fn a_released_crew_lock_leaves_no_entry_but_one_awaited_is_kept() {
        let ctx = context();
        let held = ctx.lock_crew("ns", "f", "c").await;
        assert_eq!(ctx.crew_lock_entries(), 1);
        // another caller waits on it
        let mut waiting = std::pin::pin!(ctx.lock_crew("ns", "f", "c"));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        drop(held);
        assert_eq!(ctx.crew_lock_entries(), 1, "the waiter's lock is kept");
        drop(waiting.await);
        assert_eq!(ctx.crew_lock_entries(), 0);
        // crews do not share a lock
        let a = ctx.lock_crew("ns", "f", "a").await;
        let b = tokio::time::timeout(Duration::from_millis(100), ctx.lock_crew("ns", "f", "b"))
            .await
            .expect("another crew's lock is free");
        drop((a, b));
        assert_eq!(ctx.crew_lock_entries(), 0);
    }

    #[tokio::test]
    async fn work_run_through_a_crew_lock_holds_it_after_its_caller_is_dropped() {
        let ctx = context();
        let (answer, answered) = tokio::sync::oneshot::channel::<()>();
        // a reconcile that sent a create and was dropped at its bound
        let caller = {
            let ctx = ctx.clone();
            async move {
                let guard = ctx.lock_crew("ns", "f", "c").await;
                guard
                    .hold_through(async move {
                        let _ = answered.await;
                    })
                    .await
            }
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(50), caller)
                .await
                .is_err()
        );
        // the create is still at the API server: the crew stays locked
        assert!(
            tokio::time::timeout(Duration::from_millis(50), ctx.lock_crew("ns", "f", "c"))
                .await
                .is_err()
        );
        answer.send(()).unwrap();
        let after = tokio::time::timeout(Duration::from_secs(5), ctx.lock_crew("ns", "f", "c"))
            .await
            .expect("the lock is released once the work is answered");
        drop(after);
        assert_eq!(ctx.crew_lock_entries(), 0);
    }
```

- [ ] **Step 2: Run them and check that they fail**

Run: `cd /workspace/operator && cargo nextest run --config-file .config/nextest.toml --lib -E 'test(/crew_lock/)'`
Expected: does not compile (`lock_crew` and `crew_lock_entries` are not defined).

- [ ] **Step 3: Implement the guard.** In `controllers/mod.rs`:

```rust
type CrewLocks = Arc<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>;

/// One crew's lock, held (§5.3). Dropping it releases the lock and, when
/// nobody else holds or awaits it, removes the crew's entry (§22.3, #133).
pub struct CrewGuard {
    key: String,
    lock: Arc<tokio::sync::Mutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    locks: CrewLocks,
}

impl CrewGuard {
    /// Runs `work` in a task of its own that keeps the lock until `work`
    /// ends: a reconcile dropped at its bound while its create is at the
    /// API server does not let a second one see the crew idle (§22.3).
    pub fn hold_through<T: Send + 'static>(
        self,
        work: impl Future<Output = T> + Send + 'static,
    ) -> tokio::task::JoinHandle<T> {
        tokio::spawn(async move {
            let out = work.await;
            drop(self);
            out
        })
    }
}

impl Drop for CrewGuard {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        // the map's and this guard's: nobody holds or awaits it. A caller
        // that clones it takes the map's mutex first, so it is counted
        if Arc::strong_count(&self.lock) == 2
            && locks.get(&self.key).is_some_and(|l| Arc::ptr_eq(l, &self.lock))
        {
            locks.remove(&self.key);
        }
    }
}
```

Change the `crew_locks` field to `crew_locks: CrewLocks,`, initialize it with `crew_locks: Arc::default(),`, and update its doc to say it holds only the crews that are locked or awaited. One leftover is possible: a waiter dropped in the instant between its holder's release and its own wake-up leaves the entry, and the next lock of that crew removes it. Write that in the doc too.

Replace `crew_lock` with:
```rust
    /// Takes one crew's lock: the same lock for every caller while anyone
    /// holds or awaits it.
    pub async fn lock_crew(&self, namespace: &str, fleet: &str, crew: &str) -> CrewGuard {
        let key = format!("{namespace}/{fleet}/{crew}");
        let lock = self
            .crew_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .clone();
        let guard = lock.clone().lock_owned().await;
        CrewGuard {
            key,
            lock,
            guard: Some(guard),
            locks: self.crew_locks.clone(),
        }
    }

    #[cfg(test)]
    fn crew_lock_entries(&self) -> usize {
        self.crew_locks.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
```

Add to `Error`:
```rust
    /// The task that held a crew's lock across a create ended early.
    #[error("the Job create task ended early: {0}")]
    Join(#[from] tokio::task::JoinError),
```

- [ ] **Step 4: Run the unit tests and check that they pass.** Same command. Expected: PASS.

- [ ] **Step 5: Use it in `ensure_job`.** In `controllers/jobs.rs`, change `crew_busy` to take `client: &Client` instead of `ctx: &Context` (`Api::namespaced(client.clone(), namespace)`; add `use kube::Client;`). Add:

```rust
/// What the busy check and the create came to.
enum Created {
    Made,
    /// Made by a reconcile that raced this one: it is running.
    Raced,
    Busy,
    NamespaceTerminating,
}

/// The crew's busy check and the create, under the crew's lock when
/// `crew` is given. Owns everything, so it can run in a task of its own.
async fn check_and_create(
    client: Client,
    namespace: String,
    crew: Option<(String, String)>,
    wanted: Job,
) -> Result<Created, Error> {
    if let Some((fleet, crew)) = &crew
        && crew_busy(&client, &namespace, fleet, crew, &wanted.name_any()).await?
    {
        return Ok(Created::Busy);
    }
    let jobs: Api<Job> = Api::namespaced(client, &namespace);
    match jobs.create(&PostParams::default(), &wanted).await {
        Ok(_) => Ok(Created::Made),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(Created::Raced),
        Err(kube::Error::Api(e)) if namespace_terminating(&e) => Ok(Created::NamespaceTerminating),
        Err(e) => Err(e.into()),
    }
}
```

Replace the whole `JobOutcome::Absent => { … }` arm:
```rust
        JobOutcome::Absent => {
            // the crew's lock across the check and the create (§5.3), kept
            // by a task of its own until the create is answered (§22.3)
            let work = check_and_create(
                ctx.client.clone(),
                namespace.clone(),
                crew_lock.map(|(fleet, crew)| (fleet.to_string(), crew.to_string())),
                wanted,
            );
            let created = match crew_lock {
                Some((fleet, crew)) => {
                    ctx.lock_crew(&namespace, fleet, crew)
                        .await
                        .hold_through(work)
                        .await??
                }
                None => work.await?,
            };
            match created {
                Created::Busy => {
                    tracing::debug!(job = %name, "waiting: another Job of the crew runs");
                    Ok(Ensured::new(outcome, soon))
                }
                Created::NamespaceTerminating => {
                    tracing::warn!(namespace = %namespace, job = %name, "not created: the namespace is being deleted");
                    Ok(Ensured {
                        namespace_terminating: true,
                        ..Ensured::new(outcome, ctx.run.period)
                    })
                }
                Created::Made => {
                    tracing::info!(job = %name, attempt = attempts.get(&name).copied().unwrap_or(1), "created");
                    Ok(Ensured::new(JobOutcome::Running, soon))
                }
                Created::Raced => Ok(Ensured::new(JobOutcome::Running, soon)),
            }
        }
```
`wanted` moves into `work`. If an earlier line still borrows `wanted` after this point, clone `wanted` into `work` instead. Update the module doc's sentence about the lock: "the crew's lock in the `Context` is held across the check and the create, by a task that outlives a dropped reconcile". Also fix the `crew_locks` field doc's reference to `jobs::crew_busy` if its wording no longer fits.

- [ ] **Step 6: Run the crew-lock envtest tests** (regression; they call `ensure_job` directly): `-E 'test(/crew_lock|stale_job/)'` with the envtest command. Expected: PASS for `the_crew_lock_lets_one_of_two_racing_jobs_start`, `a_stale_job_is_deleted_in_the_foreground_and_holds_the_crew_until_gone` and `the_crew_lock_holds_the_harvest_while_a_sync_runs`.

- [ ] **Step 7: Gate and commit**

Run: `cd /workspace && mise run operator`. Expected: PASS.

```bash
git add operator/src
git commit -m "fix(operator): the crew lock outlives a dropped reconcile's create and prunes itself (#132, #133)"
```

---

### Task 5: The crew lock counts pods and says when it is stuck (#132 gaps 1 and 2)

**Files:**
- Modify: `operator/src/controllers/jobs.rs` (`Holder`, `crew_holders` replacing `crew_busy`, `stuck`, `Created::Busy(Vec<Holder>)`, the `Absent` arm, tests)
- Modify: `operator/src/controllers/mod.rs` (`RunConfig::stuck_after`)
- Test: `operator/tests/controllers_it.rs` (`job_rule` sets `stuck_after`; `crew_pod`; two tests)

**Interfaces:**
- Consumes: `Context::warn` (Task 2); `check_and_create`, `Created`, the `Absent` arm (Task 4).
- Produces:
  - `pub struct Holder { pub kind: &'static str, pub name: String, pub deleting_since: Option<i64> }`
  - `pub async fn crew_holders(client: &Client, namespace: &str, fleet: &str, crew: &str, except: &str) -> Result<Vec<Holder>, Error>`
  - `RunConfig::stuck_after: Duration` (300 s by default).

- [ ] **Step 1: Write the failing unit test** in `controllers/jobs.rs`'s `tests` module:

```rust
    #[test]
    fn the_stuck_holder_is_the_one_deleting_longest_past_the_bound() {
        let holder = |kind, name: &str, since| Holder {
            kind,
            name: name.to_string(),
            deleting_since: since,
        };
        let after = Duration::from_secs(300);
        let holders = vec![
            holder("Job", "f-c-sync", None),
            holder("Pod", "young", Some(1_000)),
            holder("Pod", "old", Some(500)),
        ];
        assert_eq!(stuck(&holders, 1_200, after), Some(&holders[2]));
        // nothing has been deleting for 300 s yet
        assert_eq!(stuck(&holders, 700, after), None);
        assert_eq!(stuck(&holders[..1], 10_000, after), None);
    }
```

- [ ] **Step 2: Run it and check that it fails**

Run: `cd /workspace/operator && cargo nextest run --config-file .config/nextest.toml --lib -E 'test(the_stuck_holder)'`
Expected: does not compile (`Holder` and `stuck` are not defined).

- [ ] **Step 3: Implement `Holder`, `crew_holders` and `stuck`.** In `controllers/jobs.rs`, replace `crew_busy` with:

```rust
/// What holds a crew (§5.3): one of its Jobs unfinished or being deleted,
/// or one of its Jobs' pods not yet finished or being deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// `Job` or `Pod`.
    pub kind: &'static str,
    pub name: String,
    /// Unix seconds of its deletion timestamp, while it is being deleted.
    pub deleting_since: Option<i64>,
}

fn deleting_since(meta: &ObjectMeta) -> Option<i64> {
    meta.deletion_timestamp.as_ref().map(|t| t.0.as_second())
}

/// What holds the crew, other than the Job `except`. A stale Job deleted
/// in the foreground is listed until its pods are gone. A pod outlives its
/// Job when the Job is collected in the background, as a dropped crew's
/// sync is (§22.3); `job-name` keeps the agents' own pods out.
pub async fn crew_holders(
    client: &Client,
    namespace: &str,
    fleet: &str,
    crew: &str,
    except: &str,
) -> Result<Vec<Holder>, Error> {
    let selector = format!("balerix.ai/fleet={fleet},balerix.ai/crew={crew}");
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let mut holders: Vec<Holder> = jobs
        .list(&ListParams::default().labels(&selector))
        .await?
        .items
        .iter()
        .filter(|j| {
            j.name_any() != except && (unfinished(j) || j.metadata.deletion_timestamp.is_some())
        })
        .map(|j| Holder {
            kind: "Job",
            name: j.name_any(),
            deleting_since: deleting_since(&j.metadata),
        })
        .collect();
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let finished = |p: &Pod| {
        matches!(
            p.status.as_ref().and_then(|s| s.phase.as_deref()),
            Some("Succeeded" | "Failed")
        )
    };
    holders.extend(
        pods.list(
            &ListParams::default().labels(&format!("{selector},batch.kubernetes.io/job-name")),
        )
        .await?
        .items
        .iter()
        .filter(|p| p.metadata.deletion_timestamp.is_some() || !finished(p))
        .map(|p| Holder {
            kind: "Pod",
            name: p.name_any(),
            deleting_since: deleting_since(&p.metadata),
        }),
    );
    Ok(holders)
}

/// The holder deleting longest, once that is `after` or more: a pod on a
/// lost node never finishes its deletion (§22.3).
fn stuck(holders: &[Holder], now: i64, after: Duration) -> Option<&Holder> {
    holders
        .iter()
        .filter(|h| h.deleting_since.is_some_and(|t| now - t >= after.as_secs() as i64))
        .min_by_key(|h| h.deleting_since)
}
```
Import `k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta`. Change `Created::Busy` to `Busy(Vec<Holder>)`. In `check_and_create`:

```rust
    if let Some((fleet, crew)) = &crew {
        let holders = crew_holders(&client, &namespace, fleet, crew, &wanted.name_any()).await?;
        if !holders.is_empty() {
            return Ok(Created::Busy(holders));
        }
    }
```
In the `Absent` arm, replace the `Created::Busy` branch:
```rust
                Created::Busy(holders) => {
                    tracing::debug!(job = %name, ?holders, "waiting: the crew is held");
                    if let (Some((fleet, crew)), Some(h)) =
                        (crew_lock, stuck(&holders, ctx.now(), ctx.run.stuck_after))
                    {
                        let since = h
                            .deleting_since
                            .and_then(|t| k8s_openapi::jiff::Timestamp::from_second(t).ok())
                            .map(|t| t.to_string())
                            .unwrap_or_default();
                        let note = format!(
                            "crew {fleet}/{crew} waits on {} {}, being deleted since {since}: the crew stays locked until it is gone",
                            h.kind, h.name
                        );
                        ctx.warn(owner, "CrewLocked", note).await;
                    }
                    Ok(Ensured::new(outcome, soon))
                }
```
The owner is whatever `ensure_job` was given: the Crew for a sync, the Agent for a harvest, the Fleet for a cleanup (§22.3). `ensure_job`'s bounds already include `K::DynamicType: Default`.

In `controllers/mod.rs`, add to `RunConfig`:
```rust
    /// How long something holding a crew may take to be deleted before the
    /// waiting Job's owner gets a `CrewLocked` Event (§22.3).
    pub stuck_after: Duration,
```
and `stuck_after: Duration::from_secs(300),` in `RunConfig::new`.

- [ ] **Step 4: Run the unit test and check that it passes.** Same command. Expected: PASS.

- [ ] **Step 5: Write the failing envtest tests.** In `controllers_it.rs`:
- In `job_rule`, after `cfg.period = Duration::from_secs(1);`, add `cfg.stuck_after = Duration::from_secs(1);`.
- Add the import `use kube::api::DeleteParams;` (extend the existing `kube::api::{…}` list).
- After `crew_job`, add:

```rust
/// A pod of crew `f/c` with `extra` labels; nothing runs it. A
/// `finalizer` keeps it, deleting, until the test takes it off.
fn crew_pod(ns: &str, name: &str, extra: &[(&str, &str)], finalizer: Option<&str>) -> Pod {
    let mut labels = serde_json::json!({ "balerix.ai/fleet": "f", "balerix.ai/crew": "c" });
    for (k, v) in extra {
        labels[*k] = serde_json::json!(v);
    }
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": name, "namespace": ns, "labels": labels,
            "finalizers": finalizer.into_iter().collect::<Vec<_>>() },
        "spec": { "restartPolicy": "Never", "containers": [{ "name": "job", "image": "x" }] }
    }))
    .unwrap()
}
```

Then add the two tests after `a_stale_job_is_deleted_in_the_foreground_and_holds_the_crew_until_gone`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_crews_job_pod_without_its_job_holds_the_crew() {
    use balerix_operator::controllers::jobs::ensure_job;
    use balerix_operator::desired::common::JobOutcome;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, ctx, owner) = job_rule("podhold").await;
    let client = env.client.clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let job_name = [("batch.kubernetes.io/job-name", "f-c-sync")];
    // a dropped crew's sync pod: its Job collected in the background, the pod still running
    pods.create(&PostParams::default(), &crew_pod(&ns, "f-c-sync-x1", &job_name, None))
        .await
        .unwrap();
    // an agent's own pod, and a finished pod of an old Job, hold nothing
    pods.create(&PostParams::default(), &crew_pod(&ns, "f-c-a", &[], None))
        .await
        .unwrap();
    let old = [("batch.kubernetes.io/job-name", "f-c-old")];
    pods.create(&PostParams::default(), &crew_pod(&ns, "f-c-old-x1", &old, None))
        .await
        .unwrap();
    pods.patch_status(
        "f-c-old-x1",
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "status": { "phase": "Succeeded" } })),
    )
    .await
    .unwrap();

    let harvest = || crew_job(&ns, "f-c-a-harvest", "h");
    let held = ensure_job(&ctx, &owner, harvest(), Some(("f", "c"))).await.unwrap();
    assert_eq!(held.outcome, JobOutcome::Absent);
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());

    // no node: the pod is deleted at once, and the harvest starts
    pods.delete("f-c-sync-x1", &DeleteParams::default()).await.unwrap();
    wait_for("the sync pod gone", Duration::from_secs(30), || async {
        pods.get_opt("f-c-sync-x1").await.unwrap().is_none().then_some(())
    })
    .await;
    let made = ensure_job(&ctx, &owner, harvest(), Some(("f", "c"))).await.unwrap();
    assert_eq!(made.outcome, JobOutcome::Running);
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crew_held_by_a_pod_deleting_past_the_bound_puts_crew_locked_on_the_owner() {
    use balerix_operator::controllers::jobs::ensure_job;
    use balerix_operator::desired::common::JobOutcome;
    use k8s_openapi::api::events::v1::Event;
    if envtest().await.is_none() {
        return;
    }
    let (env, ns, ctx, owner) = job_rule("stuck").await;
    let client = env.client.clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let events: Api<Event> = Api::namespaced(client.clone(), &ns);
    let job_name = [("batch.kubernetes.io/job-name", "f-c-sync")];
    // a pod on a lost node: deleting, and never gone
    pods.create(
        &PostParams::default(),
        &crew_pod(&ns, "f-c-sync-x1", &job_name, Some("balerix.ai/test-hold")),
    )
    .await
    .unwrap();
    pods.delete("f-c-sync-x1", &DeleteParams::default()).await.unwrap();

    let harvest = || crew_job(&ns, "f-c-a-harvest", "h");
    let event = wait_for("a CrewLocked Event on the owner", Duration::from_secs(30), || async {
        let ensured = ensure_job(&ctx, &owner, harvest(), Some(("f", "c"))).await.unwrap();
        assert_eq!(ensured.outcome, JobOutcome::Absent, "the lock holds");
        events.list(&Default::default()).await.unwrap().items.into_iter().find(|e| {
            e.reason.as_deref() == Some("CrewLocked")
                && e.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some("owner")
        })
    })
    .await;
    assert_eq!(event.type_.as_deref(), Some("Warning"));
    let note = event.note.unwrap();
    assert!(note.starts_with("crew f/c waits on Pod f-c-sync-x1, being deleted since "), "{note}");
    assert!(jobs.get_opt("f-c-a-harvest").await.unwrap().is_none());

    // the user forces it out: the harvest starts
    pods.patch(
        "f-c-sync-x1",
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "metadata": { "finalizers": null } })),
    )
    .await
    .unwrap();
    wait_for("the pod gone", Duration::from_secs(30), || async {
        pods.get_opt("f-c-sync-x1").await.unwrap().is_none().then_some(())
    })
    .await;
    let made = ensure_job(&ctx, &owner, harvest(), Some(("f", "c"))).await.unwrap();
    assert_eq!(made.outcome, JobOutcome::Running);
}
```

- [ ] **Step 6: Confirm the tests fail without the change and pass with it.**
  1. Temporarily comment out the `holders.extend(…)` statement in `crew_holders` and the `ctx.warn(owner, "CrewLocked", note).await;` line.
  2. Run both tests (envtest command, `-E 'test(/job_pod_without|deleting_past_the_bound/)'`). Expected: the first fails at `assert_eq!(held.outcome, JobOutcome::Absent)`, and the second fails, either at "the lock holds" or by timing out waiting for the Event.
  3. Restore both lines and run again. Expected: both PASS.

- [ ] **Step 7: Gate and commit**

Run: `cd /workspace && mise run operator`. Expected: PASS.

```bash
git add operator
git commit -m "fix(operator): the crew lock counts the crew's Job pods and reports one stuck deleting (#132)"
```

---

### Task 6: An authority whose key does not match its certificate is re-minted (#134)

**Files:**
- Modify: `operator/src/pki.rs` (`authority_works`, test)
- Modify: `operator/src/controllers/daemon.rs` (`ensure_material`)
- Test: `operator/tests/controllers_it.rs`

**Interfaces:**
- Produces: `pub fn authority_works(authority: &Issued, namespace: &str, daemon: &str, now: i64) -> bool` in `balerix_operator::pki`.

- [ ] **Step 1: Write the failing unit test** in `pki.rs`'s `tests` module (it uses the module's `NOW`):

```rust
    #[test]
    fn an_authority_works_only_with_its_own_key() {
        let mine = new_authority("ns", "default", NOW).unwrap();
        let other = new_authority("ns", "default", NOW).unwrap();
        assert!(authority_works(&mine, "ns", "default", NOW));
        let mismatched = Issued {
            key_pem: other.key_pem.clone(),
            ..mine.clone()
        };
        assert!(!authority_works(&mismatched, "ns", "default", NOW));
        let garbage = Issued {
            key_pem: "not a key".into(),
            ..mine.clone()
        };
        assert!(!authority_works(&garbage, "ns", "default", NOW));
        let no_cert = Issued {
            cert_pem: String::new(),
            ..mine
        };
        assert!(!authority_works(&no_cert, "ns", "default", NOW));
    }
```

- [ ] **Step 2: Run it and check that it fails**

Run: `cd /workspace/operator && cargo nextest run --config-file .config/nextest.toml --lib -E 'test(an_authority_works_only)'`
Expected: does not compile (`authority_works` is not defined).

- [ ] **Step 3: Implement.** In `pki.rs`, after `verifies`:

```rust
/// Whether `authority`'s key signs serving certificates its certificate
/// verifies: a key paired with another authority's certificate, or either
/// unparseable, does not (§22.4). Checked by issuing one, so no X.509
/// parser is needed.
pub fn authority_works(authority: &Issued, namespace: &str, daemon: &str, now: i64) -> bool {
    let names = daemon_names(namespace, daemon);
    issue_serving(authority, namespace, daemon, &names, now)
        .is_ok_and(|leaf| verifies(&authority.cert_pem, &leaf.cert_pem, &names[0], now))
}
```

In `controllers/daemon.rs` `ensure_material`, replace the `let authority = match read_issued(…) { … };` block:
```rust
    let read = read_issued(
        secrets.get_opt(&names::authority(name)).await?.as_ref(),
        "ca.crt",
        "ca.key",
    );
    // an authority whose key does not sign what its certificate verifies is
    // missing (§22.4): kept, every serving certificate it issued would fail
    // `verifies`, and be reissued on every reconcile
    let authority = match read {
        Some(a) if pki::authority_works(&a, namespace, name, now) => a,
        Some(_) => {
            tracing::warn!(daemon = %name, "the authority's key does not match its certificate: minting a new one");
            pki::new_authority(namespace, name, now)?
        }
        None => {
            tracing::info!(daemon = %name, "minting the authority");
            pki::new_authority(namespace, name, now)?
        }
    };
```

- [ ] **Step 4: Run the unit test and check that it passes.** Same command. Expected: PASS.

- [ ] **Step 5: Write the failing envtest test.** In `controllers_it.rs`, after `a_serving_certificate_the_authority_cannot_verify_is_reissued`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_ca_secret_whose_key_is_not_its_certificates_is_reminted_once() {
    let Some(env) = envtest().await else { return };
    let ns = namespace(&env.client, "camismatch").await;
    let client = env.client.clone();
    Api::<Daemon>::namespaced(client.clone(), &ns)
        .create(&PostParams::default(), &Daemon::new("default", daemon_spec()))
        .await
        .unwrap();
    let clock = TestClock::default();
    let operator = spawn_operator(env, &ns, None, &clock);
    let statefulsets: Api<StatefulSet> = Api::namespaced(client.clone(), &ns);
    wait_for("the StatefulSet", Duration::from_secs(60), || async {
        statefulsets.get_opt("balerix-default").await.unwrap()
    })
    .await;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let text =
        |s: &Secret, key: &str| String::from_utf8(s.data.as_ref().unwrap()[key].0.clone()).unwrap();
    let ca = secrets.get("balerix-default-ca").await.unwrap();
    let (old_crt, old_key) = (text(&ca, "ca.crt"), text(&ca, "ca.key"));

    // a day on, so a reissued serving certificate's expiry differs
    clock.advance(86_400);
    let other = pki::new_authority(&ns, "default", clock.clock()()).unwrap();
    // the certificate kept, the key another authority's
    let broken = Patch::Merge(serde_json::json!({ "stringData": { "ca.key": other.key_pem } }));
    // a reconcile that read the old pair before the patch applies it back:
    // patch again while the old key is there
    let reminted = wait_for("a new authority", Duration::from_secs(60), || async {
        let ca = secrets.get("balerix-default-ca").await.unwrap();
        if text(&ca, "ca.crt") != old_crt {
            return Some(ca);
        }
        if text(&ca, "ca.key") == old_key {
            secrets
                .patch("balerix-default-ca", &PatchParams::default(), &broken)
                .await
                .unwrap();
        }
        None
    })
    .await;
    let new_crt = text(&reminted, "ca.crt");
    assert_ne!(new_crt, other.cert_pem);
    let now = clock.clock()();
    let names = pki::daemon_names(&ns, "default");
    let not_after = wait_for(
        "a serving certificate from the new authority in the pod template",
        Duration::from_secs(60),
        || async {
            let tls = secrets.get("balerix-default-tls").await.unwrap();
            if !pki::verifies(&new_crt, &text(&tls, "tls.crt"), &names[0], now) {
                return None;
            }
            let not_after = tls.metadata.annotations.unwrap()["balerix.ai/not-after"].clone();
            let template = statefulsets.get("balerix-default").await.unwrap();
            (template.spec.unwrap().template.metadata.unwrap().annotations.unwrap()
                ["balerix.ai/not-after"]
                == not_after)
                .then_some(not_after)
        },
    )
    .await;
    // once: the authority and the pod template hold across reconciles (period 1 s)
    hold_for("a second re-mint or roll", Duration::from_secs(5), || async {
        let ca = secrets.get("balerix-default-ca").await.unwrap();
        let template = statefulsets.get("balerix-default").await.unwrap();
        let rolled = template.spec.unwrap().template.metadata.unwrap().annotations.unwrap()
            ["balerix.ai/not-after"]
            != not_after;
        (text(&ca, "ca.crt") != new_crt || rolled).then_some(())
    })
    .await;
    operator.abort();
}
```

- [ ] **Step 6: Confirm the test fails without the fix and passes with it.** Run `git stash push operator/src` and run the test (envtest command, `-E 'test(a_ca_secret_whose_key)'`). Expected: FAIL ("timed out waiting for a new authority"). Then run `git stash pop` and run it again. Expected: PASS.

- [ ] **Step 7: Gate and commit**

Run: `cd /workspace && mise run operator`. Expected: PASS.

```bash
git add operator
git commit -m "fix(operator): re-mint a CA whose key does not match its certificate instead of rolling the pod (#134)"
```

---

### Task 7: Whole-branch verification and the draft PR

**Files:** none changed, unless a check below fails.

- [ ] **Step 1: Run the full gates**

Run: `cd /workspace && mise run operator && mise run check`.
Expected: both pass. `check` covers the root lint (hadolint and others) and catches any spec-file or workspace fallout. In the `operator` run, confirm that no controller test printed `skip:`.

- [ ] **Step 2: Repeat the controller suite three times** to catch new flakes from the 10 s bound or the spawned create:

```bash
cd /workspace/operator && for i in 1 2 3; do ENVTEST_DIR=$HOME/.local/share/mise/installs/github-kubernetes-sigs-controller-tools/envtest-v1.34.1/envtest cargo nextest run --config-file .config/nextest.toml --test controllers_it || break; done
```
Expected: three passes. Any failure gets diagnosed before Step 3, using superpowers:systematic-debugging.

- [ ] **Step 3: Push and open a draft PR.** Approving this plan authorizes pushing this branch and opening a *draft* PR. The user decides on review and merge.

```bash
cd /workspace && git push -u origin feat/operator-hardening
gh pr create --draft --base main --title "fix(operator): hardening — request bound, timeout back-off, Job failure, crew lock, CA check (Spec O §22)" --body "$(cat <<'EOF'
Spec O §22 (spec commits da7c40f, e1ac82a; plan docs/superpowers/plans/2026-10-05-balerix-o-operator-hardening.md).

- §22.1: every reconcile and test request is bounded at 10 s (tower `TimeoutLayer`; tower's `timeout` feature, no new crate). Watches stay on `watch_client`. Upstream: hyperium/hyper#4207. Closes #129, closes #138.
- §22.2: from the third consecutive reconcile timeout, back off (20 s, doubling, at most 5 min) and publish a Warning Event `ReconcileTimedOut`. Closes #130.
- §22.3: a Job with `Failed=True` and `failed: 0` is failed (closes #131); the crew lock counts the crew's Job pods, reports a holder deleting past 5 min as a `CrewLocked` Event, and holds through a dropped reconcile's create (closes #132); the lock map prunes released entries (closes #133).
- §22.4: a CA Secret whose key does not match its certificate is re-minted once (closes #134).
- §22.5: the chart (sub-project 5) must grant `events.k8s.io` `create`/`patch`.

e2e-k8s runs in CI only (no docker on the dev host).
EOF
)"
```

- [ ] **Step 4: Watch CI** with `gh pr checks --watch`. Report the result to the user, including any job that failed and its log excerpt.
