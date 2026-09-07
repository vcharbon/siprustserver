//! Ringing held PAST the default 158 s initial-INVITE bound.
//!
//! With `invite_txn_timeout_sec` raised into the telephony range the b-leg
//! client transaction must keep the callee ringing beyond the old const: the
//! app deadline (SetupTimeout / a route-supplied NoAnswer, both strictly under
//! the configured bound) owns the give-up ordering — clean CANCEL→487→ACK on
//! the b-leg, exactly ONE final to the caller — and an answer landing after
//! 158 s establishes end to end. A NoAnswer at/above the bound is clamped to
//! `bound − NO_ANSWER_CANCEL_MARGIN_SEC` at arming.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{invite_final_statuses, settle_until, B2buaScene, B2buaSut};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// A ringing call held past 158 s must NOT be CANCELed by the transaction
/// layer; at the 300 s SetupTimeout the app gives up cleanly — CANCEL→487→ACK
/// toward bob, exactly one final (408) to alice — and everything is reaped.
#[tokio::test(start_paused = true)]
async fn ring_past_158s_gives_up_cleanly_at_the_app_deadline() {
    let s = B2buaScene::with_b2bua("long-ring-app-deadline", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.invite_txn_timeout_sec = 400;
            c.setup_timeout_sec = 300;
            // No keepalive/reaper tuning: the derived reaper idle window is
            // floored above the configured bound (`reaper_idle_max_ms`), so
            // the quiet 300 s ring is reaper-safe by construction.
        })
    })
    .await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // 200 s of ringing — well past the old 158 s const. The raised bound must
    // hold: no transaction-layer CANCEL toward bob, call still in setup.
    s.h.advance(Duration::from_secs(200)).await;
    assert!(
        s.bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "no CANCEL while ringing inside the raised transaction bound",
    );
    assert_eq!(
        s.b2bua.metrics().creations_total() - s.b2bua.metrics().removals_total(),
        1,
        "the long-ringing call is still up",
    );

    // Cross ONLY the 300 s SetupTimeout: the app gives up in order.
    s.h.advance(Duration::from_secs(101)).await;
    let mut cancel = s.bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    let final_resp = call.expect(408).await;
    assert_eq!(
        final_resp.status(),
        408,
        "caller's INVITE resolves at the setup deadline"
    );
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();

    let alice_addr = s.alice.addr();
    let report = s.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice_addr),
        vec![408],
        "exactly ONE final on alice's initial INVITE",
    );
}

/// An answer landing AFTER 158 s of ringing (invite 400 / setup 350) must
/// establish end to end: 200→ACK, bridged, clean BYE teardown, fully reaped.
#[tokio::test(start_paused = true)]
async fn answer_after_158s_establishes_end_to_end() {
    let s = B2buaScene::with_b2bua("long-ring-late-answer", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.invite_txn_timeout_sec = 400;
            c.setup_timeout_sec = 350;
        })
    })
    .await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Ring ~200 s (past the old 158 s const), then answer — inside both the
    // 350 s setup deadline and the 400 s transaction bound.
    s.h.advance(Duration::from_secs(200)).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    assert_eq!(
        s.b2bua.metrics().creations_total() - s.b2bua.metrics().removals_total(),
        1,
        "call established after a 200 s ring",
    );

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    s.finish().await;
}

/// A route-supplied `NoAnswer` at/above the configured bound (250 vs 200) is
/// clamped at arming to `bound − NO_ANSWER_CANCEL_MARGIN_SEC` = 192 s: still
/// ringing just before 192 s, clean CANCEL→487→ACK just past it, one final to
/// alice (the no-answer reject branch's ADR-0022 synthesis), fully reaped.
#[tokio::test(start_paused = true)]
async fn no_answer_at_or_above_the_bound_is_clamped_to_the_margin() {
    let s = B2buaScene::with_b2bua("long-ring-clamp", |bob_port| {
        let decision: Arc<dyn CallDecisionEngine> = Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    let mut r = route_to("127.0.0.1", bob_port);
                    // 250 s meets/exceeds the 200 s bound → clamped to 192 s.
                    r.no_answer_timeout_sec = Some(250);
                    NewCallResponse::Route(r)
                })
                .build(),
        );
        B2buaSut::builder(decision).tune(|c| {
            c.invite_txn_timeout_sec = 200;
            // Disable the setup deadline so the clamped NoAnswer is
            // unambiguously what tears the ring down.
            c.setup_timeout_sec = 0;
        })
    })
    .await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Just BEFORE the clamped 192 s deadline: still ringing, no CANCEL.
    s.h.advance(Duration::from_secs(191)).await;
    assert!(
        s.bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "no CANCEL before the clamped 192 s deadline",
    );
    assert_eq!(
        s.b2bua.metrics().creations_total() - s.b2bua.metrics().removals_total(),
        1,
        "still ringing just before the clamped deadline",
    );

    // Cross ONLY the clamped 192 s deadline (the un-clamped 250 s — and the
    // 200 s bound — are never reached).
    s.h.advance(Duration::from_secs(2)).await;
    let mut cancel = s.bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    // The reject branch holds the caller's final until the CANCELed b-leg
    // quiesces (487 above) — then the ADR-0022 synthesis answers the a-leg.
    let final_resp = call.expect(503).await;
    assert_eq!(
        final_resp.status(),
        503,
        "caller's INVITE resolves at the clamped no-answer deadline",
    );

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();

    let alice_addr = s.alice.addr();
    let report = s.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice_addr),
        vec![503],
        "exactly ONE final on alice's initial INVITE",
    );
}
