# Load-Test Driver (`crates/loadgen`) — reuse functional scenarios as a partial SIPp substitute

## Context

We want to drive the real k8s (kind) cluster with a managed call rate, mixing **basic
load** (the kind SIPp does well) with **complex flows SIPp can't easily express**
(REFER blind transfer, re-INVITE, OPTIONS-keepalive long-hold) while keeping **proper
reporting** (counts, response-time percentiles, and inspectable per-call callflows).
The motivation is to reuse the substantial complex-scenario logic that already exists as
functional tests (`b2bua-harness/tests/*`, `scenario-harness::callflow`) instead of
re-authoring it as SIPp XML.

The blocker is that the functional harness is built for *one call per test*:
- `Harness` (`crates/scenario-harness/src/agent.rs:154`) is **`!Send`** — it holds
  `Rc<RefCell<…>>` (`allow_violations` L173, `anchors` L183) — so a Harness-bearing
  future can't run on a multi-threaded runtime; reusing it per-call forces **one OS
  thread per in-flight call** (~3000 threads ≈ 6 GB at SIPp scale).
- Its recording buffer is an **unbounded** `Vec` (`crates/layer-harness/src/recorder.rs:44`).
- Core agent steps **panic** on timeout/mismatch (`agent.rs` `recv` L774, `receive` L801,
  `expect_response` L1783 `assert_eq!`).

**Key asymmetry that makes this feasible:** only the `Harness` *wrapper* is `!Send`. The
`Agent` (`agent.rs:721`) holds only `Send` fields (`Arc<dyn UdpEndpoint>`, `Arc<Ids>`,
Strings), and the **recording machinery itself is `Arc<Mutex>` → `Send`**. So we keep the
`Send` agent choreography + `Send` recording, and drop the `!Send` wrapper. Outcome:
thousands of concurrent calls as ordinary `tokio` tasks on a shared multi-threaded
runtime, flat memory, **and recorded calls cost no extra OS thread** — recording becomes a
per-call opt-in flag, not a structural fork.

Intended outcome: `cargo run -p loadgen -- --cps 50 --duration 600 …` from the host
against the kind VIP, emitting a live Prometheus `/metrics` endpoint plus an on-disk
report with the first 10 callflows per `(scenario × result-class)` — **including OK** — so
OK vs failing flows are directly comparable.

## Design decisions (settled with user)

1. **Lean fallible runner**, not per-call `!Send` Harness. Reuse `Send` `Agent`
   choreography; multi-threaded runtime; flat memory.
2. **Sampled full recording**: recording is a per-call opt-in (`Send`, no OS thread).
   A sampling gate captures the first N per `(scenario × class)`, including OK, then
   converges to zero recording (flat steady-state memory).
3. **Reporting = both**: live Prometheus `/metrics` *and* an on-disk report (HTML callflow
   via existing `seq-report::render_html`, + markdown summary + per-class samples).
4. **Scenarios v1**: basic call, re-INVITE, REFER blind transfer, OPTIONS-keepalive long-hold.
5. **Early-exit cleanup is mandatory**: any non-OK exit (Err / timeout / panic) must
   CANCEL (early dialog) or BYE (confirmed dialog) every leg so no call is leaked on the
   SUT (else `active_calls`/limiter/leak-detector are contaminated).

## Architecture

```
governor (interval @ 1/cps) ──pick weighted scenario──► try_acquire(max_in_flight)
   │ at cap: drop + inc_shed (measure offered load honestly, don't block)
   └─ spawn Send task:
        binder = AgentBinder::real(record = sampler.should_record(scenario))
        agents = binder.agent(alice) [+ bob (UAS) + charlie (REFER)]
        scope  = Arc<Mutex<CallScope>>            // OUTSIDE catch_unwind → survives panic
        res    = AssertUnwindSafe(scenario.run(&binder, &scope, &ctx)).catch_unwind().await
        scope.teardown(&agents).await             // CANCEL/BYE any leg not Terminated
        class  = classify(res)
        reporter.record(scenario, class, ctx.checkpoints)
        if binder.recording: persist callflow if (scenario,class) bucket < cap
        drop(binder)                              // frees recorder Vec + permit (RAII)
```

## Components

### Phase 0 — seams added inside `crates/scenario-harness` (needs private `Agent` access)

- **`agent.rs`: fallible `try_*` siblings** (added alongside the panicking originals, which
  are independent methods — non-breaking):
  - `pub enum StepError { Timeout, QueueClosed, Unparseable(String), WrongStatus{expected,got},
    WrongMethod{expected,got}, UnexpectedKind(&'static str), Transport(String) }`
  - `Agent::try_recv_msg`, `Agent::try_receive(method)`, `ClientInvite::try_expect(status)`,
    `Dialog::try_request` — thin re-impls of `recv`/`receive`/`expect_response` that
    `return Err(..)` where the originals `panic!`. Factor the dialog-learning logic in
    `ClientInvite::expect` (`agent.rs:1196-1218`) into a shared private `learn_from_response`.
  - **Best-effort teardown helpers** (send + short timeout, ignore result, never panic):
    `ClientInvite::cancel_best_effort`, `Dialog::bye_best_effort`,
    `Agent::drain_and_ack(window)` (200-OK any relayed BYE/in-dialog req on the UAS legs).
- **`loadbind.rs` (NEW): `AgentBinder` — a `Send` agent factory.** Reproduces
  `Harness::build` (`agent.rs:245-291`) minus the two `Rc` fields and the cseq panic-gate:
  ```rust
  pub struct AgentBinder { /* Send: network, RecordingSignalingNetwork, Recorder, Arc<Ids>, recv_timeout, record: bool */ }
  impl AgentBinder {
      pub fn real(name:&str, recv_timeout:Duration, record:bool) -> Self; // RealSignalingNetwork + Clock::system + Live
      pub fn fake(name:&str, transit_ms:u64, record:bool) -> Self;        // SimulatedSignalingNetwork + Clock::test_at(0) (smoke test)
      pub fn seed_ids(&self, seed:u64);
      pub async fn agent(&self, name:&str, addr:&str) -> Agent;           // == Harness::agent_with_roles body (agent.rs:398-407)
      pub fn snapshot(&self) -> (Vec<Stamped<SignalingNetworkEvent>>, RecordedScenario); // for projection
      pub fn recording(&self) -> bool;
  }
  ```
  Reuses `with_all_contracts`, `Recorder::with_clock`, same RFC rule set. Export both from
  `lib.rs`. **This is the single reason loadgen logic must touch `scenario-harness`** — the
  `Agent` constructor is crate-private.

### Phase 1 — `crates/loadgen` crate skeleton (NEW, lib + bin)

- **`class.rs`** — `ResultClass { Ok, Timeout, WrongStatus{expected,got}, WrongMethod{..},
  TransportError(String), Unparseable(String), RfcAuditFail, Panic(String) }` and a
  low-cardinality `ClassKey` for sample bucketing (e.g. `WrongStatus(503)` keeps the code;
  `Panic`/`Transport` collapse to the variant).
- **`report.rs`** — `Reporter`:
  - per-`(scenario,class)` `AtomicU64` counts; per-scenario `shed` counts;
  - **latency via `hdrhistogram`** (new dep): per-scenario end-to-end + named **checkpoints**
    ("keywords": `time_to_200`, `time_to_202`, `time_to_first_NOTIFY`, `time_to_reinvite_200`,
    `time_to_bye_200`) recorded by the scenario via `ctx.checkpoint(name, elapsed)`;
  - **bounded sample store**: `first ≤ cap` rendered callflows per `(scenario,class)`,
    including OK;
  - **`SamplingGate`**: `should_record(scenario)` at call start = true iff some
    `(scenario,*)` bucket is still under cap; once all buckets full → false globally →
    no more recording binders → flat memory. Over-records slightly near the cap boundary
    (class unknown until end), then converges to zero — acceptable.
  - **`/metrics`**: tiny `hyper` server (hyper already in-tree via `http-net`), hand-rolled
    Prometheus text (`loadgen_calls_total{scenario,class}`, `loadgen_inflight`,
    `loadgen_e2e_seconds{scenario,quantile}`, `loadgen_shed_total`), series named to mirror
    `deploy/k8s/sipp/exporter/sipp_stat_exporter.py` so existing VictoriaMetrics/`endurance.sh`
    queries extend cleanly.
  - **`finalize(out_dir)`**: write `index.html` (links per-class callflow samples + counter
    tables + percentiles) and `summary.md`. Per-call HTML/SVG via existing
    `report::project::sip_doc` (`project.rs:28`) → `seq_report::render_html`/`render_svg`
    — **no new renderer**; the index shell is the only new HTML (~40 LOC).
- **`driver.rs`** — `Driver` (governor `tokio::time::interval(1/cps)` + `Arc<Semaphore>`
  max-in-flight + weighted-random scenario pick). Backpressure = **drop+count, don't block**
  (honest offered-load; document `max_in_flight ≳ cps × p99_call_seconds`). Weighted random
  via a small seeded xorshift (avoid a `rand` dep, matching the codebase's `RandomState`
  convention at `agent.rs:104`).
- **`scope.rs`** — `CallScope` (`Arc<Mutex<Vec<TrackedLeg>>>`, `TrackedLeg = Early(ClientInvite)
  | Confirmed(Dialog) | Terminated`). Owned by the runner outside `catch_unwind` so it
  survives a scenario panic. Scenario moves dialog handles in and promotes state; on any
  exit the runner runs `teardown(&agents)`: `cancel_best_effort`/`bye_best_effort` each
  non-Terminated leg toward the cluster a-leg, then `drain_and_ack` the in-process bob/charlie
  legs. `Dialog`/`ClientInvite` are `Send` (hold only `Send` `Agent` + dialog state), so the
  scope is `Send`.
- **`CallCtx`** (reporter handle + `t0: Instant` + `checkpoint(name,elapsed)`),
  **`Targets`** (VIP addr, local bind ip, X-Api-Call pins).

### Phase 2 — scenarios (`crates/loadgen/src/scenarios/*.rs`)

```rust
#[async_trait] pub trait LoadScenario: Send + Sync {
    fn id(&self) -> ScenarioId; fn needs_charlie(&self) -> bool;
    async fn run(&self, b:&AgentBinder, scope:&Arc<Mutex<CallScope>>, ctx:&CallCtx, t:&Targets)
        -> Result<(), StepError>;
}
```
- **basic_call** — port `callflow::establish`+`hangup` (`callflow.rs:47,53`) via `try_*`.
- **reinvite** — establish, `dialog.try_request(InDialogMethod::Invite, Some(REOFFER))`
  (`reinvite.rs:45`), 200, ACK, BYE.
- **refer** — port `refer_allow_happy` (`refer_allow.rs:55-118`): A↔B, REFER→202, NOTIFY +
  charlie INVITE chain. Needs **charlie** leg + the `X-Api-Call` refer-allow JSON
  (`refer_allow.rs:23-27` shape) authorized by the cluster's REFER backend.
- **options_hold** — establish, loop `dialog.try_request(InDialogMethod::Options, None)`
  every cadence for `--options-hold` secs, then BYE (the SIPp OPTIONS-hold replacement).

INVITE built with `Invite::through(vip).with_header("X-Api-Call", pin).with_sdp(OFFER)`
(`agent.rs:1066,1073,1106`), reproducing `EgressPolicy::ApiCallPin` (`e2e-core/src/infra.rs:183-201,586`)
without depending on `e2e-core`. `seed_ids(wall-clock entropy)` per call (mandatory vs the
stateful cluster; trap documented at `agent.rs:293-305`).

### Phase 3 — fake-network smoke test FIRST (default lane)

`#[tokio::test]` using `AgentBinder::fake` + an in-process LB/`B2buaSut` (reuse
`b2bua-harness`) at low CPS, fixed count. Assert: counts add up; OK + an injected-failure
class both produce samples; `render_html` yields a doc; `/metrics` text parses; **and
teardown leaves no live dialog** (`b2bua.assert_fully_reaped()`). Deterministic, paused
clock, ≥1 ms transit.

### Phase 4 — bin + real cluster

`bin/loadgen.rs` (`clap`): `--cps --duration --max-in-flight --scenario name=weight
--target <vip:port> --bind-ip <host-bridge-ip> --recv-timeout --sample-cap --out-dir
--metrics-addr --options-hold`. Run from host against kind VIP; `--bind-ip` must be a
kind-bridge-reachable host IP and the bob/charlie `X-Api-Call` pins must point at
`--bind-ip` (not loopback) for return traffic (VIP→HOST masquerade, per `cluster-nat-inventory`).
Validate one host-mode call, then ramp. **Phase 5 (defer):** `deploy/k8s/loadgen/` Job
manifest modeled on `40-sipp-uac-job.yaml` + VictoriaMetrics wiring.

## Files

| File | Action |
|---|---|
| `crates/scenario-harness/src/agent.rs` | add `StepError`, `try_*` siblings, best-effort teardown helpers, factor `learn_from_response` |
| `crates/scenario-harness/src/loadbind.rs` | NEW — `Send` `AgentBinder` |
| `crates/scenario-harness/src/lib.rs` | export `AgentBinder`, `StepError` |
| `crates/loadgen/{Cargo.toml,src/{lib,driver,report,scope,class,ctx}.rs,src/scenarios/*.rs,src/bin/loadgen.rs}` | NEW |
| `Cargo.toml` (workspace) | add `loadgen` member |
| `deploy/k8s/loadgen/` | NEW, Phase 5 (deferred) |

**Reused as-is:** `report::project::sip_doc` (`project.rs:28`), `seq_report::render_html/render_svg`,
`sip_net::evaluate_rfc_findings`, `Recorder::snapshot` (`recorder.rs:289`), `callflow.rs`,
scenario bodies in `b2bua-harness/tests/*`.

**Deps added:** `hdrhistogram = "7"`, `clap = { version="4", features=["derive"] }`, `hyper`
(in-tree, `["http1","server"]`). No `rand` (seeded xorshift). No `metrics-exporter-prometheus`
(hand-rolled text).

## Risks

1. **`AssertUnwindSafe` over `Agent` futures** — sound: each call owns fresh endpoints; a
   panic just drops a half-driven dialog; recorder-mutex poisoning is per-call-isolated.
   Wrap the whole per-call closure.
2. **Recorder alive without `!Send` Harness** — resolved by inspection: recorder/recording/
   channel are all `Send`; `AgentBinder` reproduces `build` minus `Rc`. Load-bearing reason
   the binder lives in `scenario-harness`.
3. **Real-cluster return path** — bob/charlie `X-Api-Call` pins must target `--bind-ip`;
   validate one host call before load.
4. **Teardown completeness** — Phase-3 asserts `assert_fully_reaped()` after injected
   failures to prove no leaked dialogs.

## Verification

- Unit: sampling-gate convergence (memory flat after N×cap), class bucketing, Prometheus
  text round-trip, teardown idempotency.
- Integration (fake net, default `just test`): Phase-3 smoke incl. `assert_fully_reaped()`.
- Real (manual host→kind VIP): Phase-4; compare OK vs failing callflow HTML side-by-side;
  watch `b2bua_active_calls` stays bounded under injected failures.
