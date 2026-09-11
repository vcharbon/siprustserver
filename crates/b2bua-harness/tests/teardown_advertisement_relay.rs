//! A relayed BYE carries the releasing peer's advertisement WHOLE (RFC 3261
//! §16.6): a set-like header the peer split over several lines is one set
//! (§7.3.1), and every line of it rides (issue 174).

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

#[tokio::test]
async fn a_relayed_bye_carries_every_line_of_the_peers_advertisement() {
    let h = Harness::with_transit_delay("b2bua-bye-advert-relay", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5070).start(&h, "b2bua", "127.0.0.1:5080").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // alice releases, advertising over two Allow lines plus an Accept.
    let mut bye = dialog
        .send_request(InDialogMethod::Bye)
        .with_header("Allow", "INVITE, ACK")
        .with_header("Allow", "OPTIONS, CANCEL, BYE")
        .with_header("Accept", "application/sdp, application/isup")
        .send()
        .await;
    let mut relayed = bob.receive("BYE").await;
    let allow: Vec<String> =
        relayed.request().raw(HeaderName::Allow).map(|v| v.to_string()).collect();
    let tokens: Vec<String> =
        allow.iter().flat_map(|line| line.split(',').map(|t| t.trim().to_string())).collect();
    for method in ["INVITE", "ACK", "OPTIONS", "CANCEL", "BYE"] {
        assert!(tokens.iter().any(|t| t == method), "{method} rides the relayed BYE: {allow:?}");
    }
    let accept: Vec<String> =
        relayed.request().raw(HeaderName::Accept).map(|v| v.to_string()).collect();
    assert_eq!(accept, ["application/sdp, application/isup"]);
    relayed.respond(200, "OK").await;
    bye.expect(200).await;

    let _ = h.finish().await;
}
