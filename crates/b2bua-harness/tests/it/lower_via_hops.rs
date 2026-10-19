//! An INVITE that crossed an earlier hop over a transport the B2BUA does not
//! speak, or whose earlier hop names port 0, is a call like any other: only
//! the topmost Via is the hop the B2BUA answers (RFC 3261 §18.2.2), and RFC
//! 3261 §25.1 admits any `other-transport` below it (RFC 4475 §3.1.1.10). The
//! responses carry the lower Via back unchanged (§8.2.6.2).

use b2bua_harness::B2buaScene;
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Alice's INVITE carries `lower` under her own Via; the call is answered,
/// confirmed and hung up, and alice's 200 carries `lower` back.
async fn call_with_lower_via(name: &str, lower: &str) {
    let s = B2buaScene::new(name).await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Via", lower)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    let vias: Vec<String> = ok.raw(HeaderName::Via).map(str::to_string).collect();
    assert_eq!(vias.last().map(String::as_str), Some(lower), "the lower Via rides back: {vias:?}");
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn an_invite_that_crossed_a_tls_sctp_hop_is_answered() {
    call_with_lower_via(
        "b2bua-lower-via-tls-sctp",
        "SIP/2.0/TLS-SCTP origin.example.com;branch=z9hG4bK-origin-1",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_invite_whose_earlier_hop_names_port_zero_is_answered() {
    call_with_lower_via(
        "b2bua-lower-via-port-zero",
        "SIP/2.0/UDP origin.example.com:0;branch=z9hG4bK-origin-1",
    )
    .await;
}
