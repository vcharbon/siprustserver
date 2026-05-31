# Slice 5 — REFER transfer service: implementation-ready design note

Study/design output for the full REFER-driven blind-transfer service-layer port
(plan: `docs/plan/ticklish-strolling-plum.md` Slice 5). TS source SHA `fffc4ac6`
under `portsource/sipjsserver`. This note is the contract for the implementing
subagents — they will NOT re-derive it. File:line citations are to the TS source
unless prefixed `RUST`.

The TS service is a *callflow service* (`defineService`, ADR-0016): a typed
per-call ext slice (`TransferCallExt`) plus ~16 rules that gate on `ext.phase`.
The Rust port has **no** `defineService`/typed-ext machinery and rules are bare
`fn(&RuleContext) -> Option<RuleHandleResult>`. We therefore port the slice the
same way Slice 3/4 ported `relayFirst18x`/`promote_pem`: a typed `TransferState`
field on `Call` (mirrors `RelayFirst18xState`/`PromotePemState`), `call::helpers`
accessors, a dedicated `SetTransfer` action, and the rules as stateless `fn`s
that read `ctx.call.transfer` and emit `SetTransfer` to advance the phase. There
is no SERVICE_LAYER ext-presence guard — activation is "the `transfer` slice is
`Some`", checked in each rule's `filter` exactly like `promote_pem_active(ctx)`.

---

## 0. The cast and the core shape

- **A** = transferee (the original caller; leg id `"a"`).
- **B** = referrer (the leg that issues REFER; `ext.referrerLegId`, usually `"b-1"`).
- **C** = transfer target (`ext.cLegId`, the freshly-created `"b-2"`).
- The transfer slice is CALL-scoped, legs addressed by id. No per-leg ext.
- TS `referTransfer.ts:38-85` defines `TransferPhase` (4 values) + `TransferCallExt`.
  Rust `TransferState` field shape — see §9.

`TransferPhase` values (`referTransfer.ts:38-47`):
`"refer-authorizing"` → `"c-ringing"` → `"c-realigning"` → `"a-realigning"`.
(There is no explicit `merged`/`terminated` phase — the terminal rules clear the
slice via `clearCallExt`.)

Two seed rules live in `TransferRules.ts` (they must fire BEFORE the slice
exists, so they are `alwaysActive` CORE-equivalent rules, not service rules):
`transfer-reject-replaces`, `transfer-reject-a-leg-refer`, `transfer-intercept-refer`.

---

## 1. Phase FSM

States gate which rules can match via each rule's `filter` reading `ext.phase`.
Events: SIP responses on C/A legs, the `refer-http-result` internal event, four
timers, and in-dialog requests from A/B/C.

| From phase | Event | Rule (TS lines) | Action summary | To phase |
|---|---|---|---|---|
| *(none)* | REFER from-b, confirmed+bridged, no Replaces, no active xfer | `transfer-intercept-refer` (TransferRules.ts:141-211) | 202; seed slice; arm `refer_subscription_expiry`(60s)+`refer_overall_safety`(120s); NOTIFY 100 active; fire `refer-async-http` | `refer-authorizing` |
| `refer-authorizing` | `refer-http-result` outcome=`allow` | `transfer-http-allow` (182-245) | build held SDP from A's INVITE; `create-leg` toward Refer-To (held/inactive SDP); set `cLegId=b-{n+1}`, `effectiveReferToUri` | `c-ringing` |
| `refer-authorizing` | `refer-http-result` outcome=`reject`/`error` | `transfer-http-reject` (131-169) | NOTIFY terminated;reason=noresource (sipfrag = reject_code or 603/500); cancel sub-expiry + overall; **clearCallExt** | *(cleared)* |
| `refer-authorizing` | timer `refer_subscription_expiry` | `transfer-http-timeout` (249-275) | NOTIFY terminated;reason=timeout (sipfrag 500); cancel overall; **clearCallExt** | *(cleared)* |
| `c-ringing` | C INVITE-resp 1xx (legState trying/early, src=cLegId) | `transfer-c-1xx-to-notify` (279-312) | dedupe vs `lastCLegNotifiedStatus`; NOTIFY active (sipfrag of the 1xx) | `c-ringing` |
| `c-ringing` | C INVITE-resp 2xx (trying/early, src=cLegId) | `transfer-c-200-initial` (316-384) | update C confirmed/bridged; confirm-dialog C; ack C; NOTIFY 200 terminated;noresource; cancel sub-expiry + `no-answer-{cLegId}`; arm `refer_reinvite_answer`(leg C); **re-INVITE C with A's SDP**; CDR answer; capture `cInitialSdp` | `c-realigning` |
| `c-ringing` | C INVITE-resp 3xx–6xx (trying/early, src=cLegId) | `transfer-c-fail-initial` (388-435) | NOTIFY terminated;noresource (sipfrag of failure); cancel sub-expiry+overall+no-answer; CDR reject; terminate C (rejected); **clearCallExt** | *(cleared)* |
| `c-ringing` | timer `no_answer` (leg=cLegId) | `transfer-c-no-answer` (439-472) | NOTIFY terminated;timeout (sipfrag 408); CDR timeout; destroy-leg C; cancel sub-expiry+overall; **clearCallExt** | *(cleared)* |
| `c-realigning` | C INVITE-resp 2xx (legState **confirmed**, src=cLegId) | `transfer-c-realign-200` (476-522) | ack C; cancel `refer_reinvite_answer-…-cLegId`; arm `refer_reinvite_answer`(leg "a"); **re-INVITE A with the SDP C answered on the c-realign re-INVITE** | `a-realigning` |
| `c-realigning` | C INVITE-resp 4xx–6xx (confirmed, src=cLegId) | `transfer-c-realign-fail` (526-568) | cancel `refer_reinvite_answer-cLegId`+overall; CDR reject; **begin-termination** (rollback) | *(slice not cleared; rollback)* |
| `c-realigning` | timer `refer_reinvite_answer` | `transfer-c-realign-timeout` (572-598) | cancel overall; CDR timeout; **begin-termination** | rollback |
| `a-realigning` | A INVITE-resp 2xx (src="a") | `transfer-a-realign-200` (620-654) | ack A; cancel `refer_reinvite_answer-…-a`+overall; **merge(a, cLegId)**; CDR answer; **clearCallExt** | *(cleared, transfer complete)* |
| `a-realigning` | A INVITE-resp 4xx–6xx (src="a") | `transfer-a-realign-fail` (658-690) | cancel `refer_reinvite_answer-a`+overall; CDR reject; **begin-termination** | rollback |
| `a-realigning` | timer `refer_reinvite_answer` (leg "a") | `transfer-a-realign-timeout` (694-720) | cancel overall; CDR timeout; **begin-termination** | rollback |
| `c-realigning`/`a-realigning` | INVITE from C (src=cLegId) | `transfer-c-glare-reinvite` (602-616) | 491 Request Pending | unchanged |
| `c-realigning`/`a-realigning` | INVITE from A | `transfer-a-glare-reinvite` (724-737) | 491 Request Pending | unchanged |
| `c-realigning`/`a-realigning` | non-BYE request from B (src=referrerLegId) | `transfer-b-in-cre-are-reject` (741-756) | 481 Call/Transaction Does Not Exist | unchanged |
| any of the 4 phases | timer `refer_overall_safety` | `transfer-overall-timeout` (760-794) | cancel sub-expiry + both `refer_reinvite_answer` ids; CDR timeout; **begin-termination** | rollback |
| any active | REFER from-b (phase-agnostic) | `transfer-reject-second-refer` (115-127) | 491 Request Pending | unchanged |

Note the two `refer-c-200` rules are distinguished only by `legState`: initial
INVITE → `trying|early`; c-realign re-INVITE → `confirmed`
(`referTransfer.ts:322-327, 484-487`). Same split on the fail rules. In Rust this
maps to `Match::response().leg_states(&[Trying, Early])` vs `.leg_states(&[Confirmed])`.

---

## 2. NOTIFY subscription lifecycle

REFER creates an implicit subscription (RFC 3515 §2.4); every phase milestone is
a NOTIFY on the **referrer B leg** (`legId = ext.referrerLegId`) with
`Event: refer` and a `message/sipfrag;version=2.0` body = a single SIP status
line built by `sipfrag_from_status(code, reason)` (RUST `sip-message::sipfrag`,
already ported; TS `SipFragUtils.sipfragFromStatus`).

Subscription-State header values (constants `referTransfer.ts:99-102`):
- `active;expires=60` — `SUB_STATE_ACTIVE_60`. Initial 100 + every 1xx NOTIFY.
- `terminated;reason=noresource` — final-success and HTTP-reject and C-fail.
- `terminated;reason=timeout` — sub-expiry timeout and C no-answer.

NOTIFY emission points and sipfrag body:

| Trigger | sub-state | sipfrag | source |
|---|---|---|---|
| REFER accepted (seed) | active;expires=60 | `100 Trying` | TransferRules.ts:163,190-196 |
| C 1xx (180/183/…), deduped | active;expires=60 | mirror C's `<status> <reason>` | referTransfer.ts:299-308 |
| C 200 OK (initial) | terminated;reason=noresource | `200 OK` | referTransfer.ts:342,355 |
| C final failure 3xx–6xx (486/603/…) | terminated;reason=noresource | mirror C's `<status> <reason>` | referTransfer.ts:407,413 |
| C no-answer | terminated;reason=timeout | `408 Request Timeout` | referTransfer.ts:455,461 |
| HTTP reject (403)/error | terminated;reason=noresource | reject_code/reason or `603 Declined`/`500` | referTransfer.ts:148-160 |
| sub-expiry timeout | terminated;reason=timeout | `500 Server Internal Error` | referTransfer.ts:260,267 |

**Dedup (cMultiple18x, refer-allow.ts:295-393):** `transfer-c-1xx-to-notify`
suppresses a 1xx whose status equals `ext.lastCLegNotifiedStatus`
(`referTransfer.ts:294-296`), updating that field on emit. So `180→183→180→180`
yields NOTIFYs for `180, 183, 180` and the trailing identical `180` is dropped.
Dedup is against the *last* status only — 183 clears the 180 dedup, so the second
180 re-emits. Each emitted NOTIFY also sets `lastCLegNotifiedStatus`.

**Subscription-expiry timer:** armed at seed for `referSubscriptionExpirySec`
(60s). Cancelled on C 200 / C fail / HTTP reject. Fires only in
`refer-authorizing` (HTTP hung) → NOTIFY 500 terminated;timeout. Note `expires=60`
on the active NOTIFYs is just the advertised value; the actual timer is the seed
timer. The 60s NOTIFY-expiry of the *active subscription itself* is not separately
re-armed — the C-progress NOTIFYs keep saying `expires=60` but the only real timer
is `refer_subscription_expiry` (and it is cancelled once C answers/fails).

Rust note: NOTIFY generation already fully supported by
`generate_in_dialog_request(InDialogMethod::Notify, …)` with `event` +
`subscription_state` + body (RUST `crates/sip-message/src/generators.rs:489-496`).
Need a `RuleAction::SendNotify { leg_id, event, subscription_state, content_type,
body }` and an executor arm (model on `executeSendNotify`, ActionExecutor.ts:2157).

---

## 3. C-leg lifecycle

- **Creation** (`transfer-http-allow`, referTransfer.ts:191-245): on HTTP allow,
  build the **held SDP** from A's INVITE snapshot — `extractCodecProfile(aLegInvite.body)`
  then `buildHeldSdpFromProfile(profile, {localIp, nowMs})` (preserves A's codec
  list, port 0, `a=inactive`). RUST equivalents `extract_codec_profile` +
  `build_held_sdp_from_profile` already exist (`crates/sip-message`, re-exported
  in lib.rs:22). `create-leg` toward `effectiveReferToUri` (Refer-To bare-URI, or
  `new_refer_to` from the allow payload), `fromInvite: "snapshot"`, `bodyUpdate:
  set(heldSdp)` (or `drop` if no profile), `ruri: set(effectiveReferTo)`, optional
  `headerUpdates`, optional `noAnswerTimeoutSec`, optional `callbackContext`.
  `cLegId = "b-" + (bLegs.length+1)` — anticipates the slot `create-leg` fills
  (TS:212; RUST `CreateLeg` already names the new leg `b-{len+1}`, actions.rs:138).
- **Driving:** the new C-leg INVITE/18x/200/ACK flow rides the *generic* CORE
  relay rules? **No** — the transfer service rules out-rank them by SERVICE_LAYER
  for the C-leg's INVITE responses (1xx/2xx/3xx in `c-ringing`). The transfer
  rules consume the C responses (NOTIFY + state) and DO NOT relay them to A. The
  ACK to C's 200 is `ack-leg cLegId` (the B2BUA is C's UAC). C never talks to A
  directly until merge.
- **No-answer timer:** id `no_answer-{callRef}-{cLegId}` — armed by `create-leg`'s
  `noAnswerTimeoutSec` (executor schedules `NoAnswer` timer with the leg id, RUST
  actions.rs:156-158). The `refer-allow-c-no-answer` scenario passes
  `no_answer_timeout_sec: 5`. `transfer-c-no-answer` matches `timer no_answer` with
  `ctx.event.legId === ext.cLegId` (referTransfer.ts:446-449). NB the default
  CORE `no-answer` rule (defaults.rs:370) ALSO matches `NoAnswer` — the transfer
  rule must out-rank it by SERVICE_LAYER + a phase/leg filter; CORE no-answer would
  `begin-termination` the whole call, which is wrong here (only C should die).
- **Teardown:** failure/no-answer destroy or terminate C and clear the slice,
  leaving A↔B intact (the refer-allow c486/c603/cNoAnswer scenarios then BYE A↔B
  normally). On C 200, C is confirmed+bridged and proceeds to realign.

---

## 4. Three-phase realignment

Goal: move A's media path from B to C, then bridge A↔C, then tear down B.

1. **c-realign** (`transfer-c-200-initial`, referTransfer.ts:316-384): when C
   answers its *initial* INVITE 200, capture `cInitialSdp = C's 200 body`, then
   immediately re-INVITE C carrying **A's SDP offer** (`aLegInvite.body`). This
   un-holds C: C now plays A's real media description. `send-reinvite cLegId
   bodyUpdate=set(aSdp)`. Phase → `c-realigning`. (The held SDP we sent on the
   initial INVITE kept C silent until A's offer is delivered.) Scenario assertion
   (`refer-c-realign.ts:120-128`): the re-INVITE to C carries A's exact SDP
   (`bodyEquals(aliceSdp)`), `CSeq.seq > 1`, Contact has `leg=b-2`.
2. **a-realign** (`transfer-c-realign-200`, referTransfer.ts:476-522): when C
   answers the c-realign re-INVITE 200 (legState confirmed), ack C, cancel C's
   `refer_reinvite_answer` and arm a fresh one on leg "a", then re-INVITE A
   carrying **the SDP C answered on the c-realign re-INVITE** (`ctx.event.message.body`,
   i.e. C's active sendrecv answer — NOT `cInitialSdp`, the held/inactive answer).
   The note at referTransfer.ts:495-497 is load-bearing: offering A the
   c-realign answer gives A C's real port/codec sendrecv; offering A the initial
   held answer would leave A inactive → one-way audio. Phase → `a-realigning`.
   Scenario predicate `isActiveAnswerFor(msg, aliceSdp)` (refer-c-realign.ts:28-37,
   refer-full-transfer.ts:26-35): the a-realign INVITE body must be an *answer*
   shape (kind=answer), echo A's offer nonce, contain `a=sendrecv`, and Contact
   has `leg=a`.
3. **merge** (`transfer-a-realign-200`, referTransfer.ts:620-654): when A answers
   the a-realign re-INVITE 200, ack A, cancel A's `refer_reinvite_answer` + overall,
   `merge(a, cLegId)` → A↔C bridged, CDR answer "transfer-completed", clearCallExt.
   B is now an orphan confirmed leg (no peer); a subsequent A BYE → begin-termination
   BYEs both B and C (refer-full-transfer.ts:124-129).

`isActiveAnswerFor` / offer-answer body shapes: the Rust harness lacks the TS
`classifySdp` helper. Implement the C/A re-INVITE-body assertions against the
exact SDP bytes the test supplies (`sdpAnswer(aliceSdp)` etc.) plus a substring
check for `a=sendrecv` / Contact `leg=a` — same approach `promote_pem.rs` uses
(byte-equality + header substring). See §9 DSL notes.

**Rollback** (any realign reject/timeout, or overall-safety): `begin-termination`
with no `clearCallExt` — the existing CORE termination path BYEs **all three**
confirmed legs (A, B, C) and the slice is dropped as the call terminates. The
scenarios expect three BYEs (refer-c-realign.ts:217-223, refer-full-transfer.ts:196-202).

---

## 5. Gating regimes

Two regimes keyed by phase (refer-gating.ts header comment, lines 1-16):

**Regime 1 — transparency** (`refer-authorizing`, `c-ringing`): no transfer rule
gates in-dialog A↔B traffic. A re-INVITE / A INFO / B INFO relay end-to-end via
the normal CORE relay rules (`relay-reinvite`, `relay-info`). The transfer slice
is present but its request-matching rules only fire in the realigning phases, so
they decline and CORE relays. Covered by refer-gating cases 1,2,4,5,6.
- Case 1: A re-INVITE during `refer-authorizing` → relay to B (HTTP hung).
- Case 2: A re-INVITE during `c-ringing` → relay to B.
- Case 4: A INFO during `refer-authorizing` → relay to B.
- Case 5: A INFO during `c-ringing` → relay to B.
- Case 6: B INFO during `refer-authorizing` → relay to A.

**Regime 2 — rejection** (`c-realigning`, `a-realigning`): the realign re-INVITE
exchanges are in flight, so foreign INVITEs glare and B's signaling is dead:
- A re-INVITE (either realign phase) → **491** (`transfer-a-glare-reinvite`,
  filter `isRealigning(phase)`, referTransfer.ts:724-737). Covered by gating
  case 3 (c-realigning) and refer-full-transfer.ts case 3 (a-realigning).
- C re-INVITE (either realign phase, src=cLegId) → **491**
  (`transfer-c-glare-reinvite`, referTransfer.ts:602-616). Covered by
  refer-c-realign.ts case 4.
- B non-BYE request (either realign phase, src=referrerLegId) → **481**
  (`transfer-b-in-cre-are-reject`, referTransfer.ts:741-756; `method !== "BYE"`).
  Covered by refer-c-realign.ts case 5 (B INFO during c-realigning → 481). B's BYE
  is allowed through (relay-bye) so the referrer can hang up.

**Second REFER, any phase** → **491** (`transfer-reject-second-refer`,
referTransfer.ts:115-127; phase-agnostic, guard = slice present). Covered by
refer-gating cases 7 (c-ringing) and 8 (c-realigning), and refer-reject case 5
(refer-authorizing). Exception: a second REFER carrying Replaces → 501 via the
seed rule `transfer-reject-replaces` which `overrides` `transfer-reject-second-refer`
(TransferRules.ts:86-104). In Rust, `overrides` is layer-agnostic and supported by
`pick_ranked` (executor.rs:25-29) — give `transfer-reject-replaces` an
`overrides: &["transfer-reject-second-refer"]`.

Gating-rule ranking note: the regime-2 reject rules (INVITE/request matchers) and
`transfer-reject-second-refer` must out-rank CORE `relay-reinvite`/`relay-info`/
`relay-non-invite-200`/`relay-message`. SERVICE_LAYER gives that automatically;
filters keep them inert in regime-1 phases.

---

## 6. Reject paths (refer-reject.ts, 5 cases)

| # | Trigger | Mechanism | Result | TS |
|---|---|---|---|---|
| 1 | REFER + X-Api-Call `refer-reject-403` | HTTP allow→reject 403; `transfer-http-reject` | 202; NOTIFY 100 active; NOTIFY 403 terminated;noresource; A↔B intact | referTransfer.ts:131-169 |
| 2 | REFER + `refer-http-timeout` (HTTP hangs) | sub-expiry fires at 60s; `transfer-http-timeout` | 202; NOTIFY 100 active; +60s → NOTIFY 500 terminated;timeout | referTransfer.ts:249-275 |
| 3 | Refer-To with `Replaces=` param | `transfer-reject-replaces` (seed, alwaysActive, overrides second-refer) | REFER → **501** Not Implemented (no subscription) | TransferRules.ts:86-104 |
| 4 | Out-of-dialog REFER (unknown Call-ID/bogus to-tag) | resolved by router BEFORE rules: dialog lookup fails | **481** (RUST `maybe_reject_orphan`, router.rs:363) | n/a (router pre-rules) |
| 5 | Second REFER while refer-authorizing | `transfer-reject-second-refer` | **491** Request Pending; first REFER later resolves at sub-expiry | referTransfer.ts:115-127 |

Reject code/reason mapping for case 1 (`transfer-http-reject`,
referTransfer.ts:144-152): if outcome=`reject` and payload has numeric
`reject_code` use it, else 603; reason from `reject_reason` else "Declined";
outcome=`error` → 500/"Server Internal Error". The 403 is carried in the **NOTIFY
sipfrag body**, never as a 4xx on the REFER (the REFER is always 202'd — see
`responses.ts` `CallReferRejectResponse` doc, lines 238-260).

Also `transfer-reject-a-leg-refer` (TransferRules.ts:109-125): a REFER from-a →
501. Not directly scenario-tested but must be ported (it is a seed rule).

Case 4 is already handled by the existing Rust router orphan path — verify a
REFER with a bogus to-tag whose call_ref does not resolve hits `maybe_reject_orphan`
and gets 481. No new rule needed.

---

## 7. Timers (4)

Defaults live in TS `AppConfig` (`referSubscriptionExpirySec=60`,
`referReinviteAnswerSec=32`, `referOverallSafetySec=120`; `no_answer_timeout` is
per-allow `no_answer_timeout_sec`, no fixed REFER default — the C no-answer uses
the same `NoAnswer` timer the create-leg arms). The Rust `TimerType` enum already
carries `ReferSubscriptionExpiry`, `ReferReinviteAnswer`, `ReferOverallSafety`
(`crates/call/src/model.rs:258-263`) and `NoAnswer`. **`B2buaConfig` has no refer
timer fields yet** — add `refer_subscription_expiry_sec` (60),
`refer_reinvite_answer_sec` (32), `refer_overall_safety_sec` (120) and a way for a
scenario to override them (the timers scenario needs `reinvite_answer=600`,
`overall=10`; see below).

Timer id format (TS `schedule-timer` → `${timerType}-${callRef}[-${legId}]`,
ActionExecutor.ts:1942). RUST `schedule` builds ids the same way via
`TimerEntry`. The cancel actions name the id string explicitly. **The RUST id
convention differs**: the existing CORE rules cancel `NoAnswer` as
`format!("NoAnswer:{b}")` (defaults.rs:179, 260) — note the `:` + leg, not the TS
`no_answer-{callRef}-{leg}`. The transfer port must use whatever
`ActionExecutor::schedule` actually emits (verify the timer-id scheme in
`crates/b2bua/src/rules/actions.rs::schedule` and `crates/b2bua/src/timers.rs`);
match the cancel-timer ids to that scheme, NOT the TS string. Safer: add a helper
that mints the id from `(TimerType, callRef, Option<leg>)` and use it on both the
schedule and cancel sides so they can never drift (CLAUDE.md timer-aliasing hazard).

| Timer | TimerType | Armed by | Cancelled by | Fires → |
|---|---|---|---|---|
| subscription-expiry (60s) | `ReferSubscriptionExpiry` | seed (`transfer-intercept-refer`) | C 200-initial, C fail-initial, C no-answer, HTTP reject, overall-timeout | `transfer-http-timeout` (only in refer-authorizing): NOTIFY 500 term;timeout, cancel overall, clear |
| no-answer (per-allow) | `NoAnswer` (leg=cLegId) | `create-leg` (`no_answer_timeout_sec`) | C 200-initial, C fail-initial | `transfer-c-no-answer` (c-ringing, leg=cLegId): NOTIFY 408 term;timeout, destroy C, clear |
| reinvite-answer (32s) | `ReferReinviteAnswer` | C 200-initial (leg cLegId), then c-realign-200 (leg "a") | c-realign-200 (cancels C's), a-realign-200 (cancels A's), both fail rules, overall-timeout | `transfer-c-realign-timeout` (c-realigning) OR `transfer-a-realign-timeout` (a-realigning, leg "a") → rollback |
| overall-safety (120s) | `ReferOverallSafety` | seed | every terminal rule (allow→c-200 keeps it; cleared on a-realign-200, all fail/reject rules, http-reject/timeout, c-fail, c-no-answer) | `transfer-overall-timeout` (any of 4 phases): cancel sub-expiry + both reinvite-answer ids, CDR, begin-termination |

`transfer-c-realign-timeout` and `transfer-a-realign-timeout` share `TimerType
ReferReinviteAnswer`; their `phase` filters (`c-realigning` vs `a-realigning`,
plus `event.legId === "a"` on the a-variant) keep them mutually exclusive
(referTransfer.ts:697-705). In Rust, both rules match `Match::timer().timer_type(
ReferReinviteAnswer)`; the filter discriminates on `transfer_phase(ctx)` and (for
the a-variant) the fired timer's `leg_id`. `CallEvent::Timer` carries `leg_id`
(event.rs:21-25) — the filter reads it.

**`skipFinalSweep` exemptions** (preserve as Rust-side comments / not-ported
justifications; these are TS-harness 24h-sweep races, NOT behaviour):
- `refer-allow-c-realign-c-timeout` (refer-c-realign.ts:298): pending c-realign
  re-INVITE Timer B (32s) interleaves with 3 BYE 200s in the fake-clock queue.
- `refer-allow-full-a-bye-during-a-realign` (refer-full-transfer.ts:361): A's
  outstanding a-realign re-INVITE retransmits during the sweep.
- `refer-allow-full-a-reinvite-timeout` (refer-full-transfer.ts:442): same.
- `refer-overall-safety-fires` (refer-timers.ts:118): same as c-realign-c-timeout.

In the Rust harness (which advances `tokio::time` in 100 ms chunks and drives the
protocol between advances, CLAUDE.md), these races may not reproduce — drive the
exact deadline, let the BYEs land between advances, and tolerate retransmits with
the equivalent of `allowExtra`. If the Rust harness has no end-of-scenario sweep,
the exemptions may be unneeded; if a retransmit/Timer-B race surfaces, replicate
the toleration (do NOT relax an assertion without the same justification).

---

## 8. `call_refer` decision adapter

**Contract** (TS `requests.ts:57-64` / `responses.ts:219-262`):

Request (`CallReferRequest`): `call_id` (A-leg Call-ID), `dialog_id`
(`Call-ID;to-tag=…;from-tag=…` from B's perspective), `callback_context?`,
`refer_to`, `referred_by?`, `sip_headers` (non-structural REFER headers, incl.
`X-Api-Call`). Built by `transfer-intercept-refer` (TransferRules.ts:199-206).

Response (`CallReferResponse`):
- `allow`: `destination {host,port?,transport?}`, `new_refer_to?`,
  `update_headers?`, `no_answer_timeout_sec?`, `call_limiter?`, `callback_context?`,
  `relay_first_18x_to_180?`, `features?` (synthesized).
- `reject`: `reject_code?`, `reject_reason?` (carried in the NOTIFY sipfrag, NOT
  the REFER).

**RUST current state / gap** (`crates/b2bua/src/decision/`):
- `CallReferRequest` (schemas.rs:127-131) currently carries only `callback_context`
  + `refer_to`. **Gap:** missing `call_id`, `dialog_id`, `referred_by`,
  `sip_headers`. The scripted adapter and the seed rule need `sip_headers` (to read
  `X-Api-Call`) at minimum; add `call_id`, `dialog_id`, `referred_by`, `sip_headers`
  to match TS.
- `CallReferResponse` (schemas.rs:133-137) is `Allow { destination }` /
  `Reject { code, reason }`. **Gap:** `Allow` is missing `new_refer_to`,
  `update_headers`, `no_answer_timeout_sec`, `callback_context`, `features`. Add
  them (mirror `RouteDecision`; the C-leg reuses route levers).
- `ScriptedDecisionEngine::call_refer` (test_adapter.rs:153-162) is a stub that
  always rejects 501. **Gap:** implement scripted `call_refer` driven by the
  request `sip_headers["X-Api-Call"]` JSON, mirroring `mockCallReferBehavior`
  (MockServer.ts:192-244):
  - `refer-reject-403` → reject 403/"Forbidden".
  - `refer-http-500` → HTTP-500-equivalent → maps to `transfer-http-reject`
    outcome=`error` → 500. In Rust return `Err(CallDecisionError)` or a
    distinguished reject; the seed→internal-event path must classify it as `error`.
  - `refer-http-timeout` → never resolves (the HTTP hangs). In Rust: the scripted
    adapter must **not** produce a `refer-http-result` event at all, so the
    sub-expiry timer is what fires. Implement by simply NOT firing the re-entry
    for this key (or sleeping past test horizon). See §9 fire-and-forget wiring.
  - `refer-allow-c` → allow with `destination` (default 127.0.0.1:5667),
    optional `new_refer_to`/`update_headers`/`no_answer_timeout_sec`/`callback_context`.
  - default (no X-Api-Call) → reject 603/"Declined".

**Driving from a test:** the Rust scenarios use a `ScriptedDecisionEngine` whose
`call_refer` branch reads the REFER's `X-Api-Call` header value (the harness puts
it on the REFER via a `with_header("X-Api-Call", json)` builder). The B2buaSut
needs a constructor that wires a transfer-capable scripted engine (a
`B2buaSut::route_all_with_refer(...)` or a builder `on_refer(closure)` — mirror
`route_all_to_with_18x`, b2bua-harness/src/lib.rs:103-124). Because the scripted
adapter is in-process (no HTTP), `refer-http-500`/`refer-http-timeout` are modelled
by the adapter return value + the fire-and-forget wiring, not a MockServer.

---

## 9. Rust mapping plan

### 9.1 New `RuleAction` variants (`crates/b2bua/src/rules/model.rs`)
Add to the `RuleAction` enum (model on the TS actions; executor arms model on the
cited ActionExecutor functions):
- `SendNotify { leg_id: String, event: String, subscription_state: String,
  content_type: Option<String>, body: Vec<u8> }` — executor: build NOTIFY on the
  leg's confirmed dialog via `generate_in_dialog_request(InDialogMethod::Notify,…)`
  with `event`/`subscription_state`/body, apply b-leg egress, push outbound
  (ActionExecutor.ts:2157-2196). The B leg dialog must exist (it is confirmed).
- `ReferAsyncHttp { request: CallReferRequest }` — executor: push
  `FireAndForgetEffect::ReferAsyncHttp { call_ref, request: <json> }` (effects.rs:69
  already exists; the executor currently has no arm). The request must serialize to
  the JSON the interpreter reposts.
- `SetTransfer { state: Option<TransferState> }` — executor: `call.transfer = state`
  (mirror `SetPromotePem`, actions.rs:275-277).
- **Re-use existing `CreateLeg`** but it must gain a body override + header updates:
  current `CreateLeg` (model.rs:293-298, actions.rs:132-159) has NO body/header
  fields. **Gap:** add `body_update: BodyUpdate` (or `held_sdp: Option<Vec<u8>>`)
  + `header_updates` so the held SDP and `update_headers` reach the C INVITE.
  `relay::build_b_leg` currently clones the a-leg INVITE body; thread the override
  through it.
- **Re-use existing `SendReinvite`** (model.rs:354-358, actions.rs:268-345). It
  already originates a re-INVITE on a leg with a body + extra headers, bumps the
  dialog CSeq, caches the INVITE handle, and classifies the response from-a via
  stamped Via cr/lg. For c-realign it targets `cLegId` (a b-leg) — confirm
  `send_reinvite` works for b-legs (it uses `leg_index`/`dialog_identity_tag`
  generically, and `apply_b_leg_egress`, so it should; the response then arrives
  direction from-b which the c-realign-200 rule matches). For a-realign it targets
  `"a"`. No `add_headers` needed for transfer (TS passes none), so pass `&[]`.
- `Respond` already exists (used for 491/481/501/202).

### 9.2 Per-call transfer state slice (`crates/call/src/model.rs` + `helpers.rs`)
Add `pub transfer: Option<TransferState>` to `Call` (after `promote_pem`,
model.rs:458) and a struct mirroring `TransferCallExt` (referTransfer.ts:56-85):

```rust
pub struct TransferState {
    pub phase: TransferPhase,            // enum: ReferAuthorizing|CRinging|CRealigning|ARealigning
    pub referrer_leg_id: String,
    pub refer_to_uri: String,
    pub effective_refer_to_uri: Option<String>,
    pub callback_context: Option<String>,
    pub c_leg_id: Option<String>,
    pub refer_cseq: Option<u32>,
    pub started_at_ms: i64,
    pub last_c_leg_notified_status: Option<u16>,
    #[serde(with = "serde_bytes")] pub c_initial_sdp: Option<Vec<u8>>, // or Vec<u8>
}
```
Add `Serialize/Deserialize/Clone/Debug/PartialEq` (the Call derives them). Add
`call::helpers`: `transfer_state(call) -> Option<&TransferState>`,
`transfer_phase(call) -> Option<TransferPhase>`, `set_transfer(call, state)`,
`transfer_active(call) -> bool`. Mirror `promote_pem_state`/`set_promote_pem`
(helpers.rs:372-387).

### 9.3 `refer-async-http` re-entry wiring (`crates/b2bua/src/router.rs`)
`process_result` currently drops `fire_and_forget` (router.rs:461 "deferred").
Implement the loop:
- `FireAndForgetEffect::ReferAsyncHttp { call_ref, request }`: `tokio::spawn` a
  task holding a cloned `Arc<RouterCtx>` that (a) deserializes the request, (b)
  calls `ctx.decision.call_refer(req).await`, (c) maps the result to a
  `CallEvent::InternalEvent { call_ref, topic: "refer-http-result", outcome:
  "allow"|"reject"|"error", payload: <json> }`, (d) re-enters via
  `on_event(&ctx, ev).await`. For the `refer-http-timeout` key the adapter never
  resolves → spawn nothing (or never produce the event); the sub-expiry timer
  fires instead. Make `on_event` reachable (it is private but in the same module).
- `FireAndForgetEffect::Reenter(ev)`: `on_event(&ctx, *ev).await` (general path).

The payload must carry what `transfer-http-allow`/`transfer-http-reject` read:
allow→`{destination, new_refer_to?, update_headers?, no_answer_timeout_sec?,
callback_context?}`; reject→`{reject_code?, reject_reason?}`. The Rust rules read
these out of `CallEvent::InternalEvent.payload` (serde_json::Value) inside the
handler — add a `RuleContext` accessor or read `ctx.event` directly.

The `Match` model already supports `topic`/`outcome` columns
(model.rs:215-224); add builder helpers `Match::internal_event().topic(t)
.outcome(o)` (the enum variant `MatchKind::InternalEvent` exists; only the
constructor/builder are missing — `Match` has no `internal_event()` ctor yet,
and `outcome` accepts a single `&'static str` so the reject/error pair needs
two rules or a filter). The TS `transfer-http-reject` matches `outcome:
["reject","error"]`; in Rust either register two rules or match
`internal_event().topic("refer-http-result")` with a filter that checks the
outcome ∈ {reject,error}.

### 9.4 REFER in the generator / harness DSL
- **REFER is in-dialog**, sent by B; the B2BUA does not *generate* a REFER (it
  receives one and 202s it). So `InDialogMethod` (generators.rs:69-90) needs a
  `Refer` variant ONLY for the **harness** to send REFER from an agent. Add
  `InDialogMethod::Refer => "REFER"` and let `generate_in_dialog_request` pass it
  through (no special headers; Refer-To/Replaces ride via `extra_headers`). The
  B2BUA never builds a REFER itself.
- **NOTIFY** generation already exists end-to-end (generators.rs:489-496). No
  generator change for NOTIFY.
- **DSL extensions** (`crates/scenario-harness/src/agent.rs`):
  - REFER send: `Dialog::send_request(InDialogMethod::Refer)` then
    `.with_header("Refer-To", uri)` / `.with_header("X-Api-Call", json)` /
    `.with_header("Referred-By", …)` — the existing `InDialogRequest` builder
    (agent.rs:635-716) + `with_header` already supports arbitrary headers, so
    only the `InDialogMethod::Refer` enum value is strictly required. Optionally
    add a `with_refer_to(uri)` convenience.
  - NOTIFY expect: the agent already does `receive("NOTIFY")` →
    `ServerTxn { request }`; assert via `get_header(&req.headers,
    "subscription-state")` (`starts_with("active"|"terminated")`),
    `get_header(_, "event") == "refer"`, and a sipfrag body substring check on
    `req.body` (e.g. contains `SIP/2.0 100 Trying`). `ServerTxn.respond(200,"OK")`
    sends the NOTIFY 200. No new predicate plumbing needed — `ServerTxn.request()`
    exposes the parsed request (agent.rs:745).
  - The C agent is just a third `h.agent("charlie", …)` (the harness already
    supports N agents; `promote_pem`/`prack_forking` use multiple). C plays UAS for
    the initial INVITE + the c-realign re-INVITE (`charlie.receive("INVITE")` then
    `ServerTxn.respond`), and UAS for the BYE.
  - c-realign / a-realign re-INVITEs are *received* by C and A respectively
    (`charlie.receive("INVITE")`, `alice.receive("INVITE")`); the agent responds
    200 + receives the B2BUA's ACK. Offer/answer assertions: byte-compare against
    the supplied SDP + substring `a=sendrecv` + Contact `leg=a`/`leg=b-2` (no
    `classifySdp` port needed — assert what the scenario already knows it sent).
- **Per-scenario timer override** (refer-timers.ts: `reinvite_answer=600`,
  `overall=10`): add the three refer timer fields to `B2buaConfig` and a
  `B2buaSut` constructor that overrides them, OR carry them as a route/refer
  feature so the allow response can tune them. Simplest: `B2buaConfig` fields +
  a SUT builder param (mirror `start_with_outbound_proxy`).

### 9.5 Which CORE/relay rules the transfer rules must beat
SERVICE_LAYER ranking handles all of these automatically (pick_ranked sorts layer
desc), with filters keeping the transfer rules inert when the slice is absent or
the phase is wrong. The collisions:
- C INVITE 1xx/2xx/3xx (from-b, c-ringing): transfer rules beat
  `relay-provisional`, `confirm-dialog`, `route-failure` (defaults.rs:141-234).
  Critically, the transfer rules must NOT relay C's responses to A and must NOT
  merge A↔C at C's initial 200 (only at a-realign). `confirm-dialog` would
  wrongly merge A↔C and relay the 200 to A — the SERVICE_LAYER `transfer-c-200-initial`
  pre-empts it.
- C/A re-INVITE responses in realigning: beat `relay-reinvite-response`
  (defaults.rs:126-139) and `relay-provisional`. The transfer rules consume the
  realign 2xx/failures explicitly (no relay). The realign re-INVITEs are
  B2BUA-originated (`SendReinvite`) so they leave NO pending-relay snapshot →
  `relay-reinvite-response`'s filter (find_pending_request) declines anyway, but
  the SERVICE_LAYER rule is the authoritative claimant.
- Glare/gating requests: beat `relay-reinvite`, `relay-info`, `relay-message`,
  `relay-non-invite-200` (defaults.rs). Filters gate by phase.
- C no-answer timer: beat CORE `no-answer` (defaults.rs:370) — else the whole
  call dies. SERVICE_LAYER + phase/leg filter.
- Second REFER: beat the relay path. `transfer-intercept-refer` (seed, CORE-equiv
  alwaysActive) only matches when `noTransferActive` so it declines once the slice
  exists; `transfer-reject-second-refer` (SERVICE_LAYER) then claims it.
- `transfer-reject-replaces` uses `overrides: ["transfer-reject-second-refer"]`
  (layer-agnostic) so a Replaces REFER → 501 even mid-transfer.

Seed rules (`transfer-intercept-refer`, `transfer-reject-replaces`,
`transfer-reject-a-leg-refer`) are NOT slice-gated (they must run before the slice
exists). Register them as CORE_LAYER `alwaysActive`-equivalent rules in
`default_rules()` (alongside the relay rules), gated by their match columns + a
`no_transfer_active` filter where TS uses `noTransferActive`. Append the
phase-gated transfer service rules after `promote_pem_rules()` in `default_rules()`
(defaults.rs:49-54), as a new `transfer::transfer_rules()` module returning
SERVICE_LAYER rules — exactly the `promote_pem`/`relay_first_18x` pattern.

### 9.6 B2BUA failover (Slice 3 blocker) — call-out
REFER C-leg creation is a normal new-leg origination, NOT a failover. None of the
6 scenario files exercise B2BUA replica failover. The one place to watch: the
realign re-INVITEs and the merge must survive the `RUST` invariant enforcer
(`invariants::enforce`) — `begin-termination` rollback already does. No failover
dependency. The `topology`/replication flush in `process_result` is gated on
`topology.bak` non-empty (router.rs:393-401), which the test harness leaves empty,
so the transfer state slice rides the normal in-memory `state.update` path.

---

## 10. Suggested implementation order (commit-sized steps)

Foundation (one commit, no scenarios green yet — it unblocks everything):
- **F0 — plumbing.** Add `TransferState`/`TransferPhase` to `call`, the
  `call::helpers` accessors, the `RuleAction` variants (`SendNotify`,
  `ReferAsyncHttp`, `SetTransfer`) + executor arms, `CreateLeg` body/header
  override, `Match::internal_event()` builder, `InDialogMethod::Refer`, the three
  `B2buaConfig` refer-timer fields, the `CallReferRequest`/`CallReferResponse`
  field gaps, the scripted `call_refer` (X-Api-Call), and the `fire_and_forget`
  interpreter (`ReferAsyncHttp` → `call_refer` → re-enter; `Reenter`). Plus the
  seed rules (`transfer-intercept-refer`, `transfer-reject-replaces`,
  `transfer-reject-a-leg-refer`) and the empty `transfer_rules()` module wired into
  `default_rules()`. Keep the whole existing suite green.

Then port the 6 families one file at a time, one scenario at a time
(plan §"Tests written & run one at a time"):

1. **refer-reject.rs** (refer-reject.ts, 5) — smallest happy-to-reach surface;
   exercises seed + HTTP reject/timeout + Replaces(501) + out-of-dialog(481) +
   second-REFER(491). Needs: seed rule, `transfer-http-reject`,
   `transfer-http-timeout`, `transfer-reject-replaces`, `transfer-reject-second-refer`,
   sub-expiry timer, NOTIFY emission, fire-and-forget reject/error mapping. Commit.
2. **refer-allow.rs** (refer-allow.ts, 5) — adds `transfer-http-allow` (held-SDP
   create-leg), `transfer-c-1xx-to-notify` (+dedup), `transfer-c-200-initial`
   (through the *first* re-INVITE C — the scenarios `allowExtra("INVITE")` and
   don't drive the realign), `transfer-c-fail-initial`, `transfer-c-no-answer`,
   no-answer timer. Commit.
3. **refer-c-realign.rs** (refer-c-realign.ts, 5) — DONE (slice 5b). Adds
   `transfer-c-realign-200` (re-INVITE A with C's active answer),
   `transfer-c-realign-fail`, `transfer-c-realign-timeout`,
   `transfer-c-glare-reinvite`, `transfer-b-in-cre-are-reject`, the
   `refer_reinvite_answer` timer + the `isActiveAnswerFor` assertion (Rust:
   byte-equality of the a-realign body to C's active answer + Contact `leg=a`
   substring — no `classifySdp` port). The Rust ports STOP once the a-realign
   re-INVITE fires (the a-realign 200 / merge are slice 5c), so the TS post-merge
   teardown is not driven. The TS `.skipFinalSweep()` exemption applies only to
   the CTimeout case; the Rust harness has no end-of-scenario sweep, so the
   toleration is the `receive_tolerating(BYE, [INVITE,CANCEL,OPTIONS])` retransmit
   drain at the 32s deadline (one advance crosses both the `refer_reinvite_answer`
   watchdog and the INVITE Timer B). Commit.
4. **refer-full-transfer.rs** (refer-full-transfer.ts, 5) — adds
   `transfer-a-realign-200` (merge), `transfer-a-realign-fail`,
   `transfer-a-realign-timeout`, `transfer-a-glare-reinvite`. Completes the FSM.
   Two `.skipFinalSweep()` cases. Commit.
5. **refer-gating.rs** (refer-gating.ts, 8) — pure verification; should pass once
   3+4 land (regime-1 transparency is just CORE relay; regime-2 + second-REFER are
   the rules already ported). Mostly new test code, few/no rule changes. Commit.
6. **refer-timers.rs** (refer-timers.ts, 1) — `transfer-overall-timeout`; needs the
   per-scenario timer override (`reinvite_answer=600`, `overall=10`).
   `.skipFinalSweep()`. Commit.

After each family: `source ~/.cargo/env && cargo test --workspace` green; then the
next. Final: update `MIGRATION_STATUS.md` (source SHA `fffc4ac6`, mark rule-engine
+ scenario rows; list any un-ported test with justification — e.g.
`transfer-reject-a-leg-refer` has no dedicated scenario but is ported as a rule),
amend ADR-0010 (SERVICE_LAYER implemented), add a REFER service-shape ADR noting
the typed-slice-instead-of-defineService divergence.

---

## Appendix — key TS snippets (load-bearing)

Seed (TransferRules.ts:176-206): `respond 202` → `set-call-ext(seed)` →
`schedule refer_subscription_expiry` + `refer_overall_safety` → `send-notify 100
active` → `refer-async-http { call_id, dialog_id, refer_to, referred_by?,
sip_headers }`.

Held SDP (referTransfer.ts:199-207): `extractCodecProfile(aLegInvite.body)` →
`buildHeldSdpFromProfile(profile,{localIp,nowMs})`; if no profile → drop body.

c-realign offer (referTransfer.ts:339-374): re-INVITE C with
`aSdp = ctx.call.aLegInvite.body`; capture `cInitialSdp = C's 200 body`.

a-realign offer (referTransfer.ts:495-518): re-INVITE A with
`cRealignSdp = ctx.event.message.body` (C's c-realign 200 body, sendrecv) — **not**
`cInitialSdp`. This is the one-way-audio guard.

merge (referTransfer.ts:643): `{ type:"merge", legA:"a", legB:cLegId }` +
`clearCallExt`.

Dedup (referTransfer.ts:294-296,310): skip NOTIFY when
`ext.lastCLegNotifiedStatus === resp.status`; else emit + set the field.

Glare/gating (referTransfer.ts:602-616, 724-737, 741-756): 491 (C/A INVITE in
realigning), 481 (B non-BYE in realigning).
