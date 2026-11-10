# Capture-replay verbs in the actor plan: one executor for every lane

Status: proposed (2026-07-19)

The verb signatures, the matcher model, the scripted CANCEL reception, the
accepted-delta hook and the long form of each rejected alternative are in the
[appendix](appendix/0024-capture-replay-actor-plan-verbs.md), under the same
section numbers.

## Context

The actor harness (ADR-0018/0021, `scenario-harness/src/actor/`) runs
declarative multi-party call plans on the functional lane and the load lane.
Replaying a captured call against a live SUT needs six things the semantic
grammar lacks: template-driven emission, scripted responses, finals observed
rather than asserted, truncated variants, capture-scoped deviations and
waivers, and a lane automatic `100 Trying`. A second interpreter for them
would duplicate the engine's reactive consumption, settle-gated verdicts and
obligation ledger.
Detail: [appendix, context](appendix/0024-capture-replay-actor-plan-verbs.md#context-long-form).

## Decision

The actor plan grammar has replay verbs, and the actor executor is the one
engine for scripted, declarative and capture-derived scenarios on every lane.
Capture-derived tooling lowers to an `ActorCall`. There is no imperative load
body.

### 1. Template emission goals (`GoalStep` variants)

`InviteTemplate`, `RequestTemplate` and `RespondTemplate` emit from a captured
`MessageTemplate` with `EmitOpts`; `Respond { status }` answers by policy
where a flow stops following the capture. Semantic goals stay. `100 Trying`,
ACK, CANCEL's 487 and PRACK are never templated: a frozen `RAck` cannot match
the live dialog (RFC 3262 §7.2), so the reactor's auto-PRACK is the only
PRACK emitter.
Detail: [appendix §1](appendix/0024-capture-replay-actor-plan-verbs.md#1-template-emission-goals-goalstep-variants).

### 2. `Disposition::Scripted` + park-or-react rule

A `Scripted` actor never answers by policy. An inbound request parks when a
remaining reception goal matches it by (method, initial-vs-in-dialog);
otherwise the reactive core answers it RFC-compliantly. Every cursor advance
re-scans the parked queue. A scripted step bound to a transaction an
automatic consumed fails fast. Requests the reactor serviced are recorded, so
divergence is never silent. Stack automatics stay reactor-owned. The caller is
the actor whose first goal originates the dialog.
Detail: [appendix §2](appendix/0024-capture-replay-actor-plan-verbs.md#2-dispositionscripted--park-or-react-rule).

### 3. Reception goals: strict expect and recorded observation

Reception goals observe the shared observed state; the reactor stays the sole
datagram consumer. `ExpectResponse` and `ExpectRequest` are strict and fail
fast on a wrong status or kind. `ObserveFinal` records captured against
observed and keys the follow-up on the observed final. `ExpectFinal` asserts
an exact status, a class, or a non-error final. A `matcher` template checks
headers and body; what it compares is decided where it is built. The verdict
is not gated on the replay record. `ExpectRequest { kind: Cancel }` claims the
CANCEL of the pending initial INVITE.
Detail: [appendix §3](appendix/0024-capture-replay-actor-plan-verbs.md#3-reception-goals-strict-expect-and-recorded-observation).

### 4. Truncated variants are a lowering pattern, not an engine mode

A truncated plan is an ordinary `ActorCall`: scripted goals up to the defect
anchor, one `ExpectFinal { assert }` there, then standard completion goals.
Lowering tools expand one case-level truncation declaration into those goals.
Post-call verification applies in full.
Detail: [appendix §4](appendix/0024-capture-replay-actor-plan-verbs.md#4-truncated-variants-are-a-lowering-pattern-not-an-engine-mode).

### 5. Lane automatics as executor config

`CallPlan` carries `Automatics { answer_100_trying }`. When set, an INVITE
parked on a `Scripted` actor is answered `100 Trying` at once (RFC 3261
§17.2.1), identically on the fake and real lanes.
Detail: [appendix §5](appendix/0024-capture-replay-actor-plan-verbs.md#5-lane-automatics-as-executor-config).

### 6. Deviations and waivers ride the plan

`ActorSpec` carries a CSeq pattern and a delayed automatic, with one CSeq
counter across every dialog the actor forms. `ActorCall` carries
`waivers: Vec<WaiverScope>`, and the load lane applies them structurally.
Unused waivers aggregate per campaign; capture-derived waivers are
conditional. Captured delays lower onto `Goal.delay`. A cross-actor ordering
edge no message mediates is rejected at lowering. An accepted-delta policy
(`with_delta_policy`) may accept a legitimate substitution at a request or
response mismatch, and every acceptance is recorded.
Detail: [appendix §6](appendix/0024-capture-replay-actor-plan-verbs.md#6-deviations-and-waivers-ride-the-plan).

### 7. The comparable standard report

Two lanes agree when their normalized `SeqDoc`s serialize identically. The
one shared projector maps lanes to role names, strips wire text and socket
ids, collapses retransmissions and orders by causal sequence. The compared
call's wire record is captured on every lane.
Detail: [appendix §7](appendix/0024-capture-replay-actor-plan-verbs.md#7-the-comparable-standard-report).

## Consequences

- Reactive consumption, the obligation ledger, settle, waiver attribution and
  report projection apply alike to every plan kind. Divergence handling is
  the reactor's normal operation.
- `ShapeDescriptor` has one load surface (`load`).
- The eight `GoalStep` variants and the `Disposition`, `CallPlan`, `ActorSpec`, `ActorCall`
  and `ObservedState` surfaces are additive; existing scenarios compile
  unchanged.
- The loadgen waiver path has structural (party, position) scoping and the
  conditional and unused distinction.
- Multi-dialog actors are out of scope; `DialogTable` holds one pending
  INVITE, one confirmed dialog and the retained fork INVITE. A capture that
  needs more lowers to one actor per dialog.

## Rejected alternatives

Long form: [appendix](appendix/0024-capture-replay-actor-plan-verbs.md#rejected-alternatives-long-form).

- A permanent imperative escape hatch: a second engine that drifts.
- A dedicated replay executor: re-implements reactive consumption and settle.
- Reception goals that pull from the transaction: block or race the reactor.
- Expects as barriers only: no fail-fast, no park-consume ordering.
- Automatics hardwired per lane kind: forks emissions across lanes.
- Templated PRACK: a frozen `RAck` never matches the live dialog.
