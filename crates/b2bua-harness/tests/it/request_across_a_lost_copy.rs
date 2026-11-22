//! A request crosses the final that ends its early dialog on a network that
//! loses datagrams. The B2BUA relays the callee's early UPDATE to the caller
//! and the network loses its first copy; the caller CANCELs, and the B2BUA
//! ends her INVITE 487 (RFC 3261 §9.2), which ends the early dialog
//! (§12.1.2). The UPDATE goes again on Timer E (§17.1.2.2) and reaches the
//! caller past that 487. Its first copy left before the B2BUA sent the 487, so
//! the caller refuses the copy 481 (§12.2.2) and it is no finding. The same
//! UPDATE held back past the 487 with nothing lost is one.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{B2buaScene, B2buaSut, OFFER_SDP};
use scenario_harness::{Harness, SIMULATED_TRANSIT_DELAY_MS};
use sip_message::generators::InDialogMethod;
use sip_message::sniff;
use sip_net_testkit::{Fate, SendDelay, SendFate};

/// Whether `raw`, sent by `src`, is the B2BUA's UPDATE toward the caller, the
/// first time `first` is asked of one.
fn first_relayed_update(src: SocketAddr, raw: &[u8], first: &AtomicBool) -> bool {
    let b2bua: SocketAddr = format!("127.0.0.1:{}", b2bua_harness::B2BUA_PORT).parse().unwrap();
    src == b2bua
        && sniff::req_method(raw).as_deref() == Some("UPDATE")
        && !first.swap(true, Ordering::SeqCst)
}

/// The fabric loses the first UPDATE the B2BUA sends; every other datagram
/// arrives.
fn lose_first_relayed_update() -> SendFate {
    let first = AtomicBool::new(false);
    Arc::new(
        move |src, _dst, raw| {
            if first_relayed_update(src, raw, &first) {
                Fate::Lose
            } else {
                Fate::Deliver
            }
        },
    )
}

/// The fabric holds the first UPDATE the B2BUA sends back 200 ms: it arrives
/// once, after the 487, and its 481 reaches the B2BUA before Timer E fires.
fn hold_first_relayed_update() -> SendDelay {
    let first = AtomicBool::new(false);
    Arc::new(move |src, _dst, raw| {
        first_relayed_update(src, raw, &first).then_some(Duration::from_millis(200))
    })
}

/// The callee rings and sends a bodiless early UPDATE; once the B2BUA relayed
/// it, the caller CANCELs and takes her 487; the UPDATE that reaches her
/// after it she refuses 481, and the callee's INVITE ends 487 too.
async fn update_crossing_the_callers_487(h: Harness) {
    let s =
        B2buaScene::on_harness(h, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let mut update = uas.dialog().request(InDialogMethod::Update, None).await;
    s.h.advance(Duration::from_millis(SIMULATED_TRANSIT_DELAY_MS + 50)).await;

    call.cancel().await.expect(200).await;
    call.expect(487).await;
    s.bob.receive("CANCEL").await.respond(200, "OK").await;
    update.expect(487).await;
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    s.alice.receive("UPDATE").await.respond(481, "Call/Transaction Does Not Exist").await;
    s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_copy_sent_before_its_dialogs_final_is_refused_and_no_finding() {
    let h = Harness::with_transit_delay_and_send_fate(
        "request-across-a-lost-copy",
        SIMULATED_TRANSIT_DELAY_MS,
        lose_first_relayed_update(),
    );
    update_crossing_the_callers_487(h).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "names a dialog a 487 final ended")]
async fn a_request_held_past_its_dialogs_final_with_nothing_lost_is_a_finding() {
    let h = Harness::with_transit_delay_and_send_delay(
        "request-held-past-the-final",
        SIMULATED_TRANSIT_DELAY_MS,
        hold_first_relayed_update(),
    );
    update_crossing_the_callers_487(h).await;
}
