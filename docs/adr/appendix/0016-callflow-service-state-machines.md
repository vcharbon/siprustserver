# Appendix: Callflow services as explicit per-call state machines

The decision part: [ADR-0016](../0016-callflow-service-state-machines.md).

## Context

The rule engine's sound parts are `RuleDefinition`s with a declarative `Match`, layer-ranked
first-match, pure actions through the `ActionExecutor`, and framework cleanup invariants. The
phase enum used as an extra `Match` column is `TransferPhase`. With "which rules are live" implicit, a
reviewer cannot read a service's control flow from one place, and an author has no canonical
shape to copy.

Nothing about this is a missing *capability*: a service can already stash a `myState` field
and branch on it. What is missing is a **rationalised, legible way** to express it, plus
generated documentation so each service's control flow can be reviewed at a glance. The ADR
proves the model on two services, one in-tree and one in a separate crate.

## Decision X1: State is a canonical rule selector, not a new dispatcher

The engine still layer-ranks and first-matches. It stays: an `(event × state)` transition table would discard layer-ranked
composition with core defaults, which every service depends on. The machine is a
guard/selector *over* the existing engine, nothing more.

## Decision X2: Machines are per-call; leg state stays data

The global call machine is enriched if a service needs finer call-lifecycle states. Legs are a
dynamic collection; a per-leg machine forces "which leg instance?" addressing onto every rule
and transition for no legibility gain (the per-leg lifecycle is RFC-standard and identical for
every leg). A wired per-leg machine tier is deferred until a service runs the *same*
sub-protocol on *several* legs concurrently (conferencing / parallel forks). No current or
near-term service does.

## Decision X3: Inter-machine coupling reuses the action union; one hop

Machines influence one another, but not through a new message bus. A service machine that
decides "terminal failure" emits `BeginTermination`, a command the global machine interprets
through its own transitions. The `→ Terminated` invariant is cancel-all-timers, write-cdr,
remove-call. A second, parallel message vocabulary beside the action union was rejected as
duplication (two ways to say "terminate") and as reintroducing cross-machine cascade/ordering
complexity that the per-machine diagrams would not show.

## Decision X4: `sm_cursors` is the single home for state; data backing is pluggable

The typed-slice pattern is that of `relay_first_18x` and `transfer`. Integrator services use
`ext` because they physically cannot add a typed field to the `call` crate they do not fork.
A decision backend never sees a core-reserved key: the engine itself replicates those slots.

This is forced by the HA constraint "**the B2BUA owns and replicates all
per-call state**": the core must serialise an integrator's state without knowing
its type. The uniform cursor map gives the engine (selection), the doc
generator, observability/CDR, and HA reconciliation a single typed view of
"where every machine is" for *all* services regardless of crate; the rich,
typed data stays where it can be typed. Keeping the label only in `sm_cursors`
(never also in the slice) removes the mirror/slice sync hazard.

## Decision X5: Authoring via `macro_rules!`; docs via `xtask`; no proc-macro

`macro_rules!` needs no new crate and no `syn`/`quote`, and is fully `cargo expand`-able. The
macro only generates the data the doc generator reads. Walking the composed registry from
`b2bua-runner` lets it see core, in-tree and separate-crate services.

A proc-macro was rejected: file IO inside a proc-macro is an anti-pattern
(breaks incremental/sandboxed builds), so doc emission would live in `xtask`
*anyway*; true compile-time transition-safety would need handles constrained
into a DSL (too much); and macro-expanded code is invisible in review — directly
against the legibility goal. The compiler already rejects references to
non-existent state variants; the CI test covers the rest. A proc-macro is
reserved for if `macro_rules!` ergonomics prove insufficient.

## Decision X6: Minimal, dogfood-driven public Rule SDK

The separate announcement crate forces a public boundary. `send-provisional-to-leg` is in the
public set because this dogfood demands it.

## Decision X7: Two worked examples: `transfer` (in-tree) + `announcement` (separate crate)

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

The example brings the primitives the source has into the port: `send-request-to-leg`
(INFO), `send-provisional-to-leg`, a minimal MSCML build/parse, and the unadopted-leg relay
gate on `Leg.adopted`.

## Decision X8: Services seed their own machine via an `init` hook

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

## Decision X9: A rule declares its tracked side effects; the type system forces it

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
