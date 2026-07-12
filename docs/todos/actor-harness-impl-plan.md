# Implementation plan — per-endpoint actor harness (v2, post-review)

Companion to the design proposal (artifact 538df671). Concrete build sequence:
modules, real signatures, edit points, test strategy, green-tree checkpoints.

**v2 folds in an independent Fable review (verdict: v1 needed rework before P0).**
Changelog vs v1, with the review IDs:
- **B1** concurrency loop rewritten (the v1 `select!` froze all actors after the
  first finished).
- **B2/B4** `SettleDriver` trait DELETED and `?Send` removed — everything stays
  `Send` (the load lane `tokio::spawn`s `run_one`; the functional lane is
  `start_paused` + tokio auto-advance, not a pump). Plain `tokio::time` is the
  lane-agnostic lever.
- **B3** reactor treats `Timeout` as *continue*, only `QueueClosed` is fatal.
- **B5** `await_pred` pins + `enable()`s the `Notified` before the predicate read.
- **B6** endpoint state machine specced (pending UAS txn; interruptible timed
  answers; delayed-offer in the default path).
- **B7** downstream-contract table is a P1 *entry* artifact; `who` uses bounded
  barrier names; the settle verdict gets an explicit `StepError` mapping.
- **B8** the trait fork touches `e2e-model` (registry) too — added to scope.
- **B9** the loss-recovery proof is mux-based (sim-net has no drop model).

Target crate: `crates/scenario-harness`, new `src/actor/` module; `realcall/*`,
`loadgen/src/driver.rs`, and `e2e-model` registry edited only at the seam.

---

## 0. Invariants at every commit
- **Green tree after each commit** — new code behind a trait fork; nothing shared
  changes signature until the P4 delete.
- **No downstream contract change** — an actor body still yields
  `Result<(), StepError>` with the *same* variant **and `who` string** the driver
  buckets on (`loadgen/src/class.rs` keys the case on `StepError` variant +
  `step_who` + last phase; `who` is bounded-cardinality — free-form text is never
  keyed). `CallEnv`/`CallScope`/`CallCtx` keep their surface.
- **Everything `Send`** — like `RealCallScenario` today (`realcall/mod.rs:41`,
  `Send + Sync`; `UdpEndpoint: Send + Sync`, `net.rs:23`; `Agent` is Arc/String;
  `CallCtx` is `Mutex`-based for `Sync`). Only `Harness` is `!Send` (Rc) and the
  executor never holds it.
- **Paused clock** (docs/testing/test-clock.md): use **`tokio::time`** exclusively
  (incl. `tokio::time::Instant`, NOT `std::time::Instant`, which freezes under
  `start_paused`). Under `start_paused` tokio auto-advances to the earliest
  pending timer when idle — so a barrier `sleep_until` can never leap past the
  transit/Timer-E deadline that produces the fact it awaits (rule 2 holds by
  construction).
- **RFC hard gate** — the *default* answer path answers offer-carrying
  INVITE/UPDATE with SDP; it MUST NOT fall through to a bodyless 200 (RFC 3264 §5).
- **WSL2** — one `cargo` build/test at a time; none concurrent with a load run.

---

## 1. Concurrency model — joined futures on ONE task (no spawn)

Actors are concurrent *futures* joined within the one per-call task the driver
already owns — not `tokio::spawn`ed. Rationale: determinism under the paused
clock and no `'static` gymnastics. (The `!Send` argument from v1 was wrong —
everything here is `Send`; and the failover harness *spawns* tasks + pumps, so it
is not a precedent for this exact shape. Joined-on-one-task is new here and must
be written carefully.)

```rust
// CallController::run — B1-corrected. An actor future resolves only on its own
// FATAL error; a normal actor that reaches its exit condition must NOT collapse
// the join.
async fn drive_actors(mut actors: FuturesUnordered<ActorFut>) -> StepError {
    while let Some(r) = actors.next().await {
        if let Err(e) = r { return e; }         // first fatal actor error wins
    }
    std::future::pending().await                 // all actors done cleanly → never resolve
}

let obs = ObservedState::new();
let actors: FuturesUnordered<_> = specs.into_iter().map(|s| run_actor(s, obs.clone())).collect();
tokio::select! {
    verdict = self.drive_to_verdict(&obs, plan, settle) => verdict,   // barriers → settle → Ok/Fail
    err     = drive_actors(actors)                       => CallVerdict::Failed(err),
}
// On resolution the loser future is DROPPED. Drop-safety (single-task cooperative):
// drops land only at await points; teardown always goes through CallScope (the
// controller owns it), so a dropped reactor cannot leak SUT state.
```

**Drop-safety rule (write it into the code):** the caller-side "ACK a 2xx →
`scope.set_confirmed(dialog)`" sequence must have **no `.await` between the ACK
send and the scope registration**, so a mid-window cancellation can never leave a
confirmed-but-unregistered dialog.

---

## 2. Module layout

```
crates/scenario-harness/src/actor/
  mod.rs      // CallController, run entrypoints, drive_actors
  state.rs    // ObservedState, StateInner, LegObservation, Observation, await_pred
  ledger.rs   // ObligationLedger, ObligationKey/Kind, InDialogCseq gap detector
  settle.rs   // SettleBarrier (plain tokio::time; NO driver trait)
  actor.rs    // ActorSpec, Disposition, ActorState, DialogTable, run_actor, default_react
  goals.rs    // Goal, GoalStep, Barrier, GoalCursor
  spec.rs     // ActorCall, ActorScenario trait, BarrierPhase, Expect, verdict→Result
```
`Agent::recv_any` (+ `Inbound`) is added to `src/agent.rs`.

---

## 3. Phase P0 — substrate (nothing else references `actor::`)

### 3.1 `Agent::recv_any` (agent.rs)  — decision 4: CONFIRMED feasible
`ServerTxn::from_request` is method-agnostic (`agent.rs:2739`) and
`try_receive_tolerating_blocking` already builds one for every request incl. ACK
(`agent.rs:1395`). Factor that path so:
```rust
pub async fn recv_any(&self) -> Result<Inbound, StepError>;   // shares the TxnView dedup path
pub enum Inbound { Request(ServerTxn), Response(SipResponse) }
```
Rider: the `TxnView` absorbs byte-identical retransmits WITHOUT replaying the
answer (`agent.rs:924`); recovery of a lost answer is the mux `CallTxns`' job
(`mux.rs:1296`, load lane only) — the settle logic must not assume the reactor
re-sees a retransmit.

### 3.2 `state.rs` — monotone fact-store + the ONE wait
```rust
#[derive(Clone)]
pub struct ObservedState { inner: Arc<Mutex<StateInner>>, tick: Arc<Notify> }
// apply(o) is monotone (LegEarly never downgrades Confirmed) AND idempotent
// (re-applying a fact is a no-op) so a double-observation is harmless.
```
```rust
// B5-corrected: register the waiter BEFORE reading the predicate.
pub async fn await_pred<F: Fn(&StateInner) -> bool>(
    obs: &ObservedState, pred: F, deadline: Instant,   // tokio::time::Instant
) -> Result<(), StepError> {
    loop {
        let notified = obs.tick.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();                       // <-- registers now, not on first poll
        if pred(&obs.snapshot()) { return Ok(()); }
        if Instant::now() >= deadline { return Err(barrier_timeout()); }  // who = BARRIER NAME (B7)
        tokio::select! { _ = &mut notified => {}, _ = tokio::time::sleep_until(deadline) => {} }
    }
}
```
`barrier_timeout()` carries `StepError::Timeout { who: <barrier-name> }` — a
bounded label the case-keyer accepts; the free-form gap description goes to a
detail/log channel, never into `who` (B7-b).

### 3.3 `ledger.rs` — the settle contract  (omission: CSeq-space fix)
```rust
pub struct ObligationLedger {
    open: HashMap<ObligationKey, Obligation>,     // sent-awaiting-final / answered-awaiting-ACK
    dialogs: HashMap<DialogKey, InDialogCseq>,     // per-DIALOG CSeq completeness, all methods
}
struct InDialogCseq { high_water: u32, seen: BTreeSet<u32> }  // NOT NOTIFY-only
```
The NOTIFY gap is detected against **all in-dialog CSeqs on that dialog**, not a
NOTIFY-only stream — the SUT's BYE/keepalive-OPTIONS share the dialog CSeq space
(RFC 3261 §12.2.1.1), so a NOTIFY-only `seen == 1..=high_water` would see phantom
holes. `Obligation.opened_at: tokio::time::Instant`. Pure unit tests (no clock):
open/close, a real gap keeps `is_closed()==false`, `describe_open` names leg+cseq,
a non-NOTIFY request filling the number closes the hole.

### 3.4 `settle.rs` — barrier, plain tokio::time (B2/B4: no driver)
```rust
pub struct SettleBarrier { pub ceiling: Duration }        // 32 s = 64*T1
pub enum SettleVerdict { Ok, Fail(Vec<String>) }
impl SettleBarrier {
    pub async fn wait(&self, obs: &ObservedState) -> SettleVerdict {
        let deadline = Instant::now() + self.ceiling;      // tokio::time::Instant
        loop {
            if obs.ledger_closed() { return SettleVerdict::Ok; }
            if Instant::now() >= deadline { return SettleVerdict::Fail(obs.describe_open()); }
            tokio::time::sleep(T1).await;                   // real on load; auto-advanced under start_paused
        }
    }
}
```
Same code both lanes. Under `start_paused` the `sleep(T1)` auto-advances only
while the reactors are idle, so a re-emit that lands mid-window wakes them first.

### 3.5 `actor.rs` — ActorState + run_actor + default_react  (B3, B6)
```rust
struct ActorState {
    agent: Agent, disposition: Disposition, media: MediaState,
    dialogs: DialogTable,          // confirmed Dialog(s) + pending ClientInvite(s) + PENDING UAS ServerTxn(s)
    pending_answer: Option<TimedAnswer>,   // a ring/answer scheduled for `at` — an interruptible select arm
    goals: GoalCursor, obs: ObservedState, scope: CallScope, ctx: CallCtx,
}
async fn run_actor(mut st: ActorState) -> Result<(), StepError> {
    loop {
        tokio::select! {
            inbound = st.agent.recv_any() => match inbound {
                Ok(m)  => default_react(&mut st, m).await?,
                Err(StepError::Timeout { .. }) => continue,          // B3: reactor deadlines are NOT fatal
                Err(StepError::QueueClosed { .. }) => return Ok(()), // only fatal receive error
                Err(e) => return Err(e),
            },
            _ = fire_timed_answer(&mut st), if st.pending_answer.is_some() => {}  // B6: ring→answer as its OWN arm
            ready = st.goals.next_ready(&st.obs), if st.goals.has_pending()
                => ready.drive(&mut st).await?,
        }
        if st.obs.snapshot().call_torn_down() && st.goals.is_exhausted() { return Ok(()); }
    }
}
```
`DialogTable` holds **pending UAS `ServerTxn`s** (B6) — required by:
- CANCEL: answer 200 to the CANCEL **and 487 on the retained INVITE txn**
  (`failures.rs:88-96`; without the 487 the SUT b-leg waits Timer C and
  `assert_fully_reaped` hangs).
- `Reject(486/603)` dispositions (`failures.rs`, `rerouting_prack.rs`).
- reliable-183 → hold the INVITE txn → answer 200 only after PRACK
  (RFC 3262 MUST-014, `realcall/mod.rs:296-317`).

`default_react` — extracted from the answer table in
`try_receive_tolerating_blocking` (`agent.rs:1376-1421`), plus the caller side.
Critical: the `INVITE/UPDATE` arm consults `disposition`/`media` and answers with
SDP; there is **no bodyless-200 fallthrough** (B6-c: a delayed-offer bodyless
re-INVITE gets 200 **+ our offer SDP**; the `_ =>` catch-all is for non-dialog-
affecting methods only). Timed dispositions (`RingThenAnswer`) schedule
`pending_answer` instead of sleeping inline, so a CANCEL mid-ring is still
processed (B6-b). `Inbound::Response` → `dialogs.on_response` (ACK a 2xx with no
intervening await before `set_confirmed`; record `LegConfirmed`/status; open a BYE
obligation on our BYE; reproduce the scope→`Terminated` flip on a ≥200 final that
`InviteReject`/`admitted_uas` do, `failures.rs:54`, `mod.rs:215`).

### 3.6 `goals.rs`
`GoalStep::{Invite{candidates,media}, Reinvite, Update, Options, Refer, Cancel,
Bye, After, EveryUntil{cadence,until}, OnObserved(pred)}`. `Barrier::{None,
Named, Pred}`. In-dialog-sending goals (Reinvite/Update/Refer) MUST carry a
barrier guard against concurrent realigns — glare is newly *possible* with two
spontaneous senders (omission), where linearity precluded it.

### 3.7 P0 exit: `cargo test -p scenario-harness` green (ledger unit, two-actor
toy call reaching torn_down under `start_paused`, a fold-order determinism test);
no `realcall` body/runner references `actor::`.

---

## 4. Phase P1 — adapter + refer + parity

### 4.1 `spec.rs`
```rust
pub struct ActorCall { pub actors: Vec<ActorSpec>, pub plan: Vec<BarrierPhase>,
                       pub settle: SettleBarrier, pub expect: Expect }
pub enum Expect { HappyBye, Reject(u16), AbandonedEarly, TransferDeclined }
#[async_trait]                                     // Send (NOT ?Send)
pub trait ActorScenario: Send + Sync {
    fn id(&self) -> ScenarioId;
    fn build(&self, env: &CallEnv<'_>) -> ActorCall;   // extracts OWNED state (clones Agents, copies knobs)
}
```

### 4.2 P1-ENTRY ARTIFACT — the downstream-contract table (B7, decisions 3/5)
Before porting, produce (verified against `class.rs`, `driver.rs:525-618`,
`e2e-model/registry.rs`) a per-body table pinning:
- **NOK terminal**: exact `StepError` variant **and `who`** — incl. the SYNTHETIC
  ones: `Timeout { who: "alice-abandoned-after-ringing" }` (`failures.rs:104`),
  `UnexpectedKind { who: "refer_charlie_reject" }` (`failures.rs:178`). These
  `who` strings are sample-directory keys → reproduce byte-for-byte.
- **Settle verdict mapping**: `CallVerdict::Settle(open)` → a FIXED
  `StepError::Timeout { who: "settle" }` (driver untouched) — decided here, not
  discovered.
- **mark_ringing**: which observation feeds `ctx.mark_ringing` (the cross-call
  >99% 18x gate, `driver.rs:595`) — entirely absent from v1.
- **phases**: `connected/referred/transferred/rerouted/pracked/updated/reinvited/
  keepalive_ack/bye_200` — several fire on OBSERVATIONS not barriers
  (`keepalive_ack` on first OPTIONS-200, `long_call.rs:44`); the chaos classifier
  + case keys consume them.
- **anchors**: the published contract `LOAD_CALL_ANCHORS`/`LOAD_REFER_ANCHORS`/
  `PRACK_ANCHORS` (`e2e-model/registry.rs:374-426`) — each is `AnchorKeys` from a
  SPECIFIC message (`env.rs:402`), so anchors attach at reaction/goal-drive time
  with the message in hand (incl. `anchor_sent` for the REFER, `refer.rs:91`, and
  `firstProvisional`'s "only when it arrived" optionality). Barriers hold no
  message — labels-on-barriers is insufficient (decision 5).

### 4.3 verdict → Result (B7)
`into_result`: `Expect::HappyBye + Ok` → `Ok`; an `Expect::Reject(486)` path
observed → the SAME `Err(StepError::WrongStatus{got:486,..})` the linear body
returned; `Settle(open)` → `Err(Timeout{who:"settle"})`; a declared expect not
reached → `Err`. The driver's bucketing (`class.rs`) is untouched.

### 4.4 adapter seam (B8: includes e2e-model)
- `realcall/mod.rs`: add `run_actor_collecting`/`run_actor_asserting` (plain
  `tokio::time`; no driver arg).
- `e2e-model/registry.rs` (`ShapeDescriptor::load_scenario` → `Arc<dyn
  RealCallScenario>`, ctor at :29-33,264,331): add an `ActorScenario` arm — a
  `Scenario` enum `{ Linear(Arc<dyn RealCallScenario>), Actor(Arc<dyn
  ActorScenario>) }`. `MixEntry::from_shape` (`driver.rs:159`) carries it.
- `loadgen/src/driver.rs::run_one` (~453-565): branch on the enum; everything
  after (teardown, `class.rs` classification, `should_record` sampling,
  `record_ringing`, phases) unchanged.
- `loadgen/src/scenarios/mod.rs`: re-export the actor bodies.
- `b2bua-harness/tests/realcall_functional.rs`: flip the refer case to
  `run_actor_asserting` (auto-advance, no pump).

### 4.5 Port `Refer`; P1 exit
- functional refer via actor runner + `assert_fully_reaped` green.
- **B9 loss proof is MUX-based** (sim-net has no drop model): a `loadgen`-lane
  test (drop_rate>0 + auto-retransmit) shows the actor refer RECOVERS a dropped
  SUT→peer ACK/NOTIFY where the linear form stranded. (Functional peers own no
  retransmit engine, so only SUT-originated drops are recoverable there — which
  is exactly failure 1/2.)
- linear + actor bodies coexist in the load mix.

---

## 5. P2 — settle verdict + smoke  |  6. P3 — collapse every body  |  7. P4 — canary + delete
- **P2**: verdict already includes the settle barrier; `loadgen/tests/smoke.rs`
  asserts the 2nd-NOTIFY ack gate (recover + permanent-fail-names-obligation).
  Note (omission): a drop-hit call holds its `max_in_flight` permit up to 32 s —
  size the end-of-run drain (`driver.rs:377`).
- **P3** order: `ReferCharlieReject` → `ReroutingPrack` → `PrackUpdate` →
  `Reinvite` → `OptionsHold` → `LongCall` → `InviteReject`/`AbandonRinging` →
  `basic_call` (degenerate). Net-new this phase: `EveryUntil` (options_hold),
  `DelayedOffer` (reinvite). Emergency variants (`basic_call_em`, `reinvite_em`)
  reuse ported bodies via descriptor flags — name them in the parity set.
- **P4**: endurance parity; anomaly classes gone; delete linear bodies +
  `establish`/`admitted_uas`/`complete_100rel`/`hangup` **and** the e2e-model
  `Linear` arm + concrete-type imports (`registry.rs:29`) — a cross-crate sweep,
  not "~260 LOC". Full `just test`.

---

## 8. Open decisions — RESOLVED by the review
1. concurrency: joined futures on one task, `drive_actors` loop (B1). ✔
2. clock: NO driver trait; plain `tokio::time`, auto-advance on the paused lane
   (B2/B4) — no deadlock (tokio advances to the earliest timer). ✔
3. Expect→class: reproduce exact variant+`who`; settle→`Timeout{who:"settle"}`;
   feed phases/checkpoints/anchors/mark_ringing (B7, P1-entry table). ✔
4. recv_any/ServerTxn for any request: confirmed (`agent.rs:2739,1395`). ✔
5. anchors: attach at reaction time with the message; pin the published contract
   table in P1 (decision 5). ✔
6. per-actor dialog ownership: correct, BUT the table also holds pending UAS
   txns (B6). ✔

## 9. Verification matrix
| Phase | Gate |
|---|---|
| P0 | `cargo test -p scenario-harness`: ledger unit, toy-call under start_paused, fold-order determinism; nothing references `actor::` |
| P1 | functional refer via actor runner + `assert_fully_reaped`; mux loss-recovery test; `cargo check --workspace`; the P1-entry contract table verified |
| P2 | smoke 2nd-NOTIFY ack-gate (recover + permanent-fail) |
| P3 | each body's functional-gate case green through the actor runner |
| P4 | endurance parity; anomaly classes gone; cross-crate dead code deleted; `just test` |
