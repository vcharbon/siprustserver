use super::*;
use sip_message::header::{HeaderValue, Rport};
use sip_message::{serialize, CustomParser, SipMessage, SipParser};

/// Where every request of these tests comes from: its Via's own sent-by.
const SRC: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)), 5555);

fn invite(branch: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@caller.test>;tag=alice-tag\r\n\
To: <sip:bob@uas.test>\r\n\
Call-ID: {branch}@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5555>\r\n\
Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

fn wire(resp: SipResponse) -> String {
    String::from_utf8(serialize(&SipMessage::Response(resp))).expect("utf-8")
}

/// The wire form of a refusal that must be built.
fn refusal_wire(resp: Result<SipResponse, GenerateError>) -> String {
    wire(resp.expect("a refusal"))
}

/// A tagged 503 echoing the request, with a floored `Retry-After` and no
/// `Reason`.
#[test]
fn a_refusal_is_a_tagged_503_with_retry_after_and_no_reason() {
    let r = Refusals::new(&IdGen::seeded(1), 0);
    let out = refusal_wire(r.refuse(&invite("z9hG4bK-a"), SRC, 0, 0));
    assert!(out.starts_with("SIP/2.0 503 Service Unavailable\r\n"), "{out}");
    assert!(out.contains("Retry-After: 1\r\n"), "a hint of 0 goes out as 1: {out}");
    assert!(!out.contains("Reason:"), "{out}");
    let to = out.lines().find(|l| l.starts_with("To:")).expect("a To line");
    assert!(to.contains(&format!(";tag={}", r.to_tag(&invite("z9hG4bK-a")))), "{to}");
}

/// Every copy within 64·T1 draws the refusal's bytes, jitter included; a
/// request never refused draws nothing.
#[test]
fn every_copy_draws_the_same_refusal() {
    let r = Refusals::new(&IdGen::seeded(1), 30);
    assert!(r.repeat(&invite("z9hG4bK-a"), SRC, 0).is_none(), "nothing is remembered yet");
    let first = refusal_wire(r.refuse(&invite("z9hG4bK-a"), SRC, 5, 0));
    let copy = r.repeat(&invite("z9hG4bK-a"), SRC, TIMER_B - 1).expect("a copy is remembered");
    assert_eq!(first, wire(copy));
    assert!(r.repeat(&invite("z9hG4bK-b"), SRC, 0).is_none(), "another INVITE is no copy");
    let other = refusal_wire(r.refuse(&invite("z9hG4bK-b"), SRC, 5, 0));
    let tag = |w: &str| w.lines().find(|l| l.starts_with("To:")).map(str::to_owned);
    assert_ne!(tag(&first), tag(&other), "distinct INVITEs get distinct tags");
}

/// The jittered `Retry-After` lies in `[base, base + jitter]`.
#[test]
fn the_retry_after_is_spread_over_the_jitter() {
    let r = Refusals::new(&IdGen::seeded(1), 4);
    for i in 0..20 {
        let out = refusal_wire(r.refuse(&invite(&format!("z9hG4bK-{i}")), SRC, 10, 0));
        let secs: u32 = out
            .lines()
            .find_map(|l| l.strip_prefix("Retry-After: "))
            .and_then(|v| v.parse().ok())
            .expect("a Retry-After");
        assert!((10..=14).contains(&secs), "{secs}");
    }
}

/// A refusal is forgotten after 64·T1, and the oldest first past the
/// capacity: a later copy is judged afresh.
#[test]
fn a_refusal_is_forgotten_after_64_t1_or_past_capacity() {
    let r = Refusals::new(&IdGen::seeded(1), 0);
    r.refuse(&invite("z9hG4bK-a"), SRC, 1, 0).expect("a refusal");
    assert!(r.repeat(&invite("z9hG4bK-a"), SRC, TIMER_B).is_none());

    // One refusal per stripe: of 100 refused, at most one per stripe stays,
    // and the latest is always among them.
    let small = Refusals::with_capacity(&IdGen::seeded(1), 0, 1);
    for i in 0..100 {
        small.refuse(&invite(&format!("z9hG4bK-{i}")), SRC, 1, 0).expect("a refusal");
    }
    let kept =
        (0..100).filter(|i| small.repeat(&invite(&format!("z9hG4bK-{i}")), SRC, 0).is_some());
    assert!(kept.count() <= STRIPES);
    assert!(small.repeat(&invite("z9hG4bK-99"), SRC, 0).is_some());
}

fn cancel(branch: &str) -> SipRequest {
    let raw = format!(
        "CANCEL sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@caller.test>;tag=alice-tag\r\n\
To: <sip:bob@uas.test>\r\n\
Call-ID: {branch}@10.0.0.1\r\n\
CSeq: 1 CANCEL\r\n\
Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

/// The CANCEL of a refused INVITE draws a 200 with the To-tag of the
/// INVITE's 503 (RFC 3261 §9.2); the CANCEL of any other INVITE draws none.
#[test]
fn the_cancel_of_a_refused_invite_draws_200_with_its_final_s_tag() {
    let r = Refusals::new(&IdGen::seeded(1), 0);
    let refused = refusal_wire(r.refuse(&invite("z9hG4bK-a"), SRC, 1, 0));
    let ok = wire(r.cancel(&cancel("z9hG4bK-a"), SRC, 0).expect("a refused INVITE"));
    assert!(ok.starts_with("SIP/2.0 200 OK\r\n"), "{ok}");
    assert!(ok.contains("CSeq: 1 CANCEL\r\n"), "{ok}");
    assert!(!ok.contains("Retry-After"), "{ok}");
    let to = |w: &str| w.lines().find(|l| l.starts_with("To:")).map(str::to_owned);
    assert_eq!(to(&ok), to(&refused));
    assert!(r.cancel(&cancel("z9hG4bK-b"), SRC, 0).is_none(), "an INVITE never refused");
}

/// A CANCEL refused for want of a target is answered but never remembered.
#[test]
fn a_refused_cancel_is_never_remembered() {
    let r = Refusals::new(&IdGen::seeded(1), 0);
    r.refuse(&cancel("z9hG4bK-a"), SRC, 1, 0).expect("a refusal");
    assert!(r.repeat(&cancel("z9hG4bK-a"), SRC, 0).is_none());
    assert!(r.cancel(&cancel("z9hG4bK-a"), SRC, 0).is_none());
}

/// `req` with `;rport` on its Via, asking for the source port back.
fn asking_rport(req: SipRequest) -> SipRequest {
    let raw = String::from_utf8(req.image().to_vec()).expect("utf-8");
    let raw = raw.replacen(":5555;branch=", ":5555;rport;branch=", 1);
    match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

/// A NAT-mapped source other than the Via's sent-by.
const MAPPED: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 7)), 40_000);

fn assert_stamped(resp: &SipResponse) {
    let top = resp.top_via();
    assert_eq!(top.received(), Some("192.0.2.7"), "{}", top.to_wire());
    assert_eq!(top.rport(), Some(Rport::Observed(40_000)), "{}", top.to_wire());
}

/// The repeated 503 and the 200 to a CANCEL record the source of the copy
/// they answer on the topmost Via (RFC 3261 §18.2.1, RFC 3581 §4).
#[test]
fn a_repeated_503_and_a_cancels_200_are_stamped_with_their_source() {
    let r = Refusals::new(&IdGen::seeded(1), 0);
    let invite = || asking_rport(invite("z9hG4bK-a"));
    r.refuse(&invite(), SRC, 1, 0).expect("a refusal");
    let copy = r.repeat(&invite(), MAPPED, 1).expect("a copy is remembered");
    assert_stamped(&copy);
    let ok = r.cancel(&asking_rport(cancel("z9hG4bK-a")), MAPPED, 1).expect("a refused INVITE");
    assert_eq!(ok.status(), 200);
    assert_stamped(&ok);
}
