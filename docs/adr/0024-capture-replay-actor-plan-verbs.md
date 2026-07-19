# 0024 — Capture-replay verbs in the actor plan: one executor for every lane

Status: proposed (2026-07-19, revised same day after adversarial review)

## Context

The actor harness (ADR-0018/0021, `scenario-harness/src/actor/`) executes
declarative multi-party call plans — per-endpoint `ActorSpec`s with goal
cursors and barriers over shared observed state, a reactive core
(`default_react`) that consumes every inbound datagram, and a settle-gated
verdict. It runs on the functional lane (fake or real infra) and on the load
lane (`LoadBody::Actor` via `run_actor_scenario`).

Replay of captured call choreography — driving simulated peers through the
exact message sequence of a recorded call against a live SUT — currently
requires an imperative driver: the `ImperativeLoadBody` seam
(`e2e-model/src/registry.rs`) exists precisely because a captured
choreography could not be expressed as an `ActorScenario`. The missing
expressiveness is bounded and known:

1. **Template-driven emission.** A replayed message is built with
   `MessageTemplate` + `EmitOpts` from exact captured bytes (frozen tier-3
   headers, optional `preserve_order`). The emission API exists on
   `Invite`/`ServerTxn`/`Dialog` builders, but no `GoalStep` can carry a
   template — goals are semantic (`Reinvite`, `Bye`), not byte-shaped.
2. **Scripted responses.** `Disposition` answers an initial INVITE by
   *policy* (ring, reject, reliable…). A replayed peer answers with the
   *captured* response — specific status, frozen headers, captured SDP — and
   answers in-dialog requests the same way. The reactive core's auto-answers
   would race the script.
3. **Observed-not-asserted finals.** When the capture's SUT locally
   originated a response (there is no far-end producer among the simulated
   peers), a different SUT may legitimately take another branch. The replay
   must *record* captured-vs-observed instead of asserting, key the peer's
   follow-up (ACK or not) on the OBSERVED response, and keep every peer
   RFC-compliant on the divergent branch.
4. **Truncated variants.** For a capture whose call was rejected in error, a
   fixed-behavior variant must be expressible as: replay up to the defect
   point, assert the fixed final there (exact status, status class, or
   "not an error class"), then STOP following the capture and complete the
   call cleanly (standard answer/ACK as needed, BYE, automatics, full
   post-call verification).
5. **Capture-scoped deviations and waivers.** CSeq patterns, delayed
   automatics (`sip-message/src/deviation.rs`) and structural audit waivers
   (`WaiverScope`) are already data-only, but only the functional lane's
   `Harness::waive` consumes scoped waivers; the load lane filters findings
   through a coarse `HashSet<String>` of rule names.
6. **Lane automatics.** On real transport, a peer that does not answer an
   INVITE's first provisional promptly draws Timer-A retransmissions from the
   SUT; emitting `100 Trying` on an inbound INVITE server transaction is
   correct UAS behavior (RFC 3261 §17.2.1) that a replayed flow elides. The
   reactor absorbs `100` today but never emits it.

Maintaining a second interpreter for these six needs duplicates the actor
engine's hardest-won properties (reactive datagram consumption, settle-gated
verdicts, obligation ledger) in a parallel code path — the exact
failure-cascade class ADR-0021 removed.

## Decision

The actor plan grammar gains replay verbs; the actor executor becomes the
ONE engine for scripted, declarative, and capture-derived scenarios on every
lane. Capture-derived tooling *lowers* to an `ActorCall`; the
`ImperativeLoadBody` seam is removed once the verbs land (no consumer
remains).

### 1. Template emission goals (`GoalStep` variants)

```rust
/// Originate the initial INVITE from a captured template.
InviteTemplate { callee: &'static str, plan: Option<InvitePlan>,
                 template: MessageTemplate, opts: EmitOpts },
/// Send an in-dialog (or early-dialog) request from a template; the method
/// is read from the template. `early: true` rides the still-pending
/// INVITE's early dialog (RFC 3311 §5.1).
RequestTemplate { template: MessageTemplate, opts: EmitOpts, early: bool },
/// Answer this actor's bound server transaction from a template; the
/// status is read from the template. The bound transaction is the one the
/// nearest preceding `ExpectRequest` consumed for this actor, or — absent
/// one — the parked initial-INVITE transaction. A status < 200 responds
/// WITHOUT consuming the binding (provisional-then-final on one
/// transaction); a status >= 200 consumes it. A 2xx to an INVITE forms the
/// dialog and defers the ACK receive exactly as a policy answer does.
/// `early` names the fork this response belongs to on a forked UAS: each
/// distinct early id allocates its own To-tag on the SAME server
/// transaction (RFC 3261 §12.1.2); the final's id names the winner and the
/// engine settles the losers (the existing forked-UAS surface).
RespondTemplate { template: MessageTemplate, opts: EmitOpts,
                  early: Option<EarlyId> },
/// Answer the bound server transaction by POLICY — the completion verb for
/// flows that stop following a capture (a truncated variant has no
/// captured template past the defect point). SDP answer from the actor's
/// `MediaState`; same dialog/ACK bookkeeping as a disposition answer.
Respond { status: u16 },
```

Existing semantic goals stay untouched; template goals are additive.
`EmitOpts::preserve_order` carries the verbatim-emission need (frozen header
order/casing/duplicates).

**Never templated (stack automatics):** `100 Trying`, ACK, CANCEL's 487,
and PRACK. PRACK's `RAck` binds the LIVE dialog's RSeq/CSeq (RFC 3262
§7.2) — a frozen `RAck` from a capture can never match the live exchange —
so the reactor's auto-PRACK is the only PRACK emitter and lowering never
produces a PRACK template.

### 2. `Disposition::Scripted` + park-or-react rule

A new disposition marks an endpoint whose responses are scripted:

```rust
/// Never auto-answers by policy. Inbound requests PARK on a per-actor
/// queue; reception goals consume them; anything the script never consumes
/// falls through to the reactive core — the divergent-branch rule: peers
/// stay RFC-compliant when the SUT relays traffic the capture never
/// modeled.
Scripted,
```

Park-or-react is keyed on **(method, initial-vs-in-dialog)** — an INVITE
with no To-tag is initial, with one a re-INVITE (the reactor's existing
distinction) — matched against the actor's *remaining* reception goals:

- A parked request some remaining `ExpectRequest` matches waits for the
  script.
- A request no remaining goal matches is auto-answered RFC-compliantly:
  initial or re-INVITE / UPDATE → `200` + answer SDP from `MediaState`
  (ringing per §5 automatics), other requests → `200`, CANCEL → `200` +
  `487` on the held transaction, ACK absorbed.
- **Requeue on advance:** every goal-cursor advance re-scans the parked
  queue and auto-reacts anything no remaining goal can consume — a parked
  request never starves behind a script that moved past it.
- **Automatic-consumed targets fail fast:** when an automatic consumes a
  parked transaction (CANCEL → 487), a later scripted step bound to it
  fails immediately with a bounded `StepError` naming the step — never by
  goal timeout.
- Requests the reactor (not the script) services on a `Scripted` actor are
  appended to the observed state's replay record (method, action taken) —
  divergence is never silent.

`Scripted` does NOT suppress the stack automatics: auto-PRACK of reliable
provisionals, ACK-to-2xx on originated INVITEs, and the non-2xx hop-ACK
stay reactor-owned.

`Scripted` is orthogonal to origination. The caller attribution used by
`into_result` (and the glare owner-dwell) keys on **which actor's first
goal originates the dialog** (`Invite`/`InviteTemplate`), not on
`Disposition::Caller`.

### 3. Reception goals: strict expect and recorded observation

Reception goals are **observations over the shared observed state** — the
reactor remains the sole datagram consumer (unchanged invariant); a
reception goal never pulls from the socket. To support them, `Observation`
gains facts the state does not record today: the status of EVERY response
on a leg (provisionals and 2xx finals, not only the failure path), body
presence per recorded message, and the early id a provisional belongs to.

```rust
/// Strict: the next response on this leg must match. `status < 200`
/// matches the next provisional of exactly that status (a final arriving
/// first fails fast); `status >= 200` matches the final. A different
/// status fails fast (`StepError::WrongStatus`), not by barrier timeout.
/// On a matched final the goal keys the same RFC-compliant follow-up as
/// `ObserveFinal`. The ACK to a delayed-offer re-INVITE 2xx carries an
/// engine-built minimal answer SDP (the leg's media port), overridable
/// with `ack_body`. `early` binds the expectation to a fork's early
/// dialog.
ExpectResponse { status: u16, body: BodyExpect,
                 early: Option<EarlyId>, ack_body: Option<Vec<u8>>,
                 matcher: Option<MessageTemplate> },
/// Strict: consume the next parked request of this kind into the actor's
/// bound server transaction (the `RespondTemplate`/`Respond` that follows
/// answers it). `Initial` names the dialog-creating INVITE.
ExpectRequest { kind: RequestKind, body: BodyExpect,
                matcher: Option<MessageTemplate> },
/// Observed, never asserted: wait for the next final on this leg, record
/// `(key, expected, observed)` into the observed state's replay record,
/// and key the RFC-compliant follow-up (ACK a 2xx to an INVITE, hop-ACK a
/// non-2xx INVITE final, nothing otherwise) on the OBSERVED status.
ObserveFinal { key: u32, expected: Option<u16> },
/// Strict with a class-shaped assertion — the truncated-variant anchor:
/// exact status, a status class (e.g. 2xx), or "any non-error final"
/// (< 400). Follow-up keyed on the observed final like `ObserveFinal`.
ExpectFinal { assert: FinalAssert },
```

with

```rust
pub enum BodyExpect { Any, Present, SdpPresent }
pub enum RequestKind { Initial, InDialog(InDialogMethod) }
```

**Detailed content verification.** A reception goal carrying a `matcher`
verifies the received message's HEADERS AND BODY through the existing
template-match surface (`match_inbound`/`expect_template`): frozen headers
compared placement-aware by name and value, reason phrase compared,
regenerated headers (Call-ID, Via/branch, tags, CSeq, Max-Forwards,
Content-Length, Contact host:port) excluded by construction, remote-target
comparison per its documented model. A mismatch fails fast with the match
surface's detailed finding. WHAT is compared is decided where the matcher
template is BUILT: lowering (or a hand author) includes only the headers
and body parts that must hold, so identity remaps, rewritten bodies
(e.g. session descriptions), and per-endpoint expected deltas are
template-construction decisions — the engine has no exception vocabulary
of its own. `matcher: None` keeps the status/kind + `BodyExpect` check
only. For a `RespondTemplate`-consumed request, the matcher runs at
consume time on the parked transaction's request; for responses, the
observed-state fact retains the typed message while a matcher-carrying
reception goal is pending on that leg.

Multipart part-level matchers beyond the template surface's body
comparison are follow-up reception-goal surface, deliberately not in this
slice.

**Incidental-failure suppression:** the reactor's shed heuristic (a non-2xx
final on the establishing INVITE with goals still pending → fail the actor
with `WrongStatus`) is suppressed while the actor's next pending goal is a
reception goal — that goal owns the final's verdict. Divergence is data,
not failure; truncated variants always have completion goals pending at
their anchor.

The replay record (`Vec<RecordedFinal { key, expected, observed }>` plus
the §2 serviced-stray entries) is queryable from `ObservedState` after the
run; the verdict is NOT gated on agreement. `into_result`/`Expect` are
unchanged: a replayed call declares `Expect::HappyBye` (or an existing
terminal) and the strict goals carry the per-step fidelity.

A goal's deadline is the standard bounded wait; a capture-declared tighter
bound lowers onto a per-goal deadline override.

### 4. Truncated variants are a lowering pattern, not an engine mode

A truncated plan is an ordinary `ActorCall`: scripted goals up to the defect
anchor, one `ExpectFinal { assert }` at it, then *standard* completion goals
(`Respond` policy answers where a peer must answer, `Bye` /
`ByeIfConfirmed`, automatics, settle). The engine adds only `FinalAssert`
and `Respond`; lowering tools MUST expand a single case-level truncation
declaration (defect anchor + `FinalAssert` + completion policy) into these
goals — hand-authoring the expansion is not the supported path. Post-call
verification (audit, settle, cleanup) applies in full.

### 5. Lane automatics as executor config

```rust
/// Per-plan (lane-chosen) stack automatics for scripted endpoints.
pub struct Automatics { pub answer_100_trying: bool }
```

carried on `CallPlan`. When set, an inbound INVITE parked on a `Scripted`
actor is answered `100 Trying` immediately (RFC 3261 §17.2.1) — required on
real transport (Timer-A suppression), and emitted IDENTICALLY on the fake
lane so lowered plans behave the same everywhere. `100` never consumes the
transaction (provisional-then-final on one server transaction is the
existing surface). The ACK-to-2xx automatic already lives in `ClientInvite`
(`Automatic::AckTo2xx`) and is unchanged; `DelayedAutomatic` remains its
deviation hook.

### 6. Deviations and waivers ride the plan

- `ActorSpec` gains `cseq: Option<CseqPattern>` and
  `delayed: Option<DelayedAutomatic>` (the carrier is single-slot). The
  CSeq pattern is attached at EVERY dialog-formation point of the actor,
  with one shared step counter — a scope-refresh dialog clone must not fork
  the counter (the documented clone hazard).
- `ActorCall` gains `waivers: Vec<WaiverScope>`. The load lane's
  `HashSet<String>` rule filter (`loadgen/src/case.rs`,
  `AgentBinder::rfc_findings`) is replaced by the structural
  `WaiverScope`/`apply_waivers` path — rule-only scopes reproduce today's
  behavior byte-for-byte. Three load-lane specifics: party attribution
  resolves mux sub-lane keys (`ip:port#name` → `name`), not the plain
  addr-to-first-bind map; a finding-preserving `apply_waivers` variant
  returns the surviving `RfcFinding`s (the load driver buckets on
  structured findings); unused-waiver enforcement aggregates per CAMPAIGN
  across sampled calls, never per call. Capture-derived waivers are lowered
  `.conditional()` (a divergent branch may legitimately never trip the
  waived rule), so the unused-waiver gate stays meaningful for hand-written
  scopes and silent for capture-scoped ones.

Captured inter-message delays lower onto the existing `Goal.delay` dwell;
whether a delay is compressed is a lowering-time, per-lane decision
(timer-linked delays are always honored). Cross-actor ordering lowers to
message-mediated arrival only — an ordering edge between actors that no
message mediates is rejected at lowering time, and a declared overlap
lowers to adjacent send goals with no intervening reception goal.

### 7. The comparable standard report

The cross-lane deliverable "the same plan produces the same report on every
lane" is defined over a *normalized projection* of `seq_report::SeqDoc`:

- lane ids and labels canonicalize to ROLE names (no addresses, no socket
  sub-lane keys — real lanes bind ephemeral ports);
- row labels keep method/status only; `detail` (wire text) and `conn`
  (socket ids) are stripped;
- retransmission rows collapse onto their first occurrence (real transport
  retransmits; the fake lane does not);
- only normalization-stable anomalies are kept;
- `at_ms` is replaced by causal sequence order (`seq` retained; wall and
  virtual timing excluded — raw timing stays in each lane's own
  un-normalized report).

Two lanes agree when their normalized `SeqDoc`s serialize identically. The
projector is ONE shared function, not per-lane-duplicated. The compared
call's wire record is captured unconditionally on every lane — load-lane
sampling does not apply to it.

## Consequences

- One engine: reactive datagram consumption, the obligation ledger, settle
  semantics, waiver attribution, and report projection apply uniformly to
  scripted, declarative, and capture-derived plans. Divergence handling is
  the reactor's normal operation, not a special path.
- `ImperativeLoadBody`, its factory, and the `LoadBody::Imperative` driver
  arm are deleted with the last consumer; `ShapeDescriptor` keeps one load
  surface (`load`).
- `GoalStep` grows eight variants; `Disposition` one; `CallPlan` one config
  struct; `ActorSpec` two fields; `ActorCall` one. All additive — existing
  scenarios compile unchanged.
- `Observation`/`ObservedState` grow response-status, body-presence and
  early-id facts plus the replay record — additive.
- The loadgen waiver path gains structural (party/position) scoping and the
  conditional/unused distinction.
- Multi-dialog actors (one endpoint owning several concurrent dialogs)
  remain out of scope; `DialogTable` keeps its deliberate minimalism (one
  pending INVITE + one confirmed dialog + the retained fork INVITE). A
  capture requiring more lowers to one actor per dialog.

## Rejected alternatives

- **Keep the imperative seam as a permanent escape hatch.** A second engine
  invites drift in exactly the properties the actor model exists to
  guarantee; every future replay feature would need implementing twice. The
  seam was transitional scaffolding and is absorbed.
- **A dedicated replay executor beside the actor engine.** Same objection,
  larger surface: it would re-implement reactive consumption and settle.
- **Reception goals that pull from the transaction directly.** Blocks the
  reactor for the pull's duration (the documented inline-pull hazard) or
  races it for the same datagram — the observation-over-shared-state form
  keeps the single-consumer invariant.
- **Expects as barriers only (no reception goals).** Barriers time out; they
  cannot fail fast with `WrongStatus` fidelity, cannot order park-consume
  against scripted responses, and push per-step semantics into predicates —
  strictly worse diagnostics for scripted flows.
- **Automatics hardwired per lane kind.** Identical cross-lane behavior is a
  correctness requirement for the normalized-report contract; a lane-kind
  switch would silently fork emissions.
- **Templated PRACK.** A frozen `RAck` cannot reference the live dialog's
  RSeq/CSeq; PRACK stays a stack automatic.
