//! The route set of a dialog a losing fork's late 2xx confirms is that 2xx's
//! own Record-Route, reversed, and empty when it records none (RFC 3261
//! §12.1.2), whatever route set the winning fork taught.
//!
//! Topology: `alice ──▶ proxy ──▶ bob`. The proxy record-routes the INVITE, so
//! the winner's 200 (f2) teaches `[<proxy;lr>]`. The loser's late 200 (f1)
//! reaches alice with its Record-Route stripped: its dialog has no route, so
//! the ACK and BYE alice sends on it carry no Route and go straight to the
//! fork's Contact.

use scenario_harness::Harness;
use sip_message::header::HeaderName;

/// The caller's first CSeq: its INVITE's.
const C: u32 = scenario_harness::Party::Caller.first_cseq();

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

#[tokio::test(start_paused = true)]
async fn a_losing_fork_without_record_route_has_no_route_set() {
    let h = Harness::new("fork-route-set").describe(
        "alice → proxy → bob, proxy record-routes; the winner's 200 (f2) teaches \
         the proxy route, the loser's late 200 (f1) records none, so its ACK and \
         BYE go to its Contact with no Route",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let proxy = h.proxy("proxy", "127.0.0.1:5080").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let alice_addr = alice.addr();
    let bob_addr = bob.addr();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    proxy.forward_request(bob_addr).await;
    let mut uas = bob.receive("INVITE").await;

    // The winner: its 200 echoes the proxy's Record-Route.
    uas.respond(200, "OK").with_sdp(ANSWER).with_to_tag("f2").send().await;
    proxy.forward_response(alice_addr).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some("f2"));
    let mut winner = call.ack().await;
    proxy.forward_request(bob_addr).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some("f2"));

    // The loser's late 200 reaches alice with no Record-Route.
    uas.respond(200, "OK").with_sdp(ANSWER).with_to_tag("f1").send().await;
    proxy
        .forward_response_altered(alice_addr, |resp| {
            resp.thaw().remove(&HeaderName::RecordRoute).freeze().expect("the 200 freezes")
        })
        .await;
    let late = call.expect(200).await;
    assert_eq!(late.to().tag(), Some("f1"));
    assert!(late.raw(HeaderName::RecordRoute).next().is_none());

    let mut fork = call.fork_dialog(&late);
    fork.ack_for(C, None).await;
    let fork_ack = bob.receive("ACK").await;
    assert_eq!(fork_ack.request().to().tag(), Some("f1"));
    assert!(
        fork_ack.request().raw(HeaderName::Route).next().is_none(),
        "the losing fork's ACK carries no Route: its 2xx recorded none (RFC 3261 §12.1.2)",
    );
    let mut fork_bye = fork.bye().await;
    let mut bye = bob.receive("BYE").await;
    assert_eq!(bye.request().to().tag(), Some("f1"));
    assert!(
        bye.request().raw(HeaderName::Route).next().is_none(),
        "the losing fork's BYE carries no Route: its 2xx recorded none (RFC 3261 §12.1.2)",
    );
    bye.respond(200, "OK").await;
    fork_bye.expect(200).await;

    // The winning dialog still routes through the proxy.
    let mut winner_bye = winner.bye().await;
    proxy.forward_request(bob_addr).await;
    let mut bye = bob.receive("BYE").await;
    assert_eq!(bye.request().to().tag(), Some("f2"));
    bye.respond(200, "OK").await;
    proxy.forward_response(alice_addr).await;
    winner_bye.expect(200).await;

    h.finish().await;
}
