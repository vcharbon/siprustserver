//! An in-dialog request overtaken after a loss. The B2BUA relays the caller's
//! PRACK to the callee and the network loses its first copy; the caller's
//! early UPDATE, relayed after it with the next CSeq, reaches the callee
//! first, and the PRACK goes again on Timer E (RFC 3261 §17.1.2.2) to arrive
//! with a CSeq gone backwards: the callee answers it 500 (§12.2.2). On a run
//! that declares a loss model, the recorded loss of the PRACK's first copy
//! explains both the gap and the 500, and neither is a finding. The same
//! overtaking with nothing lost, the fabric holding the PRACK back, is one.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{B2buaScene, B2buaSut, ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Harness, SIMULATED_TRANSIT_DELAY_MS};
use sip_message::generators::InDialogMethod;
use sip_message::sniff;
use sip_net_testkit::{Fate, SendDelay, SendFate};

/// Bob's own `RSeq`.
const BOB_RSEQ: u32 = 4711;

/// Whether `raw`, sent by `src`, is the B2BUA's PRACK toward the callee, the
/// first time `first` is asked of one.
fn first_relayed_prack(src: SocketAddr, raw: &[u8], first: &AtomicBool) -> bool {
    let b2bua: SocketAddr = format!("127.0.0.1:{}", b2bua_harness::B2BUA_PORT).parse().unwrap();
    src == b2bua
        && sniff::req_method(raw).as_deref() == Some("PRACK")
        && !first.swap(true, Ordering::SeqCst)
}

/// The fabric loses the first PRACK the B2BUA sends; every other datagram
/// arrives.
fn lose_first_relayed_prack() -> SendFate {
    let first = AtomicBool::new(false);
    Arc::new(
        move |src, _dst, raw| {
            if first_relayed_prack(src, raw, &first) {
                Fate::Lose
            } else {
                Fate::Deliver
            }
        },
    )
}

/// The fabric holds the first PRACK the B2BUA sends back 300 ms, under T1:
/// it arrives once, after the UPDATE relayed behind it.
fn hold_first_relayed_prack() -> SendDelay {
    let first = AtomicBool::new(false);
    Arc::new(move |src, _dst, raw| {
        first_relayed_prack(src, raw, &first).then_some(Duration::from_millis(300))
    })
}

/// The caller PRACKs the callee's reliable 183 and sends an early UPDATE at
/// once; the callee answers the UPDATE, then the PRACK that reaches him after
/// it 500, and the call is answered and ended by the caller.
async fn prack_overtaken_by_an_update(h: Harness) {
    let s =
        B2buaScene::on_harness(h, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .with_header("Supported", "100rel")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    let mut update = call.send_request(InDialogMethod::Update).send().await;
    s.bob.receive("UPDATE").await.respond(200, "OK").await;
    update.expect(200).await;
    s.bob.receive("PRACK").await.respond(500, "Server Internal Error").await;
    prack.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_overtaken_after_the_loss_of_its_first_copy_is_refused_500_and_no_finding() {
    let h = Harness::with_transit_delay_and_send_fate(
        "overtaken-after-a-loss",
        SIMULATED_TRANSIT_DELAY_MS,
        lose_first_relayed_prack(),
    );
    prack_overtaken_by_an_update(h).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "bob: UPDATE CSeq 3 skips")]
async fn a_request_overtaken_with_nothing_lost_is_a_finding() {
    let h = Harness::with_transit_delay_and_send_delay(
        "overtaken-with-nothing-lost",
        SIMULATED_TRANSIT_DELAY_MS,
        hold_first_relayed_prack(),
    );
    prack_overtaken_by_an_update(h).await;
}
