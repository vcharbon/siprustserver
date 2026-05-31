# Slice 3 — `relayFirst18xTo180` (suppress-18x + fake-prack) design note

TS source pinned at `portsource/sipjsserver` SHA `fffc4ac6`.
Rust under `crates/`. Branch `feat/sip-message-layer`.

## What the TS rule does

`src/b2bua/rules/custom/relayFirst18xTo180.ts` is a SERVICE_LAYER PolicyModule
activated by the per-call ext slice `relayFirst18x` (seeded in `applyRoute` from
the decision `relay_first_18x_to_180` feature). Strategy enum on the wire:

| wire value          | strategy (translate.ts) | this slice |
|---------------------|-------------------------|------------|
| `true`              | `drop-sdp`              | **yes**    |
| `"drop-sdp"`        | `drop-sdp`              | yes        |
| `"keep-sdp"`        | `keep-sdp`              | (variant defined, untested here) |
| `"fake-prack"`      | `fake-prack`            | **yes**    |
| `"promote-pem-to-200"` | promote (PEM service) | **Slice 4** (variant carried, not wired) |

`true` → `drop-sdp` is the "suppress" mode in the scenarios.

### Rules (all `layer = SERVICE_LAYER`, override the matching CORE rule)

1. **`suppress-18x`** (overrides `relay-provisional`) — response, INVITE, 1xx,
   from-b.
   - First 18x on the call: mint an a-facing To-tag, seed the tag map
     (`add-tag-mapping`), relay as a **bare 180** (status→180, drop body, drop
     `Content-Type`/`Require`/`RSeq`), CDR provisional. Set `firstRelayed=true`,
     `storedATag=<tag>`.
   - Subsequent 18x: suppress relay (CDR only).
   - Reliable 1xx (carries `Require:100rel` + numeric `RSeq`): B2BUA must PRACK
     the b-leg itself (alice never sees the reliable provisional). →
     `send-prack-to-leg`.
   - `fake-prack` + reliable + body: cache bob's SDP on the b-leg dialog
     (`cache-sdp-on-leg-dialog`), **after** the relay (so the early dialog
     exists).

2. **`force-tag-consistency`** (composes with `confirm-dialog`) — response,
   INVITE, 2xx, from-b.
   - Pre-seed the tag map with `storedATag` so `confirm-dialog` reuses it →
     200 OK To-tag matches the first 180 (hides forking/failover from alice).
   - `fake-prack`: stage the winning dialog's cached SDP into
     `policy_update_body` so the relay path substitutes it into the 200 toward
     alice.

3. **`absorb-prack-200`** (overrides `relay-non-invite-200` for PRACK) —
   response, PRACK, 2xx, from-b. Bob's 200 for the B2BUA-originated PRACK must
   not reach alice; absorb (CDR only).

4. **`fake-prack-handle-update-from-b`** (overrides `relay-update`) — request,
   UPDATE, from-b, filter `strategy==fake-prack`. UPDATE w/ no body → local 200.
   UPDATE w/ offer → build a skeleton-fit answer from alice's INVITE offer
   (`SdpAnswerFromOffer`): ok → local 200 + body + cache; no-codec/no-alice-sdp
   → 488.

5. **`fake-prack-handle-update-from-a`** (overrides `relay-update`) — request,
   UPDATE, from-a, leg trying/early, filter `strategy==fake-prack`. Local 200,
   no body (alice has no committed bob SDP to re-offer).

### applyRoute side-effects (decision/apply/applyRoute.ts)
Strategy-aware `Supported: 100rel` handling on the **outbound b-leg INVITE**:
- `drop-sdp`/`keep-sdp` → strip 100rel from Supported.
- `fake-prack` + alice has SDP → keep 100rel (we want bob reliable so we can
  originate PRACK + cache).
- `fake-prack` + **no** alice SDP (delayed offer) → strip 100rel AND
  **self-disable** the policy (don't seed the ext slice).

## Rust mapping

### Config plumbing (already mostly present)
- `call::features::RelayFirst18xStrategy { DropSdp, KeepSdp, FakePrack, PromotePemTo200 }`
  and `RelayFirst18xTo180Feature { strategy }` already exist in
  `crates/call/src/features.rs`. `FeatureActivations.relay_first_18x_to_180`
  already carries it. `RouteDecision.features` already carries it.
  **No new config types needed** — the enum (incl. `PromotePemTo200`) is done.
- Test seam: add `route_to_with_18x(host, port, strategy)` helper +
  `B2buaSut::route_all_to_with_18x(...)` to enable the feature per scenario.

### Per-call runtime state (firstRelayed / storedATag)
The Rust rule `handle`/`filter` are `fn` pointers reading only `ctx.call` — no
closure ext. So per-call state lives on the `Call`:
- `cached_sdp` already on `B2buaDialogExt` (per-dialog) — reused.
- `storedATag`: derived from the existing tag map — the first tag mapping whose
  `a_tag` is the a-facing tag *is* `storedATag`. We add a typed
  `Call.relay_first_18x` runtime field `{ first_relayed: bool, stored_a_tag:
  Option<String> }` (mirrors `cached_sdp`'s "small typed slice on the model"
  pattern; ADR-0016's full typed-ext is out of scope for the early port).
- Strategy is read from `ctx.call.features.relay_first_18x_to_180.strategy`.

### New RuleActions
- `SendPrackToLeg { leg_id, rseq, invite_cseq, b_tag }` — originate a PRACK
  toward the b-leg early dialog (RAck = `<rseq> <invite_cseq> INVITE`). Built in
  `actions.rs` reusing the in-dialog generator; the b-leg early dialog is found
  by `b_tag`.
- `CacheSdpOnLegDialog { leg_id, b_tag, body }` — store SDP on the dialog ext.
- `SetPolicyUpdateBody { body }` — set `call.policy_update_body = Bytes`.
- `SetFirstRelayed { stored_a_tag }` — set the runtime slice.
- `MessageTransform` extended with `drop_body: bool` and `remove_headers:
  Vec<&'static str>` so `relay-to-peer` can emit a bare 180.

### Relay path changes
- `relay_response`: honour `transform.drop_body` (clear body + Content-Type) and
  `transform.remove_headers` (filter the passthrough set, e.g. Require/RSeq).
- 2xx relay: if `call.policy_update_body == Some(Bytes(b))`, substitute `b` as
  the relayed body (+ `application/sdp`) — the fake-prack cached-SDP injection.
- `build_b_leg` / `apply_route`: forward alice's `Supported` to bob, and apply
  the `policy_update_headers` Supported override (strip 100rel) computed at route
  time from the strategy.

## BLOCKER — SIP b-leg failover is not implemented in the Rust port

The Rust B2BUA has **no SIP-level failover** (callee rejects / no-answers → try
the next destination). Evidence:
- `crates/b2bua/src/rules/defaults.rs`: `route-failure` relays the failure and
  `TerminateCall`s; `no-answer` does cdr+destroy+begin-termination. No second
  b-leg is ever created.
- `CallDecisionEngine::call_failure` (the `/call/failure` hook,
  `CallFailureResponse::Failover(RouteDecision)`) is **defined but never
  invoked** anywhere (`grep '\.call_failure(' crates/` → 0 hits).
- Slice 0's parity report explicitly defers failover: "Failover
  (callbackContext / `/call/failure`) is SERVICE_LAYER and explicitly deferred."
- `crates/b2bua-harness/tests/failover.rs` is HA **worker-crash** failover
  (proxy reroutes to a replica), a completely different mechanism.

Therefore the four failover-shaped scenarios CANNOT be ported in this slice
without first building the `/call/failure` service (out of Slice 3 scope, and a
STOP-and-report condition per the task brief):
- suppress-18x: `failoverNoAnswer`, `failoverReject`
- fake-prack: `forking` (modeled as failover-on-503), `failover`

### Ported in this slice (non-failover)
- **suppress-18x**: `basic`, `disabled` (2/4).
- **fake-prack**: `basic`, `multiple_18x`, `update_happy`,
  `update_codec_mismatch`, `delayed_offer_fallback`, `no_policy_control` (6/8).

The four failover cases are recorded as un-ported with this justification in
MIGRATION_STATUS.md, to be picked up when the `/call/failure` failover service
lands (a precursor to a faithful Slice 3 completion).
