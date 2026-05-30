//! Type-level guarantees of the header model. Port of
//! `tests/sip/header-registry-typing.test.ts`.
//!
//! The TS file used `expectTypeOf` compile-time assertions over the
//! `getHeader<K>` registry. The Rust port (ADR-0003) replaces the registry
//! with concrete typed fields + refined views, so the equivalent guarantees
//! are enforced by the type system directly: this file is a set of bindings
//! with explicit type annotations that only compile if the field types hold.
//! Its value is at compile time — the `#[test]` bodies are trivial.

use sip_message::{
    Contact, ContactSet, CustomParser, InDialogRequest, NameAddr, RequestUri, SipMessage,
    SipParser, SipResponseTagged, Via,
};

const BYE: &[u8] = b"BYE sip:alice@pc33.example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK2\r\n\
From: Bob <sip:bob@example.com>;tag=as83kf\r\n\
To: Alice <sip:alice@example.com>;tag=1928\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 1 BYE\r\n\
Content-Length: 0\r\n\r\n";

const OK_200: &[u8] = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>;tag=as83kf\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";

#[test]
fn mandatory_eager_headers_are_plain_t_not_option() {
    let SipMessage::Request(req) = CustomParser::new().parse(BYE).unwrap() else { unreachable!() };
    // call-id is a plain String, never Option.
    let _call_id: &str = req.call_id.as_str();
    // from / to are plain NameAddr (grammar-mandatory), never Option.
    let _from: &NameAddr = &req.from;
    let _to: &NameAddr = &req.to;
    // The context-dependent tag stays Option on the base type.
    let _from_tag: Option<&str> = req.from.tag.as_deref();
}

#[test]
fn via_is_non_empty_first_is_via_not_option() {
    let SipMessage::Request(req) = CustomParser::new().parse(BYE).unwrap() else { unreachable!() };
    // NonEmpty::first returns &Via, not Option<&Via>.
    let _top: &Via = req.via.first();
}

#[test]
fn contacts_is_a_set_not_an_option() {
    let SipMessage::Request(req) = CustomParser::new().parse(BYE).unwrap() else { unreachable!() };
    // The single-Contact accessor surfaces as Option<&Contact> on a set.
    let _maybe: Option<&Contact> = match &req.contacts {
        ContactSet::Contacts(cs) => cs.first(),
        ContactSet::Wildcard => None,
    };
}

#[test]
fn refined_views_make_tags_infallible() {
    let SipMessage::Request(req) = CustomParser::new().parse(BYE).unwrap() else { unreachable!() };
    let in_dialog = InDialogRequest::new(&req).expect("BYE is in-dialog");
    // On the refined view both tags are infallible &str.
    let _from_tag: &str = in_dialog.from_tag();
    let _to_tag: &str = in_dialog.to_tag();

    let SipMessage::Response(resp) = CustomParser::new().parse(OK_200).unwrap() else {
        unreachable!()
    };
    let tagged = SipResponseTagged::new(&resp).expect("200 has a To-tag");
    let _to_tag: &str = tagged.to_tag();
}

#[test]
fn request_uri_is_on_request_only_with_typed_fields() {
    let SipMessage::Request(req) = CustomParser::new().parse(BYE).unwrap() else { unreachable!() };
    let _ru: &RequestUri = &req.request_uri;
    let _host: &str = req.request_uri.host.as_str();
    let _port: Option<u16> = req.request_uri.port;
    // SipResponse has no `request_uri` field — guaranteed structurally (the
    // struct simply lacks it); there is nothing to assert at runtime.
}
