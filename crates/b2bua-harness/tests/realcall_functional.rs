//! Functional leak gate over the **shared** real-call scenarios.
//!
//! These are the SAME `scenario_harness::realcall` scenarios the load generator
//! drives at 20–100 cps against the real cluster — here run ONCE, in-process,
//! against a real `B2buaCore`, on a paused clock. `run_asserting` panics on any
//! deviation (the strict analogue of the load driver's count-and-classify), then
//! we assert the SUT leaked no call state. So a scenario that leaks a dialog,
//! holds a limiter token, or mis-tears-down is caught deterministically in CI —
//! no cluster, no soak — and the realistic 180→200 / re-INVITE / pre-BYE dwells
//! are exercised for free (the sleeps auto-advance under `start_paused`).
//!
//! This is the proof that one scenario definition feeds both lanes — every
//! migrated scenario is covered here: the happy-path flows (`run_asserting`) AND
//! the voluntarily-failing ones (`run_collecting`, where a FAILED call that still
//! cleans up is the leak gate's most valuable assertion). As more flows migrate
//! into `realcall::scenarios`, add a case here per flow.

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::actor::scenarios::{
    AbandonRinging as ActorAbandonRinging, InviteReject as ActorInviteReject,
    LongCall as ActorLongCall, OptionsHold as ActorOptionsHold, PrackUpdate as ActorPrackUpdate,
    Refer as ActorRefer, ReferCharlieReject as ActorReferCharlieReject, Reinvite as ActorReinvite,
};
use scenario_harness::actor::ActorScenario;
use scenario_harness::realcall::scenarios::BasicCall;
use scenario_harness::realcall::{
    run_actor_asserting, run_actor_collecting, run_asserting, run_collecting, CallEnv,
};
use scenario_harness::{Agent, RealCallScenario};

// The REFER backend the b2bua's scripted `/call/refer` authorizes (see
// `ScriptedDecisionEngine::route_all_with_refer` → `default_call_refer`); the
// refer scenarios' `X-Api-Call` must carry this `refer_key` to be allowed.
const REFER_KEY: &str = "refer-allow-c";
/// Canonical charlie (transfer-target) port — distinct from alice/bob/b2bua.
const CHARLIE_PORT: u16 = 5090;

/// Pump both callee legs (bob + charlie) with a best-effort drain-and-200 until
/// they go quiet, CONCURRENTLY. Used after a multi-leg (REFER) teardown so a
/// relayed b-leg BYE that lands just after the scenario's own short drain window
/// is still answered, letting the SUT reap its downstream legs without waiting
/// out a retransmit timer. A 1 s window comfortably outlasts the relayed BYE's
/// transit + the first retransmit under the harness fabric. Concurrent (not
/// sequential) so a BYE to one leg is not missed while we drain the other.
async fn drain_callees(bob: &Agent, charlie: &Agent) {
    let window = std::time::Duration::from_secs(1);
    tokio::join!(bob.quiesce(window), charlie.quiesce(window));
}

// ── Happy-path leak gate (alice/bob only) ──────────────────────────────────

/// Drive a shared real-call scenario through an in-process B2BUA and assert it
/// left no call state behind.
async fn assert_no_leak(name: &str, scenario: &dyn RealCallScenario) {
    let scene = B2buaScene::new(name).await;
    let env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        None,
        scene.b2bua.addr,
        "X-Loadgen-Id",
        format!("{name}-tok"),
    );

    run_asserting(scenario, &env).await;

    // No leak: the a-leg BYE released the SUT's call state and limiter token.
    settle_until(|| scene.b2bua.active_calls() == 0).await;
    scene.b2bua.assert_fully_reaped();

    // Gate the RFC 3261/3262/3264 audit over the recorded trace (consumes scene).
    let report = scene.finish().await;
    assert!(report.passed(), "RFC audit failed for `{name}`");
}

/// The ACTOR-lane twin of [`assert_no_leak`] for an alice/bob-only actor body:
/// drive it through an in-process B2BUA via the actor runner and assert no leak.
async fn assert_no_leak_actor(name: &str, scenario: &dyn ActorScenario) {
    let scene = B2buaScene::new(name).await;
    let env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        None,
        scene.b2bua.addr,
        "X-Loadgen-Id",
        format!("{name}-tok"),
    );

    run_actor_asserting(scenario, &env).await;

    settle_until(|| scene.b2bua.active_calls() == 0).await;
    scene.b2bua.assert_fully_reaped();

    let report = scene.finish().await;
    assert!(report.passed(), "RFC audit failed for `{name}`");
}

#[tokio::test(start_paused = true)]
async fn realcall_basic_call_no_leak() {
    assert_no_leak("realcall-basic", &BasicCall).await;
}

#[tokio::test(start_paused = true)]
async fn realcall_reinvite_no_leak() {
    // Since P3 the reinvite load body is the ACTOR-declared port (per-endpoint
    // reactors + the ack-gated settle barrier).
    assert_no_leak_actor("realcall-reinvite", &ActorReinvite).await;
}

#[tokio::test(start_paused = true)]
async fn realcall_options_hold_no_leak() {
    // Default `for_functional` options_hold (2 s) / cadence (1 s) drives a couple
    // of in-dialog OPTIONS pings the SUT relays to bob, then a BYE — no leak.
    // Since P3 the load body is the ACTOR-declared port.
    assert_no_leak_actor("realcall-options-hold", &ActorOptionsHold).await;
}

#[tokio::test(start_paused = true)]
async fn realcall_prack_update_no_leak() {
    // RFC 3262 + 3311 through the SUT: reliable 183 (Require:100rel/RSeq) →
    // PRACK/200 → 200/ACK → in-dialog UPDATE/200 → BYE. The b2bua relays the
    // reliable provisional, the PRACK and the UPDATE end-to-end with no special
    // config; the report gate below enforces the full RFC 3261/3262/3264 audit
    // with NO allow_violation. Since P3 this is the ACTOR-declared body
    // (per-endpoint reactors + the ack-gated settle barrier), driven through the
    // same executor the load fleet uses.
    assert_no_leak_actor("realcall-prack-update", &ActorPrackUpdate).await;
}

#[tokio::test(start_paused = true)]
async fn realcall_long_call_no_leak() {
    // Default `for_functional` long_hold (2 s) holds the call past its single
    // in-dialog OPTIONS ping then BYEs. The harness keepalive interval is 30 s
    // and the dwell is seconds, so no SUT keepalive fires — the BYE lands clean.
    // Since P3 the load body is the ACTOR-declared port (the reactors answer any
    // SUT keepalive during the hold).
    assert_no_leak_actor("realcall-long-call", &ActorLongCall).await;
}

// ── Refer (blind transfer) leak gate — needs a third (charlie) leg ──────────

/// A refer-capable scene: alice/bob at the canonical ports, the b2bua built via
/// `route_all_with_refer`, plus a charlie (transfer-target) agent bound on the
/// same harness fabric. Returns the scene and charlie; the caller builds the
/// `CallEnv` with `Some(&charlie)` and the refer pin pointing at charlie's addr.
async fn refer_scene(name: &str) -> (B2buaScene, Agent) {
    let scene = B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
    })
    .await;
    let charlie = scene
        .h
        .agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}"))
        .await;
    (scene, charlie)
}

/// Build a `CallEnv` for a refer scenario: binds charlie as the transfer
/// target. The env carries NO refer wiring anymore — the transfer target
/// resolves through the egress seam (`env.callee("charlie")` → charlie's bound
/// socket under the functional `Transparent` policy), and the `refer_key` is
/// per-run SUT auth data fed into the scenario's construction
/// (`Refer::new(REFER_KEY)`), so `env.refer_authorization(REFER_KEY)` emits
/// `{"refer_key":"refer-allow-c","destination":{…}}` — the same payload the
/// existing `refer_allow` test hand-writes.
fn refer_env<'a>(name: &str, scene: &'a B2buaScene, charlie: &'a Agent) -> CallEnv<'a> {
    CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        Some(charlie),
        scene.b2bua.addr,
        "X-Loadgen-Id",
        format!("{name}-tok"),
    )
}

#[tokio::test(start_paused = true)]
async fn realcall_refer_no_leak() {
    // Since P1 the refer shape's load body is the ACTOR-declared port
    // (per-endpoint reactors + the ack-gated settle barrier), so the leak gate
    // drives the same executor the load fleet does (plan §4.4/§4.5).
    let name = "realcall-refer";
    let (scene, charlie) = refer_scene(name).await;
    let env = refer_env(name, &scene, &charlie);

    run_actor_asserting(&ActorRefer::new(REFER_KEY), &env).await;

    // After alice BYEs, the SUT relays a BYE to BOTH downstream legs (bob + charlie)
    // and reaps the call only once each answers `200`. The scenario's own 150 ms
    // post-BYE drain answers charlie's promptly, but the relayed bob BYE can land
    // just after that window closes (the merge ordering leaves bob's BYE last), so
    // pump both legs again here until they go quiet — answering the straggler BYE
    // so the SUT closes its b-leg instead of waiting out a retransmit timer.
    drain_callees(&scene.bob, &charlie).await;

    // The transfer completed and alice BYE'd: the SUT BYE'd every leg (B + C) and
    // left no call state behind.
    settle_until(|| scene.b2bua.active_calls() == 0).await;
    scene.b2bua.assert_fully_reaped();

    // Waive the §13.2.1/§20.37 SHOULD audit: the media-realign re-INVITEs the SUT
    // sends to alice/charlie are answered by the test PEERS via the harness's
    // best-effort auto-200 (`Agent::quiesce` → `generate_response`), which does not
    // stamp Allow/Supported — a known test-PEER response-generation gap, not a SUT
    // or scenario bug (the SUT's own 200s carry both; the alice/bob-only flows pass
    // the audit unwaived). The existing hand-rolled `refer_allow` tests sidestep it
    // by not gating `passed()`; waiving the one rule lets us keep the hard gate.
    scene.h.allow_violation(
        "rfc3261.allowSupportedOnInvite",
        "realign re-INVITEs are 200'd by the harness peer auto-responder, which \
         omits Allow/Supported (test-peer gap, not a SUT defect)",
    );
    let report = scene.finish().await;
    assert!(report.passed(), "RFC audit failed for `{name}`");
}

// ── Voluntarily-failing leak gate — a FAILED call must still clean up ────────

/// Drive a scenario that FAILS by design and assert the SUT still fully reaped
/// it. The whole point of these flows is that a failed call leaves no leaked
/// dialog/limiter/lock behind, so we use `run_collecting` (which RETURNS the
/// `Err` instead of `panic!`ing on it), assert the run failed as expected, then
/// assert the no-leak invariant. No RFC gate: a deliberately-truncated flow
/// (a CANCELed handshake, a non-2xx final, a declined transfer) legitimately
/// does not satisfy the §13.3.1.4 / cross-message happy-path audit — the leak
/// invariant IS the assertion here, so we gate on that alone.
async fn assert_expected_failure_no_leak(name: &str, scenario: &dyn RealCallScenario, env: &CallEnv<'_>) {
    let result = run_collecting(scenario, env).await;
    assert!(
        result.is_err(),
        "voluntarily-failing scenario `{name}` unexpectedly succeeded"
    );
}

/// As [`assert_expected_failure_no_leak`] but for the alice/bob-only failing
/// scenarios (binds the scene, runs, asserts failure + no leak). The refer
/// failing case ([`ReferCharlieReject`]) needs the charlie scene, so it is wired
/// separately below.
async fn failing_no_leak(name: &str, scenario: &dyn RealCallScenario) {
    let scene = B2buaScene::new(name).await;
    let env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        None,
        scene.b2bua.addr,
        "X-Loadgen-Id",
        format!("{name}-tok"),
    );

    assert_expected_failure_no_leak(name, scenario, &env).await;

    settle_until(|| scene.b2bua.active_calls() == 0).await;
    scene.b2bua.assert_fully_reaped();
    // No RFC gate (see `assert_expected_failure_no_leak`): just drop the scene so
    // the recorded trace is rendered without a happy-path verdict.
    let _ = scene.finish().await;
}

/// The ACTOR-lane twin of [`failing_no_leak`] for an alice/bob-only failing
/// actor body: drive it via the actor runner (which OWNS teardown), assert it
/// surfaced its NOK terminal, and assert the SUT fully reaped.
async fn failing_no_leak_actor(name: &str, scenario: &dyn ActorScenario) {
    let scene = B2buaScene::new(name).await;
    let env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        None,
        scene.b2bua.addr,
        "X-Loadgen-Id",
        format!("{name}-tok"),
    );

    let result = run_actor_collecting(scenario, &env).await;
    assert!(
        result.is_err(),
        "voluntarily-failing actor scenario `{name}` unexpectedly succeeded: {result:?}"
    );

    settle_until(|| scene.b2bua.active_calls() == 0).await;
    scene.b2bua.assert_fully_reaped();
    let _ = scene.finish().await;
}

#[tokio::test(start_paused = true)]
async fn realcall_invite_reject_no_leak() {
    // Bob 486s the INVITE: the final completes the transaction (auto-ACKed), so
    // there is nothing to CANCEL/BYE — the SUT must reap the rejected call.
    // Since P3 the load body is the ACTOR-declared port.
    failing_no_leak_actor("realcall-invite-reject", &ActorInviteReject).await;
}

#[tokio::test(start_paused = true)]
async fn realcall_abandon_ringing_no_leak() {
    // Alice abandons after 180: the scenario drives the full CANCEL handshake
    // (CANCEL → bob 200/487) so the SUT reaps both legs immediately.
    // Since P3 the load body is the ACTOR-declared port.
    failing_no_leak_actor("realcall-abandon-ringing", &ActorAbandonRinging).await;
}

#[tokio::test(start_paused = true)]
async fn realcall_refer_charlie_reject_no_leak() {
    // A↔B establish, B REFERs to C, C 603-declines: the transfer fails but A↔B
    // stays up. Since P3 this is the ACTOR-declared body (per-endpoint reactors +
    // the ack-gated settle barrier), so the runner OWNS teardown — alice BYEs
    // A↔B herself once the decline is observed, and the SUT reaps the
    // born-and-rejected transfer leg. Needs the charlie/refer scene.
    let name = "realcall-refer-charlie-reject";
    let (scene, charlie) = refer_scene(name).await;
    let env = refer_env(name, &scene, &charlie);

    let result = run_actor_collecting(&ActorReferCharlieReject::new(REFER_KEY), &env).await;
    assert!(
        result.is_err(),
        "the declined-transfer body must surface its NOK terminal, got {result:?}"
    );

    // The actor's own teardown BYEs the still-live A↔B; pump bob (and charlie,
    // harmless — its leg was already declined) so the relayed b-leg BYE is answered
    // and the SUT reaps promptly (see the refer happy-path note on the late BYE).
    drain_callees(&scene.bob, &charlie).await;

    settle_until(|| scene.b2bua.active_calls() == 0).await;
    scene.b2bua.assert_fully_reaped();
    let _ = scene.finish().await;
}
