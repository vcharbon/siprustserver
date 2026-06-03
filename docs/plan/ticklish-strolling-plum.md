# Port the fake-clock call-flow scenario corpus to Rust (re-INVITE, PRACK, long-calls, 18x-management, early-media, REFER)

## Context

The Rust port (`crates/*`) has a complete **CORE_LAYER** B2BUA rule set and a fluent
scenario harness. The TS source (`portsource/sipjsserver`) carries a large corpus of
fake-clock call-flow scenarios that have **not** been ported, and — critically — several
of the rules those scenarios exercise are explicitly **deferred / unported** in Rust
(SERVICE_LAYER rules per MIGRATION_STATUS.md + ADR-0010 + `defaults.rs:4-5`).

The user wants the full remaining call-flow scenario corpus ported and green:
**re-INVITE, all PRACK incl. multi-early-dialog, long-calls/keepalive, the "fake-prack"
family, early-media 183→200OK promotion, suppress-18x, and the full REFER transfer
service.** This is not "verify + write tests" — for most families the underlying
SERVICE_LAYER rule must be **implemented from the TS source first**, then the DSL
extended, then the scenarios ported and passed one at a time.

### What already exists & is green (do not re-port)
- `crates/b2bua-harness/tests/prack.rs` — basic PRACK relay (RFC 3262 end-to-end).
- `crates/b2bua-harness/tests/prack_forking.rs` — multi-early-dialog PRACK forking.
- `crates/b2bua-harness/tests/keepalive.rs`, `keepalive_timeout.rs` — long-call OPTIONS.
- Full CORE_LAYER rule set: `crates/b2bua/src/rules/defaults.rs` (relay/dialog/absorb/
  lifecycle/terminating/corner-case/failure/timer), engine in `executor.rs`,
  actions in `actions.rs` (incl. PRACK relay + RAck rewrite, per-dialog CSeq, tag-map).

### What is missing (must be implemented before its tests can pass)
| Family | Rust rule status | TS rule source |
|---|---|---|
| re-INVITE glare→491 | **missing** | `CornerCaseRules.ts` `reinviteGlareRule` (L113-180) |
| fake-prack / suppress-18x | **missing** | `custom/relayFirst18xTo180.ts` (341) + `_shared/sdpDiff.ts` (96) |
| early-media 183→200 | **missing** | `custom/promote18xPemTo200.ts` (574) + `sdpDiff.ts` |
| REFER transfer | **missing** | `custom/referTransfer.ts` (799) + `defaults/TransferRules.ts` (219) |

### Execution constraints (from the user)
- **No parallel work** — exactly one implementing subagent runs at a time, sequentially.
- **Continuous run, commit per test family** — do all slices in one sequential pass;
  land a git commit when each test-family file goes green. Do not pause for review
  between slices.
- **Tests written & run one at a time** — within a family, write one scenario, run it,
  make it pass, then the next. Never bulk-write a family and run at the end.
- **REFER: study before coding** — the REFER subagent MUST fully read & understand the
  service-layer call-flow (phase machine, NOTIFY, realignment, gating, HTTP adapter,
  reference traces) and produce a short design note BEFORE writing any code. The
  service layer is hard to analyse cold.
- **Migration ritual** (CLAUDE.md): pin the source submodule SHA in MIGRATION_STATUS.md,
  port impl + tests, and list any un-ported test with a precise justification.

### Test-harness hazards (CLAUDE.md — re-read before timer/clock work)
Behaviour rides `tokio::time` directly; tests use `#[tokio::test(start_paused = true)]`
+ `Harness::advance` (100 ms chunks). Transit delay must be ≥ 1 ms. Drive the protocol
*between* advances. Timer cancellation is epoch/tombstone, never by `DelayQueue::Key`.

---

## Reference material the implementer must use
- Existing test idiom: `crates/b2bua-harness/tests/prack.rs`, `prack_forking.rs`,
  `keepalive.rs`; helpers in `crates/b2bua-harness/tests/common/mod.rs`; SUT entry
  `B2buaSut::route_all_to`.
- Fluent DSL surface: `crates/scenario-harness/src/agent.rs` (Agent/ClientInvite/
  InDialogRequest/Dialog/ServerTxn/Respond builders; `with_to_tag`, `with_rack`,
  `with_header`, `with_sdp`, `send_request`, `advance`).
- Rule shapes: `crates/b2bua/src/rules/model.rs` (`Match`, `RuleDefinition`,
  `RuleAction`), `executor.rs` (`pick_ranked` layer-ranked first-match), `actions.rs`.
- Decision seam: `crates/b2bua/src/decision/{mod.rs,schemas.rs,test_adapter.rs}`
  (`call_refer`, `CallReferRequest/Response` already defined; adapter unimplemented).
- Reference SIP traces (behavioural oracle):
  `portsource/sipjsserver/test-results/fake-clock/{b2bonly,proxy+b2b,...}`.
- TS scenario sources: `portsource/sipjsserver/tests/scenarios/*.ts`.

---

## Plan — sequential slices (one subagent each, in order)

Each slice's subagent: (1) study the named TS rule + scenarios + reference traces,
(2) port/extend the rule(s) faithfully, (3) extend the DSL only as needed,
(4) port scenarios **one test at a time**, running + passing each before the next,
(5) update MIGRATION_STATUS.md, (6) commit the family. Run the full workspace test
suite (`cargo test`) green before handing off to the next slice.

### Slice 0 — CORE rule parity audit & delta-fix ("verify all rules replicated")
Dedicated verification subagent. Diff every ported CORE rule in
`crates/b2bua/src/rules/defaults.rs` + framework (`model.rs`/`executor.rs`/`actions.rs`/
`invariants.rs`) against the TS defaults (`RelayRules`, `DialogRules`, `CornerCaseRules`,
`TimerRules`, `LifecycleRules`, `TerminatingRules`, `FailureRules`) and framework
(`Matcher`, `RuleExecutor`, `ActionExecutor`, `InvariantEnforcer`, `ByeDispositionInvariant`).
Produce a written delta report; fix discrepancies that change behaviour. Keep the existing
suite green. Commit. **No new features here** — this is the parity baseline the rest builds on.

### Slice 1 — re-INVITE (incl. glare → 491)
- Port `reinviteGlareRule` + `relayReinviteResponseRule` (`CornerCaseRules.ts:113-180`).
  Glare detection keys off a pending inbound INVITE on the source dialog
  (`Dialog.ext.inboundPendingRequests`) — add the equivalent pending-request marker to
  the Rust call/leg model (`crates/call` / `crates/b2bua/src/store`).
- DSL: allow a second in-dialog re-INVITE to be sent while one is pending (the crossing
  case interleaves two client transactions) — small extension to `agent.rs` if needed.
- Port `tests/scenarios/reinvite.ts` → `crates/b2bua-harness/tests/reinvite.rs`:
  `alice_reinvite`, `bob_reinvite`, `crossing_reinvite_glare`. Commit.

### Slice 2 — round out long-calls / keepalive
Uses existing rules (`handle-481`, `keepalive`, `absorb-options-200`).
- Port `keepalive-481.ts` (481 on OPTIONS → BYE only the healthy peer) and
  `keepalive-via-proxy.ts` (keepalive through the proxy SUT). Confirm
  `options-keepalive-timeout.ts` is already covered by `keepalive_timeout.rs`; port the
  delta if not. Commit.

### Slice 3 — 18x management: `relayFirst18xTo180` (suppress-18x + fake-prack)
- Port `custom/relayFirst18xTo180.ts` (341) + `custom/_shared/sdpDiff.ts` (96) as a
  SERVICE_LAYER rule (`layer=1`, overrides the relevant relay/provisional CORE rules).
  Implements: rewrite first 183→bare 180, suppress subsequent 18x, B2BUA-originated PRACK
  toward Bob, early-media SDP cache, inject cached SDP into the final 200, To-tag
  continuity across failover, UPDATE handling, and the disabled / delayed-offer-fallback
  self-disable mode.
- Config: plumb `relay_first_18x_to_180` through `FeatureActivations` (`crates/call`) +
  `RouteDecision` (`decision/schemas.rs`) + the test decision adapter; wire SERVICE_LAYER
  rule selection on the flag value (`true` | `"fake-prack"` | `"promote-pem-to-200"`).
- DSL: bare-180 predicate (no body / no Require / no RSeq), assert B2BUA→Bob PRACK from
  the recording, cached-SDP-on-200 assertions.
- Port `suppress-18x.ts` (4 cases) → tests, commit. Then `fake-prack.ts` (8 cases) →
  tests, commit. One test at a time each.

### Slice 4 — early-media 183→200OK: `promote18xPemTo200`
- Port `custom/promote18xPemTo200.ts` (574) as a SERVICE_LAYER rule (same config field,
  value `"promote-pem-to-200"`). Implements: promote first 183+P-Early-Media+SDP to a
  synthetic 200 toward Alice; window open/close; resync re-INVITE toward A when the real
  200's SDP differs (via `sdpDiff`); in-dialog gating during the window (UPDATE→491,
  INFO→488); forking re-seed onto the winning tag; B-failure→BYE A with `Reason`;
  A-BYE-during-window→CANCEL B.
- DSL: synthetic-200 assertion, resync re-INVITE detection, P-Early-Media header support.
- Port `promote-pem-to-200.ts` (7 cases) → tests. Commit.

### Slice 5 — REFER transfer service (FULL corpus) — the large slice
**Mandatory study step first** (subagent produces a design note, no code yet): fully read
`custom/referTransfer.ts` (799), `defaults/TransferRules.ts` (219), the phase machine
(refer-authorizing → c-ringing → c-realizing → c-realigning → a-realigning → merged), the
NOTIFY subscription lifecycle (sipfrag body; `sip-message::sipfrag` already ported), the
gating regimes (transparent vs 491/481 per phase), the reject paths, the safety timers
(`refer_subscription_expiry` 60s, `no_answer_timeout`, `refer_reinvite_answer` 32s,
`refer_overall_safety` 120s), and the HTTP `call_refer` decision adapter + `MockServer`.
Cross-check against the reference traces.

Then port:
- `referTransfer` rule + `TransferRules` (SERVICE_LAYER). C-leg lifecycle via `CreateLeg`
  toward the refer target (INVITE/180/200/ACK), NOTIFY generation (subscription-state
  active/terminated + sipfrag status), 3-phase realignment (c-realign re-INVITE C with A's
  SDP; a-realign re-INVITE A with C's SDP; `Merge` a↔c then BYE B+C), gating rules
  (491/481 per phase, reject second REFER), reject paths (403/501/481/491), safety timers.
- Decision adapter: implement `call_refer` in `test_adapter.rs` (scripted) + a
  MockServer-equivalent HTTP reference path; schemas already exist in `decision/schemas.rs`.
- DSL: add `Refer` to `InDialogMethod` (`crates/sip-message/generators.rs`) + generator +
  `Refer-To`/`Replaces` headers; `with_refer_to(uri)` builder; NOTIFY `expect` with a
  `subscription-state` predicate + sipfrag body assertions.
- Port the 6 scenario files one at a time, each committed as its own family:
  `refer-allow.ts` (5) → `refer-c-realign.ts` (5) → `refer-full-transfer.ts` (5) →
  `refer-gating.ts` (8) → `refer-reject.ts` (5) → `refer-timers.ts` (1).

### Finalisation
- Update `MIGRATION_STATUS.md`: source SHA, mark the rule-engine + scenario rows, list any
  intentionally un-ported TS test with justification.
- Amend ADR-0010 (SERVICE_LAYER rules now implemented) and add an ADR for the REFER
  service-layer shape if its design diverges from the TS.

---

## Verification
- Per slice: the family's test file passes one scenario at a time, then the **whole**
  workspace suite is green: `source ~/.cargo/env && cargo test --workspace`.
- Behavioural parity: compare emitted SIP traces against
  `portsource/sipjsserver/test-results/fake-clock/` reference traces for the matching
  flows; on a failing scenario, add temporary `eprintln!` in `sip-net::simulated::deliver`
  + the timer driver (no panic-time dump yet — CLAUDE.md).
- Regression guard: Slice 0 leaves the existing green suite untouched; every later slice
  must keep all prior families green (re-run full suite before each commit).
- Final: full `cargo test --workspace` green; MIGRATION_STATUS.md updated; un-ported-test
  justifications recorded.

## Risks / notes
- **REFER is the dominant risk** (~3,400 LOC rule+scenarios, a multi-phase FSM, NOTIFY,
  HTTP adapter). The mandatory study step + one-test-at-a-time discipline is the mitigation.
- 18x-management and PEM share `sdpDiff` and the same config field with different values —
  port `sdpDiff` once in Slice 3 and reuse in Slice 4.
- DSL gaps (REFER method, concurrent re-INVITE for glare, NOTIFY/sipfrag predicates) are
  additive extensions to `scenario-harness/src/agent.rs`; keep them minimal and idiomatic.
- Some TS scenarios are marked `skipFinalSweep` / `skipValidation` for known fake-clock
  timer races — preserve those exemptions and justify them in the Rust port.
