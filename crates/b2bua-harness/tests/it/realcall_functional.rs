//! Functional leak gate over the **shared** actor-declared scenarios.
//!
//! These are the SAME `scenario_harness::actor::scenarios` bodies the load
//! generator drives at 20–100 cps against the real cluster — here run ONCE,
//! in-process, against a real `B2buaCore`, on a paused clock.
//! `run_actor_asserting` panics on any deviation (the strict analogue of the
//! load driver's count-and-classify), then we assert the SUT leaked no call
//! state. So a scenario that leaks a dialog, holds a limiter token, or
//! mis-tears-down is caught deterministically in CI — no cluster, no soak — and
//! the realistic 180→200 / re-INVITE / pre-BYE dwells are exercised for free
//! (the sleeps auto-advance under `start_paused`).
//!
//! This is the proof that one scenario definition feeds both lanes — every
//! scenario is covered here: the happy-path flows AND the abnormal-end ones (a
//! reject, an abandon, a declined transfer — each its own accepted branch,
//! where a call that ends early yet cleans up is the leak gate's most valuable
//! assertion). As more flows land in `actor::scenarios`, add a case here per
//! flow.

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::actor::scenarios::{
    AbandonRinging as ActorAbandonRinging, BasicCall as ActorBasicCall,
    InviteReject as ActorInviteReject, LongCall as ActorLongCall, OptionsHold as ActorOptionsHold,
    PrackUpdate as ActorPrackUpdate, Refer as ActorRefer,
    ReferCharlieReject as ActorReferCharlieReject, Reinvite as ActorReinvite,
};
use scenario_harness::actor::ActorScenario;
use scenario_harness::realcall::{run_actor_asserting, CallEnv};
use scenario_harness::Agent;

// The REFER backend the b2bua's scripted `/call/refer` authorizes (see
// `ScriptedDecisionEngine::route_all_with_refer` → `default_call_refer`); the
// refer scenarios' `X-Api-Call` must carry this `refer_key` to be allowed.
const REFER_KEY: &str = "refer-allow-c";
/// Canonical charlie (transfer-target) port — distinct from alice/bob/b2bua.
const CHARLIE_PORT: u16 = 5090;

/// Pump both callee legs (bob + charlie) for post-flow BYE stragglers,
/// CONCURRENTLY and STRICTLY. Used after a multi-leg (REFER) teardown so a
/// relayed b-leg BYE that lands just after the scenario's own short drain window
/// is still answered, letting the SUT reap its downstream legs without waiting
/// out a retransmit timer. These legs are still LIVE dialog members of a
/// successful call, so the drain answers ONLY a BYE (properly, via `ServerTxn`)
/// and fails the test on any other traffic — never the failed-call release
/// policy (`Agent::release_failed_call`). A 1 s window comfortably outlasts the
/// relayed BYE's transit + the first retransmit under the harness fabric.
/// Concurrent (not sequential) so a BYE to one leg is not missed while we drain
/// the other.
async fn drain_callees(bob: &Agent, charlie: &Agent) {
    let window = std::time::Duration::from_secs(1);
    tokio::join!(bob.drain_expecting(window, &["BYE"]), charlie.drain_expecting(window, &["BYE"]));
}

// ── Happy-path leak gate (alice/bob only) ──────────────────────────────────

/// Drive an alice/bob-only actor body through an in-process B2BUA via the actor
/// runner and assert it left no call state behind, then gate the RFC audit. All
/// the shipped happy load bodies are ACTOR-declared since P3.
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

    assert_eq!(run_actor_asserting(scenario, &env).await, "answered");

    settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();

    let report = scene.finish().await;
    assert!(report.passed(), "RFC audit failed for `{name}`");
}

/// A forked establishment through the SUT's transparent relay: three forks
/// ring under distinct tags, the middle one answers.
#[tokio::test(start_paused = true)]
async fn realcall_forked_no_leak() {
    let shape = callshapes::shapes::forked(callshapes::shapes::default_binder());
    assert_no_leak_actor("realcall-forked", &shape).await;
}

/// A forked establishment answered by a fork that sent no provisional
/// (RFC 3261 §12.1.2, §13.2.2.4): the 2xx carries a To-tag no 18x announced.
#[tokio::test(start_paused = true)]
async fn realcall_forked_unrung_no_leak() {
    let shape = callshapes::shapes::forked_unrung(callshapes::shapes::default_binder());
    assert_no_leak_actor("realcall-forked-unrung", &shape).await;
}

#[tokio::test(start_paused = true)]
async fn realcall_basic_call_no_leak() {
    // Since P3 the basic_call load body is the ACTOR-declared port.
    assert_no_leak_actor("realcall-basic", &ActorBasicCall).await;
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
    let charlie =
        scene.h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await.transfer_target();
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

    assert_eq!(run_actor_asserting(&ActorRefer::new(REFER_KEY), &env).await, "answered");

    // After alice BYEs, the SUT relays a BYE to BOTH downstream legs (bob + charlie)
    // and reaps the call only once each answers `200`. The scenario's own 150 ms
    // post-BYE drain answers charlie's promptly, but the relayed bob BYE can land
    // just after that window closes (the merge ordering leaves bob's BYE last), so
    // pump both legs again here until they go quiet — answering the straggler BYE
    // so the SUT closes its b-leg instead of waiting out a retransmit timer.
    drain_callees(&scene.bob, &charlie).await;

    // The transfer completed and alice BYE'd: the SUT BYE'd every leg (B + C) and
    // left no call state behind.
    settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();

    // No waiver: every media-realign re-INVITE the SUT sends is answered by the
    // alice/charlie ACTOR reactors (`ServerTxn::respond`, which stamps
    // Allow/Supported per §13.2.1/§20.37), so the audit is clean unwaived — the
    // straggler drain above only ever answers a relayed BYE.
    let report = scene.finish().await;
    assert!(report.passed(), "RFC audit failed for `{name}`");
}

// ── Abnormal-end leak gate — a call that ends early must still clean up ──────

/// Drive an alice/bob-only abnormal-end actor body via the actor runner (which
/// OWNS teardown), assert it ended on its accepted `branch`, and assert the SUT
/// fully reaped under the RFC audit. The declined-transfer case
/// ([`ActorReferCharlieReject`]) needs the charlie scene, so it is wired
/// separately below.
async fn abnormal_end_no_leak_actor(name: &str, scenario: &dyn ActorScenario, branch: &str) {
    let scene = B2buaScene::new(name).await;
    let env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        None,
        scene.b2bua.addr,
        "X-Loadgen-Id",
        format!("{name}-tok"),
    );

    assert_eq!(run_actor_asserting(scenario, &env).await, branch, "`{name}`");

    let report = scene.finish().await;
    assert!(report.passed(), "RFC audit failed for `{name}`");
}

#[tokio::test(start_paused = true)]
async fn realcall_invite_reject_no_leak() {
    // Bob 486s the INVITE: the final completes the transaction (auto-ACKed), so
    // there is nothing to CANCEL/BYE — the SUT must reap the rejected call.
    // Since P3 the load body is the ACTOR-declared port.
    abnormal_end_no_leak_actor("realcall-invite-reject", &ActorInviteReject, "rejected").await;
}

#[tokio::test(start_paused = true)]
async fn realcall_abandon_ringing_no_leak() {
    // Alice abandons after 180: the scenario drives the full CANCEL handshake
    // (CANCEL → bob 200/487) so the SUT reaps both legs immediately.
    // Since P3 the load body is the ACTOR-declared port.
    abnormal_end_no_leak_actor("realcall-abandon-ringing", &ActorAbandonRinging, "abandoned").await;
}

/// The abandon's branch through the SUT when bob answers `ring` after his
/// 180, every hop one transit long, and whether bob sent a 2xx to the INVITE.
async fn abandon_with_ring(name: &str, ring: std::time::Duration) -> (&'static str, bool) {
    let scene = B2buaScene::new(name).await;
    let mut env = CallEnv::for_functional(
        &scene.alice,
        &scene.bob,
        None,
        scene.b2bua.addr,
        "X-Loadgen-Id",
        format!("{name}-tok"),
    );
    env.ring_delay = ring;
    let bob = scene.bob.addr();
    let branch = run_actor_asserting(&ActorAbandonRinging, &env).await;
    settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    let report = scene.finish().await;
    assert!(report.passed(), "RFC audit failed for `{name}`");
    let bob_answered = report.entries().iter().any(|e| {
        let text = String::from_utf8_lossy(&e.raw);
        e.from == bob && text.starts_with("SIP/2.0 2") && text.contains(" INVITE\r\n")
    });
    (branch, bob_answered)
}

/// The two abandon endings timing alone decides through the SUT. With no ring
/// the 200 trails the 180 by nothing and reaches alice before her CANCEL
/// leaves: `answered_before_cancel`. With a ring of 2.5 transits her CANCEL
/// (sent on the 180, two transits after bob sent it) reaches the SUT after
/// bob's 200 left him but before it arrives: the SUT answers her CANCEL 200
/// and her INVITE 487, and tears bob's answered leg down — `abandoned`, with
/// bob's answer on the wire. The no-ring case repeats: the order of two
/// messages landing in one instant must not decide it.
#[tokio::test(start_paused = true)]
async fn realcall_abandon_ringing_ending_follows_the_answer_timing() {
    let transit = std::time::Duration::from_millis(scenario_harness::SIMULATED_TRANSIT_DELAY_MS);
    for i in 0..8 {
        assert_eq!(
            abandon_with_ring(&format!("realcall-abandon-no-ring-{i}"), std::time::Duration::ZERO)
                .await,
            ("answered_before_cancel", true),
            "run {i}"
        );
    }
    assert_eq!(
        abandon_with_ring("realcall-abandon-cancel-meets-the-200", transit * 5 / 2).await,
        ("abandoned", true)
    );
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

    let branch = run_actor_asserting(&ActorReferCharlieReject::new(REFER_KEY), &env).await;
    assert_eq!(branch, "transfer_declined");

    // The actor's own teardown BYEs the still-live A↔B; pump bob (and charlie,
    // harmless — its leg was already declined) so the relayed b-leg BYE is answered
    // and the SUT reaps promptly (see the refer happy-path note on the late BYE).
    drain_callees(&scene.bob, &charlie).await;

    let report = scene.finish().await;
    assert!(report.passed(), "RFC audit failed for `{name}`");
}
