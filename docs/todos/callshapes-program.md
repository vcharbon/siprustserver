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

## Verify before phase C (user believes some may already exist)

- Callee-side "emit 18x under an explicit distinct To-tag" primitive: the
  explorer found only ONE linear early dialog per actor today; adjacent
  machinery is the `AnswerALegNewDialog` fork-confirm primitive
  (newkahneed-019, b8018e1). Inventory what exists in
  `crates/scenario-harness/src/actor/actor.rs` before building.
- RFC-audit rules that assume a single early dialog (`crates/sip-net/src/rfc_audit/`)
  — inventory which rules need multi-early-dialog awareness.
- b2bua transparent relay mode: confirm it relays multiple distinct-tag 18x
  and mints one a-leg early dialog per fork.

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

## Hard constraints (from CLAUDE.md — read before each phase)

- `docs/testing/test-clock.md` before ANY timed test; `docs/testing/harness-layers.md`
  for harness selection; every test terminates every call + asserts release.
- One compiling/testing agent at a time (WSL2 memory); cap heavy cargo runs.
- SUT output stays RFC-compliant; `allow_violation` only for deliberate peer-side
  deltas, never to mute SUT findings.
