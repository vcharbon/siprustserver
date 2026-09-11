//! **A stray a-leg ACK never becomes a b-leg ACK.** RFC 3261 §17.1.1.3: the ACK
//! for a non-2xx belongs to the INVITE TRANSACTION that drew the reject and
//! reuses its branch — it is hop-local and a B2BUA never relays it. Relaying
//! one would put an ACK on a b-leg INVITE transaction that has drawn no final
//! response at all, which is the defect this pins (capture
//! `merge-7f50f470-…-d69904a894a2`, a-leg msg 21 → b-leg).
//!
//! The trigger is real SBC traffic: after ACKing a 487 on the INVITE's own
//! branch, the caller emits a SECOND ACK on a FRESH branch. The first is
//! absorbed by the a-leg's INVITE server transaction; the second matches no
//! transaction, reaches the router as an in-dialog request, and must be
//! ABSORBED there — the b-leg holds only an early dialog.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// Every ACK this UA has taken off the wire, whatever absorbed it.
fn acks_seen(agent: &scenario_harness::Agent) -> usize {
    agent.wire_view().iter().filter(|e| e.start_line().starts_with("ACK ")).count()
}

#[tokio::test(start_paused = true)]
async fn a_leg_stray_ack_is_absorbed_and_never_acks_the_unanswered_b_leg() {
    let h = Harness::new("b2bua-stray-ack-during-cancel");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    // The deliberate corner: alice re-emits her ACK on a NEW branch, which
    // RFC 3261 §17.1.1.3 forbids (the ACK reuses the INVITE's). That
    // non-compliance IS the test's subject, so it is waived on alice alone.
    // Conditional: the audit's ACK rules pair an ACK with the INVITE sent on
    // the SAME branch, so a fresh-branch stray is no occasion and today
    // produces no finding for this waiver to filter.
    h.waive(
        WaiverScope::rule(
            "ack-preserves-invite-route",
            "alice re-emits her 487 ACK on a fresh branch (the captured SBC's second ACK) — the stray under test",
        )
        .on_party("alice")
        .conditional(),
    );

    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071)))
            .start(&h, "b2bua", "127.0.0.1:5081")
            .await;

    // ── a ringing call ───────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    // ── alice CANCELs; the a-leg is released by the transaction layer, and the
    //    §17.1.1.3 ACK to the 487 goes out on the INVITE's own branch ─────────
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    let rejected = call.expect(487).await;

    // ── the b-leg CANCEL is answered, but bob has NOT yet sent its 487 ───────
    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;

    // ── the stray: a second ACK, same dialog, FRESH branch ───────────────────
    let stray = format!(
        "ACK {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {alice_addr};branch=z9hG4bK-alice-stray-ack\r\n\
         Max-Forwards: 70\r\n\
         From: <{from_uri}>;tag={from_tag}\r\n\
         To: <{to_uri}>;tag={to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} ACK\r\n\
         Content-Length: 0\r\n\r\n",
        ruri = call.ruri(),
        alice_addr = alice.addr(),
        from_uri = rejected.from().uri(),
        from_tag = rejected.from().tag().unwrap_or_default(),
        to_uri = rejected.to().uri(),
        to_tag = rejected.to().tag().unwrap_or_default(),
        call_id = rejected.call_id(),
        cseq = rejected.cseq().seq(),
    );
    alice.try_send_datagram(stray.as_bytes(), b2bua.addr).await.unwrap();
    // Two hops of fabric transit: long enough for a relayed ACK to REACH bob
    // while bob's INVITE is still final-less, which is the ordering under test.
    h.advance(Duration::from_millis(600)).await;

    // ── THE ASSERTION: bob's INVITE has drawn no final, so nothing may ACK it ─
    bob.sight_queued().await;
    assert_eq!(
        acks_seen(&bob),
        0,
        "a stray a-leg ACK must be absorbed: the b-leg INVITE has drawn no final response, \
         so an ACK toward bob here would acknowledge a response that does not exist"
    );

    // ── bob rejects; NOW the b-leg owes exactly one ACK, on the INVITE's branch ─
    b_inv.respond(487, "Request Terminated").await;
    b_inv.expect_ack().await;
    bob.sight_queued().await;
    assert_eq!(acks_seen(&bob), 1, "exactly one b-leg ACK: the §17.1.1.3 ACK for the 487");

    // ── fully reaped ─────────────────────────────────────────────────────────
    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
