//! A 2xx to the dialling INVITE after its non-2xx final, inside Timer D. The
//! callee, broken on purpose, rings, rejects the INVITE 486, then answers it
//! 200 under the same tag. The caller has her 486 and the call is over, but
//! the 200 confirms a dialog at the callee (RFC 3261 §13.2.2.4) that no call
//! holds: that 200 is ACKed and the dialog BYEd (§15), so the callee is left
//! no confirmed dialog awaiting an ACK. Nothing more reaches the caller; one
//! CDR, fully reaped.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Past a round trip on the harness's 1 ms transit, well inside Timer D.
const PAST_A_ROUND_TRIP: Duration = Duration::from_millis(50);

/// The 200 the callee at `bob` sends the SUT at `b2bua` after its final: its
/// 180, under the same tag, Via and CSeq, turned into a 200 carrying
/// [`ANSWER`]. The harness's server transaction refuses a second final, so
/// the callee's fault goes out as a datagram.
fn late_200(h: &Harness, bob: SocketAddr, b2bua: SocketAddr) -> Vec<u8> {
    let ringing = h
        .wire_entries()
        .into_iter()
        .find(|e| e.from == bob && e.to == b2bua && e.raw.starts_with(b"SIP/2.0 180 "))
        .expect("bob's 180");
    let ringing = String::from_utf8_lossy(&ringing.raw).to_string();
    let (head, _) = ringing.split_once("\r\n\r\n").expect("the 180's head");
    let head: Vec<&str> = head
        .split("\r\n")
        .skip(1)
        .filter(|l| !l.to_ascii_lowercase().starts_with("content-"))
        .collect();
    format!(
        "SIP/2.0 200 OK\r\n{}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{ANSWER}",
        head.join("\r\n"),
        ANSWER.len()
    )
    .into_bytes()
}

#[tokio::test(start_paused = true)]
async fn a_2xx_to_the_dialling_invite_after_its_486_is_acked_and_byed() {
    let h = Harness::with_transit_delay("b2bua-dialled-2xx-after-486", 1);
    h.waive(
        WaiverScope::rule(
            "single-final-per-server-txn",
            "the callee deliberately answers 200 to the INVITE it rejected 486 — the \
             late 2xx this test exists to end",
        )
        .on_party("bob"),
    );
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", bob.addr().port()).start(&h, "b2bua", &at).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(486, "Busy Here").await;
    call.expect(486).await;
    bob.receive("ACK").await;

    let late = late_200(&h, bob.addr(), b2bua.addr);
    bob.try_send_datagram(&late, b2bua.addr).await.expect("the late 200 leaves");
    let ack = bob.receive("ACK").await;
    let acked_tag = ack.request().to().tag().map(str::to_string);
    assert!(acked_tag.is_some(), "the ACK names the dialog the 200 confirmed");
    assert_eq!(ack.request().cseq().seq(), uas.request().cseq().seq(), "the ACK of the INVITE");
    let mut bye = bob.receive("BYE").await;
    assert_eq!(bye.request().to().tag(), acked_tag.as_deref(), "the BYE ends that dialog");
    bye.respond(200, "OK").await;
    h.advance(PAST_A_ROUND_TRIP).await;
    assert_eq!(alice.drain().await, 0, "nothing more reaches the caller");
    assert_eq!(b2bua.txn_metrics().late_2xx_dropped(), 0, "no transaction absorbed it");
    assert_eq!(b2bua.metrics().late_2xx_dropped_total(), 0, "nor did the dialog gate drop it");

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.finish().await;
}
