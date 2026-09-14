//! A mid-dialog request whose To-tag names no dialog this B2BUA holds on the
//! leg it arrived on (RFC 3261 §12.2.2). A BYE so tagged is refused `481` and
//! the call it did not name stays up; an ACK so tagged draws no response
//! (§17.1.1.3), discharges nothing and reaches no far leg, so the 2xx it failed
//! to acknowledge is repeated (§13.3.1.4) until the right ACK lands — and it is
//! THAT ACK the callee gets. The dialog's own tag then ends the call as usual
//! and nothing leaks.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use sip_message::generators::InDialogMethod;

const FOREIGN_TAG: &str = "no-dialog-here";

#[tokio::test(start_paused = true)]
async fn bye_under_a_foreign_to_tag_is_refused_481_and_the_call_stays_up() {
    let s = B2buaScene::new("b2bua-foreign-tag-bye").await;
    let mut dialog = s.establish().await;

    // The scripted caller is the deviant here: it names a dialog nobody holds.
    s.h.allow_violation(
        "mid-dialog-tags",
        "the BYE under a foreign To-tag is the deviation under test (RFC 3261 §12.2.2)",
    );
    // A request in another dialog spends nothing of this dialog's CSeq space
    // (RFC 3261 §12.2.1.1): the counter is put back once it has left.
    let cseq_before = dialog.local_cseq();
    let mut foreign =
        dialog.send_request(InDialogMethod::Bye).with_to_tag(FOREIGN_TAG).send().await;
    dialog.set_local_cseq(cseq_before);
    let refused = foreign.expect(481).await;
    assert_eq!(
        refused.to().tag(),
        Some(FOREIGN_TAG),
        "the 481 echoes the To-tag the request carried (RFC 3261 §8.2.6.2)"
    );

    // Nothing crossed to the callee, and the call is still there.
    s.h.advance(Duration::from_millis(500)).await;
    assert!(
        s.bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "a BYE naming no dialog must not reach the far leg"
    );
    assert_eq!(s.b2bua.metrics().removals_total(), 0, "the call was not torn down");

    // The dialog's own tag ends it.
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn ack_under_a_foreign_to_tag_discharges_nothing_and_the_2xx_is_repeated() {
    let s = B2buaScene::new("b2bua-foreign-tag-ack").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let answer = call.expect(200).await;

    // An ACK for that 2xx under a tag the B2BUA never minted: same Call-ID,
    // From-tag and CSeq, so only the dialog id is wrong.
    s.h.allow_violation(
        "mid-dialog-tags",
        "the ACK under a foreign To-tag is the deviation under test (RFC 3261 §12.2.2)",
    );
    let target = answer
        .contacts()
        .as_slice()
        .first()
        .map(|c| c.uri().to_string())
        .expect("the 2xx carries a Contact");
    let from_tag = answer.from().tag().expect("the INVITE carried a From-tag");
    let wire = format!(
        "ACK {target} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via};branch=z9hG4bK-foreign-tag-ack\r\n\
         Max-Forwards: 70\r\n\
         From: <{from}>;tag={from_tag}\r\n\
         To: <{to}>;tag={FOREIGN_TAG}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} ACK\r\n\
         Content-Length: 0\r\n\r\n",
        via = s.alice.addr(),
        from = answer.from().uri(),
        to = answer.to().uri(),
        call_id = answer.call_id().as_str(),
        cseq = answer.cseq().seq(),
    );
    s.alice.try_send_datagram(wire.as_bytes(), s.b2bua.addr).await.expect("the ACK leaves");

    // Undischarged, the 2xx is repeated on its ladder (T1 = 500 ms).
    let mut repeats = 0;
    for _ in 0..12 {
        s.h.advance(Duration::from_millis(100)).await;
        repeats += s.alice.drain().await;
        if repeats >= 1 {
            break;
        }
    }
    assert!(repeats >= 1, "the 2xx a foreign-tagged ACK left unacknowledged is repeated");
    assert!(
        s.b2bua.metrics().retransmits_total("final-2xx", "INVITE", Some(200)) >= 1,
        "the §13.3.1.4 ladder ran"
    );

    // The right ACK stops the ladder, and it is what reaches the callee
    // (RFC 3261 §13.2.2.4); the call is up and ends cleanly.
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let ladder_so_far = s.b2bua.metrics().retransmits_total("final-2xx", "INVITE", Some(200));
    s.h.advance(Duration::from_secs(3)).await;
    assert_eq!(
        s.b2bua.metrics().retransmits_total("final-2xx", "INVITE", Some(200)),
        ladder_so_far,
        "the dialog's own ACK discharged the 2xx"
    );
    s.alice.drain().await;

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
