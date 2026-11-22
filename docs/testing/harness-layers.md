# Test-infrastructure layers — the map

One page on what each test crate provides and which harness to reach for.
The detailed contracts live in each crate's module docs; this is the map, not
the manual. The vocabulary is in [CONTEXT.md](../../CONTEXT.md). How to *drive time* in these harnesses is a separate,
critical guide: [test-clock.md](test-clock.md).

## The stack (bottom-up)

Each layer builds strictly on the ones below it. All of it runs in-process —
no real sockets, no real wall-clock dependence.

| # | Crate | Provides |
|---|-------|----------|
| 1 | `sip-clock` | The one clock seam. `Clock::system()` / `Clock::test_at(0)`; `testkit::{advance_settled, settle, pump}` — the primitives behind every harness `advance()` (`advance_settled` behind the scenario harness's, `pump` behind the replication and failover harnesses'). |
| 2 | `layer-harness` | SIP-agnostic recording substrate: `Recorder`, `RunContext`, `EventSequencer`, `RecordedAnomaly`. Tests record first, assert on the recording (ADR-0004/0006). |
| 3 | `sip-net-testkit` | `SimulatedSignalingNetwork`: the in-process datagram fabric, per-hop transit delay (0 is coerced to 1 ms, see the clock guide), drop and synthetic-reply hooks. `rfc_audit`: every rule of the `rfc-rules` registry, run after the call behind `evaluate_rfc_findings`. |
| 4 | `scenario-harness` | The fluent dialog DSL. `Harness` owns the recording sim net, the paused clock and the RFC gate; `Agent` is a fake UA that fills Via, tags, CSeq and Contact per RFC 3261; `callflow` is the INVITE/180/200/ACK choreography. Default transit is **100 ms**, so traces show `received = sent + 100`. |
| 5 | `b2bua-harness` | A **real `B2buaCore`** as the SUT: `B2buaSut`, `B2buaScene` (alice :5060 / bob :5070 / b2bua :5080), `settle_until`, and the **`assert_fully_reaped` leak oracle** (creations == removals; no call, lock, reaper entry, setup-CANCEL mark or limiter hold left). |
| 6 | `failover-harness` | The full HA stack under one paused clock: a **real `ProxyCore`** and N replicating workers over a simulated replication fabric. Fault, membership and drain primitives, the views ledger, the cluster invariants, the ADR-0055 §13 turn points and `Cluster`, and the [failover sweep](#the-failover-sweep). Transit is 1 ms. |
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
  `failover-harness` adds one narrower scope for the **window table** of
  [ADR-0055](../adr/0055-services-are-ha-blind.md) §11, the only SUT-side
  deviations an incident may cause: a test strikes the incident it causes
  (`FailoverHarness::strike(incident)`), and the rows of that incident open
  their RFC windows and dialog-verdict waivers on the messages captured from
  then on. The run's measured tier decides, when the harness drops, which of
  the rows that fired it accepts: none in the Guaranteed tier. What they
  accepted is listed in `accepted_rfc_deviations()` and `fired_rows()`, each
  with its row, and in the unified report as advisory; the rule keeps gating
  on establishment and on every fault-free run. A test never picks a window:
  a SUT-side deviation no row declares is a bug to fix, never a window. A
  party that plays a broken peer by construction opts out with
  `allow_rfc_violation(rule, justification)`, the justification recorded.
- **Dialog table** — every simulated UA keeps an RFC 3261 §12 dialog table
  (`scenario_harness::dialogs`, whose module doc lists the verdicts). An
  in-dialog request on no dialog it holds, on an ended dialog, or with a
  CSeq that goes backwards or skips is a verdict: a scripted `receive` fails
  at once with a `StepError::Dialog`, the actor answers it 481 (500, 491) on
  its own, and `finish()` fails on it. A request that crosses an end of its
  dialog is answered 481 and fails only on a rule it breaks on the dialog
  before that end (the module doc says when it crosses): on any run, the
  UA's own non-2xx final before the peer showed it saw it; on a fabric that
  runs a loss model, also a non-2xx final or a BYE of either side, the
  request a retransmission whose first copy the run's recording shows lost
  and sent before its sender sent that end itself, or at an instant no
  later than its sender read the UA's end from its socket. On such a
  fabric, a CSeq gone backwards that the recorded loss of a first copy of
  that same transaction explains, or skipping over numbers all recorded
  lost on their way to the UA, is answered as ever (500, taken) and is no
  finding. A test that sends such a message on
  purpose waives the `dialog-…` rule on the receiving UA
  (`h.waive(WaiverScope::rule(DIALOG_UNKNOWN, why).on_party("alice"))`); a
  dialog waiver naming no UA covers nothing.
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
  `B2buaScene::finish()` runs `B2buaSut::assert_fully_reaped()` for you; a test on a bare `Harness`
  asserts it explicitly (single-SUT), or `assert_call_fully_over` /
  `assert_call_fully_released` in failover tests. Drain async teardown first
  with `settle_until`. `B2buaScene::finish()` waits through
  `Harness::serve_parties_until`: every UA the harness bound keeps serving
  its transactions (answers a request copy, re-ACKs a repeated final) until
  the SUT is reaped, within 1 s, or, on a fabric with a loss model
  (`with_transit_delay_and_send_fate`), also sends what its own timers owe,
  until the parties' side is quiet for T2 + T1, within 66·T1.

## The actor parties

The reactive actor (`scenario_harness::actor`) plays each party as an RFC 3261
UA over UDP, with its transactions' retransmissions: every request but ACK
goes again until it is answered (an INVITE on Timer A until any response, any
other request on Timer E until its final), and every final to an INVITE goes
again until its ACK, each given up at 64·T1 (`actor/client_retransmit.rs`, the
ladders in `absorption`). A party that plays a peer that never retransmits
opts out by name: `Agent::silence_retransmissions()`.

**Clean end.** A test whose fault may make the SUT end the call arms
`CallCtx::accept_clean_ends()` when the fault strikes. From then on, a party
the SUT ends cleanly (a non-2xx final to her establishing INVITE, a BYE she
answers 200) drops her remaining goals; once the caller is ended so, the call
is, and the verdict records it (`Observation::EndedCleanly`). A request left
without its final or a dialog left open still fails the call.

**Lost datagrams.** The goals and endings read the datagrams the call's loss
model dropped (`CallCtx::loss_notes`): a 2xx lost on its way to the caller,
sent before any final to her CANCEL (arrived or itself lost), crossed the
CANCEL; a lost provisional sent unreliably stands for her ringing, a
reliable one being recovered by its retransmission. The load generator's
loss model notes them; on a
harness fabric with a loss model, `Harness::note_losses` notes each
`SendLost` the recording shows to or from a party of the call, ordered
against the responses delivered to that party before it and, for a datagram
lost on its way to the party, those sent to it before it and not lost.

## The failover sweep

`failover-harness::sweep` is the in-process proof of
[ADR-0055](../adr/0055-services-are-ha-blind.md) §11, in a lane of its own:
`just sweep` (nextest profile `sweep`), on the paused clock, one test per
shape, incident and sync level (`failover_sweep::<shape>::<level>::<incident>`).
A test past 30 s or a lane adding minutes to `just test` keeps it out of the
default lane: the upstream sweep alone is about an hour of CPU.

- **Shapes.** A catalogue of call shapes (`sweep::catalogue`), each a
  `SweepShape`: how the cluster decides the call and what the actor parties
  play. A consumer adds its own shapes with `failover_sweep!` and a ratchet
  of its own, nothing else.
- **Turn points.** Every turn the call takes at its primary, numbered from 1,
  split into three sub-points (`TurnPoint`): `Logged` (the write handed to
  replication), `Left` (the turn's effects sent) and `Applied` (the write
  applied at the backup; only for a turn that wrote). The harness holds the
  turn at the armed sub-point until the incident struck (`turn_points`).
- **Incidents.** `failover_harness::Incident`: primary killed, primary
  killed unannounced (the proxy's probe reads it dead 2 s later, its endpoint
  reads not ready 40 s later and leaves the slice 5 min later), primary rebooted (restarted at a new address
  1 s later, readmitted once ready, it reclaims its calls), backup killed,
  split, split healed, stalled then killed, unbacked then killed. A cell is
  named `<shape>/<level>/<incident>/t<k>-<point>`.
- **Verdict.** Every cell requires no leak on any node or in the limiter, an
  RFC-clean call the parties judge as their play declares, and one CDR, or
  two when the primary dies. The cell strikes its incident on the harness and
  is given the incident's rows of the window table (`failover_harness::windows`:
  RFC windows, dialog-verdict waivers, party outcomes, the clean-end goal
  drop, CDR counts); its measured tier (`windows::tier`) decides which of the
  rows that fired it accepts. A Guaranteed cell fires none, and the evidence
  rule ends none of its calls (no shape sends what can still end one, a
  re-INVITE a proxy refuses after the kill: ADR-0055 §11). Each row names an upstream witness cell that
  must fire it.
- **Ratchet.** `failover-sweep.ratchet` beside the crate's manifest (and a
  consumer's own) lists the cells that fail today, each with the ADR-0055
  section that owns its correction and its cause, and each test's count of
  Guaranteed cells (`sweep::Ratchet`). A test fails on a failing cell the
  ratchet does not list, on a listed cell that passes and on a Guaranteed
  count other than the listed one, so the list only shrinks. An empty list of
  cells is the steady state.
- **Report.** Each test writes what its cells measured to
  `target/sweep-report/<test>.json`; `just sweep-report` adds them into counts
  per row and per tier, and lists the failing Guaranteed cells. It gates
  nothing.

## Writing a new b2bua or failover test

Do NOT hand-roll the INVITE/180/200/ACK dance, it lives once in
`scenario_harness::callflow`. Single-SUT b2bua test: use `B2buaScene::new(name)`
(alice :5060 / bob :5070 / b2bua :5080, routes to bob) then `scene.establish()`
→ interesting part → `scene.hangup(&mut dialog)` → `scene.finish()` (the
reaped check runs there); for a non-default decision use
`B2buaScene::with_b2bua(name, |bob_port| …builder…)`.
HA failover test: `scenario_harness::callflow::establish(&alice,&bob,proxy.addr())`
(or `Call::new(..).no_ring()` for the 200-only variant) and `hangup` for teardown.
ONLY for the uninterrupted happy-path setup, any dance that asserts on the 18x,
reads the relayed cookie/SDP, or injects a crash/partition mid-handshake stays
hand-rolled (those are the subject of the test). When unsure, hand-roll it.

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
