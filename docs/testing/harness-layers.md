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
| 1 | `sip-clock` | The one clock seam. `Clock::system()` / `Clock::test_at(0)`; `testkit::{advance_settled, settle, pump}` — the primitives behind every harness `advance()` (`advance_settled` behind the scenario harness's, `pump` behind the replication and failover harnesses'). |
| 2 | `layer-harness` | SIP-agnostic recording substrate: `Recorder`, `RunContext`, `EventSequencer`, `RecordedAnomaly`. Tests record first, assert on the recording (ADR-0004/0006). |
| 3 | `sip-net-testkit` | `SimulatedSignalingNetwork` — in-process datagram fabric keyed by `SocketAddr`, per-hop transit delay (0 is coerced to 1 ms — see the clock guide), `SendFault` + per-bind `PreIngress` hooks for drop/synthetic-reply injection. Plus `rfc_audit`: the RFC 3261/3262/3264 post-call rule suite (~77 rules, subject-dispatched per bind role, advisory/gating lanes) behind the single evaluator `evaluate_rfc_findings` (`evaluate_rfc_audit` adds each rule's population: occasions, decided, hits). |
| 4 | `scenario-harness` | The fluent dialog DSL. `Harness` owns the recording-wrapped sim net, `Clock::test_at(0)`, and three RAII guards (`CseqGate`: RFC hard gate even if you forget `finish()`; `FailureDump`: the failure dump on panic, see [Reading a failure](#reading-a-failure); `ArtifactDump`: full report artifacts on any drop without `finish()`, gated by `SCENARIO_ARTIFACT_DIR`). `Agent` is a fake UA that auto-fills Via/tags/CSeq/Contact per RFC 3261. `callflow` is the canonical INVITE/180/200/ACK choreography (`establish`, `hangup`, `Call::new(..).no_ring()`). Default transit is **100 ms** (`Harness::new`), so traces show `received = sent + 100`. |
| 5 | `b2bua-harness` | Binds a **real `B2buaCore`** as the SUT. `B2buaSut` (`route_all_to`, metrics, `cdr_records`), `B2buaScene` (alice :5060 / bob :5070 / b2bua :5080, the first three addresses of its harness's allocator; `establish`/`hangup`/`finish`, which runs the leak oracle), `settle_until` (bounded yield-poll to drain async teardown), and the **`assert_fully_reaped` leak oracle**: creations == removals, `active_calls == 0`, `lock_count == 0`, reaper touched-ledger empty, no setup-CANCEL mark, every limiter hold released once and the limiter store empty. Every `B2buaSut` traces every call (`failure_view::trace_every_call`; `B2buaSutBuilder::untraced` for a cost measurement) and adds its timer ledger and model-invariant record to the failure dump. |
| 6 | `failover-harness` | The full HA stack under ONE paused clock: a **real LB `ProxyCore`** + N replicating `b2bua` workers over a simulated replication fabric, one shared `EventSequencer` across the SIP + repl planes. Fault primitives: `crash()`, `reboot()` (fresh SIP IP, **hard-asserts pristine**), repl `partition`/`heal`/`delay` (named on two workers, they reach every replication stream between the two nodes, present and future — the fabric attributes a puller's ephemeral local to its node), SIP-plane `cut_signalling`/`restore_signalling`, `set_health`, `with_worker_clock_offset` (deterministic inter-node wall skew — see the clock guide), and `with_worker_tune` (a `B2buaConfig` mutator re-applied on every worker spawn AND reboot). Membership primitives: `withdraw(ord)`/`readmit(ord, addr)` (the orchestrator takes an endpoint out of the proxy's registry AND every peer's membership while the process keeps running), `spawn_replacement(ord)` (a second live incarnation of one ordinal, gen+1, fresh SIP + repl addresses, peers re-pointed at it), `begin_drain` / `begin_drain_pending` (the drain left in flight, so the timeline runs inside the shutdown window). The **views ledger** records what the orchestrator, the proxy and each incarnation believed about each worker — written by those primitives and by a sampler that reads the live components after every advance chunk — and the report renders it as a views table plus the disagreements it found. Cluster invariants: `assert_single_owner`, `assert_call_fully_over`, `assert_call_fully_released`, the `transparent_matrix!` differential oracle. ADR-0055 §13 verbs: turn points (`turn_points()`: arm one sub-point of one turn, get the turn held there) and the `Cluster` (`place_next_call` on a named primary, `steer` to a role by health, kill or partition, `view` by role). The failover sweep (`failover_sweep!`, ADR-0055 §11) cuts a catalogue of shapes at every turn point by each incident and holds the failing cells to a ratchet, in its own lane (`just sweep`). Transit is 1 ms; its `advance` is `testkit::pump_sampled` (settled 100 ms chunks). |
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
| A SUT's HTTP exchanges stated as a program: any request the program does not state fails the run | `http-net-testkit::scripted` — `ScriptedHttpService` bound on the scene's `SimulatedHttpNetwork` (or `RealHttpNetwork`), one `add` per expected conversation, `verdict()` at the end; served through a `RecordingHttpNetwork` built on `Harness::recorder()`, its exchanges join the ladder and `report::http::verdict_anomalies` puts the verdict in the report |
| Real cluster / load / endurance | `crates/loadgen` + `e2e/` (see their READMEs) — out of scope for this doc |

## What is checked automatically vs. what you must assert

Automatic (you cannot opt out, only waive per-rule):

- **RFC hard gate** — `Harness::finish()` runs the audit over the recorded
  trace and panics on any non-advisory, non-waived finding. A harness dropped
  without `finish()` still gets the gate via the `CseqGate` Drop backstop;
  `FailoverHarness` gates in its own Drop. To take a **peer-side** deviation out
  of the gate, waive it (see the [cookbook](#cookbook)); a waiver without a
  party covers simulated parties only, so a finding charged to the SUT stays
  gated unless a waiver names the SUT's lane, and `finish()` counts and prints
  every such waived SUT finding. See the default test requirements in
  [CLAUDE.md](../../CLAUDE.md), which forbid waiving a finding the SUT caused
  unless it is an accepted deviation.
  `failover-harness` adds one narrower scope for a deviation an ADR declares an
  **accepted trade-off** (today only ADR-0014's dual-owner in-dialog CSeq
  overlap): `accept_rfc_deviations_from_now(rule, justification)` /
  `resume_rfc_gate(rule)` accept the rule only on messages captured inside the
  window (arm it at the fault injection), list what they accepted in
  `accepted_rfc_deviations()` + the unified report as advisory, and leave the
  rule gating on establishment and on every fault-free scenario — so the run
  still has a baseline. It is not a waiver hatch: an accepted deviation needs
  the ADR entry AND the scenario's own assertion on the accepted outcome. A
  SUT-side deviation with no such ADR entry is a bug to fix, never a window.
- **Dialog table** — every simulated UA (an `Agent`, the reactive actor's
  agents, the load lane's) keeps an RFC 3261 §12 dialog table
  (`scenario_harness::dialogs`), fed by every message it sends, sights,
  drains or leaves unread at `finish()`. A received in-dialog request on no
  dialog it holds (an unknown From- or To-tag, or another fork's early
  dialog past the §13.2.2.4 window), on a dialog a non-2xx final or a BYE
  ended, a re-INVITE or a callee's BYE on an early dialog, a CSeq that goes
  backwards or skips, and a 2xx to a request it sent on a dialog a non-2xx
  final ended, are verdicts: a scripted `receive` or `expect` of the message
  fails at once with a `StepError::Dialog` naming the UA, the message and
  the tags it holds (`alice: OPTIONS From-tag A1 names no dialog (holds
  A2)`); the actor answers a refused request 481 (500 for a CSeq going
  backwards, 491 for an early re-INVITE) on its own; and `finish()`, the
  Drop backstop, `FailoverHarness`'s Drop and the load lane's
  `rfc_findings` fail on every verdict. A test that sends such a message on
  purpose opts in on the receiving UA:
  `h.waive(WaiverScope::rule(DIALOG_UNKNOWN, why).on_party("alice"))`
  (`dialog-unknown`, `dialog-ended`, `dialog-early-request`,
  `dialog-ended-answered`, `dialog-cseq-backwards`, `dialog-cseq-skip`). A
  dialog waiver naming no UA covers nothing and is dead, like one that covers
  nothing; a waived verdict counts on the `waived SUT findings` line. A
  proxy agent keeps no table (`Agent::hold_no_dialogs` for a hand-built one),
  and the scenario interpreter reads the verdicts itself
  (`Agent::hand_dialogs_to_driver`).
- **Model invariants** — every harness B2BUA (`B2buaConfig::model_invariants`,
  on by default in debug builds) checks the call record at the end of each
  turn against the list in `b2bua::rules::model_invariants` and panics with
  the invariant's name, the turn and the call's dump on a violation.
- **Rule coverage** — `finish()` writes one TSV row per RFC rule the run
  exercised (`test, rule, occasions, decided, hits, waived`) under
  `<target>/rule-coverage/`, one file per test, from
  `sip_net_testkit::evaluate_rfc_audit`'s per-rule population, after a
  `# run=<nextest run id>\texe=<test binary>` header. `waived` names whose
  findings of the rule a waiver covered in that run (`-`, `sut`, `simulated`,
  `sut+simulated`).
- **Failure dump** — on any panic before `finish()` returns, gate failures
  included, the harness prints the failure dump to stderr
  ([Reading a failure](#reading-a-failure)). Read it before adding
  instrumentation.
- **Failure artifacts** (env-gated) — with `SCENARIO_ARTIFACT_DIR=<dir>` set, a
  harness dropped without `finish()` (panic unwind, a finish-time gate failure,
  or a forgotten `finish`) writes the full SVG/HTML/text ladders under
  `<dir>/<name>/`, FAIL-bannered and carrying the panic message. Unset ⇒ off,
  so CI stays artifact-free. `Harness::snapshot_report(&self)` renders the same
  report from a live harness without consuming it.

NOT automatic (deliberately — timeout/reap/stall fixtures would false-fail):

- **Leak / termination checks.** The structural close anomalies
  (`inFlightImbalance`, `queueLeak`) do not gate. A call left un-terminated is
  invisible to `Harness::finish()` unless it also violated an RFC rule.
  `B2buaScene::finish()` and the e2e fake infra's `InfraRuntime::finish()` run
  `B2buaSut::assert_fully_reaped()` for you; a test on a bare `Harness`
  asserts it explicitly (single-SUT), or `assert_call_fully_over` /
  `assert_call_fully_released` in failover tests. Drain async teardown first
  with `settle_until`.

## Cookbook

The agent verbs a test reaches for most, each in a few lines.

**Receive a request, or fail with a step error** (`receive` panics on the
same error):

```rust
let mut uas = bob.try_receive("INVITE").await?; // Err(StepError::…) names what came instead
uas.respond(180, "Ringing").await;
```

**Expect nothing for a window**: advance the paused clock, then nothing is
queued. (`try_receive` on silence is `Err(StepError::Timeout { .. })` after
the receive bound.)

```rust
h.advance(Duration::from_secs(2)).await;
assert!(bob.take_queued().await.is_none(), "bob hears nothing");
```

**PRACK a reliable provisional** on the early dialog, with an explicit `RAck`
(`<RSeq> <CSeq> INVITE`, RFC 3262 §7.2):

```rust
call.expect(180).await;                                  // reliable, RSeq 1
let mut prack = call.send_request(InDialogMethod::Prack).with_rack("1 1 INVITE").send().await;
bob.receive("PRACK").await.respond(200, "OK").await;
prack.expect(200).await;
```

**Waive a deliberate deviation** on the party that commits it. A waiver
without `.on_party(..)` (`allow_violation`, `WaiverScope::rule` alone)
covers simulated parties only; a finding charged to the SUT needs a waiver
naming the SUT's lane, with the reason it is accepted:

```rust
h.waive(WaiverScope::rule("cseq-in-dialog-order", "alice reuses a CSeq on purpose").on_party("alice"));
h.waive(WaiverScope::rule(rule, why).on_party("alice").at_position(12)); // one message only
```

**Send a request a UA's dialog table refuses** (an unknown tag, an ended
dialog, a CSeq going backwards or skipping): opt in on the receiving UA with
the `dialog-…` rule.

```rust
use scenario_harness::dialogs::DIALOG_UNKNOWN; // also DIALOG_ENDED, DIALOG_CSEQ_BACKWARDS, DIALOG_CSEQ_SKIP
h.waive(WaiverScope::rule(DIALOG_UNKNOWN, "bob's BYE names a foreign tag").on_party("alice"));
```

**Answer from a fork that sent no provisional** (RFC 3261 §13.2.2.4): mint
the winner's tag, then send the 2xx.

```rust
let mut uas = bob.receive("INVITE").await;
let tag = uas.answer_unrung();
uas.respond(200, "OK").with_sdp(ANSWER).send().await;
```

**Take addresses from the harness** instead of picking ports:

```rust
let carol = h.agent_auto("carol").await;   // bound on h.next_addr()
let lb = h.next_addr();                    // a fresh address for a SUT bind
```

**Read a failure**: see [Reading a failure](#reading-a-failure); the first
line says why.

## Reading a failure

A failing harness test says why in the first line of its panic message, and
the failure dump on stderr opens with the same line:

- **A step** names the party, what it expected and what it got, then the
  step's ordinal among the run's waits and the test line that awaited it:
  `alice expected 481, got 200 OK at step 7 (expect at tests/it/foo.rs:42)`.
- **A timeout** names the awaited message and its dialog: a response by its
  status and request (`alice timed out after 5s waiting for a 200 to BYE
  CSeq 3 (Call-ID c, From-tag a1, To-tag b1)`), a request by its method and
  the dialogs the UA holds, the ones it should name.
- **The RFC audit gate** says so apart from an assertion: `red because RFC
  audit rule <id> fired (charged to <party>) at message #<n> (<sender> →
  <receiver>  <first line>)`, then one line per finding. The `#n` is the
  message's position in the audit's view, the index a
  `WaiverScope::at_position` takes. Do not flip an `expect` to see what the SUT
  did: the dump's ladder and trace already show it.
- **A dialog-table verdict** reads `red because dialog rule <rule> fired on
  <ua>: <request> …`.
- **A model invariant** a B2BUA turn broke reads `model invariant <name>
  violated after <turn>: <detail>`, the call's record following. The
  dispatcher isolates that turn's panic, so the scenario may fail later on
  something else (a timeout): the dump then leads with `red because model
  invariant …` and puts the scenario's own failure on a `then:` line.
  `finish()` fails on it too.

The dump follows, in this order:

1. `══ FAILED '<name>': <digest> ══` — the line above.
2. **wire ladder** — every recorded message, sender → receiver, relative time.
3. **SUT trace per call** — each sampled call's trace-plane facts (ADR-0026),
   grouped per call, one line each: `sip.in`/`sip.out` with the message's
   first line, `rule.fired`, `rule.lifecycle`, `rule.transition`,
   `call.transition`, `limiter`, `http.*`. Any trace kind a SUT adds (timer
   arm/cancel/fire facts) appears here with no harness change. Bounded: the
   last 8 calls, the last 80 facts of each.
4. **What each SUT adds** (`FailureView`): for a B2BUA, the timer ledger of
   every live call — id, deadline relative to now, leg.
5. **UA dialog tables** — each simulated UA's dialogs (Call-ID, local and
   remote tag, state, last remote CSeq) and the verdicts it recorded.
6. **SUT log** — the tail of the lifecycle log lines (the last 200).

`scenario_harness::expect_failure` runs a scenario expected to fail and
returns its panic message and dump: the seam a test of the failure output
uses.

## Module-doc pointers (the manual)

| Where | What it explains |
|---|---|
| `crates/sip-clock/src/lib.rs` module doc | The clock-seam rationale: behaviour on `tokio::time`, `now_ms()` timestamps only, HA absolute-deadline + skew re-anchor |
| `crates/sip-net-testkit/src/simulated.rs` — `SimulatedSignalingNetwork::new` doc | The definitive ≥1 ms transit-coercion rationale |
| `crates/sip-net-testkit/src/rfc_audit/mod.rs` module doc | The RFC audit suite: rule layout, subject dispatch, advisory lanes |
| `crates/scenario-harness/src/agent/mod.rs` module doc + `finish()` doc | What the DSL auto-fills; exactly what gates at `finish()` and what doesn't |
| `crates/scenario-harness/src/agent/failure_dump.rs` module doc | The failure dump: the digest, its sections, `FailureView` |
| `crates/scenario-harness/src/callflow.rs` module doc | The reusable choreography — "use these; don't re-type the dance" |
| `crates/b2bua-harness/src/lib.rs` — `B2buaScene` + `assert_fully_reaped` docs | The scene's addresses, the six-check leak oracle |
| `crates/failover-harness/src/lib.rs` + `src/harness.rs` | Cluster invariants, `transparent_matrix!`, `worker_clock_offsets` design |
| `crates/ha-harness/src/lib.rs` module doc | Pure repl-plane harness |
| `crates/http-net-testkit/src/scripted/mod.rs` module doc | The scripted HTTP service: program, continuation token and its codec (ADR-0036), faults, hard-failure rule |
| `crates/http-net-testkit/src/recording.rs` module doc | HTTP on the `Recorder`: the client and served sides, abandoned exchanges, pairing |
| `crates/b2bua/src/timers.rs` module doc | The epoch+`Key` `DelayQueue` timer driver (cancellation correctness, physical removal) |
| [`CONTEXT.md`](../../CONTEXT.md) | Vocabulary — e.g. "callflow choreography" vs ADR-0018 "Callflow shape" vs ADR-0013 `CallScenario` |
