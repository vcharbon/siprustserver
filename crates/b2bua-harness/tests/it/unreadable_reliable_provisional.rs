//! A callee's reliable provisional whose `Require` is no option-tag list
//! (RFC 3261 §25.1): ingress drops it, as it drops any response it refuses, so
//! no early dialog opens and nothing PRACKs it. The callee retransmits it on
//! the RFC 3262 §3 ladder, each copy dropped alike, and at 64·T1 rejects the
//! INVITE with a 5xx, which ends the call as a failure: its CDR written and its
//! state reaped.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene};
use scenario_harness::WaiverScope;
use sip_message::header::HeaderName;
use sip_net_testkit::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// RFC 3261 T1, and RFC 3262 §3's 64·T1 bound.
const T1_MS: u64 = 500;
const BOUND_MS: u64 = 64 * T1_MS;

/// The datagrams `from` sent `to` whose first line starts with `head`.
fn sent(entries: &[RecordedSipEntry], from: SocketAddr, to: SocketAddr, head: &str) -> usize {
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(head.as_bytes()))
        .count()
}

/// Bob answers alice's INVITE (which offers `100rel`) with a 183 stating
/// `Require: 100rel, x y` and `RSeq: 1`, retransmits it at T1, 2·T1, … and,
/// no PRACK having come by 64·T1, rejects the INVITE with a 500.
#[tokio::test(start_paused = true)]
async fn a_reliable_provisional_with_an_unreadable_require_is_dropped_until_the_callee_rejects() {
    let s = B2buaScene::new("unreadable-reliable-provisional").await;
    s.h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "bob's 183 states a Require that is no option-tag list (RFC 3261 §25.1), so it is \
             no reliable provisional: ingress drops it, and no PRACK is owed — that drop is \
             this test's subject",
        )
        .on_party("b2bua"),
    );
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(100, "Trying").await;
    let req = uas.request();
    let via = req.raw(HeaderName::Via).next().expect("a Via").to_string();
    let from = req.raw(HeaderName::From).next().expect("a From").to_string();
    let to = req.raw(HeaderName::To).next().expect("a To").to_string();
    let call_id = req.call_id().to_string();
    let cseq = req.cseq().seq();
    let reliable_183 = format!(
        "SIP/2.0 183 Session Progress\r\n\
         Via: {via}\r\n\
         From: {from}\r\n\
         To: {to};tag=bob-183\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} INVITE\r\n\
         Contact: <sip:bob@{bob}>\r\n\
         Require: 100rel, x y\r\n\
         RSeq: 1\r\n\
         Content-Length: 0\r\n\r\n",
        bob = s.bob.addr(),
    );

    // The RFC 3262 §3 ladder: sent at 0, then T1 doubling, while under 64·T1.
    let mut at_ms = 0;
    let mut interval_ms = T1_MS;
    let mut copies = 0;
    loop {
        s.bob.try_send_datagram(reliable_183.as_bytes(), s.b2bua.addr).await.expect("it leaves");
        copies += 1;
        if at_ms + interval_ms >= BOUND_MS {
            s.h.advance(Duration::from_millis(BOUND_MS - at_ms)).await;
            break;
        }
        s.h.advance(Duration::from_millis(interval_ms)).await;
        at_ms += interval_ms;
        interval_ms *= 2;
    }

    // RFC 3262 §3: no PRACK by 64·T1, so bob rejects the INVITE with a 5xx.
    uas.respond(500, "Server Internal Error").with_to_tag("bob-183").await;
    call.expect(500).await;
    s.bob.receive("ACK").await;

    let (alice, bob, b2bua) = (s.alice.addr(), s.bob.addr(), s.b2bua.addr);
    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records().len();
    let report = s.finish().await;
    let entries = report.entries();
    assert_eq!(copies, 7, "the ladder sends the 183 at 0, 0.5, 1.5, 3.5, 7.5, 15.5 and 31.5 s");
    assert_eq!(sent(&entries, bob, b2bua, "SIP/2.0 183"), copies, "every copy reached the stack");
    assert_eq!(sent(&entries, b2bua, bob, "PRACK "), 0, "a dropped provisional is never PRACKed");
    assert_eq!(sent(&entries, b2bua, alice, "SIP/2.0 183"), 0, "nor shown to the caller");
    assert_eq!(cdrs, 1, "the failed call's CDR is written");
}
