use super::*;
use sip_message::parser::custom::CustomParser;
use sip_message::parser::SipParser;

fn parse(raw: &[u8]) -> SipMessage {
    CustomParser::new().parse(raw).expect("well-formed")
}

#[test]
fn classifies_tier1_vs_frozen_by_name() {
    assert_eq!(HeaderName::class_of("Via"), HeaderClass::Structural);
    assert_eq!(HeaderName::class_of("CALL-ID"), HeaderClass::Structural);
    assert_eq!(HeaderName::class_of("cseq"), HeaderClass::Structural);
    assert_eq!(HeaderName::class_of("Content-Length"), HeaderClass::Structural);
    assert_eq!(HeaderName::class_of("Record-Route"), HeaderClass::Structural);
    // Content-Type is FROZEN even though Content-Length is regenerated.
    assert_eq!(HeaderName::class_of("Content-Type"), HeaderClass::EndToEnd);
    assert_eq!(HeaderName::class_of("P-Asserted-Identity"), HeaderClass::EndToEnd);
    assert_eq!(HeaderName::class_of("X-Custom"), HeaderClass::EndToEnd);
}

#[test]
fn from_message_preserves_casing_order_and_duplicates() {
    let raw = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=at\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: c-1@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
p-AsSeRtEd-IdEnTiTy: <sip:+15550001@example.com>\r\n\
X-Dup: one\r\n\
X-Dup: two\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 0\r\n\r\n";
    let msg = parse(raw);
    let tmpl = MessageTemplate::from_message(&msg);
    assert_eq!(tmpl.method(), Some(&Method::Invite));

    let frozen = tmpl.frozen_headers();
    // Tier-1 headers (Via/From/To/Call-ID/CSeq/Max-Forwards/Contact/
    // Content-Length) are excluded; Content-Type + the customs remain.
    let names: Vec<&str> = frozen.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["p-AsSeRtEd-IdEnTiTy", "X-Dup", "X-Dup", "Content-Type"]);
    // Casing preserved verbatim.
    assert_eq!(frozen[0].name, "p-AsSeRtEd-IdEnTiTy");
    assert_eq!(frozen[0].value, "<sip:+15550001@example.com>");
    // Both duplicate rows preserved in order.
    assert_eq!(frozen[1].value, "one");
    assert_eq!(frozen[2].value, "two");
}

#[test]
fn classifies_rfc3261_compact_forms_before_freezing() {
    // Compact tier-1 names expand and classify Regenerated — else a templated
    // compact Via/From/… would freeze and DUPLICATE on the wire.
    assert_eq!(HeaderName::class_of("v"), HeaderClass::Structural, "v = Via");
    assert_eq!(HeaderName::class_of("V"), HeaderClass::Structural, "case-insensitive");
    assert_eq!(HeaderName::class_of("f"), HeaderClass::Structural, "f = From");
    assert_eq!(HeaderName::class_of("t"), HeaderClass::Structural, "t = To");
    assert_eq!(HeaderName::class_of("i"), HeaderClass::Structural, "i = Call-ID");
    assert_eq!(HeaderName::class_of("m"), HeaderClass::Structural, "m = Contact");
    assert_eq!(HeaderName::class_of("l"), HeaderClass::Structural, "l = Content-Length");
    // `c` = Content-Type is FROZEN (like its long form), as are the other
    // compact forms that map to non-tier-1 headers.
    assert_eq!(HeaderName::class_of("c"), HeaderClass::EndToEnd, "c = Content-Type (frozen)");
    assert_eq!(HeaderName::class_of("s"), HeaderClass::EndToEnd, "s = Subject");
    assert_eq!(HeaderName::class_of("k"), HeaderClass::EndToEnd, "k = Supported");
    assert_eq!(HeaderName::class_of("e"), HeaderClass::EndToEnd, "e = Content-Encoding");
    // A non-compact single char is not a compact form.
    assert_eq!(HeaderName::class_of("x"), HeaderClass::EndToEnd);
}

#[test]
fn frozen_headers_excludes_compact_tier1() {
    let tmpl = MessageTemplate::request(
        Method::Invite,
        vec![
            TemplateHeader::classified("v", "SIP/2.0/UDP 1.2.3.4:5060;branch=z9hG4bK-x"),
            TemplateHeader::classified("i", "cap@1.2.3.4"),
            TemplateHeader::classified("c", "application/sdp"),
            TemplateHeader::classified("Subject", "hi"),
        ],
        Vec::new(),
    );
    let names: Vec<String> = tmpl.frozen_headers().iter().map(|h| h.name.to_string()).collect();
    // `v` and `i` are tier-1 (dropped); `c` (Content-Type) and Subject frozen.
    assert_eq!(names, vec!["c".to_string(), "Subject".to_string()]);
}

#[test]
fn from_message_recovers_compact_wire_names() {
    let body = "hi";
    let raw = format!(
        "INVITE sip:bob@ex SIP/2.0\r\n\
v: SIP/2.0/UDP 1.2.3.4:5060;branch=z9hG4bK-x\r\n\
Max-Forwards: 70\r\n\
f: <sip:a@ex>;tag=at\r\n\
t: <sip:b@ex>\r\n\
i: cid@1.2.3.4\r\n\
CSeq: 1 INVITE\r\n\
m: <sip:a@1.2.3.4:5060>\r\n\
c: text/plain\r\n\
l: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let msg = parse(raw.as_bytes());
    let tmpl = MessageTemplate::from_message(&msg);
    let find =
        |canon: &str| tmpl.headers().iter().find(|h| h.name.eq_ignore_ascii_case(canon)).unwrap();
    // Compact tier-1 headers: canonical name, compact wire form, Regenerated.
    assert_eq!(find("Via").wire_name.as_deref(), Some("v"));
    assert_eq!(find("Via").class, HeaderClass::Structural);
    assert_eq!(find("From").wire_name.as_deref(), Some("f"));
    assert_eq!(find("To").wire_name.as_deref(), Some("t"));
    assert_eq!(find("Call-ID").wire_name.as_deref(), Some("i"));
    assert_eq!(find("Contact").wire_name.as_deref(), Some("m"));
    assert_eq!(find("Content-Length").wire_name.as_deref(), Some("l"));
    // Compact Content-Type is FROZEN and emits under `c`.
    assert_eq!(find("Content-Type").wire_name.as_deref(), Some("c"));
    assert_eq!(find("Content-Type").class, HeaderClass::EndToEnd);
    assert_eq!(
        tmpl.frozen_headers().iter().filter(|h| h.name == "c").count(),
        1,
        "frozen Content-Type emits compact",
    );
    // A header captured full stays canonical.
    assert_eq!(find("Max-Forwards").wire_name, None);
    // regenerated_name_forms: v/f/t/i/m present; content-length excluded.
    let forms = tmpl.regenerated_name_forms();
    for pair in [("Via", "v"), ("From", "f"), ("To", "t"), ("Call-ID", "i"), ("Contact", "m")] {
        assert!(forms.contains(&(pair.0.to_string(), pair.1.to_string())), "missing {pair:?}");
    }
    assert!(
        !forms.iter().any(|(c, _)| c.eq_ignore_ascii_case("content-length")),
        "Content-Length name-form is serializer-owned, never rewritten",
    );
}

#[test]
fn from_message_full_names_yield_no_wire_forms() {
    let msg = parse(
        b"INVITE sip:bob@ex SIP/2.0\r\n\
Via: SIP/2.0/UDP 1.2.3.4:5060;branch=z9hG4bK-x\r\n\
Max-Forwards: 70\r\n\
From: <sip:a@ex>;tag=at\r\n\
To: <sip:b@ex>\r\n\
Call-ID: cid@1.2.3.4\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:a@1.2.3.4:5060>\r\n\
Content-Length: 0\r\n\r\n",
    );
    let tmpl = MessageTemplate::from_message(&msg);
    assert!(tmpl.headers().iter().all(|h| h.wire_name.is_none()), "no compact forms");
    assert!(tmpl.regenerated_name_forms().is_empty(), "nothing to rewrite");
}

#[test]
fn apply_name_forms_rewrites_only_mapped_headers() {
    let headers = vec![
        SipHeader { name: "Via".into(), value: "x".into() },
        SipHeader { name: "From".into(), value: "y".into() },
        SipHeader { name: "Subject".into(), value: "z".into() },
    ];
    let forms = vec![("Via".to_string(), "v".to_string()), ("From".to_string(), "f".to_string())];
    let out = apply_name_forms(&headers, &forms);
    assert_eq!(out[0].name, "v");
    assert_eq!(out[1].name, "f");
    assert_eq!(out[2].name, "Subject", "unmapped header untouched");
    // Empty map = identity (full names).
    assert_eq!(apply_name_forms(&headers, &[]), headers);
}

#[test]
fn header_identity_is_compact_aware() {
    use sip_message::header::HeaderName;
    assert!(HeaderName::Supported.matches("Supported"));
    assert!(HeaderName::Supported.matches("k"), "compact Supported");
    assert!(HeaderName::Supported.matches("K"), "compact, case-insensitive");
    assert!(HeaderName::ContentType.matches("c"), "compact Content-Type");
    assert!(HeaderName::From.matches("f"));
    // Allow has no compact form; `a` must not falsely match.
    assert!(HeaderName::Allow.matches("Allow"));
    assert!(!HeaderName::Allow.matches("a"));
    // A different header never matches.
    assert!(!HeaderName::Supported.matches("Require"));
    assert!(!HeaderName::Supported.matches("x"));
}

#[test]
fn regenerated_name_forms_mixed_via_collapses() {
    // A capture with one full `Via:` and one compact `v:` row (v1 boundary).
    let raw = "INVITE sip:b@ex SIP/2.0\r\n\
Via: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
v: SIP/2.0/UDP 2.2.2.2:5060;branch=z9hG4bK-2\r\n\
Max-Forwards: 70\r\n\
From: <sip:a@ex>;tag=at\r\n\
To: <sip:b@ex>\r\n\
Call-ID: c@1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:a@1.1.1.1:5060>\r\n\
Content-Length: 0\r\n\r\n";
    let tmpl = MessageTemplate::from_message(&parse(raw.as_bytes()));
    let forms = tmpl.regenerated_name_forms();
    // Only the compact row yields a form (canonical Via -> v).
    assert_eq!(forms.iter().filter(|(c, _)| c.eq_ignore_ascii_case("via")).count(), 1,);
    assert!(forms.contains(&("Via".to_string(), "v".to_string())));
    // apply_name_forms rewrites EVERY Via row to the one form (the collapse).
    let headers = vec![
        SipHeader { name: "Via".into(), value: "one".into() },
        SipHeader { name: "Via".into(), value: "two".into() },
    ];
    let out = apply_name_forms(&headers, &forms);
    assert_eq!(out[0].name, "v");
    assert_eq!(out[1].name, "v", "v1: both rows collapse to the compact form");
}

/// The captured Contact keeps its display name as written; the host:port moved
/// onto the bound socket is the one of the URI the name-addr grammar reads,
/// never a URI quoted inside the display name.
#[test]
fn remote_target_emit_rewrites_the_address_not_the_quoted_display_name() {
    let headers = vec![SipHeader::new("Contact", "<sip:uac@192.0.2.10:5070>")];
    let emits = vec![(
        "contact".to_string(),
        "\"Alice <sip:evil@example.com>\" <sip:real@example.com>".to_string(),
    )];
    let out = apply_remote_target_emits(&headers, &emits);
    assert_eq!(out[0].value, "\"Alice <sip:evil@example.com>\" <sip:real@192.0.2.10:5070>");
}

#[test]
fn response_template_carries_status_and_reason() {
    let tmpl = MessageTemplate::response(
        486,
        "Busy Here",
        vec![TemplateHeader::frozen("Retry-After", "30")],
        Vec::new(),
    );
    assert_eq!(tmpl.status(), Some((486, "Busy Here")));
    assert_eq!(tmpl.method(), None);
    assert_eq!(tmpl.frozen_headers().len(), 1);
}
