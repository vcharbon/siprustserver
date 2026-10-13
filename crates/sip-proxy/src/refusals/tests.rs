use super::*;
use sip_message::{serialize, CustomParser, SipMessage, SipParser};

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

/// A tagged 503 echoing the request, with a floored `Retry-After` and no
/// `Reason`.
#[test]
fn a_refusal_is_a_tagged_503_with_retry_after_and_no_reason() {
    let r = Refusals::new(&IdGen::seeded(1), 0);
    let out = wire(r.refuse(&invite("z9hG4bK-a"), 0, 0));
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
    assert!(r.repeat(&invite("z9hG4bK-a"), 0).is_none(), "nothing is remembered yet");
    let first = wire(r.refuse(&invite("z9hG4bK-a"), 5, 0));
    let copy = r.repeat(&invite("z9hG4bK-a"), TIMER_B - 1).expect("a copy is remembered");
    assert_eq!(first, wire(copy));
    assert!(r.repeat(&invite("z9hG4bK-b"), 0).is_none(), "another INVITE is no copy");
    let other = wire(r.refuse(&invite("z9hG4bK-b"), 5, 0));
    let tag = |w: &str| w.lines().find(|l| l.starts_with("To:")).map(str::to_owned);
    assert_ne!(tag(&first), tag(&other), "distinct INVITEs get distinct tags");
}

/// The jittered `Retry-After` lies in `[base, base + jitter]`.
#[test]
fn the_retry_after_is_spread_over_the_jitter() {
    let r = Refusals::new(&IdGen::seeded(1), 4);
    for i in 0..20 {
        let out = wire(r.refuse(&invite(&format!("z9hG4bK-{i}")), 10, 0));
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
    r.refuse(&invite("z9hG4bK-a"), 1, 0);
    assert!(r.repeat(&invite("z9hG4bK-a"), TIMER_B).is_none());

    // One refusal per stripe: of 100 refused, at most one per stripe stays,
    // and the latest is always among them.
    let small = Refusals::with_capacity(&IdGen::seeded(1), 0, 1);
    for i in 0..100 {
        small.refuse(&invite(&format!("z9hG4bK-{i}")), 1, 0);
    }
    let kept = (0..100).filter(|i| small.repeat(&invite(&format!("z9hG4bK-{i}")), 0).is_some());
    assert!(kept.count() <= STRIPES);
    assert!(small.repeat(&invite("z9hG4bK-99"), 0).is_some());
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
    let refused = wire(r.refuse(&invite("z9hG4bK-a"), 1, 0));
    let ok = wire(r.cancel(&cancel("z9hG4bK-a"), 0).expect("a refused INVITE"));
    assert!(ok.starts_with("SIP/2.0 200 OK\r\n"), "{ok}");
    assert!(ok.contains("CSeq: 1 CANCEL\r\n"), "{ok}");
    assert!(!ok.contains("Retry-After"), "{ok}");
    let to = |w: &str| w.lines().find(|l| l.starts_with("To:")).map(str::to_owned);
    assert_eq!(to(&ok), to(&refused));
    assert!(r.cancel(&cancel("z9hG4bK-b"), 0).is_none(), "an INVITE never refused");
}

/// A CANCEL refused for want of a target is answered but never remembered.
#[test]
fn a_refused_cancel_is_never_remembered() {
    let r = Refusals::new(&IdGen::seeded(1), 0);
    r.refuse(&cancel("z9hG4bK-a"), 1, 0);
    assert!(r.repeat(&cancel("z9hG4bK-a"), 0).is_none());
    assert!(r.cancel(&cancel("z9hG4bK-a"), 0).is_none());
}
