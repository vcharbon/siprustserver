# B-leg failover (`/call/failure`) — implementation-ready design note

Source (TS, pinned): `portsource/sipjsserver` @ `fffc4ac6`,
`src/b2bua/rules/defaults/FailureRules.ts`. Goal: wire the dormant
`CallDecisionEngine::call_failure` into the rule engine so a b-leg final-failure
response or a no-answer timeout can fail over to a new destination, then port the
4 blocked scenarios (`suppress-18x` `failoverNoAnswer`/`failoverReject`,
`fake-prack` `forking`/`failover`).

## The mechanism we reuse (REFER's fire-and-forget + re-entry)

`call_failure` is async (HTTP); rule `handle` closures are sync `fn`. So we copy
the REFER pattern exactly (router.rs ~L475, `FireAndForgetEffect::ReferAsyncHttp`):

1. A **seed** rule (sync) emits a `FailureAsyncHttp { request }` fire-and-forget
   effect carrying the `/call/failure` request JSON plus `failed_leg_id`.
2. The router interpreter spawns `decision.call_failure(...)`, maps the result to
   an `InternalEvent { topic: "call-failure-result", outcome: "failover"|"terminate", payload }`,
   and re-enters via `reentry_tx` (single-threaded, breaks the non-Send cycle).
3. A **resolution** rule matches `call-failure-result` and emits the real actions
   (`CreateLeg` on failover, relay+terminate / begin-termination otherwise).

## Faithful TS behaviour

### route-failure (b-leg 3xx-6xx response)
Sync part: `add-cdr-event reject` + `terminate-leg (rejected)` on the failed leg.
Then, **only if `call.callback_context` is set**, call `/call/failure`
(`origin: external`, `sip_code`, `sip_reason`); on `failover` → cancel the failed
leg's `no-answer` timer + `create-leg` toward the new destination (snapshot of A's
INVITE, optional header/RURI/no-answer/callback overrides). Otherwise relay the
failure to A + begin-termination. When `callback_context` is **None** the rule
keeps today's behaviour (relay + `TerminateCall`) synchronously — no async hop.

### no-answer-failover (no-answer timer)
Sync part: `add-cdr-event timeout` + `destroy-leg`. Then if `callback_context` is
set, call `/call/failure` (`origin: no_answer_timeout`, no code); on `failover` →
`create-leg`. Otherwise `begin-termination`. None → today's behaviour
(begin-termination) synchronously.

## Why deferring the relay is correct (not just convenient)
`failoverReject`: bob1 sends 503; alice must NOT see it (she still holds the bare
180) and only sees 200 when bob2 answers. So we cannot relay the failure until we
know the decision is *terminate*. The async hop makes this natural.

## Rust mapping

### New `RuleAction`s (`rules/model.rs`)
- `FailureAsyncHttp { request: serde_json::Value }` → pushes
  `FireAndForgetEffect::FailureAsyncHttp { call_ref, request }`.
- `RelayFailureToALeg { status, reason }` → synthesizes a final failure response
  on the a-leg INVITE server txn (terminate-after-callback path; reuses
  `relay::response_to_a_leg` with the a-dialog tag). Untested by the 4 cases but
  required for faithfulness.

### New effect (`effects.rs`)
`FireAndForgetEffect::FailureAsyncHttp { call_ref, request }`.

### Router (`router.rs`)
Spawn `decision.call_failure(parse_call_failure_request(request))`; on
`Ok(Failover(route))` emit outcome `failover` with payload
`{destination, new_ruri, update_headers, no_answer_timeout_sec, callback_context, failed_leg_id}`;
on `Ok(Terminate)`/`Err` emit outcome `terminate` with payload
`{status, reason, failed_leg_id}` (status/reason echoed from the request for the
response path; absent for the no-answer path).

### Rules (`rules/defaults.rs`)
- `route-failure` (modify): split into the sync CDR+terminate-leg + (callback) the
  async kick, else current relay+terminate.
- `no-answer` (modify): sync CDR+destroy + (callback) async kick, else current
  begin-termination.
- `failover-create-leg` (new, CORE): match `internal_event` topic
  `call-failure-result` outcome `failover` → `cancel-timer NoAnswer:<failed_leg_id>`
  + `CreateLeg`.
- `failover-terminate` (new, CORE): match outcome `terminate`. If the payload
  carries a `status` (response path) → `RelayFailureToALeg` + `begin-termination`;
  else (no-answer path) → `begin-termination`.

### Tag continuity across failover (the property the 4 cases assert)
The failover must NOT clear `call.relay_first_18x`. The first 180 (bob1) stored the
a-facing tag (`stored_a_tag`) and set `first_relayed`. `CreateLeg` does not touch
features or the relay slice, so: bob2's 18x is suppressed (`first_relayed` true) and
bob2's 200 reuses `stored_a_tag` via `force-tag-consistency`. The call-level
`features.relay_first_18x_to_180` already activates the new leg — no re-application
needed. This is exactly why these 4 cases test failover.

### Scripted adapter + harness
- `test_adapter`: add `failover_route_to(...)` helper building
  `CallFailureResponse::Failover(RouteDecision{...})`, and a builder so the SUT can
  route the first call to bob1 (with `callback_context` set) and fail over to bob2.
- `B2buaSut::route_all_to_with_18x_failover(...)`: initial route → bob1 (18x +
  callback_context), `on_failure` → `Failover` to bob2 (new_ruri + 18x).

## Cases
1. `suppress-18x failoverNoAnswer` — no-answer timeout on bob1(180) → CANCEL bob1,
   failover to bob2(new_ruri); bob2 200 To-tag == bob1 180 To-tag (delayed offer).
2. `suppress-18x failoverReject` — bob1 180 then 503 → failover bob2; To-tag cont.
3. `fake-prack forking` — bob1 183(100rel)+PRACK+503 → failover bob2 (unreliable);
   alice 200 carries bob2 SDP (b1 cache discarded). B2BUA-driven failover (confirmed).
4. `fake-prack failover` — same shape as 3, separate trace.
