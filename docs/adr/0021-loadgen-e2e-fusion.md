# Loadgen × e2e fusion: one axis data model, two run surfaces

## Status

accepted

The long form of each decision and of each rejected option is in the
[appendix](appendix/0021-loadgen-e2e-fusion.md).

## Context

The **e2e framework** (ADR-0018/0019) authors JSON Test cases with checks,
binds Endpoint configs and runs one strict cell at a time. The **load
generator** (`crates/loadgen`, the SIPp substitute) runs thousands of calls at
a governed rate. Both need the same concepts: shapes, identities, checks and a
run spec. Two vocabularies for them drift silently on what a shape is, keep a
load run from reusing authored checks, and break correlation against a
third-party SUT that strips unknown headers.

## Decision

Fuse the two surfaces on **one axis data model with two run surfaces**: the
*data* is unified, the *engines* deliberately are not. Seven decisions, one per
fused concern.
Detail: [appendix](appendix/0021-loadgen-e2e-fusion.md#decision-long-form).

**(a) One axis data model; two run surfaces kept distinct.** The authored axes
(Test case, Check sets and the check evaluator, Campaign, Endpoint config,
the Message-anchor vocabulary, shape descriptors, Load profile) live in the
dependency-light `crates/e2e-model`, which depends on no SUT crate. The
functional executor (`e2e-core`) runs `!Send` bodies, paused-clock capable,
where a deviation panics the cell. The load fleet (`loadgen`) runs `Send`
bodies on the wall clock, where an expected failure is a counted `StepError`.

**(b) Unified open shape registry.** One `ShapeDescriptor` per Callflow shape
in `e2e_model::registry::ShapeRegistry`, in one id space; a duplicate id
panics at registration. The descriptor holds anchors, inputs, load attributes
and an optional load body factory; `e2e-core` attaches functional bodies by
id. A shape may carry either body or both. A third-party crate registers
shapes without touching a workspace table.

**(c) Pluggable correlation strategies.** The per-call token's path through
the SUT is a per-run `loadgen::Correlation`: `header` (`X-Loadgen-Id`),
`header_templated` (a structured header the SUT already relays) or `to_user`
(the To user-part), which needs no SUT configuration.

**(d) Binding pools.** A Test case may carry `bindings: { mode, entries }`, a
pool of `Input` overlays merged per call, walked `seq` or `random`, wrapping
past its end. Fields expand `${seq}`, `${seq:N}` and `${rand:N}`; extras
become per-call dwell overrides. A malformed token or an empty pool fails at
startup in `validate_case`.

**(e) Checks + `allowViolations` on sampled calls.** One shared engine
(`e2e_model::checks`) evaluates a case's checks over a sampled call's trace.
A failed check reclassifies an OK call as `check_fail`. `allowViolations`
exempts named RFC audit rules per call. Checks are a per-sample oracle, not a
per-call gate.

**(f) Load profile + runtime rate control.** One authored `LoadProfile` JSON
document is the whole run spec; an explicit CLI flag overrides it. `POST
/rate?cps=` re-targets the rate live through a re-anchoring fixed-grid
governor: no catch-up burst on a cut, a raise within one slot, `cps=0` pauses
admission.

**(g) Auth / TLS / REGISTER deferred behind named seams.** Each is isolated to
one adapter: auth behind `ChallengeResponder` with one retry point, TCP/TLS in
the mux, REGISTER as a future shape descriptor. The READMEs name each seam.

## Considered options

Long form: [appendix](appendix/0021-loadgen-e2e-fusion.md#considered-options-long-form).

- One engine for both surfaces: rejected, thread model, clock and failure model differ on purpose.
- Two closed registries synced by convention: rejected, silent drift and no third-party shapes.
- Checks on every call: rejected, recording every call is unbounded memory at load.
- A loadgen-private case and profile dialect: rejected, it reopens the drift.

## Consequences

- **Checks are sampling-scoped.** A check can never serve as a per-call SLA
  gate on the fleet; population-level gating stays with result classes and
  metrics. This is the trade for flat memory.
- **Mixed shape-id casing in the one id space.** Load-born ids are snake_case
  (`basic_call`, `options_hold`), e2e-born ids kebab-case (`basic-call`,
  `transfer-refer-media`); the dual-body `rerouting_prack` is snake. Both are
  load-bearing surfaces (report directories, metrics labels, committed cases,
  dashboards), so the ids stay as they are.
- **`e2e-model` is not a pure-serde leaf.** Carrying the check evaluator pulls
  in sip-message/sip-net/scenario-harness (the message + recording surface).
  Accepted to keep ONE check engine next to the model it interprets; the crate
  still depends on no SUT crate, which is the boundary that matters.
- **Schema discipline.** Every schemars-derived model change requires
  `cargo run -p xtask -- e2e-schema` and committing the regenerated
  `e2e/schemas` files (a drift test enforces it).
- The shape registry is the open `ShapeRegistry` in `e2e-model`, not an
  in-process registry in `e2e-core`; `e2e-core` attaches functional bodies to
  it (ADR-0018 amended).
- Glossary in `CONTEXT.md` → "Loadgen fusion vocabulary".
