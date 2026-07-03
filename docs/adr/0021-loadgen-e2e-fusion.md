# Loadgen × e2e fusion — one axis data model, two run surfaces

## Status

accepted

## Context

Two test surfaces grew the same concepts twice. The **e2e framework**
(ADR-0018/0019) authors JSON Test cases with checks, binds Endpoint configs,
declares Callflow shapes in a closed in-process registry inside `e2e-core`, and
executes one strict cell at a time (a `!Send` body over an `InfraRuntime`,
paused-clock capable, any deviation panics the cell). The **load generator**
(`crates/loadgen`, the SIPp substitute) grew a parallel vocabulary of its own:
a second closed registry (hand-rolled `by_id` / `default_scenarios` /
`failure_scenarios` match tables), a hardcoded `X-Loadgen-Id` correlation header
that *assumes SUT cooperation* (`B2BUA_RELAY_HEADERS`), one fixed identity and
global flag-only dwells for every call, a flag-line run spec, and no access to
authored Test cases or checks.

The duplication was biting: the two registries could silently drift on what a
shape *is* (id, anchors, attributes); a load run could not reuse the identity
checks the e2e surface already authors; pointing loadgen at a third-party SUT
(one that strips unknown headers) broke correlation entirely; and an endurance
run's shape was a shell script's flag line rather than a committed, reviewable
document.

## Decision

Fuse the two surfaces on **one axis data model with two run surfaces** — the
*data* is unified; the *engines* deliberately are not. Seven decisions, one per
fused concern.

**(a) One axis data model; two run surfaces kept distinct.** The authored axes
move into a dependency-light **`crates/e2e-model`**: the Test case (now with a
**binding pool** and `allowViolations`), Check sets + the post-call **check
evaluator**, Campaign, Endpoint config + the `EgressPolicy` model, the canonical
Message-anchor vocabulary (`ShapeSpec`/`ShapeCatalog` as the load-time metadata
seam `validate_case` consumes), the **shape descriptors** (b), and the **Load
profile** (f) with the load-run result model. No SUT crates — only
serde/schemars/regex plus the message/recording surface the evaluator reads.
Both run surfaces consume this one model:

- the **functional executor** (`e2e-core` + e2e-cli/e2e-web): `!Send` bodies
  driving an `InfraRuntime`, paused-clock capable, strict — a deviation panics
  the cell; one cell at a time under a per-infra concurrency cap;
- the **load fleet** (`loadgen`): `Send` bodies over the `AgentBinder`
  (bypassing the `!Send` `Harness`), wall clock only, **fallible** `try_*`
  surface where an expected failure is a counted `StepError`, thousands of
  concurrent calls at a governed rate.

Merging the two engines was considered and **rejected** — see Considered
options. The incompatibility (thread model, clock, failure model) is
load-bearing, not accidental.

**(b) Unified open shape registry.** One `ShapeDescriptor` per Callflow shape in
`e2e_model::registry::ShapeRegistry` — **one id space** (the Test-case/campaign
selector, the load report directory, the metrics label and the functional-body
attachment key are the same string; a duplicate id panics at registration, never
a silent shadow). The descriptor is the shape's single declaration: published
anchors, required input, optional authoring params schema, the load attributes
the driver consults per call (`needs_charlie`/`needs_bob2`/`emergency`, default
and failure mix weights), and an optional **load body** factory
(`LoadFactory → Arc<dyn RealCallScenario>`, minted from per-run
`ScenarioInputs`). The **functional body** cannot live in the light crate (it
drives an `InfraRuntime`), so `e2e-core` `attach()`es its `!Send` bodies **by
id** onto the same descriptors. A shape may carry one body, the other, or
**both** (a *dual-body* shape — `rerouting_prack` is the first). The registry is
an open builder: `with_defaults()` (or `empty()`) then `register()` — a
third-party crate adds shapes without touching any workspace table; loadgen's
match tables are deleted and the driver's `MixEntry` is built from descriptors
(`MixEntry::by_id` / `default_mix` / `failure_mix`).

**(c) Pluggable correlation strategies.** How the per-call token travels through
the SUT is a per-run strategy (`loadgen::Correlation`) with two halves — *stamp*
(applied by `CorrelationStamp` inside `CallEnv::outgoing_invite`, orthogonal to
the egress rewrite) and *extract* (the mux demux): `header` (the historic
relayed `X-Loadgen-Id`), `header_templated` (the token rides a **structured**
header a third-party SUT already relays — RFC 7433 UUI, PCV `icid-value` — with
the extract regex derived from the template), and `to_user` (the token IS the
To-header user-part, which a SIP-correct B2BUA copies onto its originated leg).
SUT cooperation is **no longer assumed**: `to_user` correlates against a
third-party SUT with zero configuration.

**(d) Binding pools.** A Test case may carry `bindings: { mode, entries }` — a
pool of `Input` overlays merged per call over the case's base input, walked
`seq` or `random`, **wrap-allowed** (a load run dials a finite subscriber pool;
identities repeat past the pool by design). String fields expand per-call tokens
`${seq}` / `${seq:N}` / `${rand:N}`; recognized extras become per-call dwell
overrides (killing the "dwells are global flags" limitation). Malformed tokens
and empty pools fail at startup via the same `validate_case` load-time
validation the e2e surface runs — never silently mid-run.

**(e) Checks + `allowViolations` on sampled calls.** An attached case's checks
(inline blocks + referenced Check sets) are evaluated over a call's recorded
trace by the ONE shared engine (`e2e_model::checks`), with `${input.*}` bound to
that call's resolved pool-expanded input. A failed check reclassifies an
otherwise-OK call to the **`check_fail`** result class; the sampled callflow
page lists every verdict, pass and fail. `allowViolations` is the authored
analogue of `Harness::allow_violation`: named RFC audit rules exempted per call.
Honest scope: this runs on **sampled calls only** — the unsampled majority binds
no recording (that is what keeps memory flat at load), so checks are a
**per-sample oracle, not a per-call gate**, exactly like the RFC audit.

**(f) Load profile + runtime rate control.** The whole run spec is one authored
JSON document (`LoadProfile`, schema-emitted like every other axis document) —
the load analogue of a Campaign: a Campaign expands a {case × shape × infra}
matrix; a Load profile parameterizes one sustained run (rate, duration,
concurrency, sampling/report cadence, loss/retransmit robustness, and the shape
**mix** with per-entry case and overrides). Precedence: the profile supplies
defaults; an explicitly-passed CLI flag overrides. The offered rate is
re-targetable live — `POST /rate?cps=` on the metrics socket — through a
**re-anchoring fixed-grid governor**: a rate change resets the grid (fresh
anchor + slot counter), so a cut fires **no catch-up burst** and a raise takes
effect within one slot; `cps=0` pauses admission, in-flight calls untouched.

**(g) Auth / TLS / REGISTER deferred behind named seams.** Not implemented, by
design, each isolated to a single adapter so adding it never touches the
choreography: **auth** is the `ChallengeResponder` trait with ONE retry point
(the fallible INVITE path + `OutOfDialogRequest::try_send_authed`; without a
responder, a 401/407 is a counted `status_401`/`status_407`); **TCP/TLS** is a
socket-layer change confined to the mux (`EndpointSpec` transport kind + a
`reliable` flag short-circuiting the retransmit timers); **REGISTER** is a
future shape (out-of-dialog builder + authed send + the existing `RegistrarAor`
egress policy), declared as a descriptor like every other shape. The READMEs
name each seam and its wiring point.

## Considered options

- **One engine for both surfaces.** Rejected: the differences are load-bearing.
  Thread model (`!Send` bodies over the recording `Harness` vs `Send` tasks over
  the `AgentBinder`), clock (paused-deterministic vs wall-clock rate), failure
  model (panic-on-deviation — what makes a functional regression loud — vs
  counted `StepError` — what makes a load tool robust to reordering and loss).
  A merged engine weakens both: a strict executor cannot tolerate a lossy
  fabric; a fleet cannot ride a paused clock. Fusing the *data* gives the reuse;
  fusing the *engines* would only trade one surface's guarantees for the
  other's.
- **Keeping the two closed registries, synced by convention.** Rejected: the
  drift is silent (an id, anchor set or attribute diverges with no compile
  error) and the match tables made third-party shapes impossible without
  patching the workspace.
- **Evaluating checks on every call, not just sampled ones.** Rejected: checks
  need the recorded trace, and recording every call is unbounded memory at load
  — the bounded first-N-per-bucket sampling is the loadgen's core memory
  design. The per-sample oracle matches the RFC audit's existing scope; raise
  coverage with `--sample-cap` / `--background-record-every 1` when needed.
- **A loadgen-private case/profile dialect.** Rejected: the point is one
  authored artifact valid on both surfaces; a dialect re-opens the drift and
  doubles the schemas and editor support.

## Consequences

- **Checks are sampling-scoped.** A check can never serve as a per-call SLA
  gate on the fleet; population-level gating stays with result classes and
  metrics. This is documented as the honest trade for flat memory.
- **Mixed shape-id casing in the one id space.** Load-born ids are snake_case
  (`basic_call`, `options_hold`), e2e-born ids kebab-case (`basic-call`,
  `transfer-refer-media`); the dual-body `rerouting_prack` is snake. Both are
  load-bearing surfaces (report directories, metrics labels, committed cases,
  dashboards), so the rename was judged not worth the breakage; the seam shows
  and we live with it.
- **`e2e-model` is not a pure-serde leaf.** Carrying the check evaluator pulls
  in sip-message/sip-net/scenario-harness (the message + recording surface).
  Accepted to keep ONE check engine next to the model it interprets; the crate
  still depends on no SUT crate, which is the boundary that matters.
- **Schema discipline.** Every schemars-derived model change requires
  `cargo run -p xtask -- e2e-schema` and committing the regenerated
  `e2e/schemas` files (a drift test enforces it).
- ADR-0018's "in-process registry in a shared `e2e-core` crate" is superseded:
  the registry is the open `ShapeRegistry` in `e2e-model`; `e2e-core` attaches
  functional bodies to it (ADR-0018 amended).
- Glossary in `CONTEXT.md` → "Loadgen fusion vocabulary".
