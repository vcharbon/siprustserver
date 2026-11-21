//! A request requiring `timer` that the stack answers itself, in its peer's
//! place: the stack runs no session timer of its own (RFC 4028 §7.1), so it
//! cannot accept the requirement in a 2xx it writes. It answers 420 Bad
//! Extension naming `timer` (RFC 3261 §8.2.2.3) and the exchange the 2xx
//! would have made stands unmade. Under `fake-prack` the stack answers both
//! faces' UPDATEs in the early dialog it masks: alice's bodyless refresh and
//! bob's re-offer.

use b2bua_harness::{settle_until, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::{Agent, ClientInvite, Harness, ServerTxn};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";
/// bob's re-offer in his early dialog: a new port, the same format, so the
/// stack could answer it.
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n";

/// The `Unsupported` lines `resp` carries.
fn unsupported(resp: &SipResponse) -> Vec<&str> {
    resp.raw(HeaderName::Unsupported).collect()
}

/// alice offers [`OFFER`] with the session timer; bob answers it in a reliable
/// 183 the stack PRACKs itself, so alice sees a bare 180. Returns her INVITE,
/// bob's server transaction and alice's early To tag.
async fn masked(alice: &Agent, bob: &Agent, b2bua: &B2buaSut) -> (ClientInvite, ServerTxn, String) {
    let mut call = alice
        .invite(bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel, timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let bare180 = call.expect(180).await;
    let tag = bare180.to().tag().expect("a To tag on the bare 180").to_string();
    bob.receive("PRACK").await.respond(200, "OK").await;
    (call, uas, tag)
}

/// bob answers alice's INVITE with a bare 200; alice acknowledges it and
/// hangs up; the call is reaped. Returns the body of alice's 200.
async fn answered_and_ended(
    h: Harness,
    mut call: ClientInvite,
    mut uas: ServerTxn,
    bob: &Agent,
    b2bua: &B2buaSut,
) -> String {
    uas.respond(200, "OK").await;
    let ok = call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
    String::from_utf8_lossy(ok.body()).into_owned()
}

/// alice's bodyless refresh UPDATE requiring `timer` in the masked early
/// dialog draws 420 naming it, never the bare 200 the stack writes a refresh.
#[tokio::test(start_paused = true)]
async fn the_callers_early_refresh_requiring_timer_is_refused_420() {
    let h = Harness::with_transit_delay("own-answer-timer-caller", 1);
    let alice = h.agent("alice", "127.0.0.1:5769").await;
    let bob = h.agent("bob", "127.0.0.1:5779").await;
    let b2bua =
        B2buaSut::route_all_to_with_18x("127.0.0.1", 5779, RelayFirst18xStrategy::FakePrack)
            .start(&h, "b2bua", "127.0.0.1:5789")
            .await;
    let (mut call, uas, tag) = masked(&alice, &bob, &b2bua).await;

    let mut update = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&tag)
        .with_header("Supported", "timer")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .send()
        .await;
    let refusal = update.expect(420).await;
    assert_eq!(unsupported(&refusal), ["timer"]);

    let body = answered_and_ended(h, call, uas, &bob, &b2bua).await;
    assert_eq!(body, ANSWER, "alice's 200 carries the 183's answer");
}

/// bob's re-offer requiring `timer` in his early dialog draws 420 naming it,
/// never the 200 the stack answers a re-offer with; the 183's exchange
/// stands, so his bare 200 gives alice the 183's answer, not one built from
/// the refused re-offer.
#[tokio::test(start_paused = true)]
async fn the_callees_early_reoffer_requiring_timer_is_refused_420() {
    let h = Harness::with_transit_delay("own-answer-timer-callee", 1);
    let alice = h.agent("alice", "127.0.0.1:5768").await;
    let bob = h.agent("bob", "127.0.0.1:5778").await;
    let b2bua =
        B2buaSut::route_all_to_with_18x("127.0.0.1", 5778, RelayFirst18xStrategy::FakePrack)
            .start(&h, "b2bua", "127.0.0.1:5788")
            .await;
    let (call, uas, _tag) = masked(&alice, &bob, &b2bua).await;

    let mut update = uas
        .dialog()
        .send_request(InDialogMethod::Update)
        .with_sdp(BOB_REOFFER)
        .with_header("Supported", "timer")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uas")
        .send()
        .await;
    let refusal = update.expect(420).await;
    assert_eq!(unsupported(&refusal), ["timer"]);

    let body = answered_and_ended(h, call, uas, &bob, &b2bua).await;
    assert_eq!(body, ANSWER, "alice's 200 carries the 183's answer");
}
