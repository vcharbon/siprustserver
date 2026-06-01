# Call-limiter HTTP port — implementation-ready design

Source (TS, pinned): `portsource/sipjsserver` — `src/call/CallLimiter*.ts`,
`src/decision/apply/applyRoute.ts`, `src/b2bua/rules/framework/FrameworkLimiterRefresh.ts`,
`src/redis/LimiterRedisClient.ts`.

Goal: replace the dormant `NoopLimiter` seam with a faithful sliding-window call
limiter, but back it with a **dedicated stateless HTTP service** (not Redis),
reachable over a new **reusable fake-able HTTP transport layer** (mirror of
`sip-net` / `repl-net`). Wire the full admission/release/refresh/failover path
into the B2BUA and deploy the service to k8s.

This is a behavioural port: the server's internal windowing is the exact
high-level semantics of the TS in-memory limiter (`CallLimiter.memory.ts`), and
the atomic per-op rules are ported from the Redis Lua scripts
(`CallLimiter.redis.ts`).

---

## Decisions (resolved in design review)

1. **Transport seam shape** — model on `repl-net` (not `sip-net`): unary
   request/response, *bind a handler on one side, `request()` on the other*. No
   streaming responses (simplified for our tiny JSON POSTs). Recorder included
   for tests.
2. **Real vs simulated** — the production impl uses **real HTTP with proper
   connection-pool management** (hyper server + pooled `reqwest` client), but it
   lives **behind the `HttpTransport` trait** and is feature-gated, so every
   paused-clock test injects `SimulatedHttpNetwork` and `reqwest`/`hyper` never
   run under simulated time.
3. **Crate split** — `http-net` (transport, reusable, b2bua-agnostic) +
   `call-limiter` (DTOs + window core + server handler, no b2bua dep) +
   `call-limiter-runner` (deployed binary). The HTTP **client logic lives in
   b2bua**, importing DTOs from `call-limiter`, issuing requests over an injected
   `Arc<dyn HttpTransport>`.
4. **Wire API** — **batched + transactional**: one `admit` call per call carries
   all limiter entries and increments **all or none** (TS's per-entry
   partial-success + eager-DECR was an error we deliberately drop). Server owns
   the clock and returns the window timestamp.
5. **b2bua trait** — reports the honest outcome
   (`Admitted{window} | Rejected{limiter_id} | Unavailable`); the **b2bua call
   site owns the fail-open policy** (mirrors where TS put it, `catchTags` at
   `applyRoute`).
6. **Leak control** — whole-store **sweep-on-access** (port of TS) **+** a
   periodic janitor task in the **runner only** (off in tests — a background
   sleep would auto-advance the paused clock, the CLAUDE.md hazard). Clock is the
   injected `sip-clock::Clock`.
7. **Cluster topology** — **single replica**, ClusterIP, **no persistence**. On
   restart the empty state is re-filled within ≤ `windowSec` by active calls'
   refresh timers; fail-open covers the gap.
8. **b2bua scope** — full: transactional admit + **486 on reject** +
   release-on-terminate + **refresh timer** + **failover-on-reject**
   (`/call/failure`, bounded depth).
9. **Faults / timeout** — `Fault::{Delay, Stall, Resume, Cut, ErrorAfter}`,
   **keyed by `dst`**. The fail-open **timeout budget lives in the b2bua client**
   (`tokio::time::timeout`, default 150 ms): on timeout *or* `HttpError` →
   `Unavailable` → fail-open.
10. **Metrics** — global counters + 2 gauges, **no per-id labels**. Metrics are
    **GET-only, side-effect-free, never on the decision path** → explicitly
    refactorable later (per-id labels, real Prometheus crate) as a non-goal.

---

## Ported semantics (the source of truth)

Window math: `window = epochSec - (epochSec % windowSec)`.

- **admit (port of `CHECK_AND_INCREMENT_LUA`, batched/transactional)** — under
  one lock: for every entry sum its last `activeWindows` window counts; if **any**
  entry's total `>= limit` → reject, increment **nothing**, return the first
  `rejected_id`; else `INCR current` (+ refresh TTL) for **every** entry and
  return the single shared `window`.
- **release (port of `DECREMENT_LUA`, but floored)** — `count = max(0, count-1)`
  on the given window key. NB: Redis `DECR` could go negative; we take the
  `CallLimiter.memory.ts` flooring (more correct) since we own the store.
- **refresh (port of `REFRESH_LUA`)** — if `origin == current` → noop; else
  `INCR current` (+TTL) **then** `DECR origin` (incr-before-decr: briefly
  overcounts, never undercounts). Returns the new `current` window.
- **TTL / sweep** — `expiresAtMs = now + ttlSec`; whole-store sweep drops expired
  keys on every access (and via the janitor).
- **FailureInfo.origin for limiter rejects** = `"call_limiter"`
  (`applyRoute.ts:291`).

---

## Crate / module layout

```
crates/
  http-net/                         # reusable unary HTTP transport seam
    src/lib.rs
    src/transport/mod.rs            # HttpTransport / HttpService / HttpServer traits, DTOs, errors
    src/transport/simulated.rs      # SimulatedHttpNetwork: in-mem router, dst-keyed Fault, transit delay >=1ms
    src/transport/real.rs           # RealHttpNetwork: hyper server + pooled reqwest client (feature "real")
    src/transport/recording.rs      # RecordingHttpNetwork: CapturedRequest + Direction + Clock ts
    tests/simulated.rs              # fault + recorder tests

  call-limiter/                     # DTOs + window core + server handler (NO b2bua dep)
    src/lib.rs
    src/wire.rs                     # AdmitRequest/Response, ReleaseRequest, RefreshRequest/Response (serde)
    src/window.rs                   # WindowStore core: admit/release/refresh/sweep over injected Clock
    src/server.rs                   # impl HttpService: route /v1/* -> WindowStore; bump LimiterMetrics
    src/metrics.rs                  # LimiterMetrics (AtomicU64) + prometheus_text()
    tests/window_props.rs           # property tests + oracle comparison

  call-limiter-runner/              # the deployed process
    src/main.rs                     # RealHttpNetwork serve + janitor task + /metrics + /healthz; env config

crates/b2bua/src/
  limiter.rs                        # enriched CallLimiter trait + AdmitOutcome + Hold + NoopLimiter
  limiter_http.rs                   # HttpCallLimiter: Arc<dyn HttpTransport> + addr + timeout budget; fail-open map
  decision/apply_route.rs           # rewrite admission: transactional admit -> 486 | failover | fail-open
  rules/...                         # emit release(Hold) on terminate; limiter_refresh timer handler
```

Dependency edges (no cycle): `b2bua → {http-net, call-limiter}`,
`call-limiter → http-net`, `call-limiter-runner → {http-net (feat real), call-limiter}`.

---

## HTTP API

| Method / path        | Request                              | 200 response                          |
|----------------------|--------------------------------------|---------------------------------------|
| `POST /v1/admit`     | `{entries:[{id,limit}]}`             | `{admitted:true,window}` \| `{admitted:false,rejected_id}` |
| `POST /v1/release`   | `{entries:[{id,window}]}`            | `{}`                                   |
| `POST /v1/refresh`   | `{entries:[{id,window}]}`            | `{entries:[{id,window}]}` (new windows) |
| `GET  /metrics`      | —                                    | Prometheus text                        |
| `GET  /healthz`      | —                                    | `ok`                                   |

Client contract: **any** non-2xx, transport error, or timeout (budget) =
`Unavailable` → fail-open. Only a clean `200` carries an authoritative
`admitted` true/false.

---

## b2bua trait + admission flow

```rust
pub struct Entry { pub id: String, pub limit: i64 }
pub struct Hold  { pub limiter_id: String, pub window: i64 }

pub enum AdmitOutcome {
    Admitted { window: i64 },        // all entries incremented at this window
    Rejected { limiter_id: String }, // nothing incremented
    Unavailable,                     // transport/server down
}

#[async_trait]
pub trait CallLimiter: Send + Sync {
    async fn admit(&self, entries: &[Entry]) -> AdmitOutcome;
    async fn release(&self, holds: &[Hold]);
    async fn refresh(&self, holds: &[Hold]) -> Vec<Hold>;
}
```

`apply_route` (replacing the placeholder loop at `apply_route.rs:38-47`):

```
outcome = limiter.admit(entries).await
match outcome:
  Admitted{window} -> holds = entries.map(|e| Hold{e.id, window}); record on call; build b-leg
  Unavailable      -> fail-open: build b-leg, record NO holds (so no release fires)
  Rejected{id}     -> if call.callback_context.is_some():
                         resp = decision.call_failure(FailureInfo{origin:"call_limiter", limiter_id:Some(id)}).await
                         match resp:
                           Failover(route2) -> apply_route(route2)   // re-admits route2.call_limiter; bounded depth (default 5)
                           Terminate        -> answer 486 Busy Here + terminate
                       else:
                         answer 486 Busy Here + terminate
```

- **release on terminate** — emit `SoftBoundedEffect::DecrementLimiter` for every
  recorded `Hold` (port the InvariantEnforcer guarantee; the consumer already
  exists at `router.rs:460`). Skip entries with `increment_succeeded == false`
  (fail-open admits carry no holds, so this is automatic).
- **refresh timer** — schedule `TimerType::LimiterRefresh` at call confirm,
  reschedule every `windowSec`; handler calls `limiter.refresh(holds)` and
  updates each hold's window (port of `FrameworkLimiterRefresh.ts`).

---

## Configuration (env)

Server (`call-limiter-runner`):
`LIMITER_LISTEN=0.0.0.0:8080`, `LIMITER_WINDOW_SECONDS=300`,
`LIMITER_ACTIVE_WINDOWS=3`, `LIMITER_TTL_SECONDS=1200`,
`LIMITER_JANITOR_INTERVAL_SECONDS` (default = `windowSec`).

b2bua client:
`LIMITER_URL` (unset/empty → `NoopLimiter`, preserving today's behaviour),
`LIMITER_TIMEOUT_MS=150`, max chained limiter-failover depth = `5`.

---

## Metrics (global, no per-id labels — freely refactorable, non-behavioural)

Counters: `limiter_admit_total`, `limiter_admitted_total`,
`limiter_rejected_total`, `limiter_release_total`, `limiter_refresh_total`,
`limiter_auto_cleared_total` (TTL-swept entries). Gauges: `limiter_live_keys`
(map size — leak monitor), `limiter_current_total` (sum of live counts — current
concurrent across all ids). Hand-rolled `AtomicU64` + `prometheus_text()`,
served on the same listener.

---

## k8s

- `deploy/k8s/manifests/50-call-limiter.yaml`: Deployment `replicas: 1`, no
  volumes, the `call-limiter` image; pod annotations
  `prometheus.io/scrape: "true"`, `prometheus.io/port: "8080"`,
  `prometheus.io/path: "/metrics"`; readiness/liveness probe `GET /healthz`.
- ClusterIP Service `call-limiter:8080`.
- Worker (`20-worker.yaml`): add `LIMITER_URL=http://call-limiter:8080`.
- Dockerfile/build wiring for the new binary.

---

## Slice plan (commit per green test family — project convention)

1. **`http-net` transport** — traits + DTOs + `SimulatedHttpNetwork`
   (dst-keyed faults, transit delay ≥1ms) + `RecordingHttpNetwork`. Tests:
   happy-path serve/request, `Delay`, `Stall`→client-timeout, `Cut`,
   `ErrorAfter`, recorder capture. *(real.rs stub/feature-gated; filled in slice 3.)*
2. **`call-limiter` core + server** — `window.rs` (admit/release/refresh/sweep
   over injected Clock) + `wire.rs` + `server.rs` + `metrics.rs`. Tests:
   windowing **property tests** (sum-of-N, reject-at-limit, refresh
   never-undercounts, release floors at 0, TTL sweep, transactional all-or-none)
   + **oracle/Layer comparison** (HTTP server over `SimulatedHttpNetwork` vs the
   in-process `WindowStore` must agree).
3. **`call-limiter-runner` + `RealHttpNetwork`** — hyper + pooled reqwest behind
   the trait, feature `real`; janitor task; env config; `/metrics` + `/healthz`.
   Smoke test against a real loopback bind.
4. **b2bua wiring** — enriched trait + `HttpCallLimiter` (fail-open map) +
   `apply_route` rewrite + release-on-terminate + `limiter_refresh` timer +
   failover-on-reject. Tests (b2bua-harness over sim fabric with a **real limiter
   server bound on it**): `rejection→486`, `fail-open` (server `Cut`/`Stall`),
   `release-on-BYE`, `refresh-long-call` (advance past `windowSec`),
   `shared-cross-worker` (two SUTs, one server), `failover-on-reject`. Metrics
   assertions.
5. **HA-coupled tests** — `decrement-after-respawn`, `switchback/backup-BYE
   decrement` (exercise the replication + crash-recovery interplay).
6. **k8s** — manifests + Service + `LIMITER_URL` + image/Dockerfile + run-script
   wiring.

**Non-goal / follow-up (not blocking):** refactor the metrics surface (per-id
labels, real Prometheus crate). Safe to defer — GET-only, no behavioural effect.

**Un-ported TS tests (justification):** `limiter-parity` (memory-vs-Redis) — N/A,
there is no Redis backend in the Rust port; its windowing-equivalence intent is
covered by slice 2's oracle/property tests. `proxy-limiter-soak` — load/soak, not
a unit/scenario test; belongs to the endurance suite, not this port.
