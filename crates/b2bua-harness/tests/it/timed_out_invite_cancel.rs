//! A b-leg INVITE whose client transaction gives up (RFC 3261 §17.1.1.2) is
//! abandoned the §9.1 way: a callee that answered a provisional is CANCELed,
//! and the transaction lives on to ACK the 487 that CANCEL provokes
//! (§17.1.1.3) and to resolve the leg on it. The caller is answered in the
//! turn the transaction gives up, before its own server transaction can age
//! out.

use std::time::Duration;

use b2bua_harness::{invite_final_statuses, settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;

use crate::common::unrun::one_permit_one_deep_with;

/// The INVITE bound at its floor, with no setup deadline under it: the
/// transaction's give-up is what ends the ring.
const BOUND_SEC: i64 = 33;

/// bob rings, then goes silent past the INVITE bound. alice is answered 408
/// at the bound; bob is CANCELed, answers 200 + 487, and the 487 is ACKed.
#[tokio::test(start_paused = true)]
async fn a_callee_silent_past_the_invite_bound_is_cancelled_and_its_487_acked() {
    let s = B2buaScene::with_b2bua("timed-out-invite-487-acked", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.invite_txn_timeout_sec = BOUND_SEC;
            c.setup_timeout_sec = 0;
            c.keepalive_interval_sec = 300;
        })
    })
    .await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    s.h.advance(Duration::from_secs(BOUND_SEC as u64) + Duration::from_millis(300)).await;
    call.try_expect(408).await.expect("the caller is answered when the transaction gives up");
    let mut cancel = s.bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    s.bob.try_receive("ACK").await.expect("the 487 the CANCEL provoked is ACKed");

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let alice_addr = s.alice.addr();
    let report = s.finish().await;
    assert_eq!(invite_final_statuses(&report, alice_addr), vec![408], "one final to alice");
}

/// The same silence while a parked call holds the only handler permit: the
/// Timeout waits past the full queue until the permit frees at 40.8 s, and
/// alice is answered then, well before her INVITE server transaction ages out
/// (the bound plus the sweep's 35 s).
#[tokio::test(start_paused = true)]
async fn a_timeout_run_late_still_answers_the_caller_and_acks_the_487() {
    let s = one_permit_one_deep_with("timed-out-invite-late-turn", |c| {
        c.invite_txn_timeout_sec = BOUND_SEC;
        c.setup_timeout_sec = 0;
        c.call_control_timeout_ms = 40_000;
        c.keepalive_interval_sec = 300;
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_secs(40)).await;

    // carol's decision deadline frees the permit; the Timeout runs.
    parked.expect(503).await;
    call.try_expect(408).await.expect("the caller is answered once the Timeout runs");
    let mut cancel = s.bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    s.bob.try_receive("ACK").await.expect("the 487 the CANCEL provoked is ACKed");

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// bob answers nothing at all, not even a 100. The hop is dead at the
/// first-response bound: alice is answered 408 there, and under the strict
/// §9.1 wait no CANCEL goes to bob, who answered nothing. The leg his
/// silence leaves unresolved rides the terminating backstop out.
#[tokio::test(start_paused = true)]
async fn a_callee_that_answers_nothing_is_not_cancelled_and_the_caller_is_answered() {
    let s = B2buaScene::with_b2bua("timed-out-invite-dead-hop", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.invite_first_response_timeout_sec = 5;
            c.cancel_strict_rfc3261_wait = true;
            c.keepalive_interval_sec = 300;
        })
    })
    .await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let _dead = s.bob.receive("INVITE").await;

    s.h.advance(Duration::from_secs(5) + Duration::from_millis(300)).await;
    call.try_expect(408).await.expect("the caller is answered at the first-response bound");

    s.h.advance(Duration::from_millis(call::helpers::TERMINATING_TIMEOUT_MS as u64 + 1_000)).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let bob_addr = s.bob.addr();
    let report = s.finish().await;
    assert!(
        !report.entries().iter().any(|e| e.to == bob_addr && e.raw.starts_with(b"CANCEL ")),
        "no CANCEL to a callee that answered nothing (RFC 3261 §9.1)"
    );
}
