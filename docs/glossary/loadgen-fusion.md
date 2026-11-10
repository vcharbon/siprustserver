# Loadgen fusion vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Loadgen fusion vocabulary".

The language of the loadgen × e2e fusion
([ADR-0021](../../docs/adr/0021-loadgen-e2e-fusion.md)): **one axis data model**
(`crates/e2e-model`, holding Test case, checks, Endpoint config, shape descriptors,
Load profile) feeding **two deliberately distinct run surfaces**, the
functional per-cell executor (`e2e-core`: `!Send`, paused-clock capable,
panic-on-deviation) and the load fleet (`loadgen`: `Send`, wall-clock, fallible,
rate-driven). The axes of the [E2E test-management vocabulary](e2e-test-management.md)
(Test case, Endpoint config, Check, Message anchor) carry over unchanged; these are the fusion-specific terms.

**Shape descriptor** (`e2e_model::ShapeDescriptor`):
The ONE declaration of a **Callflow shape** in the unified open `ShapeRegistry`:
its stable id (**one id space** across both surfaces, so the Test-case selector,
load report directory, metrics label, and functional-body attachment key are the
same string; a duplicate id panics at registration), its published **Message
anchors** and required input, an optional authoring params schema, the load
attributes the driver consults per call (`needs_charlie`/`needs_bob2`/
`emergency`, default/abnormal-end mix weights), and the optional **load body**
factory. Third-party crates `register()` their own on top of `with_defaults()`.
_Avoid_: "scenario" for a registered shape (say **shape**; `--scenario name=w`
selects one by id but does not name the concept); declaring a shape's
id/attributes in more than one place (the silent drift separate closed
registries invite).

**Functional body** vs **Load body** (a **dual-body shape** carries both):
The two run-surface implementations a **Shape descriptor** may carry. The
*functional* body is the `!Send` `CallflowShape` that `e2e-core` `attach()`es by
id (it drives an `InfraRuntime`, which cannot live in the light model crate;
it is strict, so a deviation panics the cell). The *load* body is the `Send + Sync`
`RealCallScenario` minted by the descriptor's `LoadFactory` from per-run
`ScenarioInputs` (fallible `try_*` surface; a deviation is a counted
`StepError`). `rerouting_prack` is the first dual-body shape.
_Avoid_: merging the two body types or their engines (the thread/clock/
failure-model split is load-bearing, ADR-0021); "LoadScenario" as the trait
name in new code (a re-export alias of `RealCallScenario`).

**Binding pool**:
A **Test case**'s optional `bindings: { mode, entries }`, a **wrap-allowed**
pool of `Input` overlays (core From/To/R-URI + extras), one entry resolved per
call (`seq` walk or seeded `random`) and merged over the case's base input, with
`${seq}` / `${seq:N}` / `${rand:N}` expansion tokens, so ONE authored case
dials a finite subscriber population (identities repeat past the pool by
design). Recognized extras become per-call dwell overrides. Malformed tokens and
empty pools fail at startup (`validate_case`), never silently mid-run.
_Avoid_: leaking pool identities into Prometheus labels (buckets stay
scenario-keyed; a pool never becomes label cardinality); "user database" (it is
an overlay pool, not a store).

**Correlation strategy** (`loadgen::Correlation`):
The pluggable per-run answer to "how does the per-call token travel through the
SUT so the mux can demux an arriving initial leg", in two halves, **stamp**
(written into the outgoing INVITE by `CorrelationStamp` inside
`CallEnv::outgoing_invite`) and **extract** (recovered by the mux demux).
Strategies: `header` (the relayed `X-Loadgen-Id`; needs `B2BUA_RELAY_HEADERS`),
`header_templated` (the token rides a structured header a third-party SUT
already relays, such as RFC 7433 UUI or PCV `icid-value`), `to_user` (the token IS the
To user-part; zero SUT cooperation).
_Avoid_: assuming SUT cooperation (the assumption `to_user` removes); conflating
it with the **egress rewrite** (identity vs addressing, orthogonal halves of
`outgoing_invite`).

**Load profile**:
The authored JSON run spec for the load fleet (`--load-profile`, schema
`load-profile.schema.json`): rate/duration/concurrency, sampling + report
cadence, global loss/retransmit robustness, the overload refusal it accepts
(`overloadReject`: the final the system under test refuses overload with;
unset, none), and the shape **mix** (id, weight, attached case, per-entry
overrides). The load analogue of a **Campaign**: a
Campaign expands a functional {case × shape × infra} matrix; a Load profile
parameterizes one sustained run. Profile supplies defaults; an explicit CLI flag
overrides. The rate is live-retargetable (`POST /rate`, re-anchoring governor).
_Avoid_: "campaign" for a load run; putting addressing/egress in it (that is the
environment axis, **Endpoint config**).

**Accepted ending** (`scenario_harness::actor::{Branch, Expect}`):
One named way a scenario's call may end and still be a success (`answered`,
`rejected`, `early_media_rejected`, `abandoned`, `answered_despite_cancel`,
`answered_before_cancel`, `transfer_declined`, or a
scenario-named predicate over the observed state). A call that tears down and
settles on one of them is `ScenarioVerdict::Success { branch }`, classed `ok`
and counted under `loadgen_branch_total{stream,shape,branch}`. Nothing else is
accepted by default. The run, not the scenario, may add `refused_overload`: the
final its **Load profile** declares as the system under test's overload refusal
(`overloadReject`, e.g. 503), received by a non-emergency call before it
connects.
_Avoid_: excusing a non-200 ending outside the scenario (a consumer-side class
filter, a per-descriptor reject status): the scenario states it, and `ok` means
ok everywhere.

**Failed step** (`scenario_harness::actor::FailedAt`):
Where a call failed: the agent, its pending goal (kind and index) and the
controller barrier pending, keyed `<who>[:<kind>#<index>]@<barrier>` (a barrier
that never held reads as its bare name, e.g. `settle`). It is the failed call's
sample case and the `step` label of `loadgen_failed_step_total`.
_Avoid_: the last lifecycle phase reached as the failure's location (it names
where the call got to, not what failed).

**check_fail** (result class):
The load report/metrics class an otherwise-OK **sampled** call reclassifies to
when an attached Test case's check fails (verdicts, pass AND fail, render on the
sampled callflow page). Checks and `allowViolations` run on sampled calls only
(the unsampled majority binds no recording, which is what keeps memory flat), so
this is a **per-sample oracle, not a per-call gate**, exactly like the RFC
audit; raise coverage via `--sample-cap` / `--background-record-every 1`.
_Avoid_: reading `check_fail` counts as full-population totals; using checks as
a per-call SLA gate (population gating stays with result classes + metrics).
