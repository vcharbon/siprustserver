//! A CANCEL of the setup on the ring: the one the transaction layer answers
//! itself, with what this stack sends on, and a callee's 200 to the CANCEL
//! crossing its answer.

use super::*;

/// A CANCEL the transaction layer matches and answers itself (RFC 3261
/// §9.2: 200 to the CANCEL, 487 to the INVITE) reaches the call as its own
/// event; the ring records the three messages, then the CANCEL this stack
/// sends onward and the hop-by-hop ACK its 487 draws (§17.1.1.3).
#[tokio::test(start_paused = true)]
async fn a_cancelled_setup_records_the_layer_answered_cancel() {
    let h = Harness::new("message-ring-cancel");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, ring_on()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let invite_cseq = call.invite_cseq();

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut bob_cxl = bob.receive("CANCEL").await;
    bob_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(1)).await;

    let done = sut.assert_reaped().await;
    assert_eq!(
        rows(&done.a_leg.messages.entries),
        vec![
            (Received, "INVITE", invite_cseq, None),
            (Authored, "INVITE", invite_cseq, Some(100)),
            (Relayed, "INVITE", invite_cseq, Some(180)),
            (Received, "CANCEL", invite_cseq, None),
            (Authored, "CANCEL", invite_cseq, Some(200)),
            (Authored, "INVITE", invite_cseq, Some(487)),
        ],
        "the a-leg: the CANCEL and the two finals the layer answered it with"
    );
    let b = rows(&b_leg(&done).messages.entries);
    let b_cseq = b[0].2;
    assert_eq!(
        b,
        vec![
            (Relayed, "INVITE", b_cseq, None),
            (Received, "INVITE", b_cseq, Some(180)),
            (Authored, "CANCEL", b_cseq, None),
            (Received, "CANCEL", b_cseq, Some(200)),
            (Received, "INVITE", b_cseq, Some(487)),
            (Authored, "ACK", b_cseq, None),
        ],
        "the b-leg: the CANCEL this stack sent, its answers, the 487's hop ACK"
    );
    assert_eq!(
        done.a_leg.messages.entries[3].to_tag, None,
        "the CANCEL, like the INVITE it cancels, carries no To tag"
    );
    assert_eq!(
        done.a_leg.messages.entries[5].to_tag, done.a_leg.messages.entries[2].to_tag,
        "the 487 carries the a-dialog tag the 180 established"
    );
    let invite_from = &done.a_leg.messages.entries[0].from_tag;
    assert!(invite_from.is_some(), "the INVITE records alice's From tag");
    assert!(
        done.a_leg.messages.entries[3..].iter().all(|e| e.from_tag == *invite_from),
        "the CANCEL and its two answers carry the INVITE's From tag (RFC 3261 §9.1)"
    );
    assert_eq!(
        turns(&done.a_leg.messages.entries),
        vec![1, 1, 2, 3, 3, 3],
        "the layer's answers carry the turn that handles the CANCEL they answer"
    );
    assert_eq!(
        turns(&b_leg(&done).messages.entries),
        vec![1, 2, 3, 4, 5, 5],
        "the CANCEL sent on carries that turn too; the hop ACK the callee's 487 draws, the 487's"
    );

    let _report = h.finish().await;
}

/// A callee's 200 to a CANCEL carries the INVITE's CSeq number and tag: it is
/// a message of its own, never a copy of the INVITE 2xx the stack ACKed when
/// the answer crossed the CANCEL. bob answers the INVITE before the CANCEL
/// takes effect, then answers the CANCEL (RFC 3261 §9.2); the stack ACKs
/// the answer and BYEs the leg. The waiver covers bob's deliberate 200
/// alone: the SUT's own output stays under the audit.
#[tokio::test(start_paused = true)]
async fn a_200_to_cancel_crossing_the_answer_is_recorded() {
    let h = Harness::new("message-ring-cancel-crossing");
    h.waive(
        WaiverScope::rule(
            "no-200-after-cancel",
            "bob deliberately answers 200 after taking the CANCEL — the crossing under test",
        )
        .on_party("bob"),
    );
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, ring_on()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut bob_cancel = bob.receive("CANCEL").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob_cancel.respond(200, "OK").await;
    bob.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    h.advance(Duration::from_secs(1)).await;

    let done = sut.assert_reaped().await;
    let b = rows(&b_leg(&done).messages.entries);
    let b_cseq = b[0].2;
    assert!(
        b.contains(&(Received, "CANCEL", b_cseq, Some(200))),
        "the 200 to the CANCEL is a row of its own: {b:?}"
    );
    assert!(b.contains(&(Received, "INVITE", b_cseq, Some(200))), "{b:?}");
    assert_eq!(b.iter().filter(|r| r.0 == Received && r.3 == Some(200)).count(), 3, "{b:?}");
    assert_eq!(b.last().map(|r| (r.0, r.1, r.3)), Some((Received, "BYE", Some(200))), "{b:?}");

    let _report = h.finish().await;
}
