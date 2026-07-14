# Call-Shapes Program — reusable cross-platform load call shapes

Status: SPEC AGREED (grill session 2026-07-14). Implementation phases A→D below.
Reference consumer: `/home/vince/newkahsip` (consumes upstream loadgen as a
pinned submodule; routes by dialed-number suffix, NOT X-Api-Call).

## Goal

A composable family of reusable call shapes for the loadgen, usable by
external platforms that do not use the X-Api-Call routing mechanism, covering
complex establishment (forking, PRACK/100rel, reroute-without-18x, CANCEL×200
crossing) and complex in-dialog sequences (re-INVITE ×N, UPDATE pre- and
post-connect, crossing BYE, re-INVITE glare, UPDATE-vs-re-INVITE collision),
with the in-dialog material reusable over ANY established dialog context
(initial, post-reroute, post-REFER A↔C).

## Decisions (each confirmed with the user, 2026-07-14)

1. **Deliverable / placement.** New crate `crates/callshapes` in siprustserver
   (depends on scenario-harness + e2e-model). Its `README.md` is the
   downstream-consumption guide, written with newkahsip as the worked example
   (dial-plan binder against `routing-mock/config/numbers.json`, ~30-LOC bin
   via `ShapeRegistry`). Nothing is written into the newkahsip repo; it picks
   the crate up on its next submodule pin bump.

2. **Forking scope — both sides.**
   - Caller: multi-early-dialog UAC. Early dialogs keyed by To-tag (a set, not
     one linear early dialog), PRACK per early dialog where required, exactly
     one fork wins with 2xx; a losing fork's late 2xx gets ACK+BYE per
     RFC 3261 §13.2.2.4.
   - Callee: a "forking UAS" primitive — bob himself emits n×18x with
     DISTINCT, explicitly chosen To-tags on one INVITE server transaction (as
     if a proxy downstream of him had forked), then the 2xx under the winning
     tag; variant: losing tag sends a late 200.
   - Byte-identical-180 dedup is a non-issue: forks always carry distinct tags.
   - RFC-audit consequence: recorded-trace rules must accept multi-early-dialog
     traces waiver-free (audit the SUT's output as compliant, per CLAUDE.md).

3. **Routing seam — abstract behavior slots + binder.** A shape declares named
   abstract requirements with no wire syntax (e.g.
   `needs_sut: RerouteOnRejectNo18x`, `targets: [bob, bob2]`). A per-platform
   `RouteBinder` turns each slot into concrete INVITE input: R-URI user (dial
   plan), an arbitrary header, or X-Api-Call routes. Upstream ships the
   X-Api-Call binder (used by our own fake-clock tests); newkahsip writes a
   dial-plan binder mapping slots to numbers.json entries. Case JSON supplies
   per-run values (numbers, header names).

4. **Race oracle — branch-aware.** New `Expect::EitherOf(&[...])`: the shape
   declares the set of RFC-legal terminal outcomes and, per branch, the
   follow-through obligations (CANCEL×200 where 200 wins ⇒ caller ACKs then
   BYEs; CANCEL wins ⇒ 487+ACK). Paused-clock tests pin EACH branch
   deterministically with timed goals (one test per branch); the load lane
   accepts whichever legal branch occurred. One shape definition serves both.

5. **Composition — arbitrary-depth dialog-context pipeline (v1).** A shape is
   a chain of stages, each yielding a NAMED established dialog; in-dialog
   scripts attach to any stage's dialog by name (default: current). Covers
   reroute→refer, refer→refer (ct_chain), scripts on intermediate dialogs.
   Scripts declare their required dialog state (early vs confirmed): early
   scripts (UPDATE pre-connect, PRACK) attach to the reliable establishment's
   early dialog. "Changing legs" from the original ask = the active remote leg
   changing over the call's life — covered by this pipeline, nothing extra.

6. **Test substrate — loadgen driver on the fake net.** Abstract the loadgen
   mux's transport so the REAL driver + mux + demux + DropModel stack runs
   over the simulated signaling network under `start_paused`, against an
   in-process B2buaCore. All shape tests (including loss) run paused-clock in
   the default lane; the real-UDP lane shrinks to a thin smoke.

7. **Catalog v1 — FULL matrix**, generated from a declared compatibility
   matrix (not every pair is legal), stable generated shape ids
   (e.g. `nk-reroute-no18x+reinvite10`):
   - Establishments: **E1** transparent · **E2** reliable/100rel
     (183+RSeq→PRACK; UPDATE-pre-connect variant) · **E3** forked multi-18x
     (distinct To-tags, one wins; loser-late-200 variant) · **E4**
     reroute-no-18x (bob rejects 4xx WITHOUT any 18x → SUT reroutes to bob2)
     · **E5** CANCEL×200 crossing (branch oracle; terminal or → dialog).
   - Transfer stage: **T1** REFER → A↔C dialog, chainable at any depth.
   - Scripts: **S0** talk+BYE · **S1** re-INVITE ×N (param, default 10) ·
     **S2** UPDATE post-connect · **S3** crossing BYE · **S4** INFO/MESSAGE ·
     **S5** re-INVITE glare (491, §14.1 owner/non-owner retry windows driven
     exactly under paused clock) · **S6** UPDATE-vs-re-INVITE collision.
   - v1 shapes ≈ {E1..E5}×{S0..S6} where compatible, plus chains
     E1→T1→{S0,S1,S3} and E4→T1→S0 (and deeper chains as the matrix allows).

8. **SUT-side forking activation.** The b2bua already has several relay modes
   including a transparent one; E3 shapes only make sense — and are only
   generated/run — under transparent relay mode. The forking is modeled at
   bob (the UAS), never requires new SUT forking logic.

9. **Loss coverage — soak matrix + targeted drops.** Every generated shape
   runs a paused-clock probabilistic loss soak (drop_rate ~0.05–0.12 +
   auto-retransmit, STRICT audit==0, `assert_fully_reaped`), PLUS one
   deterministic `TargetedDrop` test per NEW protocol element: each forked
   18x, PRACK, early UPDATE, loser late-200, crossing-BYE, the reroute-trigger
   reject, glare 491. Deterministic tests prove both recovery
   (`permanent:false`) and bounded give-up (`permanent:true` → settle Fail).

10. **Sequencing — A→B→C→D, one green commit per stage.**
    - **A** mux transport seam → loadgen driver runs on the fake net under
      paused clock; prove with paused-clock equivalents of representative
      existing smoke tests.
    - **B** pipeline algebra + `RouteBinder` + the `callshapes` crate;
      regenerate the EXISTING shapes through it (no behavior change) as proof.
    - **C** new protocol capabilities, each landed with its stage/script and
      targeted-drop tests: forking caller+callee, branch oracle, CANCEL×200,
      crossing BYE, glare S5/S6, early UPDATE, reroute-no-18x stage.
    - **D** full matrix generation + loss soaks + the crate README.

## Pre-phase-C verification findings (explored 2026-07-14 — seams confirmed)

1. **Callee 18x tag control**: the chosen-tag primitive EXISTS at the agent
   layer — `Respond::with_to_tag` (agent.rs ~3076/3111, per-fork override that
   does not disturb the txn's sticky tag); two distinct-tag 18x on one
   retained `ServerTxn` are legal today. Missing: the orchestration — add
   `Disposition::ForkingRing{tags, winner, loser_late_200}` (actor.rs ~67-84),
   an `apply_disposition` arm (~695-723), and teach
   `TimedAnswer`/`answer_initial_invite` (~203-208, 497-523) to 200 under the
   winning tag (+ optional losing-tag late 200). `AnswerALegNewDialog`
   (newkahneed-019) is SUT-side (b2bua RuleAction), not reusable as the UAS
   primitive — but proves the downstream wire shape.
2. **Caller early-dialog model**: single linear slot —
   `DialogTable.pending_invite: Option<ClientInvite>` (actor.rs ~225-237);
   `learn_from_response` (agent.rs ~2255-2279) keeps the FIRST fork's tag and
   silently drops later distinct-tag provisionals; only a 2xx overrides
   (§13.2.2.4 winner). PRACK dedup is RSeq-only (`pracked_rseqs:
   HashSet<u32>`) — must re-key to `(to_tag, rseq)`. Good news: per-fork CSeq
   plumbing already exists (`ClientInvite.fork_cseq`,
   `InDialogRequest::with_to_tag`/`with_fork_cseq` agent.rs ~2717-2823, winner
   CSeq promotion in `ack` ~2468).
3. **rfc_audit**: `project_per_dialog` already splits forks into per-To-tag
   slices (newkahneed-029). ONE high-value fix: the pending (pre-tag)
   INVITE+100 bucket migrates into the FIRST fork's slice only
   (dialog_model.rs ~558-573) — replicate it into EVERY fork's slice so
   rfc3264 offer/answer rules check non-first forks (today they under-check,
   not false-positive). `cseqInDialogOrder`, `unackedInvite2xxByed` (the
   loser-late-200→ACK+BYE case), `prackOnReliable1xx`,
   `noByeOutsideOrEarlyDialog` are already fork-aware. Caller must only ever
   BYE a fork AFTER its own 2xx (a BYE on a never-confirmed early fork
   correctly trips `noByeOutsideOrEarlyDialog`).
4. **SUT activation for E3**: transparent CORE relay
   (`FeatureActivations.relay_first_18x_to_180 = None`, call/features.rs ~91)
   relays each b-leg fork under its own a-facing tag (actions.rs ~1147-1206) —
   exactly E3. The `relayFirst18x` masking service (any `Some{...}`) COLLAPSES
   forks to one tag: E3 must NOT run under it (`Relay18xMessages::All` is not
   the enabler).
5. **S1 re-INVITE ×N**: `SUBFLOW_RENEG` is a monotonic max-latch
   (state.rs `advance_subflow`), never reset by `GoalStep::Reinvite` — a
   `reneg_done` guard CANNOT serialize N cycles. Phase C must add a per-cycle
   completion barrier (per-CSeq, mirroring `sent_reinvites: HashSet<u32>`).
   Until then `ShapePlan::validate()` rejects `Reinvite { n != 1 }`.

## Phase A findings (landed — the fake-net seam)

- The seam is `MuxCore::bind_on(fabric: &dyn SignalingNetwork, …)` — the mux's
  endpoints are `Arc<dyn UdpEndpoint>` from `sip_net`; `MuxCore::bind` keeps
  its signature and delegates with `RealSignalingNetwork`. The `CallTxns`
  retransmit engine and resender tasks send through the same seam
  (`CallTxns::send` became fire-and-forget via a detached task — the fabric
  send is async; behaviourally equivalent to the `try_send_to` it replaced).
- The mux's pending-slot deadline + reap sweep moved from `std::time::Instant`
  to `tokio::time::Instant` — under `start_paused` the std clock barely moves
  and the reaper went inert. Behaviour on the real clock is identical.
- Paused-clock rig pattern (copy for phases B–D): ONE
  `SimulatedSignalingNetwork::new(1)` shared by the SUT (bound through a
  `Harness::with_network_and_clock(…, Clock::test_at(0), TransportKind::Fake, …)`
  recording harness — PanicDump works) and the mux (bound raw via `bind_on`;
  the loadgen's own per-call audit is the gate). Tests:
  `crates/loadgen/tests/fake_net.rs`. Driver, governor, recv timeouts,
  retransmit ladders, and the SUT's 32 s reap all ride virtual time — the
  3-test file (incl. a loss soak + a targeted-drop recovery) runs in <1 s.
- The paused lane can assert what the real-UDP lane cannot: after the soak,
  `h.advance(40 s)` pushes past the SUT's terminating safety timer and
  `assert_fully_reaped()` gates strictly (no 45 s wall-clock settle).
- Deterministic-substrate tests assert strictly (`ok == total`), no
  contention-headroom ratios.
- Fixed in passing: three pre-existing port-base collisions in
  `tests/smoke.rs` (6560, 6490, 6600 each used by two tests → intermittent
  AddrInUse under full-suite runs; rebased to 6580/6590/6640).

## Phase C findings (in progress — new protocol capabilities)

Landed so far (each its own green commit; `just test` default lane green modulo
the pre-existing real-clock smoke contention flake below):

- **C6 — re-INVITE ×N serialization (S1)** — commit `c0ba501`. The per-cycle
  counter the phase-B finding §5 called for is `LegObservation::reneg_cseqs`
  (a grow-only `BTreeSet<u32>`; `reneg_count()` = its cardinality). The caller's
  reactor records `Observation::RenegCompleted{leg,cseq}` in the SAME block that
  removes from `sent_reinvites` (so it fires exactly once per CSeq — a
  retransmitted 2xx under loss can't double-count). `compile_script` gates cycle
  `i` on `reneg_count() >= i` (cycle 0 on the incoming gate), so no two re-INVITEs
  overlap (which would glare into a 491); the teardown gate is `>= n`. n=1 is
  byte-for-byte the old `reneg_done` gate. `validate()` now rejects only n==0.
  New: `shapes::reinvite_n(binder, id, n)` + the id-addressable `reinvite10`
  registry shape (NO mix weight — phase D assigns). Tests:
  `plan.rs::reinvite_n_validates_and_rejects_zero`, fake-net
  `loadgen_fake_net_reinvite_x10_serialized` + `…_x10_loss_soak`.
  GOTCHA: a ×N call carries ~N× the datagrams, so under a paused loss soak the
  governor's *delivered-call count* is variable under concurrent scheduling —
  keep the `total >= …` floor conservative (5), and don't assert retransmit
  *dominance* (only `ok > 0` + audit==0 + every-NOK-is-a-timeout).

- **C3 — crossing BYE (S3)** — commit `4057e03`. The reactor already handles an
  inbound BYE while its own BYE is in flight order-independently (it 200s the
  inbound BYE, then `discharge_on_teardown` subsumes its own still-open BYE
  obligation) — NO reactor fix was needed; pinned by the SUT-less machinery test
  `actor::tests::two_actor_crossing_bye_both_terminate`. New pipeline type:
  `Teardown::CrossingBye{after}` — the caller AND the winning callee (tracked as
  `Build::winner`, "bob" or "bob2" after a reroute) both BYE on the final gate.
  Shape `crossing_bye` + registry entry. The in-process B2BUA relays the crossing
  BYEs cleanly (verified). Tests: `loadgen_fake_net_crossing_bye` +
  `…_crossing_bye_recovers_dropped_byes` (drops BOTH crossing BYEs via
  `leg:None, Outbound, permanent:false`; both recover by retransmit).

- **C1(d) — mux fork-aware response dedup** — commit `bcacd4e`. `CallTxns::
  on_inbound` keyed the response dedup on `(branch, status)`, so two 18x with
  DISTINCT To-tags on ONE INVITE branch (a true fork, §12.1.2) were absorbed as
  retransmits of each other, and a loser's late 200 never reached the body. Now
  keyed `(branch, status, To-tag)` via a new `to_tag()` raw extractor. A genuine
  same-tag retransmit still dedups (033 ask D2 unchanged). Tests:
  `mux::tests::calltxns_distinct_fork_tags_are_not_deduped`,
  `…::to_tag_extracts_the_to_parameter`. This is the transport half of C1 and is
  independently correct — required before the caller/callee forking machinery.

- **C1(a) — forking-UAS callee `Disposition::ForkingRing`** — commit `b9549d2`.
- **C1(b) — fork-aware caller (multi-early-dialog set keyed by To-tag)** — commit
  `ee7bc02`.
- **C1(c) — rfc_audit: replicate the establishing INVITE into every fork's
  slice** — commit `b66a6ca`. Landed differently from the deferred sketch below:
  a simple "clone the pending `ordered` into each fork" over-includes in two
  cases the sketch missed, so replication copies only the ESTABLISHING INVITE
  transaction via `establishing_tail()`, keyed on the highest-CSeq empty-To-tag
  INVITE (NOT the last INVITE *event*):
  1. an AUTH RETRY accumulates INVITE CSeq1 (401'd) + INVITE CSeq2 in ONE
     pending bucket (same From-tag, both empty To-tag) — cloning CSeq1 into the
     confirmed dialog made the establishing retry look like an in-dialog
     re-INVITE → false `rfc3261` §13.2.1/§20.37 SHOULD findings (regressed
     `auth_retry_*`/`actor_caller_retries_through_a_401_challenge`).
  2. a RELAY bind both RECEIVES and SENDS the one establishing INVITE (same
     CSeq) — keying on the last INVITE *event* dropped the received copy, so the
     SDP/route relay-skip logic no longer saw the slot as a relay → false
     `SdpOriginContinuity` finding (regressed `sdp_origin_skips_relay_slots`).
  CSeq-keying fixes both: a true fork has ONE INVITE CSeq so its whole bucket is
  kept. `OrderedEvent`/`Bucket` gained `Clone`. Fork fixtures in
  `dialog_model.rs` + `rfc3264_cross.rs`.

### Deferred — design notes for the C-completion / phase-D agent

The following C sub-capabilities are DESIGNED here but NOT yet landed. They
share a theme: each needs reactor-state surgery whose blast radius touches the
~77-rule RFC audit that gates every test, so land each as its own green commit
with the audit watched closely.

- **C1(e) — TRUE FORKING pipeline wiring (E3)** — commit `e62a2f9`. C1 COMPLETE.
  `Establishment::Forked{tags, winner, reliable, loser_late_200}` compiles bob
  to `Disposition::ForkingRing`; shapes `forked`/`forked_reliable` registered
  and proven end-to-end through the transparent-CORE-relay `B2buaSut`
  (`route_all_with_refer` leaves `relay_first_18x_to_180 = None` — verified) in
  `loadgen/tests/fake_net.rs` (`loadgen_fake_net_forked_plain`,
  `…_forked_reliable`, `…_forked_loss_soak`). The three distinct-tag 18x relay
  through as three a-facing early dialogs and the caller confirms on the
  winner's 2xx. `validate()` enforces ≥2 tags + winner/loser membership.
  **KEY FINDING — `forked_loser_late_200` is PEER-TO-PEER ONLY.** A dialog-
  terminating B2BUA forwards only the FIRST b-leg 2xx to the caller and absorbs
  the loser's late 200, so the caller's ACK+BYE-the-loser path is UNREACHABLE
  through a SUT (the losing fork dangles on the callee → settle NOK). It is a
  valid composition (kept + documented, NOT in the loadgen registry); its
  behavior is pinned SUT-less by
  `actor::tests::{forking_ring_loser_late_200_is_acked_and_byed,
  actor_caller_acks_and_byes_losing_fork_late_200}` (C1a/C1b). Phase D: any
  loser-late-200 matrix cell must be a peer-to-peer harness shape, never a
  through-SUT load cell.

  (Historical seam notes for the now-landed a/b/c below; kept for reference.)
  pre-phase-C findings above (trust but re-verify lines). Concretely:
  - Callee: add `Disposition::ForkingRing{tags:&[&str], winner:&str,
    loser_late_200:bool}` (actor.rs ~68) + an `apply_disposition` arm that emits
    one 18x per tag via `ServerTxn::respond(180,..).with_to_tag(tag)` (the seam
    at agent.rs ~3076) on the ONE retained INVITE txn, then answers 200 under
    `winner` (extend `answer_initial_invite`/`TimedAnswer` to carry a chosen
    To-tag — today it uses the txn's sticky tag). `loser_late_200` emits a second
    200 under a losing tag after the winner's.
  - Caller: the actor's `DialogTable.pending_invite` is a single slot and
    `pracked_rseqs` is RSeq-only. Re-key PRACK dedup to `(to_tag, rseq)` and let
    the ONE `ClientInvite` (which already carries `fork_cseq: HashMap<tag,cseq>`
    and `with_to_tag`/`with_fork_cseq` PRACK plumbing) PRACK each early dialog.
    The 2xx's tag is the winner (§13.2.2.4 — `learn_from_response` already
    overrides the early tag on a 2xx). A LOSING fork's late 200 → ACK then BYE,
    but ONLY after that fork's own 2xx (a BYE on a never-2xx'd early fork
    correctly trips `rfc3261.noByeOutsideOrEarlyDialog`).
  - callshapes: `Establishment::Forked{forks, winner_reliable, loser_late_200}`.
    E3 shapes MUST run under the SUT's transparent CORE relay
    (`FeatureActivations.relay_first_18x_to_180 = None` — the default plain
    `B2buaSut` config already is; any `relayFirst18x` masking COLLAPSES forks and
    is incompatible — say so in the stage doc). C1(d) is already landed so the
    mux won't collapse distinct-tag forks.
  - C1(c) rfc_audit: in `dialog_model.rs::project_per_dialog` (~558-573) the
    pending pre-tag INVITE+100 bucket is `remove`d on the FIRST fork's migration,
    so later forks' slices lack the establishing INVITE (they UNDER-check — not a
    false positive today). Fix: seed each new confirmed-fork bucket with a CLONE
    of the pending bucket's `ordered` events (probe both `pending_key(ft)` and
    `pending_key(tag)` orientations), and DROP a pending bucket only after it has
    been cloned into ≥1 confirmed fork (a never-confirmed reject keeps its
    pending slice, as today). `OrderedEvent`/`Bucket` will need `Clone`. Add
    synthetic fork fixtures (distinct-tag 18x, reliable-18x-per-fork,
    loser-late-200→ACK+BYE) asserting the fork-aware rules stay clean. HIGH RISK
    to the whole suite — do this WITH real multi-fork traces from C1(a,b), not
    synthetic-only, so the migration is validated end-to-end.
  - Per-element TargetedDrop tests: each forked 18x, each PRACK, the loser late
    200.

- **C2 — branch oracle + CANCEL×200 crossing (E5).** New
  `Expect::EitherOf(&'static [ExpectBranch])` in spec.rs, each branch carrying
  its terminal `who`/detail; `into_result` maps whichever branch the observed
  state shows (200-wins → the happy `Ok`; CANCEL-wins → the `abandon` terminal).
  The reactor ALREADY confirms a 200 that crosses a CANCEL (the `Answered` arm
  fires even after `GoalStep::Cancel`, which keeps `pending_invite`), so the
  200-wins branch needs only a follow-through BYE goal gated on "confirmed after
  cancel"; the CANCEL-wins branch is the existing 487→terminated path. New
  `Establishment::CancelAnswerCrossing`: drive alice's CANCEL and bob's 200 from
  timed goals under the paused clock, one transit quantum apart per branch. Keep
  classification bounded — check which `ResultClass` each branch lands in
  (loadgen `class.rs`) and pin it. TWO paused tests (one per branch, timing
  varied by one transit quantum) + a loadgen fake-net test accepting either.

- **C4 — glare (S5 re-INVITE 491, S6 UPDATE-vs-re-INVITE).** react_request's
  INVITE arm always 200s; make it answer 491 when THIS leg has an unacked
  re-INVITE it originated in flight (`!sent_reinvites.is_empty()`), then retry
  after the §14.1 dwell (owner = higher Call-ID per §14.1; drive the retry timer
  exactly under the paused clock). The 491 closes the txn, the retry opens a new
  ReInvite obligation — ensure NO obligation is left open by the 491'd attempt
  (close the ReInvite key on the 491, like a final). S6: UPDATE has no ACK —
  resolve per §14.1/3311 §5.2 and pin the choice. Bounded new barrier vocab:
  reuse `reinvited`.

- **C5 — early UPDATE (RFC 3311 on the early dialog).** Scripts need a
  required-dialog-state notion (Early vs Confirmed). `Script::UpdateEarly`
  attaches to E2/Reliable's early dialog (after PRACK, before the 200 — 3311
  §5.1 requires a reliable provisional first). `compile()`/`validate()` must
  reject `UpdateEarly` on any establishment without a reliable early dialog. The
  caller sends UPDATE within the early dialog (the `ClientInvite` knows the early
  To-tag post-PRACK); the ReliableAnswer callee 200s it then proceeds to the
  INVITE 200.

## Hard constraints (from CLAUDE.md — read before each phase)

- `docs/testing/test-clock.md` before ANY timed test; `docs/testing/harness-layers.md`
  for harness selection; every test terminates every call + asserts release.
- One compiling/testing agent at a time (WSL2 memory); cap heavy cargo runs.
- SUT output stays RFC-compliant; `allow_violation` only for deliberate peer-side
  deltas, never to mute SUT findings.
