# Slice 0 — CORE rule parity audit vs sipjs defaults

TS source pinned at `portsource/sipjsserver` SHA `fffc4ac6`. Rust under
`crates/b2bua/src/rules/`. Audit diffs every TS default rule + framework piece
against the Rust port: match columns, layer/override ordering, action sequences,
invariant enforcement, per-dialog CSeq/tag-map/passthrough, and timer constants.

## Rule parity table

| TS rule (file) | Rust id | Status | Note |
|---|---|---|---|
| `relayOptionsRule` (RelayRules) | `relay-options` | MATCH | |
| `relayInfoRule` | `relay-info` | MATCH | |
| `relayUpdateRule` | `relay-update` | MATCH | |
| `relayMessageRule` | `relay-message` | MATCH | |
| `relayAckRule` | `relay-ack` | MATCH | |
| `relayReinviteRule` | `relay-reinvite` | MATCH | bare relay; glare/response correlation are Slice 1 |
| `relayPrackRule` | `relay-prack` | MATCH | RAck CSeq rewrite in `actions.rs` (RFC 3262 §7.2) |
| `relayByeRule` | `relay-bye` | **DELTA — FIXED** | Rust omitted the `terminate-leg{bye_received}` pre-mark; relied on a bespoke source-leg branch in `begin_termination`. Now emits it, matching TS. |
| `relayProvisionalRule` (DialogRules) | `relay-provisional` | MATCH | Rust adds an idempotent `UpdateLegState{Early}` before relay; `track_b_early_dialog` already sets Early. Benign, same output. |
| `confirmDialogRule` | `confirm-dialog` | MATCH (minor) | Rust does not emit "destroy losing forks"; the CORE port never has >1 live b-leg (failover terminates the old leg before `CreateLeg`), and TS guards `state !== terminated`, so it is a no-op here. Tag-map seeding is implicit via `ensure_a_dialog`. limiter_refresh timer not ported (limiters are out of CORE scope). |
| `absorbBye200Rule` | `absorb-bye-200` | MATCH | |
| `absorbOptions200Rule` | `absorb-options-200` | **DELTA — FIXED** | Rust had no filter and absorbed *every* OPTIONS 2xx (relayed in-dialog OPTIONS would never reach the peer). Added the TS filter: absorb only when the source dialog carries no pending-relay snapshot for the response CSeq (B2BUA-originated keepalive); otherwise decline so `relay-non-invite-200` forwards it. |
| `absorbNotify200Rule` | `absorb-notify-200` | MATCH | |
| `relayNonInvite200Rule` | `relay-non-invite-200` | MATCH | same method set (OPTIONS/INFO/PRACK/UPDATE/REFER/MESSAGE/SUBSCRIBE) |
| `cancel200CrossingRule` (CornerCaseRules) | `cancel-200-crossing` | MATCH | confirm→ack→destroy(BYE) |
| `retransmit200Rule` | — | MISSING (benign) | Re-ACK of a retransmitted initial 200 on an already-confirmed leg. Not reachable in the current harness (the simulated network does not retransmit a delivered 200). Falls under the re-INVITE/response-correlation work; track with Slice 1. No behavioural impact on ported scenarios. |
| `reinviteGlareRule` | — | MISSING (deferred) | Slice 1 (re-INVITE glare → 491). Explicitly out of scope here. |
| `relayReinviteResponseRule` | — | MISSING (deferred) | Slice 1 (re-INVITE response correlation). Out of scope. |
| `handleTimeoutRule` (LifecycleRules) | `handle-timeout` | MATCH | |
| `handleCancelRule` | `handle-cancel` | MATCH | per-leg destroy/cancel + cdr + begin-termination. Double-CANCEL avoided by the `begin_termination` fix (skips `cancelling` legs). |
| `resolveCancelResponseRule` | `resolve-cancel-response` | MATCH | overrides `route-failure` + `absorb-stale-failure` |
| `handle481Rule` | `handle-481` | MATCH | Rust omits the explicit cseqMethod column (481 on any method tears down); equivalent. |
| `resolveByeResponseRule` (TerminatingRules) | `resolve-bye-response` | MATCH | filter on `bye_disposition == bye_sent`; overrides `absorb-bye-200` |
| `resolveCrossByeRule` | `resolve-cross-bye` | MATCH | respond 200 + terminate-leg{bye_received} |
| `terminatingSafetyTimeoutRule` | `terminating-safety-timeout` | MATCH | canary no-op |
| `maxDurationRule` (TimerRules) | `max-duration` | MATCH | cdr(a) + begin-termination |
| `keepaliveRule` | `keepalive` | MATCH (constant) | Behaviour parity. Keepalive-timeout delay is a hard-coded 5 s in Rust vs TS `config.keepaliveTimeoutSec` (default 10). Documented in `keepalive_timeout.rs`; intentional test-clock simplification, not a rule-logic delta. Rust does not replicate the "skip leg with a pending keepalive_timeout" guard, but the CORE harness never overlaps probes. |
| `keepaliveTimeoutRule` | `keepalive-timeout` | MATCH | terminate-leg{bye_timeout} + cdr + begin-termination |
| `routeFailureRule` (FailureRules) | `route-failure` | DELTA (intentional, not fixed) | No-failover path uses `TerminateCall` (hard mark-all-terminated) instead of TS `terminate-leg{rejected}` + `begin-termination`. Reaches the same `terminated` end state for a rejected INVITE (alice's leg is trying/early — no BYE owed); only the per-leg terminal disposition label differs. Failover (callbackContext / `/call/failure`) is SERVICE_LAYER and explicitly deferred. The Rust source comments this ("no failover decision this slice"). `failure.rs` is green. |
| `noAnswerFailoverRule` | `no-answer` | DELTA (intentional, not fixed) | Rust is the no-failover core: cdr(timeout) + destroy-leg + begin-termination. The `/call/failure` failover branch is SERVICE_LAYER (deferred). Matches the ported no-answer scenarios. |
| `absorbStaleFailureRule` | `absorb-stale-failure` | MATCH | terminated-leg failure absorbed; ordered before `route-failure` (first-match) |
| `transferDefaultRules` (TransferRules) | — | MISSING (deferred) | Slice 5 (REFER). Out of scope. |

## Framework parity

| TS framework | Rust | Status |
|---|---|---|
| `Matcher.pickRanked` (column+filter accept, override drop, stable layer-desc sort) | `executor.rs::pick_ranked` | MATCH — overrides collected only from rules that pass columns+filter; sort is `(layer desc, registration-index asc)`, the stable-sort equivalent. |
| `RuleExecutor` iterate-first-handles → ActionExecutor → finalize → enforce → default | `executor.rs::execute_rules` | MATCH (CORE subset). Composition (`composesWith`), service-ext decode, auto-flush, and bye-disposition WARN logging are SERVICE_LAYER / persistence concerns not in the CORE engine. |
| `InvariantEnforcer` (→terminated: cancel-all-timers first, write-cdr, remove-call last) | `invariants.rs::enforce` + `finalize` | MATCH. Limiter-decrement guarantee not ported (limiters out of CORE scope). |
| `enforceByeDispositionInvariant` / `ByeDispositionInvariant` | — | MISSING (benign net) | The framework safety net that force-corrects a leg left in `bye_sent` after a BYE-final/`terminating_timeout`. In the Rust port `resolve-bye-response` and `terminating-safety-timeout` always emit the terminal transition, so no rule leaves a `bye_sent` leg dangling — the net never fires. Worth porting if/when bespoke service rules can absorb a BYE-final; recorded as a deferred framework hardening, not a behaviour delta for the current rule set. |
| `ActionExecutor` relay (per-dialog CSeq §12.2.1.1, RAck rewrite §7.2, pending-request snapshot §8.1.3.3, tag-map, passthrough Require/Supported/RSeq) | `actions.rs` + `relay.rs` | MATCH |
| `executeBeginTermination` (skip terminated / already-disposed / `cancelling`; confirmed→BYE, trying/early b→CANCEL+cancelled+terminated, trying/early a→none; →terminating; safety timer) | `actions.rs::begin_termination` | **DELTA — FIXED** (see below) |

## Behaviour-changing deltas fixed

1. **`begin_termination` did not skip already-resolved / `cancelling` legs.**
   The old Rust version special-cased only the *source* leg and otherwise
   keyed teardown purely on `LegState`. A `cancel-leg` (handle-cancel on a
   trying/early b-leg) sets disposition `cancelling` but leaves state
   trying/early, so the follow-on `begin-termination` re-CANCELed it (duplicate
   CANCEL on the wire) and stamped `bye_sent` instead of leaving the
   cancel-resolution rules to finish it. Rewrote `begin_termination` to mirror
   `executeBeginTermination`: skip a leg that is `terminated`, already carries a
   `bye_disposition`, or is `cancelling`; trying/early b-leg → CANCEL +
   `cancelled` + terminated; trying/early a-leg → `none`. Paired with making
   `relay-bye` pre-mark its source leg `bye_received` (TS does this via an
   explicit `terminate-leg`), so the skip guard subsumes the old source branch.
   `crates/b2bua/src/rules/actions.rs`, `defaults.rs` (relay-bye).

2. **`absorb-options-200` absorbed every OPTIONS 2xx.** Added the TS filter so
   only a B2BUA-originated keepalive OPTIONS (no pending-relay snapshot for the
   response CSeq on the source dialog) is absorbed; a relayed in-dialog OPTIONS
   declines and `relay-non-invite-200` forwards the 200 to the peer.
   `crates/b2bua/src/rules/defaults.rs`.

## Deltas intentionally NOT fixed (documented)

- **`route-failure` / `no-answer` failover branches** — SERVICE_LAYER
  (`/call/failure`), deferred per the plan. The CORE no-failover path reaches
  the same terminated state for the ported scenarios.
- **RFC 3326 `Reason` header on teardown BYEs** — TS stamps `action.reason`
  onto every BYE from `begin-termination`; Rust carries the reason on the
  action but does not emit the header (the BYE generator opts would need
  threading). Additive wire header only; no call-flow-state effect and no test
  depends on it. Recorded as a minor fidelity gap.
- **`keepaliveTimeoutSec` config** — Rust hard-codes 5 s; intentional
  paused-clock test simplification (documented in `keepalive_timeout.rs`).

## Deferred SERVICE_LAYER rules (out of scope for Slice 0)

`reinviteGlareRule`, `relayReinviteResponseRule`, `retransmit200Rule`
(re-INVITE family, Slice 1); `relayFirst18xTo180` / `promote18xPemTo200`
(18x management / PEM, Slices 3–4); `referTransfer` + `transferDefaultRules`
(REFER, Slice 5). These are not parity deltas — they are unported features.

## Test result

`cargo test -p b2bua -p b2bua-harness -p scenario-harness`: all green
(b2bua lib 73, rules 3, harness families basic_call/failover/failure/keepalive/
keepalive_timeout/prack/prack_forking/proxy_b2bua, scenario-harness 4) — 0 failures.
Full `cargo test --workspace` green.
