//! **A relayed PRACK crossing the callee's INVITE final is still answered**
//! (RFC 3262 §3, RFC 3261 §8.2.6: every non-INVITE request draws a final).
//!
//! The caller PRACKs a relayed reliable provisional; before answering the
//! PRACK the callee rejects the INVITE. The rejection ends the early dialog
//! on both faces, but the caller's PRACK transaction is still open: her ACK
//! to the final may wait on it, so an unanswered PRACK leaves the final
//! retransmitting to the give-up bound. The callee will never answer into a
//! leg that is gone, so the B2BUA answers the caller's PRACK itself, 200: it
//! named a provisional this stack showed under its own number, and as that
//! face's UAS the stack acknowledges it. The callee's own late answer (§3's
//! 481 once he no longer holds the provisional) is absorbed.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, ServerTxn, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// Bob's own sequence, far from anything this stack mints.
const BOB_RSEQ: u32 = 4711;

fn reliable_180(uas: &mut ServerTxn) -> scenario_harness::Respond<'_> {
    uas.respond(180, "Ringing")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
}

async fn reaped(b2bua: &B2buaSut) {
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
}

/// The callee's 481 to the PRACK arrives after his 600: the caller's PRACK
/// was already answered by this stack, and the late 481 is absorbed.
///
/// ```text
///   INVITE(100rel) → 180(100rel, RSeq) → PRACK ⇒ PRACK
///   600 ⇐ 600 ; ACK ⇒ ACK           [the final crosses the PRACK's answer]
///   200(PRACK) ⇐                     [this stack's, as the a-face UAS, behind the final]
///              ⇐ 481(PRACK)          [absorbed]
/// ```
#[tokio::test(start_paused = true)]
async fn the_callees_late_prack_answer_after_his_final_is_absorbed() {
    let h = Harness::with_transit_delay("b2bua-prack-crossing-final-relayed", 0);
    // On the wire bob's 481 contradicts the reliable 180 he sent, so the audit
    // reads it as a §3 breach — which is the fixture: after his own final he
    // no longer holds that provisional. Waived on bob alone, so every B2BUA
    // bind stays gated.
    h.waive(
        WaiverScope::rule(
            "prack-2xx-or-481",
            "bob deliberately answers the relayed PRACK 481 after his final (RFC 3262 §3's \
             answer for a provisional he no longer holds) — absorbing it is what this test \
             measures",
        )
        .on_party("bob"),
    );
    let alice = h.agent("alice", "127.0.0.1:5461").await;
    let bob = h.agent("bob", "127.0.0.1:5471").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5471)))
            .start(&h, "b2bua", "127.0.0.1:5481")
            .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    let ringing = call.expect(180).await;

    // ── alice PRACKs; the PRACK reaches bob, who rejects the INVITE first ──
    let mut prack = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    let mut bob_prack = bob.receive("PRACK").await;
    uas.respond(600, "Busy Everywhere").await;
    call.expect(600).await; // auto-ACKed on the INVITE's branch (§17.1.1.3)
    bob.receive("ACK").await;
    let answer = prack.expect(200).await;
    assert_eq!(answer.cseq().method(), sip_message::Method::Prack);

    // ── bob answers the PRACK only now: §3's 481, the provisional is gone ──
    bob_prack.respond(481, "Call/Transaction Does Not Exist").await;

    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let _report = h.finish().await;
}

/// The callee never answers the PRACK at all: the caller's PRACK is answered
/// by this stack alone, and the call is reaped clean.
#[tokio::test(start_paused = true)]
async fn an_unanswered_relayed_prack_is_answered_by_the_stack_after_the_final() {
    let h = Harness::with_transit_delay("b2bua-prack-crossing-final-unanswered", 0);
    let alice = h.agent("alice", "127.0.0.1:5462").await;
    let bob = h.agent("bob", "127.0.0.1:5472").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5472)))
            .start(&h, "b2bua", "127.0.0.1:5482")
            .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    let ringing = call.expect(180).await;

    let mut prack = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    let _bob_prack = bob.receive("PRACK").await;
    uas.respond(600, "Busy Everywhere").await;
    call.expect(600).await;
    bob.receive("ACK").await;
    // Nothing from bob. The caller's PRACK still draws a final from this stack,
    // behind the INVITE's own.
    let answer = prack.expect(200).await;
    assert_eq!(answer.cseq().method(), sip_message::Method::Prack);

    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let _report = h.finish().await;
}
