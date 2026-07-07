# Test-infrastructure layers — the map

One page on what each test crate provides and which harness to reach for.
The detailed contracts live in module docs (pointers at the bottom) — this is
the map, not the manual. How to *drive time* in these harnesses is a separate,
critical guide: [test-clock.md](test-clock.md).

## The stack (bottom-up)

Each layer builds strictly on the ones below it. All of it runs in-process —
no real sockets, no real wall-clock dependence.

| # | Crate | Provides |
|---|-------|----------|
| 1 | `sip-clock` | The one clock seam. `Clock::system()` / `Clock::test_at(0)`; `testkit::{advance_in_100ms_chunks, settle, pump}` — the primitives behind every harness `advance()`. |
| 2 | `layer-harness` | SIP-agnostic recording substrate: `Recorder`, `RunContext`, `EventSequencer`, `RecordedAnomaly`. Tests record first, assert on the recording (ADR-0004/0006). |
| 3 | `sip-net` | `SimulatedSignalingNetwork` — in-process datagram fabric keyed by `SocketAddr`, per-hop transit delay (0 is coerced to 1 ms — see the clock guide), `SendFault` + per-bind `PreIngress` hooks for drop/synthetic-reply injection. Plus `rfc_audit`: the RFC 3261/3262/3264 post-call rule suite (~77 rules, subject-dispatched per bind role, advisory/gating lanes) behind the single evaluator `evaluate_rfc_findings`. |
| 4 | `scenario-harness` | The fluent dialog DSL. `Harness` owns the recording-wrapped sim net, `Clock::test_at(0)`, and two RAII guards (`PanicDump`: wire-trace dump on panic; `CseqGate`: RFC hard gate even if you forget `finish()`). `Agent` is a fake UA that auto-fills Via/tags/CSeq/Contact per RFC 3261. `callflow` is the canonical INVITE/180/200/ACK choreography (`establish`, `hangup`, `Call::new(..).no_ring()`). Default transit is **100 ms** (`Harness::new`), so traces show `received = sent + 100`. |
| 5 | `b2bua-harness` | Binds a **real `B2buaCore`** as the SUT. `B2buaSut` (`route_all_to`, metrics, `cdr_records`), `B2buaScene` (alice :5060 / bob :5070 / b2bua :5080; `establish`/`hangup`/`finish`), `settle_until` (bounded yield-poll to drain async teardown), and the **`assert_fully_reaped` leak oracle**: creations == removals, `active_calls == 0`, `lock_count == 0`, reaper touched-ledger empty. |
| 6 | `failover-harness` | The full HA stack under ONE paused clock: a **real LB `ProxyCore`** + N replicating `b2bua` workers over a simulated replication fabric, one shared `EventSequencer` across the SIP + repl planes. Fault primitives: `crash()`, `reboot()` (fresh SIP IP, **hard-asserts pristine**), repl `partition`/`heal`, `set_health`, and `with_worker_clock_offset` (deterministic inter-node wall skew — see the clock guide). Cluster invariants: `assert_single_owner`, `assert_call_fully_over`, `assert_call_fully_released`, the `transparent_matrix!` differential oracle. Transit is 1 ms; its `advance` is `testkit::pump` (settle → advance → settle). |
| 7 | `ha-harness` | The replication engine alone — **no SIP, no router, no rules**. `HaCluster`/`HaNode` with put/delete/crash/reboot/partition, convergence assertions, `ReplReport`. |

Note the crate boundary people trip on: `callflow`/`Harness`/`Agent` live in
`scenario-harness`; **`B2buaScene`, `B2buaSut`, `settle_until`,
`assert_fully_reaped` live in `b2bua-harness`**.

## Which harness do I use?

| You are testing… | Use |
|---|---|
| B2BUA behaviour of one SUT (rules, dialog relay, timers, keepalive, reaping) | `b2bua-harness` — `B2buaScene` for the happy-path frame, hand-rolled `Harness` + `B2buaSut` when the handshake itself is the subject |
| Pure message/dialog choreography with no SUT (harness self-tests, agent idioms) | `scenario-harness` |
| LB proxy behaviour (routing, compliance, stateless contract) | `crates/sip-proxy/tests/` (e.g. `rfc_proxy_compliance.rs`, `stateless_final_response_contract.rs`) — `Harness` + real proxy |
| HA failover with SIP: kill/reboot/partition mid-call, takeover transparency, limiter parity | `failover-harness` |
| Replication-plane correctness in isolation (convergence, split-brain) | `ha-harness` |
| Real cluster / load / endurance | `crates/loadgen` + `e2e/` (see their READMEs) — out of scope for this doc |

## What is checked automatically vs. what you must assert

Automatic (you cannot opt out, only waive per-rule):

- **RFC hard gate** — `Harness::finish()` runs the audit over the recorded
  trace and panics on any non-advisory, non-waived finding. A harness dropped
  without `finish()` still gets the gate via the `CseqGate` Drop backstop;
  `FailoverHarness` gates in its own Drop. `allow_violation(rule,
  justification)` is the **only** sanctioned waiver — see the default test
  requirements in [CLAUDE.md](../../CLAUDE.md).
- **Panic trace** — on any panic before `finish()`, `PanicDump` prints the
  compact wire trace to stderr. Read it before adding instrumentation.

NOT automatic (deliberately — timeout/reap/stall fixtures would false-fail):

- **Leak / termination checks.** The structural close anomalies
  (`inFlightImbalance`, `queueLeak`) do not gate. A call left un-terminated is
  invisible to `finish()` unless it also violated an RFC rule. Assert
  explicitly: `B2buaSut::assert_fully_reaped()` single-SUT,
  `assert_call_fully_over` / `assert_call_fully_released` in failover tests.
  Drain async teardown first with `settle_until`.

## Module-doc pointers (the manual)

| Where | What it explains |
|---|---|
| `crates/sip-clock/src/lib.rs` module doc | The clock-seam rationale: behaviour on `tokio::time`, `now_ms()` timestamps only, HA absolute-deadline + skew re-anchor |
| `crates/sip-net/src/simulated.rs` — `SimulatedSignalingNetwork::new` doc | The definitive ≥1 ms transit-coercion rationale |
| `crates/sip-net/src/rfc_audit/mod.rs` module doc | The RFC audit suite: rule layout, subject dispatch, advisory lanes |
| `crates/scenario-harness/src/agent.rs` module doc + `finish()` doc | What the DSL auto-fills; exactly what gates at `finish()` and what doesn't |
| `crates/scenario-harness/src/callflow.rs` module doc | The reusable choreography — "use these; don't re-type the dance" |
| `crates/b2bua-harness/src/lib.rs` — `B2buaScene` + `assert_fully_reaped` docs | Canonical ports, the 4-invariant leak oracle |
| `crates/failover-harness/src/lib.rs` + `src/harness.rs` | Cluster invariants, `transparent_matrix!`, `worker_clock_offsets` design |
| `crates/ha-harness/src/lib.rs` module doc | Pure repl-plane harness |
| `crates/b2bua/src/timers.rs` module doc | The epoch+`Key` `DelayQueue` timer driver (cancellation correctness, physical removal) |
| [`CONTEXT.md`](../../CONTEXT.md) | Vocabulary — e.g. "callflow choreography" vs ADR-0018 "Callflow shape" vs ADR-0013 `CallScenario` |
