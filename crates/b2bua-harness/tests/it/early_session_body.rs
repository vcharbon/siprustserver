//! An INVITE whose only session description is disposed `early-session`
//! (RFC 3959) makes no offer: the offer/answer model binds only bodies whose
//! disposition is `session` (RFC 3261 §13.2.1, §20.11). The B2BUA relays the
//! body to the callee as it came, beside its disposition, and the call runs as
//! a delayed offer: the callee's 200 offers and the caller's ACK answers
//! (RFC 3264 §4), each relayed.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::HeaderName;

const EARLY: &str = "v=0\r\no=alice 301 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const OFFER: &str = "v=0\r\no=bob 401 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=alice 302 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                      t=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

fn line(txn: &scenario_harness::ServerTxn, name: &str) -> Option<String> {
    txn.request().raw(HeaderName::from(name)).next().map(str::to_string)
}

#[tokio::test(start_paused = true)]
async fn an_invite_whose_only_description_is_early_session_runs_as_a_delayed_offer() {
    let h = Harness::new("b2bua-early-session-body");
    let alice = h.agent("alice", "127.0.0.1:5471").await;
    let bob = h.agent("bob", "127.0.0.1:5481").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5481).start(&h, "b2bua", "127.0.0.1:5491").await;

    let mut call = alice
        .invite(&bob)
        .with_body("application/sdp", EARLY.as_bytes().to_vec())
        .with_header("Content-Disposition", "early-session")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(uas.request().body().as_ref(), EARLY.as_bytes(), "the body rides as it came");
    assert_eq!(line(&uas, "Content-Disposition").as_deref(), Some("early-session"));
    assert_eq!(uas.request().sdp(), None, "the callee's INVITE makes no offer");

    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.sdp(), Some(OFFER.as_bytes()), "the callee's offer reaches the caller");

    let mut dialog = call.ack_with(Some(ANSWER)).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().body().as_ref(), ANSWER.as_bytes(), "the caller's answer");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call record");
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
