//! A transaction-stateful proxy ACKs a callee's non-2xx final itself (RFC
//! 3261 §17.1.1.3), before the caller has seen that final:
//!
//!   alice ──▶ proxy ──▶ bob
//!
//! The proxy's ACK shows nothing of what alice saw, so the dialog table of
//! bob holds his early dialog open to her answers after it: a 2xx she sends
//! before his 487 reaches her crosses the final and is no verdict at bob.

use std::time::Duration;

use scenario_harness::callflow::OFFER_SDP;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

use crate::common::stateful_proxy::spawn_stateful_proxy;

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5090";
/// One hop's transit: the INFO's round trip through the proxy ends well
/// before Timer E first fires (T1, 500 ms), so no INFO is sent twice.
const TRANSIT_MS: u64 = 50;

/// bob rings and sends alice an INFO on the early dialog, then refuses the
/// call 487. The proxy ACKs the 487 to bob and forwards it to alice; she
/// answers the INFO 200 half a hop after bob's 487 left, before it reaches
/// her, and her 200 reaches bob after the proxy's ACK.
#[tokio::test(start_paused = true)]
async fn a_2xx_crossing_the_callees_final_passes_after_the_proxys_ack() {
    let h = Harness::with_transit_delay("hop-ack-of-a-callees-final", TRANSIT_MS);
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let proxy = spawn_stateful_proxy(&h, "proxy", PROXY).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).through(proxy.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").send().await;
    call.expect(180).await;
    let mut info = uas.dialog().send_request(InDialogMethod::Info).send().await;
    let mut at_alice = alice.receive("INFO").await;
    uas.respond(487, "Request Terminated").await;
    h.advance(Duration::from_millis(TRANSIT_MS / 2)).await;
    at_alice.respond(200, "OK").await;
    uas.expect_ack().await;
    info.expect(200).await;
    call.expect(487).await;
    let _ = h.finish().await;
}
