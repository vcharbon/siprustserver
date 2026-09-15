//! A turn is quiet by what it does, not by which action it ran: the retained
//! re-ACK (RFC 3261 §13.2.2.4) is a quiet repeat only when it is the whole of
//! the turn. Here the same re-ACK leaves beside a BYE, so the turn is a write.
//!
//! The shape is the CANCEL/200 crossing: alice CANCELs a ringing call, bob's
//! 200 crosses the CANCEL, the `cancel-200-crossing` rule ACKs the answer and
//! BYEs bob. Bob then withholds his answer to that BYE and repeats his 200 —
//! the peer-side deviation, and the subject: the crossing rule fires again on
//! the repeated 2xx (the leg is still `Cancelling`), re-sends the retained ACK
//! and puts a second BYE on the wire. A turn that sends a BYE changes what a
//! node restoring the call needs, whatever else it did. Bob answers the BYEs
//! afterwards, so the call ends properly with one CDR.
//!
//! The scenario reads settled metrics after each advance and depends on no
//! in-order draining of the paused runtime.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

#[tokio::test(start_paused = true)]
async fn a_re_ack_beside_a_bye_is_a_write_not_a_quiet_turn() {
    let h = Harness::new("b2bua-crossing-2xx-repeat-is-a-write");
    let alice = h.agent("alice", "127.0.0.1:5464").await;
    let bob = h.agent("bob", "127.0.0.1:5474").await;
    h.waive(
        WaiverScope::rule(
            "no-200-after-cancel",
            "bob deliberately answers 200 after taking the CANCEL (RFC 3261 §9.2) — the crossing under test",
        )
        .on_party("bob"),
    );
    let decision =
        Arc::new(b2bua::decision::ScriptedDecisionEngine::route_all_to("127.0.0.1", 5474));
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5484").await;

    // ── ringing call; alice CANCELs ──────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut bob_cancel = bob.receive("CANCEL").await;

    // ── the crossing: bob's 200 meets the CANCEL; ACK then BYE toward bob ────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob_cancel.respond(200, "OK").await;
    bob.receive("ACK").await;
    let mut first_bye = bob.receive("BYE").await;

    // ── bob withholds the BYE's answer and repeats his 2xx ───────────────────
    // The crossing rule runs again: the retained ACK leaves as a re-ACK, and a
    // second BYE with it. Both hops settle inside the window.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    h.advance(Duration::from_millis(800)).await;
    let mut second_bye = bob.try_receive_tolerating("BYE", &["ACK"]).await;
    assert_eq!(
        b2bua.metrics().retransmits_total("trigger", "ACK", None),
        1,
        "the repeated 2xx drew the retained ACK again (RFC 3261 §13.2.2.4)",
    );
    assert_eq!(
        b2bua.metrics().repl_quiet_turns_total("re-ack"),
        0,
        "a turn that put a BYE on the wire beside the re-ACK is a write: the quiet class \
         belongs to the turn's effect set, not to the action that re-sent the ACK",
    );

    // ── bob answers the BYEs; the call ends properly ─────────────────────────
    first_bye.respond(200, "OK").await;
    if let Some(bye) = second_bye.as_mut() {
        bye.respond(200, "OK").await;
    }
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    let _report = h.finish().await;
}
