//! URI equivalence is RFC 3261 §19.1.4: the sets below are the RFC's own
//! examples. `==` stays the comparison of the parts as read.

use sip_message::header::Uri;
use sip_message::{CustomParser, SipParser, SipStr};

fn uri(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).unwrap_or_else(|e| panic!("{text}: {}", e.reason))
}

fn equivalent(a: &str, b: &str) -> bool {
    let (a, b) = (uri(a), uri(b));
    let forward = a.equivalent(&b);
    assert_eq!(forward, b.equivalent(&a), "symmetric");
    forward
}

#[test]
fn the_rfc_equivalent_sets() {
    for (a, b) in [
        ("sip:%61lice@atlanta.com;transport=TCP", "sip:alice@AtLanTa.CoM;Transport=tcp"),
        ("sip:carol@chicago.com", "sip:carol@chicago.com;newparam=5"),
        ("sip:carol@chicago.com", "sip:carol@chicago.com;security=on"),
        ("sip:carol@chicago.com;newparam=5", "sip:carol@chicago.com;security=on"),
        (
            "sip:biloxi.com;transport=tcp;method=REGISTER?to=sip:bob%40biloxi.com",
            "sip:biloxi.com;method=REGISTER;transport=tcp?to=sip:bob%40biloxi.com",
        ),
        (
            "sip:alice@atlanta.com?subject=project%20x&priority=urgent",
            "sip:alice@atlanta.com?priority=urgent&subject=project%20x",
        ),
    ] {
        assert!(equivalent(a, b), "{a} ~ {b}");
    }
}

#[test]
fn the_rfc_non_equivalent_sets() {
    for (a, b) in [
        ("SIP:ALICE@AtLanTa.CoM;Transport=udp", "sip:alice@AtLanTa.CoM;Transport=UDP"),
        ("sip:bob@biloxi.com", "sip:bob@biloxi.com:5060"),
        ("sip:bob@biloxi.com", "sip:bob@biloxi.com;transport=udp"),
        ("sip:bob@biloxi.com", "sip:bob@biloxi.com:6000;transport=tcp"),
        ("sip:carol@chicago.com", "sip:carol@chicago.com?Subject=next%20meeting"),
        ("sip:bob@phone21.boxesbybob.com", "sip:bob@192.0.2.4"),
        ("sip:carol@chicago.com;security=on", "sip:carol@chicago.com;security=off"),
    ] {
        assert!(!equivalent(a, b), "{a} !~ {b}");
    }
}

#[test]
fn the_other_rules_of_the_section() {
    // A SIP and a SIPS URI never match.
    assert!(!equivalent("sip:alice@atlanta.com", "sips:alice@atlanta.com"));
    // user, ttl, method and maddr present on one side only differ.
    for param in ["user=phone", "ttl=1", "method=INVITE", "maddr=192.0.2.1"] {
        let b = format!("sip:alice@atlanta.com;{param}");
        assert!(!equivalent("sip:alice@atlanta.com", &b), "{param}");
    }
    // A password, present on one side only, differs.
    assert!(!equivalent("sip:alice@atlanta.com", "sip:alice:secret@atlanta.com"));
    // A reserved character differs from its escape.
    assert!(!equivalent("sip:a%3Bb@atlanta.com", "sip:a;b@atlanta.com"));
    // An escaped `%` is no escape of what follows.
    assert!(!equivalent("sip:a%2541@atlanta.com", "sip:aA@atlanta.com"));
}

#[test]
fn equality_is_the_parts_as_read() {
    let (a, b) =
        (uri("sip:alice@atlanta.com;transport=TCP"), uri("sip:alice@AtLanTa.CoM;Transport=tcp"));
    assert!(a.equivalent(&b));
    assert_ne!(a, b, "== is not §19.1.4");
    assert_eq!(a, uri("sip:alice@atlanta.com;transport=TCP"));
}

/// The scheme is case-insensitive (RFC 3986 §3.1): a `SIP:` Contact names a
/// dialog target.
#[test]
fn an_upper_case_sip_contact_names_a_dialog_target() {
    let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-scheme\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: scheme-1@192.0.2.10\r\n\
CSeq: 1 INVITE\r\n\
Contact: <SIP:alice@192.0.2.10:5060>\r\n\
Content-Length: 0\r\n\r\n";
    if let Err(e) = CustomParser::new().parse(raw.as_bytes()) {
        panic!("{}", e.reason);
    }
}
