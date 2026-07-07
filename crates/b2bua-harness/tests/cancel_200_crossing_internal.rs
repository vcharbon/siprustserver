//! **Crossing 200 OK / internally-originated CANCEL** — the timer-driven sibling
//! of `cancel_200_crossing.rs` (which covers the explicit *caller*-CANCEL).
//!
//! When the B2BUA CANCELs a still-ringing b-leg for an INTERNAL reason (the
//! no-answer timer, or a pending-INVITE transaction timeout), the callee's 200
//! OK can cross the CANCEL on the wire. On the explicit-CANCEL path the b-leg is
//! marked `Cancelling` and the `cancel-200-crossing` rule ACK+BYEs the crossing
//! 200; the internal paths (`DestroyLeg`) previously set only `bye_disposition`,
//! so a crossing 200 matched no rule and the late-answering callee was orphaned
//! in a one-sided established dialog. The fix marks the leg `Cancelling` on the
//! internal paths too, so the reap is uniform regardless of who CANCELed.
//!
//! Both scenarios need the call to OUTLIVE the CANCEL so a live call exists to
//! reap the crossing 200 — i.e. a **failover-capable** call: `DestroyLeg` +
//! `/call/failure` consult keeps the call Active while the reroute is decided
//! (the non-failover path tears the whole call down in the same turn, before any
//! 200 could arrive). The abandoned callee is reaped and the reroute proceeds to
//! the second target, caller flow unchanged.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::{route_to, route_to_with_18x};
use b2bua::decision::{CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The **no-answer timer** fires on a ringing b-leg → the B2BUA CANCELs it and
/// consults `/call/failure` (call stays Active). The callee then answers 200,
/// crossing the CANCEL. The abandoned callee MUST be reaped (ACK + BYE), and the
/// failover reroute to the second target MUST proceed as normal.
#[tokio::test(start_paused = true)]
async fn no_answer_cancel_crossed_by_200_reaps_the_abandoned_callee_and_failover_proceeds() {
    // 1 ms transit so the reroute INVITE is answered inside its Timer A window
    // (mirrors decision_context's timeout-failover reroute).
    let h = Harness::with_transit_delay("noanswer-cancel-200-crossing", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // rings, no-answer'd, answers late
    let bob = h.agent("bob", "127.0.0.1:5071").await; // reroute target

    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                // Short no-answer deadline + a callback context so the no-answer
                // path CANCELs the ringing leg AND keeps the call alive for the
                // /call/failure consult (the reroute below).
                r.no_answer_timeout_sec = Some(30);
                r.callback_context = Some("noanswer-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| CallTreatment::Route(route_to("127.0.0.1", 5071)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            // Setup deadline past the no-answer so the NoAnswer timer is what
            // fires; keepalive far out for a quiet tail; reaper off to isolate
            // the crossing-200 mechanism from the liveness sweep.
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Trip the no-answer timer (30 s) but stop just past it, so the reroute
    // INVITE (emitted on the /call/failure result) is still inside its 500 ms
    // Timer A window when we answer it below.
    h.advance(Duration::from_secs(30) + Duration::from_millis(300)).await;

    // The B2BUA CANCELs the ringing b-leg.
    let mut cancel = carol.receive("CANCEL").await;
    cancel.respond(200, "OK").await;

    // ── CROSSING: carol answers 200 OK, crossing the CANCEL on the wire ───────
    // Pre-fix: no rule matched this 200 (the leg was Terminated, not
    // `Cancelling`) → carol orphaned in a one-sided established dialog.
    // Now `cancel-200-crossing` reaps it: ACK then immediate BYE.
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    carol.receive("ACK").await;
    let mut bye = carol.receive("BYE").await;
    bye.respond(200, "OK").await;

    // ── Caller flow unchanged: the failover reroute reaches bob, who answers ──
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    let confirmed = call.expect(200).await;
    assert_eq!(confirmed.status, 200, "caller bridged to the reroute target");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Clean teardown of the surviving bridged call.
    let mut d_bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    d_bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The **reject / no-failover sibling** — the case the call-liveness ordering fix
/// exists for. The same no-answer timer fires on a ringing b-leg, but the route
/// carries NO callback context, so `/call/failure` is never consulted: the
/// `no-answer` rule takes its `None` branch (`DestroyLeg` + `BeginTermination`).
///
/// Pre-fix, that teardown promoted `Terminating → Terminated → RemoveCall` in the
/// SAME turn as the CANCEL (the CANCELled b-leg's interim `Cancelled` bye
/// disposition read terminal), so a `200 OK` crossing the CANCEL landed on a
/// REMOVED call: no ACK, no BYE — the answering callee orphaned in a one-sided
/// established dialog. The fix keeps a `Cancelling` leg UNRESOLVED
/// (`leg_is_resolved`), so finalization HOLDS while the internal CANCEL is in
/// flight and the crossing 200 still has a live call to be reaped against —
/// `cancel-200-crossing` ACK+BYEs the abandoned callee — and the caller's reject
/// is delivered once that callee has quiesced (the spec's callee-reap-before-
/// caller-reject ordering). Uniform with the failover path above, no failover.
#[tokio::test(start_paused = true)]
async fn no_answer_reject_cancel_crossed_by_200_reaps_the_abandoned_callee() {
    let h = Harness::with_transit_delay("noanswer-reject-cancel-200-crossing", 1);
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let carol = h.agent("carol", "127.0.0.1:5072").await; // rings, no-answer'd, answers late

    // Reject path: a short no-answer deadline but NO callback context, so the
    // no-answer teardown rejects the caller WITHOUT consulting /call/failure.
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5072);
                r.no_answer_timeout_sec = Some(30);
                NewCallResponse::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            // Setup deadline past the no-answer so the NoAnswer timer is what
            // fires; keepalive far out; reaper off to isolate the crossing-200
            // mechanism from the liveness sweep.
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5082")
        .await;

    let mut call = alice.invite(&carol).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Trip the no-answer timer (30 s).
    h.advance(Duration::from_secs(30) + Duration::from_millis(300)).await;

    // The B2BUA CANCELs the ringing b-leg. The call MUST outlive the CANCEL —
    // pre-fix it was already gone in this same turn.
    let mut cancel = carol.receive("CANCEL").await;
    cancel.respond(200, "OK").await;

    // ── CROSSING: carol answers 200 OK, crossing the CANCEL on the wire ───────
    // The abandoned callee MUST be reaped — ACK then immediate BYE — not orphaned.
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    carol.receive("ACK").await;
    let mut bye = carol.receive("BYE").await;
    bye.respond(200, "OK").await;

    // The caller still gets its final reject — synthesized once the abandoned
    // callee has quiesced (ADR-0022; the no-answer/None path answers via the
    // →Terminated funnel), NOT dropped on the removed call.
    let failed = call.expect(503).await;
    assert_eq!(failed.status, 503, "caller's INVITE resolves with a final failure");

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// **Composition regression (newkahneed-024):** the same no-answer CANCEL /
/// crossing-200 flow with the `relayFirst18xTo180` **drop-sdp** machine armed.
/// Pre-fix, the SERVICE_LAYER 2xx rule (`force-tag-consistency`, active in
/// `Masking`/`Suppressing`) out-ranked CORE `cancel-200-crossing` and
/// relayed/merged the crossing 200 into the being-rejected a-leg — orphaning
/// the late-answering callee exactly like 012/016, resurrected by the machine
/// composition. Now the 2xx rule defers on a `Cancelling` source leg: the
/// crossing 200 is reaped (ACK + BYE), and the masking property survives the
/// reap — the reroute target's 200 still reuses the first bare 180's To-tag.
#[tokio::test(start_paused = true)]
async fn drop_sdp_no_answer_cancel_crossed_by_200_reaps_the_abandoned_callee() {
    let h = Harness::with_transit_delay("dropsdp-noanswer-cancel-200-crossing", 1);
    let alice = h.agent("alice", "127.0.0.1:5064").await;
    let carol = h.agent("carol", "127.0.0.1:5074").await; // rings, no-answer'd, answers late
    let bob = h.agent("bob", "127.0.0.1:5075").await; // reroute target

    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r =
                    route_to_with_18x("127.0.0.1", 5074, RelayFirst18xStrategy::DropSdp);
                r.no_answer_timeout_sec = Some(30);
                r.callback_context = Some("noanswer-ctx".into());
                NewCallResponse::Route(r)
            })
            // The reroute keeps drop-sdp armed (failover/initial-route feature
            // parity), so the masking machine stays active across the leg swap.
            .on_failure(|_| {
                CallTreatment::Route(route_to_with_18x(
                    "127.0.0.1",
                    5075,
                    RelayFirst18xStrategy::DropSdp,
                ))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5084")
        .await;

    let mut call = alice.invite(&carol).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut carol_uas = carol.receive("INVITE").await;

    // Carol rings 183 WITH SDP; the drop-sdp machine masks it: alice sees a
    // bare 180 (no body) under a minted a-facing To-tag.
    carol_uas.respond(183, "Session Progress").with_sdp(ANSWER).await;
    let p180 = call.expect(180).await;
    assert!(p180.body.is_empty(), "drop-sdp masked the 183 into a bare 180");
    let first_to_tag = p180.to.tag.clone().expect("bare 180 has a To-tag");

    // Trip the no-answer timer (30 s); stay inside the reroute INVITE's Timer A
    // window (mirrors the plain no-answer test above).
    h.advance(Duration::from_secs(30) + Duration::from_millis(300)).await;

    // The B2BUA CANCELs the ringing b-leg.
    let mut cancel = carol.receive("CANCEL").await;
    cancel.respond(200, "OK").await;

    // ── CROSSING: carol answers 200 OK, crossing the CANCEL on the wire ───────
    // Pre-fix `force-tag-consistency` claimed this 2xx (the machine was in
    // `Suppressing`) and bridged the CANCELled callee to alice. It must be
    // reaped instead: ACK then immediate BYE.
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    carol.receive("ACK").await;
    let mut bye = carol.receive("BYE").await;
    bye.respond(200, "OK").await;

    // ── Caller flow unchanged: the reroute reaches bob under the same mask ────
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await; // suppressed (mask already out)
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(
        ok.to.tag.as_deref(),
        Some(first_to_tag.as_str()),
        "200 To-tag reuses the first bare 180's tag across the reap + leg swap",
    );
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Clean teardown of the surviving bridged call.
    let mut d_bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    d_bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The sibling internal trigger: a **pending b-leg INVITE transaction timeout**
/// (Timer B / the long-INVITE backstop) CANCELs the ringing leg through the same
/// `DestroyLeg` path and consults `/call/failure`. A crossing 200 must be reaped
/// identically. Proves the treatment is uniform across the internal CANCEL
/// origination sites, not just the no-answer timer.
#[tokio::test(start_paused = true)]
async fn transaction_timeout_cancel_crossed_by_200_reaps_the_abandoned_callee() {
    let h = Harness::with_transit_delay("txn-timeout-cancel-200-crossing", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // rings, dead air, then answers late
    let bob = h.agent("bob", "127.0.0.1:5071").await; // reroute target

    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("txn-timeout-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| CallTreatment::Route(route_to("127.0.0.1", 5071)))
            .build(),
    );
    // Setup deadline past the sip-txn INVITE backstop (158 s) so the transaction
    // timeout is what fires; keepalive far out; reaper off.
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.setup_timeout_sec = 300;
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Dead gateway: carol rings then goes silent. At the sip-txn INVITE backstop
    // (158 s) the b-leg transaction times out → DestroyLeg CANCELs the ringing
    // leg and /call/failure is consulted (call stays Active). Stop just past the
    // deadline so the reroute INVITE lands inside its Timer A window.
    h.advance(Duration::from_secs(158) + Duration::from_millis(300)).await;

    let mut cancel = carol.receive("CANCEL").await;
    cancel.respond(200, "OK").await;

    // ── CROSSING: carol answers 200 OK, crossing the CANCEL ───────────────────
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    carol.receive("ACK").await;
    let mut bye = carol.receive("BYE").await;
    bye.respond(200, "OK").await;

    // ── Reroute reaches bob, who answers; caller flow unchanged ───────────────
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut d_bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    d_bye.expect(200).await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
