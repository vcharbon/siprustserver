//! RFC 3261 §17.1.3 on the scripted UAC: a client transaction's expect reads
//! the responses of that transaction alone, matched by top-Via branch and
//! CSeq method. A response to another transaction is that transaction's: set
//! aside for its own expect, in arrival order, unless it repeats one an
//! expect already read, like a 2xx retransmission of the INVITE (§13.3.1.4).

use std::time::Duration;

use scenario_harness::{Harness, Inbound};
use sip_message::generators::{InDialogMethod, OutOfDialogMethod};

/// RFC 3261 T1: the UAS's first 2xx retransmission interval.
const T1: Duration = Duration::from_millis(500);

const OFFER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// The caller sends an INFO before her ACK; the callee retransmits its 200
/// (INVITE) before it answers the INFO 403. The INFO's expect reads the 403
/// and drops the retransmission, which the caller already holds: the
/// callee's BYE is the next message she receives.
#[tokio::test(start_paused = true)]
async fn an_unacked_2xx_retransmission_is_not_another_transactions_final() {
    let h = Harness::new("client-txn-match-2xx-repeat");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;

    let mut info = call.send_request(InDialogMethod::Info).send().await;
    let mut info_uas = bob.receive("INFO").await;
    h.advance(T1).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    info_uas.respond(403, "Forbidden").send().await;
    info.expect(403).await;

    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = uas.dialog().bye().await;
    alice.receive("BYE").await.respond(200, "OK").send().await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

/// Two INFOs await their finals; the first's arrives first. The second's
/// expect sets it aside and reads its own, and the first's expect then reads
/// the one set aside.
#[tokio::test(start_paused = true)]
async fn a_fresh_response_of_another_transaction_waits_for_its_own_expect() {
    let h = Harness::new("client-txn-match-fresh-other");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut first = dialog.send_request(InDialogMethod::Info).send().await;
    let mut first_uas = bob.receive("INFO").await;
    let mut second = dialog.send_request(InDialogMethod::Info).send().await;
    let mut second_uas = bob.receive("INFO").await;
    first_uas.respond(403, "Forbidden").send().await;
    second_uas.respond(200, "OK").send().await;

    second.expect(200).await;
    first.expect(403).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").send().await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

/// The INVITE's 200 and then its retransmission land while an OPTIONS awaits
/// its final: the OPTIONS expect sets the 200 aside and drops the copy. A
/// second OPTIONS expect sets the 200 aside again, and the INVITE's expect
/// reads it. The callee's BYE is the next message the caller receives.
#[tokio::test(start_paused = true)]
async fn a_set_aside_2xx_outlives_its_retransmission() {
    let h = Harness::new("client-txn-match-set-aside-2xx");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    let mut ping = alice.request(OutOfDialogMethod::Options, &bob).send().await;
    let mut ping_uas = bob.receive("OPTIONS").await;
    h.advance(T1).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    ping_uas.respond(200, "OK").send().await;
    ping.expect(200).await;

    let mut again = alice.request(OutOfDialogMethod::Options, &bob).send().await;
    bob.receive("OPTIONS").await.respond(200, "OK").send().await;
    again.expect(200).await;
    call.expect(200).await;

    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = uas.dialog().bye().await;
    alice.receive("BYE").await.respond(200, "OK").send().await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

/// Already sighted, in arrival order: the finals of INFO 1 and INFO 2, the
/// final of INFO 3, then a request from the callee. INFO 3's expect sets the
/// first two aside; the next receives read them in arrival order, ahead of
/// the request.
#[tokio::test(start_paused = true)]
async fn set_aside_responses_keep_their_arrival_order() {
    let h = Harness::new("client-txn-match-set-aside-order");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let _first = dialog.send_request(InDialogMethod::Info).send().await;
    let mut first_uas = bob.receive("INFO").await;
    let _second = dialog.send_request(InDialogMethod::Info).send().await;
    let mut second_uas = bob.receive("INFO").await;
    let mut third = dialog.send_request(InDialogMethod::Info).send().await;
    let mut third_uas = bob.receive("INFO").await;
    first_uas.respond(200, "OK").send().await;
    second_uas.respond(403, "Forbidden").send().await;
    third_uas.respond(200, "OK").send().await;
    let mut bye = uas.dialog().bye().await;
    alice.sight_queued().await;

    let first_seq = third.expect(200).await.cseq().seq() - 2;
    let order: Vec<String> = [alice.recv_any().await, alice.recv_any().await]
        .into_iter()
        .map(|inbound| match inbound.expect("a message") {
            Inbound::Response(r) => format!("{} {}", r.status(), r.cseq().seq()),
            Inbound::Request(r) => panic!("{} ahead of a set-aside response", r.request().method()),
        })
        .collect();
    assert_eq!(order, [format!("200 {first_seq}"), format!("403 {}", first_seq + 1)]);
    alice.receive("BYE").await.respond(200, "OK").send().await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

/// The load lane's wait for the INVITE's final sorts responses as every
/// expect does: an OPTIONS 200 that lands first is set aside for the OPTIONS
/// expect, and the INVITE's 200 it reads is a repeat to a later INFO expect
/// when the callee retransmits it before the ACK.
#[tokio::test(start_paused = true)]
async fn the_final_wait_reads_only_the_invites_responses() {
    let h = Harness::new("client-txn-match-final-wait");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    let mut ping = alice.request(OutOfDialogMethod::Options, &bob).send().await;
    bob.receive("OPTIONS").await.respond(200, "OK").send().await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    let (answer, _) = call.try_expect_final(200).await.expect("the INVITE's 200");
    assert_eq!(answer.cseq().method(), "INVITE", "the final wait read the INVITE's 200");
    ping.expect(200).await;

    let mut info = call.send_request(InDialogMethod::Info).send().await;
    let mut info_uas = bob.receive("INFO").await;
    h.advance(T1).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    info_uas.respond(403, "Forbidden").send().await;
    info.expect(403).await;

    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = uas.dialog().bye().await;
    alice.receive("BYE").await.respond(200, "OK").send().await;
    bye.expect(200).await;
    let _ = h.finish().await;
}
