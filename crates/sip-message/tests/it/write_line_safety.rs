//! Text a caller hands the write API never splits a header line (RFC 3261 §7,
//! §25.1): a header name is a `token`, and no CR or LF reaches a header value
//! or the start line, whichever writer carried it. A draft holding such text
//! does not freeze, and an identity read from caller text refuses it.
//!
//! Every message that does freeze reads back as the object it froze to
//! ([`frozen_agrees_with_reparse`]).

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::{self, NameAddr, ParamValue, Uri};
use sip_message::{
    CustomParser, HeaderName, SipMessage, SipParser, SipRequest, SipResponse, SipStr,
};

const VIA: &str = "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-write";
const INJECTED_VIA: &str = "Via: SIP/2.0/UDP evil.example;branch=z9hG4bKinjected";

fn request() -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\nVia: {VIA}\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=a1\r\nTo: <sip:bob@example.com>\r\n\
         Call-ID: write-1@192.0.2.10\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:alice@192.0.2.10:5060>\r\nContent-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("the base request parses: {other:?}"),
    }
}

fn response() -> SipResponse {
    let raw = format!(
        "SIP/2.0 200 OK\r\nVia: {VIA}\r\nFrom: <sip:alice@example.com>;tag=a1\r\n\
         To: <sip:bob@example.com>;tag=b1\r\nCall-ID: write-1@192.0.2.10\r\n\
         CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()) {
        Ok(SipMessage::Response(r)) => r,
        other => panic!("the base response parses: {other:?}"),
    }
}

/// `msg`'s header lines as (name, value) text, in wire order.
fn lines(msg: &SipMessage) -> Vec<(String, String)> {
    msg.headers().iter().map(|h| (h.name.to_string(), h.value.to_string())).collect()
}

/// The test-only agreement check: the frozen message and a re-parse of its
/// wire bytes carry the same header lines and the same Via count.
fn frozen_agrees_with_reparse(frozen: SipMessage) {
    let reparsed = CustomParser::new()
        .parse(frozen.image())
        .unwrap_or_else(|e| panic!("a frozen message re-parses: {e:?}"));
    assert_eq!(lines(&frozen), lines(&reparsed), "{:?}", String::from_utf8_lossy(frozen.image()));
    assert_eq!(frozen.via().len(), reparsed.via().len());
}

/// A request draft holding unsafe text: refused at freeze, never frozen into
/// an object its own bytes contradict.
fn assert_request_refused(draft: sip_message::draft::RequestDraft, what: &str) {
    let bytes = draft.clone().freeze_bytes();
    match draft.freeze() {
        Ok(msg) => panic!(
            "{what}: froze with {} Via, wire {:?}",
            msg.via().len(),
            String::from_utf8_lossy(msg.image())
        ),
        Err(_) => assert!(bytes.is_err(), "{what}: freeze_bytes refuses as freeze does"),
    }
}

#[test]
fn a_clean_draft_freezes_and_agrees_with_its_reparse() {
    let msg = request().thaw().push_raw(HeaderName::from("X-Test"), "ok").freeze().expect("freeze");
    frozen_agrees_with_reparse(SipMessage::Request(msg));
    let resp = response().thaw().with_status(486, "Busy Here").freeze().expect("freeze");
    frozen_agrees_with_reparse(SipMessage::Response(resp));
    let generated = generate_response(&request(), 180, "Ringing", &GenerateResponseOpts::default())
        .expect("generate");
    frozen_agrees_with_reparse(SipMessage::Response(generated));
}

/// A typed value's text reaches the wire escaped as its position asks: a
/// control byte in a display name is a `quoted-pair`, and the name reads
/// back unchanged.
#[test]
fn typed_text_freezes_escaped_and_reads_back_unchanged() {
    let from = request().from().clone().with_display("Ali\"ce \\ \u{1}");
    let to = request().to().clone().with_param("x", ParamValue::text("a b;c"));
    let contact = header::Contact::from_uri(Uri::sip_user("alice", "192.0.2.10"));
    let msg = request().thaw().set(from).set(to).set(contact).freeze().expect("freeze");
    assert_eq!(msg.from().display(), Some("Ali\"ce \\ \u{1}"));
    let reparsed = match CustomParser::new().parse(msg.image()) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("re-parses: {other:?}"),
    };
    assert_eq!(reparsed.from().display(), Some("Ali\"ce \\ \u{1}"));
    assert_eq!(reparsed.to().params().value("x"), Some("a b;c"));
    frozen_agrees_with_reparse(SipMessage::Request(msg));
}

/// A line stated from outside the stack is refused before it is written, so
/// a caller skips that line alone.
#[test]
fn a_checked_raw_entry_refuses_what_the_wire_cannot_carry() {
    use sip_message::draft::Entry;
    assert!(Entry::checked_raw(HeaderName::from("X-Ok"), "fine").is_ok());
    for (name, value) in [
        ("X-Test", format!("ok\r\n{INJECTED_VIA}")),
        ("X-Test", "a\rb".to_string()),
        ("X-Test", "a\0b".to_string()),
        ("X/Bad", "value".to_string()),
        ("X-A: b\r\nVia", "value".to_string()),
    ] {
        assert!(
            Entry::checked_raw(HeaderName::from(name), value.as_str()).is_err(),
            "{name:?}: {value:?}"
        );
    }
}

/// The 2-Via split: the value's CRLF would start a Via line the frozen object
/// never read.
#[test]
fn a_raw_value_with_crlf_does_not_freeze() {
    let draft =
        request().thaw().push_raw(HeaderName::from("X-Test"), format!("ok\r\n{INJECTED_VIA}"));
    assert_request_refused(draft, "raw value");
}

#[test]
fn a_raw_value_with_a_bare_cr_or_lf_does_not_freeze() {
    for value in ["ok\rX-Other: 1", "ok\nX-Other: 1"] {
        let draft = request().thaw().push_raw(HeaderName::from("X-Test"), value);
        assert_request_refused(draft, value);
    }
}

#[test]
fn a_header_name_with_crlf_does_not_freeze() {
    let draft = request()
        .thaw()
        .push_raw(HeaderName::from("X-A: b\r\nVia"), "SIP/2.0/UDP evil.example;branch=z9hG4bKname");
    assert_request_refused(draft, "header name");
}

#[test]
fn a_header_name_that_is_no_token_does_not_freeze() {
    let draft = request().thaw().push_raw(HeaderName::from("X/Bad"), "value");
    assert_request_refused(draft, "non-token name");
}

#[test]
fn a_display_name_with_crlf_does_not_freeze() {
    let from = request().from().clone().with_display(format!("Alice\r\n{INJECTED_VIA}"));
    assert_request_refused(request().thaw().set(from), "display name");
}

#[test]
fn a_uri_user_with_crlf_does_not_freeze() {
    let contact =
        header::Contact::from_uri(Uri::sip_user(format!("a\r\n{INJECTED_VIA}"), "192.0.2.10"));
    assert_request_refused(request().thaw().set(contact), "URI user");
}

#[test]
fn a_param_value_with_crlf_does_not_freeze() {
    let to =
        request().to().clone().with_param("x", ParamValue::text(format!("a\r\n{INJECTED_VIA}")));
    assert_request_refused(request().thaw().set(to), "param value");
}

#[test]
fn a_param_name_with_crlf_does_not_freeze() {
    let to = request().to().clone().with_param(format!("x\r\n{INJECTED_VIA}"), ParamValue::Flag);
    assert_request_refused(request().thaw().set(to), "param name");
}

#[test]
fn a_reason_phrase_with_crlf_does_not_freeze() {
    let draft = response().thaw().with_status(486, format!("Busy\r\n{INJECTED_VIA}"));
    assert!(draft.clone().freeze_bytes().is_err(), "freeze_bytes");
    if let Ok(msg) = draft.freeze() {
        panic!("froze: {:?}", String::from_utf8_lossy(msg.image()));
    }
}

#[test]
fn a_generated_response_refuses_a_reason_phrase_with_crlf() {
    let generated = generate_response(
        &request(),
        486,
        &format!("Busy\r\n{INJECTED_VIA}"),
        &GenerateResponseOpts::default(),
    );
    if let Ok(msg) = generated {
        panic!("generated: {:?}", String::from_utf8_lossy(msg.image()));
    }
}

/// An identity read from caller text admits no line break inside a quoted
/// display name or a quoted parameter (RFC 3261 §25.1 `quoted-string`).
#[test]
fn an_identity_with_crlf_inside_quotes_is_refused() {
    for text in [
        "\"x\r\nVia: SIP/2.0/UDP evil.example;branch=z9hG4bKi\" <sip:a@example.com>",
        "\"x\\\r\nVia: SIP/2.0/UDP evil.example;branch=z9hG4bKi\" <sip:a@example.com>",
        "<sip:a@example.com>;p=\"\r\nVia: SIP/2.0/UDP evil.example\"",
        "\"x\ry\" <sip:a@example.com>",
    ] {
        let read = NameAddr::parse_untagged_identity(&SipStr::owned(text));
        assert!(read.is_err(), "{text:?} read as {read:?}");
    }
}

/// A control byte other than HTAB stands in a value only as the escaped byte
/// of a quoted-pair (RFC 3261 §25.1), and caller text carries no NUL at all:
/// a peer reading a value up to its first NUL reads another value.
#[test]
fn caller_text_carries_no_bare_control_byte_and_no_nul() {
    use sip_message::write_check;
    for bad in ["a\x07b", "a\x7fb", "a\x1fb", "a\0b", "\"a\\\0\"", "a\\\x07"] {
        assert!(write_check::header_value(bad).is_err(), "value {bad:?}");
    }
    for good in ["a\tb", "\"BEL:\\\x07 DEL:\\\x7f\"", "ü"] {
        assert!(write_check::header_value(good).is_ok(), "value {good:?}");
    }
    for bad in ["O\x07K", "O\x7fK", "O\0K"] {
        assert!(write_check::reason_phrase(bad).is_err(), "reason {bad:?}");
    }
    assert!(write_check::reason_phrase("O\tK ü").is_ok());
}

#[test]
fn a_raw_value_with_a_bare_control_byte_does_not_freeze() {
    for value in ["a\x07b", "a\0b", "a\x7fb"] {
        let draft = request().thaw().push_raw(HeaderName::from("X-Test"), value);
        assert_request_refused(draft, value);
    }
}

#[test]
fn a_reason_phrase_with_a_control_byte_does_not_freeze() {
    let draft = response().thaw().with_status(486, "Busy\x07Here");
    assert!(draft.freeze_bytes().is_err(), "freeze_bytes");
}

/// The quoted-pairs a peer sent (RFC 4475 §3.1.1.2) freeze and relay as
/// received.
#[test]
fn a_received_quoted_pair_control_byte_freezes() {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\nVia: {VIA}\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=a1\r\n\
         To: \"BEL:\\\x07 NUL:\\\0 DEL:\\\x7f\" <sip:bob@example.com>\r\n\
         Call-ID: write-1@192.0.2.10\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:alice@192.0.2.10:5060>\r\nContent-Length: 0\r\n\r\n"
    );
    let req = match CustomParser::new().parse(raw.as_bytes()) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("RFC 4475 §3.1.1.2 quoted-pairs parse: {other:?}"),
    };
    let frozen = req.thaw().push_raw(HeaderName::from("X-Test"), "ok").freeze().expect("freeze");
    frozen_agrees_with_reparse(SipMessage::Request(frozen));
}
