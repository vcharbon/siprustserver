//! The ACK to a 2xx carries the delayed-offer answer as the body the
//! acknowledging party types (RFC 3261 §13.2.2.4, RFC 3264 §4): a session
//! description framed beside other parts goes under its multipart type
//! (RFC 5621 §3), and an answer given as SDP alone goes as `application/sdp`.

use scenario_harness::{Harness, ServerTxn};
use sip_message::header::HeaderName;
use sip_message::SipRequest;

const SDP_OFFER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const SDP_ANSWER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const MULTIPART: &str = "multipart/mixed;boundary=B";

/// SDP_ANSWER framed beside a `text/plain` note under [`MULTIPART`].
fn framed_answer() -> Vec<u8> {
    format!(
        "--B\r\nContent-Type: application/sdp\r\n\r\n{SDP_ANSWER}\r\n\
         --B\r\nContent-Type: text/plain\r\n\r\na note\r\n--B--\r\n"
    )
    .into_bytes()
}

fn content_types(req: &SipRequest) -> Vec<String> {
    req.raw(HeaderName::ContentType).map(str::to_string).collect()
}

/// Alice INVITEs bob with no offer and bob's 200 carries the offer: returns
/// alice's pending INVITE and bob's server transaction.
async fn offer_in_200(
    h: &Harness,
) -> (scenario_harness::ClientInvite, ServerTxn, scenario_harness::Agent) {
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let mut call = alice.invite(&bob).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(SDP_OFFER).await;
    call.expect(200).await;
    (call, uas, bob)
}

#[tokio::test(start_paused = true)]
async fn a_framed_answer_rides_the_ack_under_its_multipart_type() {
    let h = Harness::new("ack-typed-body-framed");
    let (mut call, _uas, bob) = offer_in_200(&h).await;

    let mut dialog = call.ack_with_body_stating(MULTIPART, &framed_answer(), &[]).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(content_types(ack.request()), [MULTIPART]);
    assert_eq!(ack.request().body(), framed_answer().as_slice());

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn an_sdp_answer_rides_the_ack_as_application_sdp() {
    let h = Harness::new("ack-typed-body-sdp");
    let (mut call, _uas, bob) = offer_in_200(&h).await;

    let mut dialog = call.ack_with(Some(SDP_ANSWER)).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(content_types(ack.request()), ["application/sdp"]);
    assert_eq!(ack.request().body(), SDP_ANSWER.as_bytes());

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = h.finish().await;
}
