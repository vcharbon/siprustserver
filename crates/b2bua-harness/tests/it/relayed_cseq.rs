//! A relayed request carries the next number of the dialog it goes out on
//! (RFC 3261 §12.2.1.1): the B2BUA's own sequence on that face, one above its
//! last request there, whatever number the sender stated. A gap in the
//! sender's sequence (§12.2.2 lets a UAS take one) is not reproduced on the
//! other face. The B2BUA's sequence on the caller-facing dialog starts at 1
//! (§8.1.1.5), as it does on the callee leg, where its INVITE is 1.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness, ServerTxn, WaiverScope};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";

fn cseq_of(txn: &ServerTxn) -> u32 {
    txn.request().cseq().seq()
}

/// An INFO `from` sends on `dialog`, relayed to `to`, which answers it 200.
/// The CSeq the relayed copy carried.
async fn relayed_info(dialog: &mut scenario_harness::Dialog, to: &Agent) -> u32 {
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    let mut at_peer = to.receive("INFO").await;
    let seen = cseq_of(&at_peer);
    at_peer.respond(200, "OK").await;
    info.expect(200).await;
    seen
}

/// The caller skips six numbers between two INFOs; the callee sees the
/// B2BUA's INVITE (1), then 2 and 3 for the INFOs and 4 for the BYE.
///
/// ```text
///   alice → B2BUA          B2BUA → bob
///   INVITE n               INVITE 1
///   INFO n+1               INFO 2
///   INFO n+8               INFO 3
///   BYE n+9                BYE 4
/// ```
#[tokio::test(start_paused = true)]
async fn a_gap_in_the_callers_sequence_is_not_reproduced_toward_the_callee() {
    let h = Harness::with_transit_delay("b2bua-relayed-cseq-gap-not-reproduced", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    h.waive(
        WaiverScope::rule(
            "cseq-in-dialog-order",
            "alice skips numbers between two INFOs: the gap under test (RFC 3261 §12.2.2)",
        )
        .on_party("alice"),
    );

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    let mut seen = vec![cseq_of(&uas)];
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    seen.push(relayed_info(&mut alice_dialog, &bob).await);
    let skipped = alice_dialog.local_cseq() + 6;
    alice_dialog.set_local_cseq(skipped);
    seen.push(relayed_info(&mut alice_dialog, &bob).await);

    let mut bye = alice_dialog.bye().await;
    let mut at_bob = bob.receive("BYE").await;
    seen.push(cseq_of(&at_bob));
    at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    assert_eq!(seen, [1, 2, 3, 4], "the callee sees the B2BUA's own run: INVITE, INFO, INFO, BYE");

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The callee's INFO and BYE, numbered from its own sequence, reach the
/// caller as the B2BUA's first two requests on her dialog: 1 and 2.
///
/// ```text
///   bob → B2BUA            B2BUA → alice
///   INFO m                 INFO 1
///   BYE m+1                BYE 2
/// ```
#[tokio::test(start_paused = true)]
async fn the_first_request_toward_the_caller_carries_cseq_1() {
    let h = Harness::with_transit_delay("b2bua-caller-facing-cseq-starts-at-1", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut seen = vec![relayed_info(&mut bob_dialog, &alice).await];

    let mut bye = bob_dialog.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    seen.push(cseq_of(&at_alice));
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    assert_eq!(seen, [1, 2], "the B2BUA's sequence toward the caller starts at 1");

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The B2BUA's own BYE toward the caller, after the callee's BYE, is its
/// first request on her dialog: CSeq 1.
#[tokio::test(start_paused = true)]
async fn the_bye_toward_the_caller_after_the_callee_hangs_up_carries_cseq_1() {
    let h = Harness::with_transit_delay("b2bua-caller-facing-bye-cseq-1", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut bye = bob_dialog.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    assert_eq!(cseq_of(&at_alice), 1, "the B2BUA's first request toward the caller");
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
