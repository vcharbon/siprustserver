# 0016 — Callflow services as explicit per-call state machines

**Status:** accepted (2026-06-06)

## Context

Adding a new in-call service to the B2BUA is hard to do well and hard to
review. The rule engine (ADR-0010 X5) is sound — `RuleDefinition`s with a
declarative `Match`, layer-ranked first-match, pure actions through the
`ActionExecutor`, framework cleanup invariants — but "which rules are live right
now, and why" is *implicit*: it is scattered across slice-presence, ad-hoc phase
enums used as extra `Match` columns (`TransferPhase`), the `DeactivateRule`
action, and the `active_rules` list. A reviewer cannot read a service's control
flow from one place, and an author has no canonical shape to copy.

Nothing about this is a missing *capability* — a service can already stash a
`myState` field and branch on it. What is missing is a **rationalised, legible
way** to express it, plus generated documentation so each service's control flow
can be reviewed at a glance. This ADR formalises the source's *callflow service*
+ *phase machine* model (see `portsource/sipjsserver/CONTEXT.md`) into the Rust
port as explicit, doc-generated state machines, and proves it on two services —
one in-tree, one in a separate crate.

We adopt the source vocabulary unchanged (**callflow service**, **phase
machine**, **media leg**, **adopted/unadopted leg**, **Rule SDK**,
**integrator**); the new Rust-port terms are in `CONTEXT.md`.

## Decision X1 — State is a canonical rule selector, not a new dispatcher

A **machine** is a named, per-call state cursor. A rule declares, at wiring
time, the **machine** it belongs to, the **states** it is active in, and the
**transitions** it may cause. The engine is unchanged: it still layer-ranks and
first-matches, but a rule is a *candidate* only when its owner machine's current
state is in its `active_states`. Core rules carry no machine and are always
candidates. We did **not** replace the rule engine with an `(event × state)`
transition table — that would discard layer-ranked composition with core
defaults, which every service depends on. The machine is a guard/selector
*over* the existing engine, nothing more.

## Decision X2 — Machines are per-call; leg state stays data

Two machine tiers exist, both single-cursor **per call**:

- the always-on **global call machine** (= the existing `CallModelState`,
  enriched if a service needs finer call-lifecycle states), and
- one **service machine** per active callflow service.

Per-leg SIP lifecycle (`LegState`) and `active_peer` remain **data the rules
peek at in guards** — *not* a wired machine tier. Legs are a dynamic collection;
a per-leg machine forces "which leg instance?" addressing onto every rule and
transition for no legibility gain (the per-leg lifecycle is RFC-standard and
identical for every leg). A service that genuinely needs per-leg sub-state
carries it as service-owned `legExt` data; a wired per-leg machine tier is
deferred until a service runs the *same* sub-protocol on *several* legs
concurrently (conferencing / parallel forks). No current or near-term service
does.

## Decision X3 — Inter-machine coupling reuses the action union; one hop

Machines influence one another, but **not** through a new message bus. The
global call machine's published inbound command interface **is the
call-lifecycle subset of the existing `RuleAction` union** (`BeginTermination`,
`TerminateCall`, `Merge`, `Split`, `CreateLeg`, `CancelLeg`). A service machine
that decides "terminal failure" emits `BeginTermination` — a command the global
machine interprets through its own transitions; it never reaches in and mutates
global state directly. Coupling is kept to **one synchronous hop, service →
global**: the global machine does not synchronously call back into services;
service-slice cleanup rides the existing `→ Terminated` invariant
(cancel-all-timers / write-cdr / remove-call). A second, parallel message
vocabulary beside the action union was rejected as duplication (two ways to say
"terminate") and as reintroducing cross-machine cascade/ordering complexity that
the per-machine diagrams would not show.

## Decision X4 — `sm_cursors` is the single home for state; data backing is pluggable

Every machine's current state lives in **one** uniform field,
`Call.sm_cursors: BTreeMap<MachineId, StateLabel>` — the only home for the state
*label*. The `SetState` action is its sole writer. Associated **data** lives
separately and is **not** duplicated into the slice:

- **in-tree** services keep a typed slice on `Call` (the `relay_first_18x` /
  `transfer` pattern) — data only, no `state` field;
- **out-of-crate integrator** services keep opaque `ext[serviceId]` data
  (ADR-0002), because they physically cannot add a typed field to the `call`
  crate they do not fork.

This is forced by the HA constraint "**the B2BUA owns and replicates all
per-call state**": the core must serialise an integrator's state without knowing
its type. The uniform cursor map gives the engine (selection), the doc
generator, observability/CDR, and HA reconciliation a single typed view of
"where every machine is" for *all* services regardless of crate; the rich,
typed data stays where it can be typed. Keeping the label only in `sm_cursors`
(never also in the slice) removes the mirror/slice sync hazard.

## Decision X5 — Authoring via `macro_rules!`; docs via `xtask`; no proc-macro

Machines/states/transitions are declared with a **declarative** `define_service!`
/ `sm_rule!` macro (`macro_rules!`, not a proc-macro — no new crate, no
`syn`/`quote`, fully `cargo expand`-able). It expands to the per-service state
enum and to `RuleDefinition` values carrying the new `machine` / `active_states`
/ `transitions` **data fields**. The macro only generates the data the next step
reads.

`cargo run -p xtask -- state-machine-docs` walks the **composed** runtime rule
registry from `b2bua-runner` (so it sees core + in-tree + separate-crate
services) and emits one **Mermaid** diagram per machine under `docs/sm/`. A CI
test asserts (a) committed diagrams are fresh and (b) every transition a handle
can emit is in the declared graph.

A proc-macro was rejected: file IO inside a proc-macro is an anti-pattern
(breaks incremental/sandboxed builds), so doc emission would live in `xtask`
*anyway*; true compile-time transition-safety would need handles constrained
into a DSL (too much); and macro-expanded code is invisible in review — directly
against the legibility goal. The compiler already rejects references to
non-existent state variants; the CI test covers the rest. A proc-macro is
reserved for if `macro_rules!` ergonomics prove insufficient.

## Decision X6 — Minimal, dogfood-driven public Rule SDK

The separate announcement crate forces a public boundary the port has lacked.
We carve the **smallest** surface that crate actually needs and keep everything
else `pub(crate)` ("easier to open than to close"): the `define_service!` macro,
a narrowed `RuleContext`, and a public **action subset** —
`CreateLeg{kind:media}` / `DestroyLeg` / `CancelLeg`, `send-request-to-leg`
(INFO, opaque body), `send-provisional-to-leg` (early-media SDP broker — newly
**widened into** the public set, demanded by this dogfood), `Respond`, timer
schedule/cancel, `SetState`, `BeginTermination`. Internal plumbing
(`SetTransfer`, PEM/PRACK actions, raw send, `Merge`/`Split`) stays private.

## Decision X7 — Two worked examples: `transfer` (in-tree) + `announcement` (separate crate)

- **`transfer`** is retrofitted onto the framework in-tree: its existing
  `TransferPhase` becomes a declared service machine, its rules become
  `sm_rule!`s, and it gains a generated diagram. Proves the framework subsumes a
  complex existing service via the typed-slice path.
- **`announcement`** (MRF pre-call announcement via MSCML) is built **in its own
  crate** against the public Rule SDK only, with `ext`-backed data. Proves the
  out-of-crate **integrator** path end-to-end.

The announcement machine (early-media-then-dial) is four states:
`OfferingMrf → Announcing → Bridging → (clear)`, with `→ BeginTermination` on any
media-leg failure. Flow: on the routing decision, create a `media` (unadopted)
leg to the MRF; on its `200 OK`, broker the MRF SDP to A as a `183` (early media)
and send MSCML `<play>` over INFO; on the MSCML completion INFO, BYE the media
leg and dial the real destination, bridging normally. The B2BUA is **not** a
media relay — the MRF's SDP is brokered straight to A, so the `media` crate
(RTP/G.711) is uninvolved. The media leg is unadopted (generic relay/keepalive/
failover never touch it) but is reaped by ordinary `→ Terminated` cleanup like
any leg, so an A-side hangup mid-announcement needs no special rule.

This also ports the primitives the source has but Rust lacks:
`send-request-to-leg` (INFO), `send-provisional-to-leg`, a minimal MSCML
build/parse, and the unadopted-leg relay gate on `Leg.adopted`.

## Decision X8 — Services seed their own machine via an `init` hook

A service is given broad latitude to **create and initialise its own initial
state and data**. Each service declares
`init(descriptor, &Call) -> Option<ServiceSeed>`, run once at call setup (the
source's `call-routed` re-entry point, after routing has built the `Call`).
Applicability is decided here — returning `None` keeps the service dormant and
costs a vanilla call nothing. A returning service seeds, in one atomic batch
folded through the normal executor/effects pipeline: its **initial cursor**
(`sm_cursors[id] = S0`), its **data backing** (typed slice in-tree, or
`ext[id]`), and an **initial action set** (e.g. the announcement service's
`CreateLeg{kind:media}` toward the MRF — launched at setup, in parallel with the
destination B-leg, exactly as the source's PRBT does). `define_service!` carries
the `init` clause. Seeding rides the same audited `RuleAction`/effects path as
every later transition — there is no privileged back-door that writes call state
outside the executor.

## Decision X9 — A rule declares its tracked side effects; the type system forces it

A machine-bound rule **declares the side effects its handler may emit**, the same
declare-then-verify contract X1 already uses for `transitions`. The declaration is
**type-forced**: `sm_rule!` requires the `effects` field (a service rule that omits
it does not compile), and a *total* `RuleAction::effect_kind` maps every action to
a category (the compiler's exhaustiveness check forces each present and future
action into one). The executor verifies the actually-emitted actions are a subset
of the declared effects **by category** (`emitted ⊆ declared`); payloads are
diagram labels only, so a handler targeting a dynamically-named leg still
satisfies its declaration. Drift is a debug-build panic (a test failure), never a
live-worker panic — identical to `check_declared_transition`.

The taxonomy is cut by the **attribution principle** — *who authors the message*,
not which bytes hit the wire — yielding exactly three **tracked side effects**:

- **leg message** — a SIP message the service rule authors toward a leg
  (originate / relay / respond / provisional); carries the canonical `Method` +
  a free label. The rule owns the semantic payload; the core engine fills the
  mechanical SIP layer (Contact / From-tag / Via / CSeq).
- **call-lifecycle command** — the one service → global hop of X3
  (`BeginTermination` / `TerminateCall` / `Merge` / `Split`); the global call
  machine owns any wire messages it then generates (they belong on *its* diagram).
- **guard timer** — a service safety/watchdog timer the rule arms or cancels.

Two `RuleAction`s are deliberately **not** side effects: `SetState` *is* the
transition (drawn as the edge), and `ClearState` is **machine deactivation**
(drawn as the transition to the terminal `[*]` — see X4). Pure data bookkeeping
(CDR events, tag-map / typed-slice writes, async-HTTP kicks) is **auto-allowed**:
categorised for exhaustiveness but neither declared nor rendered.

A coarser single "call-lifecycle command swallows leg creation/teardown" cut (per
the literal X3 subset, which lists `CreateLeg`/`CancelLeg`) was rejected: it would
hide the precise outgoing messages a rule author needs to see (the MRF INVITE, the
re-INVITEs to A and C). `CreateLeg` is attributed to the service as a *leg message*
(method `INVITE`) because the rule initiates the leg and authors its INVITE
payload; the global machine owns only leg *registration* and the mechanical layer.

This assumes a **single canonical `sip-message::Method`** (in- and out-of-dialog,
shared by every crate) for the leg-message method. Today methods are fragmented
(`InDialogMethod` + `OutOfDialogMethod` + bare `String`); unifying them is a
**prerequisite cleanup tracked independently** (see the consequences) — this ADR
assumes it has landed.

## Consequences

- `RuleDefinition` gains optional `machine` / `active_states` / `transitions`
  fields; existing core rules leave them unset and are unaffected (always
  candidates).
- `RuleDefinition` gains a required-for-service-rules `effects` field
  (`RuleDefinition::core` defaults it empty); `RuleAction` gains `ClearState`
  (machine deactivation) and a total `effect_kind`.
- **Prerequisite cleanup (independent):** consolidate `InDialogMethod` +
  `OutOfDialogMethod` + bare-`String` methods into a single canonical
  `sip-message::Method` shared by all crates. The leg-message effect assumes it;
  it is *not* part of this work and lands before it.
- `Call` gains `sm_cursors`; the global call machine's cursor projects from
  `CallModelState`, so the termination invariants are untouched.
- `xtask` depends on `b2bua-runner` for doc-gen (it must see the composed
  registry, not just `b2bua::default_rules()`).
- The public SDK surface is now a stability contract; widening it is cheap,
  retracting it is not — keep additions dogfood-driven.

## References

- [ADR-0010](./0010-b2bua-dispatch-rules-rust-shape.md) (rule engine shape this
  extends), [ADR-0008](./0008-call-context-data-model.md) (Call/Leg/Dialog
  model), ADR-0002 (no premature shared types crate → opaque `ext`).
- Source model: `portsource/sipjsserver/CONTEXT.md` (*callflow service*, *phase
  machine*, *media leg*, *adopted/unadopted leg*, *Rule SDK*, *integrator*);
  source ADRs `0014-leg-kind-and-singleton-active-peer`,
  `0016-callflow-services-typed-ext`.
- `crates/b2bua/src/rules/` (engine, actions, invariants),
  `crates/call/src/model.rs` (`LegKind`, `Leg.adopted`, `CallModelState`).
