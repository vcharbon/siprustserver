# Slice 4 — `promote18xPemTo200` (early-media 183→synthetic 200) design note

TS source pinned at `portsource/sipjsserver` SHA `fffc4ac6`.
Rust under `crates/`. Branch `feat/sip-message-layer`.

## What the TS service does

`src/b2bua/rules/custom/promote18xPemTo200.ts` is a SERVICE_LAYER service
activated by the `relayFirst18xTo180` strategy value `"promote-pem-to-200"`. It
promotes Bob's first `183 Session Progress + SDP + P-Early-Media` (RFC 5009)
into a **synthetic 200 OK** toward Alice, so a constrained caller (cannot send
DTMF in an early dialog) can interact with Bob's already-running early media.

The TS service owns a typed call-ext slice `PemCallExt`:
`{ promoted, promotedSdp?, windowOpen, resyncReinviteCSeq? }`. Rules read the
decoded slice and return an updated one. There is no per-leg ext / phase
machine — `promoted` + `windowOpen` booleans gate everything.

### The 8 TS rules

1. **`promote-183-pem`** (response, INVITE, 183, from-b; filter: body≠∅, has
   P-Early-Media, `!promoted`). Mint an a-facing To-tag + seed tag map; set
   a-leg confirmed + stamp its To-tag; relay the 183 as a **200 OK** toward
   Alice (drop Require/RSeq/P-Early-Media, set Allow + Supported-no-100rel, keep
   the SDP body); CDR provisional. Reliable (RSeq) → B2BUA PRACKs b locally.
   `callExt = { promoted:true, promotedSdp: resp.body, windowOpen:true }`.
2. **`suppress-post-promote-18x`** (response, INVITE, 1xx, from-b; filter:
   `promoted`). Drop on a-side (CDR only); PRACK b if reliable.
3. **`confirm-after-promote`** (response, INVITE, 2xx, legState trying/early,
   legDisposition pending/bridged, from-b; filter: `promoted`). Replicate the
   structural confirm pieces (confirm-dialog, ack-leg b, merge a↔b, destroy
   other forks, cancel no-answer, schedule keepalive/duration, CDR answer) but
   **DO NOT relay 200 to Alice** (she already saw it). Re-seed `(bLegId, bTag)→
   aFacingTag` under the winning fork's bTag using the SAME aTag (forking). Diff
   b's SDP vs `promotedSdp` (`sdpMediaEquivalent`): equal → window closes
   (`callExt = INITIAL`); different → `send-reinvite` on the a-leg with b's SDP
   + Allow/Supported, stash `resyncReinviteCSeq = aDialog.localCSeq + 1`.
4. **`promote-resync-reinvite-response`** (response, INVITE, from-a; filter:
   `cseq == resyncReinviteCSeq`). <200 → unclaimed (wait). 2xx → `ack-leg a` +
   CDR + close window. ≥300 → CDR reject + `begin-termination` with a
   `Reason: SIP;cause=<status>` header → BYE both legs.
5. **`promote-reject-a-reinvite-update`** (request, INVITE|UPDATE, from-a;
   filter: `windowOpen`) → 491 Request Pending.
6. **`promote-reject-a-other-indialog`** (request, INFO|MESSAGE, from-a; filter:
   `windowOpen`) → 488 Not Acceptable Here.
7. **`promote-absorb-a-ack`** (request, ACK, from-a; filter: `promoted` &&
   `activePeer == null`). Absorb Alice's ACK to the synthetic 200 (b still
   early; relaying would land on a phantom 2xx). CDR only. Once merged
   (activePeer set) the core `relay-ack` takes over.
8. **`promote-b-fails-post-promote`** (response, INVITE, 3xx/4xx/5xx/6xx,
   from-b, callState active; filter: `promoted`). Alice is already confirmed and
   can't be 4xx'd → CDR reject + `terminate-leg{rejected}` on b +
   `begin-termination` with `Reason: SIP;cause=<status>` → BYE Alice.

`reasonHeader(status, phrase) = SIP ;cause=<status>;text="<phrase or Unspecified>"`.

## The 7 scenario cases (tests/scenarios/promote-pem-to-200.ts)

1. **happy-no-resync** — A INVITE(promote) → 100; B 183(PEM,SDP) → A sees
   **200** carrying B's SDP, no P-Early-Media, Allow has INVITE/BYE, Supported
   has no 100rel. A ACK (absorbed; B sees nothing). B 200(same SDP) → B2BUA ACKs
   B; SDP equal → no resync. Quiet 1s (A sees no INVITE). Normal teardown.
2. **resync-sdp-changed** — like 1 but B 200 has **different** SDP (port 30000)
   → A receives a **re-INVITE** carrying `m=audio 30000` + Allow/Supported; A
   200s; B2BUA ACKs A (window closes). Then A INFO is **relayed** to B (proof
   window closed). Teardown.
3a. **b-fails-post-promote** — after promotion B 503s (B auto-ACKs the 503) →
    A gets a **BYE** with `Reason: …cause=503`; A 200s.
3b. **resync-failed-by-a** — B 200(diff SDP) → resync re-INVITE to A; A 488s it
    (A auto-ACKs the 488) → BYE **both** legs with `Reason: …cause=488`.
3c. **a-bye-during-window** — after promotion A BYEs before B's final response →
    A's BYE gets 200; B's still-open INVITE gets **CANCEL** (B auto-ACKs 487).
4. **no-policy-control** — same packet flow, policy OFF → A sees a **183** (not
   200), body + PEM survive; then normal 200/ACK/teardown. Regression guard.
5. **forking-resync** — B 183(PEM) with To-tag FORK_T1 (promote); B 200(diff
   SDP) with To-tag FORK_T2 → B2BUA's local ACK carries **FORK_T2**, A gets a
   resync re-INVITE with the new SDP, A's BYE routes via FORK_T2.
6. **in-dialog-rejection** — during the window A UPDATE→**491**, A INFO→**488**;
   then B 200(same SDP) → no resync; teardown.

All cases are **upstream** behaviour (one b-leg, possibly two To-tags from a
forking proxy). NONE require B2BUA-driven SIP failover / leg recreation, so all
7 are portable (the known Slice 3 failover blocker does not apply here).

## Rust mapping

### Per-call state
The TS `PemCallExt` maps to a new typed runtime slice on `Call`:
`Call.promote_pem: Option<PromotePemState> { promoted, promoted_sdp: Vec<u8>,
window_open, resync_reinvite_cseq: Option<i64> }` (mirrors the existing
`relay_first_18x` slice — ADR-0016 full typed-ext is out of scope for the early
port). Helpers in `call::helpers`: read accessors + setters returning a new
`Call`. Strategy read from `features.relay_first_18x_to_180.strategy ==
PromotePemTo200`.

### New / extended RuleActions
- `SendReinvite { leg_id, body, add_headers }` — originate a re-INVITE on a leg
  (here always "a"), CSeq = dialog.localCSeq + 1, carrying `body` + Allow/
  Supported. Built in `actions.rs` via the in-dialog generator; the response
  comes back classified from-a (the B2BUA's stamped Via cr/lg), claimed by
  `promote-resync-reinvite-response`.
- `SetPromotePem { state }` — write the runtime slice (incl. clearing/INITIAL).
- `AckLeg` extended to also ACK the **a-leg** (TS `ack-leg legId:a`).
- `BeginTermination` already carries `reason: Option<String>`; the executor now
  **emits** it as a `Reason:` header on every teardown BYE (was dropped before —
  Slice 0 recorded this as a fidelity gap; PEM needs it). Same for `DestroyLeg`-
  driven BYEs via `begin_termination`.
- `MessageTransform` gains `add_headers: Vec<(&'static str, String)>` (replace
  semantics) so the synthetic-200 / resync-reINVITE can stamp Allow + Supported.
  `remove_headers` already exists (Require/RSeq); P-Early-Media is not in the
  relay passthrough set so it never reaches Alice anyway.

### sdpMediaEquivalent
Ported as `call`-free pure fn `sdp_media_equivalent(a,b)` in a new
`crates/b2bua/src/rules/sdp_diff.rs` (m= blocks + sorted c/b/a/i/k attribute
sets; session-level lines ignored; both-empty equal, one-empty differ).

### Rule selection
PEM rules are appended to `default_rules()` like the relay_first_18x rules;
their column gate + a `promote_pem_active` filter (`strategy ==
PromotePemTo200`) keep them dormant otherwise, and SERVICE_LAYER ranks them
above the CORE rules they displace. They do not need explicit `overrides`
because they win by layer and always consume (mirrors the TS note).

### confirm-after-promote relay suppression
The TS rule replays confirm-dialog's pieces minus `relay-to-peer`. In Rust I
emit the same action sequence (ConfirmDialog/AckLeg b/Merge/CancelTimer/
ScheduleTimer×2/CDR) and, on SDP mismatch, append `SendReinvite{a}`. No
RelayToPeer is emitted, so Alice sees nothing on the wire for B's real 200.
The forking re-seed is `AddTagMapping{aFacingTag, bLegId, winningBTag}` using
the stored a-tag; `confirm_dialog` already reuses a pre-seeded a-tag via
`find_by_b_tag`, and `ack-leg`/in-dialog routing pick the dialog by remote_tag.

### DSL additions (scenario-harness/src/agent.rs)
- `ClientInvite::dialog_view()` / a way for Alice to act as UAS for the resync
  re-INVITE: Alice uses the existing `Agent::receive("INVITE")` + `ServerTxn`
  (the re-INVITE is a normal request to Alice's bound endpoint; `respond` echoes
  Via back to the B2BUA, ACK handled by the B2BUA's `ack-leg a`). No new DSL
  surface needed for receiving — the agent already exposes `receive`.
- P-Early-Media is a normal header via `Respond::with_header` / asserted with
  `get_header`. Forking tags via the existing `Respond::with_to_tag`.

## Portability / blockers
No case requires the missing `/call/failure` failover. All 7 ported. Nothing
marked blocked.
