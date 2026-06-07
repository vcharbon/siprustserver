# Implementation plan — Callflow services as explicit per-call state machines

Implements [ADR-0016](../adr/0016-callflow-service-state-machines.md). Goal:
make adding an in-call service a legible, doc-generated, single-machine job, and
prove it on two services — `transfer` (in-tree retrofit) and `announcement`
(MRF/MSCML, built in a **separate crate** against the public Rule SDK).

## Principles for every slice

- **Independently green.** Each slice compiles, passes the full suite, and lands
  with **zero warnings** (project gate) on its own. No slice leaves a half-wired
  framework behind a feature flag.
- **Behaviour-preserving until X7/X8.** Slices 0–6 add machinery only; no
  existing call flow changes. The 687-test suite is the regression oracle.
- **Replication-safe.** `Call` is msgpack-encoded and replicated; every new
  `Call` field is `#[serde(default)]` + skip-if-empty so old/new nodes interop
  (verified in slice 0).
- **Source-pinned.** Slices that port source behaviour (5, 7, 8) record the exact
  `sipjsserver` commit in `MIGRATION_STATUS.md`, per `CLAUDE.md`.
- **Tests are the deliverable.** Unit tests for framework mechanics; `b2bua-harness`
  e2e for the two services; an `xtask` freshness test for the diagrams.

## Dependency graph

```
0 scaffold ─┬─▶ 1 selection ─▶ 2 global-machine ─┬─▶ 3 macro+init ─▶ 4 doc-gen
            │                                     │
            └─────────────── 5 primitives ───────┘
                                   │
              6 SDK crate ◀────────┘
                  │
                  ├─▶ 7 transfer retrofit   (needs 3,4; uses in-tree path)
                  └─▶ 8 announcement crate  (needs 3,4,5,6; out-of-crate path)
                              │
                              └─▶ 9 observability (optional)
```

Slices 5 and 6 can proceed in parallel with 1–4. Slices 7 and 8 are the
payoff and can also run in parallel once their prerequisites land.

---

## Slice 0 — Compatibility scaffold (no behaviour change)

**Changes**
- `crates/call/src/model.rs`: add `Call.sm_cursors: BTreeMap<MachineId, StateLabel>`
  (newtypes over `&'static str` / `String`), `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]`.
- `crates/b2bua/src/rules/model.rs`: add to `RuleDefinition` the fields
  `machine: Option<MachineId>`, `active_states: &'static [StateLabel]`,
  `transitions: &'static [(StateLabel, StateLabel)]`. To avoid churning every
  literal in `defaults.rs`, introduce a `RuleDefinition::core(id, layer, matcher, handle)`
  constructor (machine-less) and migrate existing literals to it.

**Tests**
- Existing suite passes unchanged.
- New serde round-trip: a `Call` with and without `sm_cursors` decodes under the
  msgpack codec; an old-shape body (no field) decodes to empty.

**Exit:** green, zero warnings, replication-compat proven.

---

## Slice 1 — Machine-gated selection + `SetState` + transition check

**Changes**
- `crates/b2bua/src/rules/executor.rs:16` `pick_ranked`: extend the filter with
  `&& machine_active(r, ctx.call)`, where `machine_active(r, call) = r.machine
  .map_or(true, |m| r.active_states.contains(&cursor(call, m)))`. Core (machine-less)
  rules stay always-candidate.
- `crates/b2bua/src/rules/model.rs`: add `RuleAction::SetState { machine, to }`.
- `crates/b2bua/src/rules/actions.rs`: apply `SetState` by writing
  `call.sm_cursors[machine] = to`.
- Transition legality: a checked helper asserts `(from, to) ∈ r.transitions`
  under `cfg(test)`/`debug_assert`; in release, log-and-proceed (never panic a
  live worker).

**Tests** (unit, `rules` module)
- A machine-bound rule is a candidate only when the cursor ∈ `active_states`;
  skipped otherwise.
- `SetState` moves the cursor; the next event sees the new state.
- An undeclared `(from,to)` transition trips the debug assert.

**Exit:** green; no rule in the tree carries a machine yet, so behaviour is
identical.

---

## Slice 2 — Global call machine projection

**Changes**
- Project `CallModelState` into `sm_cursors["global-call"]` at the single
  finalize point (`crates/b2bua/src/rules/invariants.rs` `finalize`), so the
  global machine is readable uniformly without changing the authoritative
  `state` field or the termination invariants.

**Tests**
- Across an existing call lifecycle test, the `global-call` cursor tracks
  `Active → Terminating → Terminated`.
- All termination-invariant tests pass unchanged.

**Exit:** green; global machine observable, termination logic untouched.

---

## Slice 3 — `define_service!` / `sm_rule!` macro + `init` hook

**Changes**
- New `crates/b2bua/src/rules/service.rs`: `macro_rules!` `define_service!` /
  `sm_rule!` expanding to (a) the state enum, (b) a `Vec<RuleDefinition>` with the
  machine fields populated, (c) an `init(descriptor, &Call) -> Option<ServiceSeed>`,
  (d) a typed data accessor. `ServiceSeed { initial_state, data_write, actions }`.
- A `ServiceDef { id, init, rules }` registry type. `RouterCtx`/`B2buaCore`
  compose `services: Vec<ServiceDef>` instead of a bare rule `Vec`; the engine's
  rule list is `flatten(services.rules) ++ core_rules()`
  (`b2bua_core.rs:238` updated).
- Setup hook: in `handle_initial_invite` (`initial_invite.rs:154`), after
  `apply_route`, run each service's `init`; fold returned seeds (cursor + data +
  initial actions) through the normal `ActionExecutor`/effects pipeline — no
  back-door state write.

**Tests**
- A **test-only stub service** (`states S0→S1`, one `init`, one state-gated rule)
  registered in a unit test: `init` seeds cursor=S0 + data; the rule fires in S0,
  `SetState`s to S1, and declines in S1. Validates macro + init + selection
  together with no real service.

**Exit:** green; services are first-class and seedable.

---

## Slice 4 — `xtask state-machine-docs` + CI freshness test

**Changes**
- `xtask/src/main.rs`: add `state-machine-docs` subcommand (mirrors `abnf-regen`).
  It pulls the **composed** service/rule registry (via a `b2bua-runner`-exposed
  `compose_services()` or equivalent so it sees core + in-tree + separate-crate
  services), builds each machine's graph from its rules' `transitions`, and emits
  one Mermaid diagram per machine under `docs/sm/<machine>.md`.

**Tests**
- A workspace test regenerates in memory and asserts equality with the committed
  `docs/sm/*` (fails on drift).
- A static check: every rule's declared `transitions` reference states in its
  machine's enum, and every machine referenced by a rule exists.
- Generate `global-call` + the stub machine; assert non-empty + well-formed
  Mermaid.

**Exit:** green; `docs/sm/global-call.md` committed; drift fails CI.

---

## Slice 5 — Port media/INFO primitives (parallel with 1–4)

Ports the SIP primitives the source has but Rust lacks. **Source-pinned.**

**Changes**
- `crates/b2bua/src/rules/model.rs` + `actions.rs`:
  - extend `RuleAction::SendRequestToLeg` (already present, `{leg_id, method}`)
    with `body` + `content_type` (INFO/OPTIONS/UPDATE/MESSAGE only; default
    `application/sdp` when a body is present and no type given) — carries an
    **opaque** body (MSCML rides here).
  - add `RuleAction::SendProvisionalToLeg { leg_id, status, reason, body,
    content_type, to_tag, p_early_media }` — broker an unadopted leg's SDP onto
    the A-leg as an unreliable `183` (RFC 3262 early media).
  - extend `CreateLeg` with `kind: Option<LegKind>` so a service can park a
    `media` (unadopted) leg.
- Unadopted-leg relay gate: the generic relay-to-peer implicit-`"a"` fallback
  must be gated on `Leg.adopted` (`crates/call/src/model.rs` already has
  `Leg.adopted`/`LegKind::Media`) — update `crates/b2bua/src/rules/relay.rs` /
  `defaults.rs` so a parked media/transfer leg is never mis-routed to A. This is
  the single core enabler the media-callflow work hangs off.

**Tests** (port the source's)
- Unadopted media leg: `relay-to-peer` is NOT routed to A (port
  `leg-kind-gate.test.ts`).
- INFO with `application/mediaservercontrol+xml` body is emitted to a named leg.
- `183` brokers an unadopted leg's SDP to A.
- Existing in-dialog INFO relay still transparent (port `indialog-info.ts`).

**Exit:** green; primitives available, no service uses them yet.

---

## Slice 6 — Public Rule SDK (`crates/b2bua-sdk`)

Carves the minimal, dogfood-driven public surface (ADR-0016 X6) as a **lower
crate** so the boundary is real (no path from a service to `b2bua` internals).

**Changes**
- New `crates/b2bua-sdk` depended on by `b2bua` (not vice-versa — no cycle).
  Exposes: the `define_service!` / `sm_rule!` macros, a **narrowed
  `RuleContext`**, and a **public action subset** — `CreateLeg{kind:media}` /
  `DestroyLeg` / `CancelLeg`, `SendRequestToLeg` (INFO, opaque body),
  `SendProvisionalToLeg`, `Respond`, `ScheduleTimer`/`CancelTimer`, `SetState`,
  `BeginTermination`. Internal actions (`SetTransfer`, PEM/PRACK, raw send,
  `Merge`/`Split`) stay out.
- **Open sub-decision flagged for review:** whether the public actions are a
  distinct SDK type `b2bua` maps from (hard boundary, more glue) or curated
  re-exports of the internal `RuleAction` (soft boundary, less glue). Recommend
  the distinct type only if slice 8 shows leakage; start with curated re-exports
  validated by slice 8's dependency check.

**Tests**
- Unit: each public constructor yields the expected internal action.
- The real boundary test is **slice 8**: the announcement crate compiles with
  `b2bua-sdk` as its only `b2bua`-family dependency.

**Exit:** green; SDK surface published; internals still `pub(crate)`.

---

## Slice 7 — Retrofit `transfer` in-tree (needs 3, 4)

**Changes**
- `crates/b2bua/src/rules/refer_transfer.rs`: re-express via `define_service!`.
  `TransferPhase` becomes the declared `transfer` machine (its
  `ReferAuthorizing → CRinging → CRealigning → ARealigning` edges); rules become
  `sm_rule!`s with `active_states`/`transitions`; the phase **match-column** is
  replaced by machine gating; the phase **label** moves into `sm_cursors`, leaving
  `Call.transfer` (`TransferState`) holding data only.

**Tests**
- **All** existing transfer tests pass unchanged (behaviour-preserving refactor;
  the suite is the oracle).
- Committed `docs/sm/transfer.md` matches the generator.

**Risk:** transfer is complex and mid-port (slice 5+ deferred per ADR-0010). Keep
the change a pure refactor; do not alter transfer semantics here.

**Exit:** green; transfer is the in-tree worked example with a generated diagram.

---

## Slice 8 — `announcement` crate, out-of-crate capstone (needs 3,4,5,6)

**Changes**
- New `crates/announcement` depending **only** on `b2bua-sdk` (+ `call`,
  `sip-message`, and a tiny local MSCML helper). `ext["announcement"]`-backed
  data `{ media_leg, mrf_addr, clip_id, mscml_req_id, pending_route }`; state in
  `sm_cursors`.
- Machine (early-media-then-dial), four states:
  `OfferingMrf → Announcing → Bridging → (clear)`.
  - `init` (applicable iff the decision descriptor requests an announcement):
    seed cursor=`OfferingMrf`, ext data, and `CreateLeg{kind:media}` toward the
    MRF — launched at setup in parallel with routing.
  - rule @`OfferingMrf`, on media-leg `200 OK`: `SendProvisionalToLeg` (183 + MRF
    SDP → A) + `SendRequestToLeg` INFO MSCML `<play clip_id>` + `SetState
    Announcing`.
  - rule @`Announcing`, on INFO MSCML `<response success>` on the media leg:
    `SetState Bridging` + `CancelLeg`/BYE media leg + `CreateLeg{kind:destination,
    pending_route}`.
  - rule @`Bridging`, on destination `200 OK`: answer A with the destination SDP
    + bridge (the framework's normal merge); clear the service cursor.
  - rule @any, on media-leg failure/timeout: `BeginTermination` (the one-hop
    service→global command).
- Minimal MSCML build/parse in the crate (build `<play>`, parse `<response
  status>`).
- Register the service in `b2bua-runner` (compose `core_services ++ announcement`).

**Tests** (`b2bua-harness` e2e — the capstone)
- Happy path: alice ↔ b2bua ↔ {MRF, dest}. Assert A receives a `183` with the MRF
  SDP (early media); b2bua sends INFO MSCML `<play>` to the MRF; on the MRF's INFO
  `<response success>`, the media leg is BYE'd, a destination INVITE is sent, A is
  answered with the destination SDP and bridged.
- Failure path: MRF rejects/times out → call terminates cleanly (one CDR, all
  legs reaped, no leaked timers).
- A-side hangup mid-announcement → ordinary `→Terminated` cleanup BYEs the
  unadopted media leg (no special rule).
- **Boundary check:** `crates/announcement/Cargo.toml` lists no `b2bua`-internal
  dependency — only `b2bua-sdk`.
- Committed `docs/sm/announcement.md` matches the generator.

**Exit:** green; out-of-crate integrator seam proven end-to-end.

---

## Slice 9 — Observability (optional)

Surface `sm_cursors` in CDR events / structured logs / a gauge, and a
"dump-all-cursors" debug aid, so a live call's machine positions are reviewable
(and useful for HA reconciliation debugging). Can be folded into earlier slices
if cheap.

---

## Out of scope (this work item)

- Retrofitting `relay_first_18x` / `promote_pem` onto the framework (follow-up;
  pattern proven on `transfer` first).
- A true per-leg machine tier (deferred until a 1:N concurrent-sub-protocol
  service needs it — ADR-0016 X2).
- A proc-macro (reserved until `macro_rules!` ergonomics prove insufficient —
  ADR-0016 X5).
