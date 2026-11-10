# 0016. Callflow services as explicit per-call state machines

**Status:** accepted (2026-06-06). Amended with ADR-0055 §10: the SDK carries no HA event and
no HA configuration.

The rationale of each section, the announcement worked example and the consequences in
detail: [appendix](appendix/0016-callflow-service-state-machines.md).

## Context

The rule engine (ADR-0010 X5) is sound, but "which rules are live right now, and why" is
implicit: scattered across slice presence, ad-hoc phase enums used as `Match` columns, the
`DeactivateRule` action and the `active_rules` list. This ADR brings the source's
*callflow service* + *phase machine* model (`portsource/sipjsserver/CONTEXT.md`) into the Rust
port as explicit, doc-generated state machines. It adopts the source vocabulary unchanged
(callflow service, phase machine, media leg, adopted/unadopted leg, Rule SDK, integrator). The
Rust-port terms are in `CONTEXT.md`. Detail: [appendix, context](appendix/0016-callflow-service-state-machines.md#context).

## Decision X1: State is a canonical rule selector, not a new dispatcher

A machine is a named, per-call state cursor. A rule declares at wiring time its machine, the
states it is active in, and the transitions it may cause. A rule is a candidate only when its machine's current state is in its
`active_states`. Core rules carry no machine and are always candidates. An `(event × state)`
transition table was rejected: it would discard layer-ranked composition with core defaults.

## Decision X2: Machines are per-call; leg state stays data

Two machine tiers exist, both single-cursor per call: the always-on global call machine (the
`CallModelState`) and one service machine per active callflow service. Per-leg SIP lifecycle
(`LegState`) and `active_peer` stay data the rules peek at in guards. A service that needs
per-leg sub-state carries it as service-owned `legExt` data. Detail:
[appendix X2](appendix/0016-callflow-service-state-machines.md#decision-x2-machines-are-per-call-leg-state-stays-data).

## Decision X3: Inter-machine coupling reuses the action union; one hop

The global call machine's inbound command interface is the call-lifecycle subset of the
`RuleAction` union (`BeginTermination`, `TerminateCall`, `Merge`, `Split`, `CreateLeg`,
`CancelLeg`). A service machine never mutates global state directly. Coupling is one
synchronous hop, service → global; the global machine never calls back into services.
Service-slice cleanup rides the `→ Terminated` invariant, made exactly-once by
[ADR-0020](./0020-call-reaper-obligations-rule-view.md). A parallel message vocabulary was
rejected. Detail: [appendix X3](appendix/0016-callflow-service-state-machines.md#decision-x3-inter-machine-coupling-reuses-the-action-union-one-hop).

## Decision X4: `sm_cursors` is the single home for state; data backing is pluggable

`Call.sm_cursors: BTreeMap<MachineId, StateLabel>` is the only home for every machine's state
label, and `SetState` is its sole writer. Data lives apart and never duplicates the label:
in-tree services keep a typed slice on `Call` with no `state` field, and out-of-crate
integrator services keep opaque `ext[serviceId]` data (ADR-0002). `Call.ext` also holds
core-reserved keys, named by `rules::relay::is_core_reserved_ext`: `CallSnapshot::of` filters
them out, and no service id may collide with them. Detail:
[appendix X4](appendix/0016-callflow-service-state-machines.md#decision-x4-sm_cursors-is-the-single-home-for-state-data-backing-is-pluggable).

## Decision X5: Authoring via `macro_rules!`; docs via `xtask`; no proc-macro

`define_service!` and `sm_rule!` are declarative `macro_rules!` macros. They expand to the
per-service state enum and to `RuleDefinition` values carrying the `machine`, `active_states`
and `transitions` data fields. `cargo run -p xtask -- state-machine-docs` walks the composed
rule registry from `b2bua-runner` and emits one Mermaid diagram per machine under `docs/sm/`.
A CI test asserts the committed diagrams are fresh and every transition a handle can emit is
in the declared graph. A proc-macro was rejected. Detail:
[appendix X5](appendix/0016-callflow-service-state-machines.md#decision-x5-authoring-via-macro_rules-docs-via-xtask-no-proc-macro).

## Decision X6: Minimal, dogfood-driven public Rule SDK

The public SDK is the smallest surface the separate announcement crate needs. Everything else
stays `pub(crate)` ("easier to open than to close"). Public: the `define_service!` macro, a
narrowed `RuleContext`, and the action subset `CreateLeg{kind:media}`, `DestroyLeg`,
`CancelLeg`, `send-request-to-leg` (INFO, opaque body), `send-provisional-to-leg` (early-media
SDP broker), `Respond`, timer schedule/cancel, `SetState`, `BeginTermination`. Private:
`SetTransfer`, PEM actions, raw send, `Merge`/`Split`. The PRACK this stack owes is the
engine's (ADR-0032 X6).

## Decision X7: Two worked examples: `transfer` (in-tree) + `announcement` (separate crate)

`transfer` is a declared service machine in-tree on the typed-slice path. `announcement` (MRF
pre-call announcement via MSCML) lives in its own crate against the public Rule SDK only, with
`ext`-backed data. Detail:
[appendix X7](appendix/0016-callflow-service-state-machines.md#decision-x7-two-worked-examples-transfer-in-tree--announcement-separate-crate).

## Decision X8: Services seed their own machine via an `init` hook

Each service declares `init(descriptor, &Call) -> Option<ServiceSeed>`, run once at call setup,
after routing has built the `Call`. `None` keeps the service dormant at no cost. A seed sets,
in one atomic batch through the executor and effects pipeline, the initial cursor, the data
backing and an initial action set. `define_service!` carries the `init` clause. No back-door
writes call state outside the executor. Detail:
[appendix X8](appendix/0016-callflow-service-state-machines.md#decision-x8-services-seed-their-own-machine-via-an-init-hook).

## Decision X9: A rule declares its tracked side effects; the type system forces it

`sm_rule!` requires an `effects` field, and a total `RuleAction::effect_kind` maps every action
to a category. The executor checks `emitted ⊆ declared` by category. Drift is a debug-build
panic, never a live-worker panic, like `check_declared_transition`. By the attribution
principle (who authors the message), the three tracked side effects are:

- **leg message**: a SIP message the service rule authors toward a leg, with its `Method` and a
  free label. `CreateLeg` is a leg message (method `INVITE`).
- **call-lifecycle command**: the X3 hop (`BeginTermination`, `TerminateCall`, `Merge`, `Split`).
- **guard timer**: a service safety or watchdog timer the rule arms or cancels.

`SetState` is the transition and `ClearState` is machine deactivation; neither is a side
effect. Pure data bookkeeping is auto-allowed. The leg-message method is a single canonical
`sip-message::Method`. Detail: [appendix X9](appendix/0016-callflow-service-state-machines.md#decision-x9-a-rule-declares-its-tracked-side-effects-the-type-system-forces-it).

## Consequences

- `RuleDefinition` carries optional `machine`, `active_states`, `transitions`, and an `effects`
  field required for service rules. `Call` carries `sm_cursors`, and the global cursor
  projects from `CallModelState`.
- The public SDK surface is a stability contract. Keep additions dogfood-driven.

The rest: [appendix, consequences](appendix/0016-callflow-service-state-machines.md#consequences).

## References

- [ADR-0010](./0010-b2bua-dispatch-rules-rust-shape.md) (the rule engine this extends),
  [ADR-0008](./0008-call-context-data-model.md) (Call/Leg/Dialog model), ADR-0002 (opaque
  `ext`).
- Source model: `portsource/sipjsserver/CONTEXT.md`; source ADRs
  `0014-leg-kind-and-singleton-active-peer`, `0016-callflow-services-typed-ext`.
- `crates/b2bua/src/rules/` (engine, actions, invariants), `crates/call/src/model.rs`
  (`LegKind`, `Leg.adopted`, `CallModelState`).
