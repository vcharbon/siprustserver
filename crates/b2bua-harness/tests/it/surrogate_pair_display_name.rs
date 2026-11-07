//! A caller's display name holding one character as a surrogate pair encoded
//! byte by byte (CESU-8: `ED A0 BC ED BC B9` for U+1F339) reads as that
//! character: the call is set up and ended as any other, nothing the B2BUA
//! sends holds a surrogate half, and what it echoes holds the character in
//! UTF-8 (`F0 9F 8C B9`) with the From URI and tag as they were written.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness};
use sip_message::{SipMessage, SipResponse};

const CESU8_ROSE: &[u8] = b"\xed\xa0\xbc\xed\xbc\xb9";
const UTF8_ROSE: &[u8] = b"\xf0\x9f\x8c\xb9";
const FROM_TAIL: &str = "<sip:alice@127.0.0.1:5060>;tag=a-rose";
const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const CALL_ID: &str = "rose-1@127.0.0.1";

/// Alice's request of `method` to `ruri`, her display name in CESU-8, as one
/// datagram; inside the dialog when `to_tag` is given. Only the INVITE and its
/// ACK carry her Contact (RFC 3261 §15.1).
fn request(
    alice: &Agent,
    method: &str,
    ruri: &str,
    cseq: u32,
    branch: &str,
    to_tag: Option<&str>,
    body: &str,
) -> Vec<u8> {
    let contact = match method {
        "BYE" => String::new(),
        _ => format!("Contact: <sip:alice@{}>\r\n", alice.addr()),
    };
    let to_tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    let content_type =
        if body.is_empty() { String::new() } else { "Content-Type: application/sdp\r\n".into() };
    let head = format!(
        "{method} {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via};branch=z9hG4bK-{branch}\r\n\
         Max-Forwards: 70\r\n\
         From: \"Rose ",
        via = alice.addr(),
    );
    let tail = format!(
        "\" {FROM_TAIL}\r\n\
         To: <sip:bob@127.0.0.1:5070>{to_tag}\r\n\
         Call-ID: {CALL_ID}\r\n\
         CSeq: {cseq} {method}\r\n\
         {contact}\
         {content_type}\
         Content-Length: {len}\r\n\r\n{body}",
        len = body.len(),
    );
    [head.as_bytes(), CESU8_ROSE, tail.as_bytes()].concat()
}

/// The responses queued at `agent` once the fabric had `wait` to deliver.
async fn responses(h: &Harness, agent: &Agent, wait: Duration) -> Vec<SipResponse> {
    h.advance(wait).await;
    let mut out = Vec::new();
    while let Some(msg) = agent.take_queued().await {
        if let SipMessage::Response(r) = msg {
            out.push(r);
        }
    }
    out
}

fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Whether `bytes` holds a surrogate half's lead: `ED` then `A0`..`BF`.
fn holds_a_surrogate_half(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|w| w[0] == 0xed && (0xa0..=0xbf).contains(&w[1]))
}

/// The URI the Contact of `raw` names: the remote target of the dialog it opens.
fn remote_target(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let line = text.lines().find(|l| l.starts_with("Contact:")).expect("the 200 names a Contact");
    let uri = line.split_once('<').and_then(|(_, rest)| rest.split_once('>'));
    uri.expect("a bracketed Contact").0.to_string()
}

/// The From line of `raw`.
fn from_line(raw: &[u8]) -> Vec<u8> {
    raw.split(|&b| b == b'\n')
        .find(|line| line.starts_with(b"From:"))
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

#[tokio::test(start_paused = true)]
async fn a_surrogate_pair_in_the_callers_display_name_reads_as_its_character() {
    let h = Harness::with_transit_delay("surrogate-pair-display-name", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5070).start(&h, "b2bua", "127.0.0.1:5080").await;
    let sut: SocketAddr = b2bua.addr;

    let invite = request(&alice, "INVITE", "sip:bob@127.0.0.1:5070", 1, "rose-inv", None, OFFER);
    alice.try_send_datagram(&invite, sut).await.expect("the INVITE leaves");
    let mut uas = bob.try_receive("INVITE").await.expect("the INVITE reaches the callee");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let answered = responses(&h, &alice, Duration::from_millis(50)).await;
    let ok = answered
        .iter()
        .find(|r| r.status() == 200 && r.cseq().method().as_str() == "INVITE")
        .expect("alice is answered 200");
    let to_tag = ok.to().tag().expect("the 200 is tagged").to_string();
    let echoed = from_line(ok.image());
    assert!(holds(&echoed, UTF8_ROSE), "the 200 echoes the character in UTF-8: {echoed:?}");
    assert!(holds(&echoed, FROM_TAIL.as_bytes()), "URI and tag as written: {echoed:?}");

    let target = remote_target(ok.image());
    let ack = request(&alice, "ACK", &target, 1, "rose-ack", Some(&to_tag), "");
    alice.try_send_datagram(&ack, sut).await.expect("the ACK leaves");
    bob.receive("ACK").await;
    let bye = request(&alice, "BYE", &target, 2, "rose-bye", Some(&to_tag), "");
    alice.try_send_datagram(&bye, sut).await.expect("the BYE leaves");
    bob.receive("BYE").await.respond(200, "OK").await;
    let ended = responses(&h, &alice, Duration::from_millis(50)).await;
    let bye_ok = ended
        .iter()
        .find(|r| r.status() == 200 && r.cseq().method().as_str() == "BYE")
        .expect("alice's BYE is answered 200");
    assert!(holds(&from_line(bye_ok.image()), UTF8_ROSE), "the BYE's 200 echoes it in UTF-8");

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let sent: Vec<_> = h.wire_entries().into_iter().filter(|e| e.from == sut).collect();
    assert!(!sent.is_empty(), "the B2BUA sent");
    for entry in &sent {
        assert!(
            !holds_a_surrogate_half(&entry.raw),
            "the B2BUA sends no surrogate half: {}",
            String::from_utf8_lossy(&entry.raw)
        );
    }
    let _ = h.finish().await;
}
