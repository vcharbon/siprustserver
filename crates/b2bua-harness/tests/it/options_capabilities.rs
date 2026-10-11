//! RFC 3261 §11.2: every 200 this stack sends to an OPTIONS states its own
//! capability set — `Allow`, `Supported`, `Accept` — whether it answers the
//! request itself or relays the peer's 200, whose own advertisement describes
//! the peer, not this UA.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::{CapabilitySet, InDialogMethod};
use sip_message::header::HeaderName;
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// The `Allow` / `Supported` / `Accept` lines `resp` carries, in order.
fn advertisement(resp: &SipResponse) -> Vec<(HeaderName, String)> {
    [HeaderName::Allow, HeaderName::Supported, HeaderName::Accept]
        .into_iter()
        .flat_map(|name| {
            resp.raw(name.clone()).map(move |v| (name.clone(), v.to_string())).collect::<Vec<_>>()
        })
        .collect()
}

/// alice's OPTIONS in a confirmed dialog is relayed to bob, whose 200 states
/// a narrow set of his own; the 200 alice receives states the stack's set.
#[tokio::test(start_paused = true)]
async fn a_relayed_options_200_states_the_stack_capabilities() {
    let h = Harness::with_transit_delay("options-200-relayed", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut options = dialog.send_request(InDialogMethod::Options).send().await;
    bob.receive("OPTIONS")
        .await
        .respond(200, "OK")
        .with_header("Allow", "INVITE, ACK, BYE")
        .with_header("Accept", "application/sdp")
        .await;
    let ok = options.expect(200).await;
    assert_eq!(advertisement(&ok), CapabilitySet::default().lines());

    dialog.bye().await.expect(200).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// bob hangs up; alice's OPTIONS crossing the B2BUA's BYE toward her is
/// answered by the stack in her ending dialog, stating the stack's set.
#[tokio::test(start_paused = true)]
async fn an_options_answered_in_an_ending_dialog_states_the_stack_capabilities() {
    let h = Harness::with_transit_delay("options-200-absorbed", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    uas.dialog().bye().await.expect(200).await;
    let mut ours = alice.receive("BYE").await;

    let ok = dialog.send_request(InDialogMethod::Options).send().await.expect(200).await;
    assert_eq!(advertisement(&ok), CapabilitySet::default().lines());

    h.advance(Duration::from_millis(100)).await;
    ours.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
