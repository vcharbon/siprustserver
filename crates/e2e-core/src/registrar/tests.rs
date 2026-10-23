use super::*;
use sip_message::SipStr;

/// A REGISTER carrying `extra` on top of the mandatory headers, as the
/// parser reads it.
fn parse_register(extra: &[(&str, &str)]) -> Result<SipMessage, sip_message::SipParseError> {
    let mut raw =
        "REGISTER sip:127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-t\r\n\
             From: <sip:bob@register.example>;tag=t\r\nTo: <sip:bob@register.example>\r\n\
             Call-ID: c\r\nCSeq: 1 REGISTER\r\n"
            .to_string();
    for (name, value) in extra {
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    raw.push_str("Content-Length: 0\r\n\r\n");
    CustomParser::new().parse(raw.as_bytes())
}

fn register(extra: &[(&str, &str)]) -> SipRequest {
    match parse_register(extra).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("not a request"),
    }
}

fn hops(req: &SipRequest) -> Option<u32> {
    forwarded_max_forwards(req).ok().map(|h| h.value())
}

#[test]
fn max_forwards_decrements_by_one() {
    assert_eq!(hops(&register(&[("Max-Forwards", "70")])), Some(69));
}

#[test]
fn max_forwards_zero_is_rejected() {
    assert_eq!(hops(&register(&[("Max-Forwards", "0")])), None);
}

#[test]
fn max_forwards_absent_is_the_decremented_default() {
    // A request missing the header is treated as the §8.1.1.6 default (70).
    assert_eq!(hops(&register(&[])), Some(69));
}

/// A hop count no reader accepts never reaches the forwarding path: the
/// parser refuses the whole message, so the proxy's default covers the
/// request that states no count at all.
#[test]
fn max_forwards_non_numeric_never_parses() {
    assert!(parse_register(&[("Max-Forwards", "garbage")]).is_err());
}

/// RegisterStrategy.computeEffectiveExpires precedence: Expires header wins,
/// then Contact `;expires`, then the default; `0` is preserved (de-register).
#[test]
fn effective_expires_precedence() {
    let contact = |value: &str| Contact::parse(&SipStr::owned(value)).unwrap();
    let plain = contact("<sip:bob@127.0.0.1:5170>");

    // Expires header wins over the default.
    let r = register(&[("Contact", "<sip:bob@127.0.0.1:5170>"), ("Expires", "120")]);
    assert_eq!(effective_expires(&r, Some(&plain)), 120);

    // Header-level Contact `;expires=` when no Expires header.
    let tagged = contact("<sip:bob@127.0.0.1:5170>;expires=42");
    let r = register(&[("Contact", "<sip:bob@127.0.0.1:5170>;expires=42")]);
    assert_eq!(effective_expires(&r, Some(&tagged)), 42);

    // Default when neither is present.
    let r = register(&[("Contact", "<sip:bob@127.0.0.1:5170>")]);
    assert_eq!(effective_expires(&r, Some(&plain)), DEFAULT_EXPIRES_SEC);

    // 0 is preserved (de-registration).
    let r = register(&[("Contact", "<sip:bob@127.0.0.1:5170>"), ("Expires", "0")]);
    assert_eq!(effective_expires(&r, Some(&plain)), 0);
}

/// A wildcard Contact reads as no binding at all: it may de-register, and
/// the 200 OK echoes the bytes the UA sent (RFC 3261 §10.3 step 6).
#[test]
fn wildcard_contact_carries_no_binding() {
    assert!(Contact::parse(&SipStr::owned("*")).is_err());
    let r = register(&[("Contact", "*"), ("Expires", "0")]);
    assert_eq!(effective_expires(&r, None), 0);
}
