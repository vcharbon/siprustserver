//! A party's transaction layer outlives its scenario's last step over a
//! network that loses datagrams: a copy of a request it answered draws that
//! final again until Timer J (RFC 3261 §17.2.2), a repeat of a non-2xx final
//! it ACKed draws its ACK again until Timer D (§17.1.1.2), so the SUT's own
//! retransmissions complete its transactions and the call is reaped.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use b2bua_harness::{B2buaScene, B2buaSut, OFFER_SDP};
use scenario_harness::{Harness, SIMULATED_TRANSIT_DELAY_MS};
use sip_message::sniff;
use sip_net_testkit::Fate;

/// The fabric loses the first datagram from `from` that `pick` names; every
/// other datagram arrives.
fn lose_first(from: SocketAddr, pick: fn(&[u8]) -> bool) -> sip_net_testkit::SendFate {
    let lost = AtomicBool::new(false);
    Arc::new(move |src, _dst, raw| {
        if src == from && pick(raw) && !lost.swap(true, Ordering::SeqCst) {
            Fate::Lose
        } else {
            Fate::Deliver
        }
    })
}

/// A final response to `method`.
fn final_to(raw: &[u8], method: &str) -> bool {
    sniff::resp_status(raw).is_some_and(|s| s >= 200)
        && sniff::header_value(raw, "CSeq").is_some_and(|c| c.ends_with(method))
}

fn ok_to_bye(raw: &[u8]) -> bool {
    final_to(raw, "BYE")
}

fn ack(raw: &[u8]) -> bool {
    sniff::req_method(raw).as_deref() == Some("ACK")
}

/// Bob's 200 to the B2BUA's BYE is lost. The B2BUA sends the BYE again on
/// Timer E, after alice's BYE was answered and the scenario's last step;
/// bob's transaction layer answers the copy with its 200 again and the
/// B2BUA reaps the call.
#[tokio::test(start_paused = true)]
async fn a_bye_copy_after_a_lost_200_draws_the_200_again_and_the_call_is_reaped() {
    let bob_addr: SocketAddr = format!("127.0.0.1:{}", b2bua_harness::BOB_PORT).parse().unwrap();
    let h = Harness::with_transit_delay_and_send_fate(
        "served-after-the-call-bye",
        SIMULATED_TRANSIT_DELAY_MS,
        lose_first(bob_addr, ok_to_bye),
    );
    let s =
        B2buaScene::on_harness(h, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await;
    assert_eq!(s.bob.addr(), bob_addr);
    let mut dialog = s.establish().await;
    s.hangup(&mut dialog).await;
    s.finish().await;
}

/// Alice's ACK of the 486 the B2BUA relays is lost. The B2BUA sends the 486
/// again on Timer G after the scenario's last step; alice's INVITE client
/// transaction ACKs the repeat and the B2BUA reaps the call.
#[tokio::test(start_paused = true)]
async fn a_non_2xx_final_repeated_after_a_lost_ack_draws_the_ack_again() {
    let alice_addr: SocketAddr =
        format!("127.0.0.1:{}", b2bua_harness::ALICE_PORT).parse().unwrap();
    let h = Harness::with_transit_delay_and_send_fate(
        "served-after-the-call-ack",
        SIMULATED_TRANSIT_DELAY_MS,
        lose_first(alice_addr, ack),
    );
    let s =
        B2buaScene::on_harness(h, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await;
    assert_eq!(s.alice.addr(), alice_addr);
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.bob.receive("INVITE").await.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    call.expect(486).await;
    s.finish().await;
}
