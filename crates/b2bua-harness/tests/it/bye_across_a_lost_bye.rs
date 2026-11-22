//! Two BYEs cross on a network that loses datagrams. The B2BUA relays the
//! caller's BYE to the callee and the network loses its first copy; the
//! callee hangs up meanwhile, and his BYE takes its final. The B2BUA's BYE
//! goes again on Timer E (RFC 3261 §17.1.2.2) and reaches a dialog the
//! callee's own BYE ended. Its first copy left before the B2BUA received the
//! callee's BYE, so the callee refuses the copy 481 (§15.1.2) and it is no
//! finding.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use b2bua_harness::{B2buaScene, B2buaSut, ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Harness, SIMULATED_TRANSIT_DELAY_MS};
use sip_message::sniff;
use sip_net_testkit::{Fate, SendFate};

/// The fabric loses the first BYE `from` sends; every other datagram arrives.
fn lose_first_bye(from: SocketAddr) -> SendFate {
    let lost = AtomicBool::new(false);
    Arc::new(move |src, _dst, raw| {
        let bye = src == from && sniff::req_method(raw).as_deref() == Some("BYE");
        if bye && !lost.swap(true, Ordering::SeqCst) {
            Fate::Lose
        } else {
            Fate::Deliver
        }
    })
}

#[tokio::test(start_paused = true)]
async fn a_bye_copy_sent_before_the_callees_own_bye_arrived_is_refused_and_no_finding() {
    let b2bua_addr: SocketAddr =
        format!("127.0.0.1:{}", b2bua_harness::B2BUA_PORT).parse().unwrap();
    let h = Harness::with_transit_delay_and_send_fate(
        "bye-across-a-lost-bye",
        SIMULATED_TRANSIT_DELAY_MS,
        lose_first_bye(b2bua_addr),
    );
    let s =
        B2buaScene::on_harness(h, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await;
    assert_eq!(s.b2bua.addr, b2bua_addr);

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut alice = call.ack().await;
    s.bob.receive("ACK").await;

    let mut alice_bye = alice.bye().await;
    alice_bye.expect(200).await;
    let mut bob_bye = uas.dialog().bye().await;
    bob_bye.expect(200).await;
    s.bob.receive("BYE").await.respond(481, "Call/Transaction Does Not Exist").await;
    s.finish().await;
}
