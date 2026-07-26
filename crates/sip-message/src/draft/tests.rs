//! Draft engine behaviour: seeding, editing, and the two exits.

use bytes::Bytes;

use crate::draft::{IncompleteDraft, RequestDraft, ResponseDraft};
use crate::header::{self, HeaderName, HeaderValue, MediaType, Uri, Via};
use crate::method::Method;
use crate::parser::custom::CustomParser;
use crate::parser::SipParser;
use crate::sip_str::SipStr;
use crate::types::{SipMessage, SipRequest};

const INVITE: &str = concat!(
    "INVITE sip:bob@biloxi.com SIP/2.0\r\n",
    "Via: SIP/2.0/UDP client.atlanta.com:5060;branch=z9hG4bK74bf9\r\n",
    "Max-Forwards: 70\r\n",
    "Route: <sip:p1.example.com;lr>, <sip:p2.example.com;lr>\r\n",
    "From: Alice <sip:alice@atlanta.com>;tag=9fxced76sl\r\n",
    "To: Bob <sip:bob@biloxi.com>\r\n",
    "Call-ID: 3848276298220188511@atlanta.com\r\n",
    "CSeq: 1 INVITE\r\n",
    "Contact: <sip:alice@client.atlanta.com:5060>\r\n",
    "Supported: 100rel\r\n",
    "Content-Type: application/sdp\r\n",
    "Content-Length: 4\r\n",
    "\r\n",
    "v=0\n",
);

fn invite() -> SipRequest {
    match CustomParser::new().parse(INVITE.as_bytes()).expect("fixture parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("fixture is a request"),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_blank_draft_names_every_header_it_still_needs() {
    let draft = RequestDraft::new(Method::Options, Uri::sip("biloxi.com"));
    match draft.freeze() {
        Err(IncompleteDraft::Missing(missing)) => {
            assert_eq!(missing.len(), 6, "{missing:?}");
            assert!(missing.contains(&HeaderName::CallId));
            assert!(missing.contains(&HeaderName::MaxForwards));
        }
        other => panic!("expected a missing-header report, got {other:?}"),
    }
}

#[test]
fn a_blank_draft_builds_a_message_that_reparses_to_itself() {
    let frozen = RequestDraft::new(Method::Options, Uri::sip("biloxi.com"))
        .push(Via::udp("atlanta.com", 5060).with_branch("z9hG4bKopt"))
        .push(header::MaxForwards::new(70))
        .push(header::From::from_uri(Uri::sip_user("alice", "atlanta.com")).with_tag("t1"))
        .push(header::To::from_uri(Uri::sip_user("bob", "biloxi.com")))
        .push(header::CallId::new("call-1@atlanta.com"))
        .push(header::CSeq::new(1, Method::Options))
        .freeze()
        .expect("every mandatory header is present");

    assert_eq!(frozen.method, Method::Options);
    assert_eq!(frozen.from.tag.as_deref(), Some("t1"));
    assert_eq!(frozen.via.first().host, "atlanta.com");
    // The built message carries a real image: re-parsing its own bytes yields
    // the same header list.
    let reparsed = CustomParser::new().parse(&frozen.raw).expect("built bytes parse");
    assert_eq!(reparsed.headers(), frozen.headers.as_slice());
}

#[test]
fn thawing_and_freezing_preserves_every_line_and_the_body() {
    let original = invite();
    let frozen = original.thaw().freeze().expect("a thawed draft is complete");

    let before: Vec<(String, String)> = original
        .headers
        .iter()
        .map(|h| (HeaderName::of(&h.name).as_wire_str().to_owned(), h.value.to_string()))
        .collect();
    let after: Vec<(String, String)> =
        frozen.headers.iter().map(|h| (h.name.to_string(), h.value.to_string())).collect();
    assert_eq!(before, after);
    assert_eq!(frozen.body, original.body);
    assert_eq!(frozen.from.tag, original.from.tag);
    assert_eq!(frozen.via.len(), original.via.len());
}

#[test]
fn a_proxy_hop_touches_only_the_routing_headers() {
    let original = invite();
    let hop = original
        .thaw()
        .prepend(Via::udp("proxy.example", 5080).with_branch("z9hG4bKhop"))
        .update::<header::MaxForwards>(|mf| mf.decremented().unwrap_or(mf))
        .expect("Max-Forwards reads")
        .list::<header::RouteEntry>(|mut routes| {
            routes.pop_front();
            routes
        })
        .expect("the route set reads")
        .freeze()
        .expect("still complete");

    assert_eq!(hop.via.len(), 2);
    assert_eq!(hop.via.first().host, "proxy.example");
    assert_eq!(hop.raw(HeaderName::MaxForwards).next(), Some("69"));
    let routes = hop.route_set().expect("routes read back");
    assert_eq!(routes.len(), 1);
    assert_eq!(routes.first().unwrap().uri().host(), "p2.example.com");
    // Untouched lines keep their bytes exactly.
    assert_eq!(hop.raw(HeaderName::CallId).next(), Some("3848276298220188511@atlanta.com"));
}

#[test]
fn a_line_that_does_not_read_fails_the_edit_loudly() {
    // A second Max-Forwards line no reader can make sense of: the decrement
    // must report, never quietly forward the hop budget untouched.
    let draft = invite().thaw().push_raw(HeaderName::MaxForwards, "seventy");
    let outcome = draft.update::<header::MaxForwards>(|mf| mf.decremented().unwrap_or(mf));
    assert!(outcome.is_err(), "an unreadable line must not leave the draft unedited");
}

#[test]
fn a_top_line_edit_leaves_the_lines_below_it_unread() {
    // A hop stamps its own Via while a lower hop's line is one no reader can
    // make sense of: the relay must still leave, and that line must survive
    // byte for byte.
    let hop = invite()
        .thaw()
        .push_raw(HeaderName::Via, "SIP/2.0")
        .update_top::<Via>(|via| via.with_received("192.0.2.7"))
        .expect("only the top line is read")
        .freeze()
        .expect("still complete");
    let vias: Vec<&str> = hop.raw(HeaderName::Via).collect();
    assert_eq!(vias[0], "SIP/2.0/UDP client.atlanta.com:5060;branch=z9hG4bK74bf9;received=192.0.2.7");
    assert_eq!(vias[1], "SIP/2.0");
}

#[test]
fn popping_the_top_entry_keeps_the_rest_of_a_folded_line() {
    let popped = invite().thaw().pop_top::<header::RouteEntry>().expect("the top line reads");
    let routes = popped.values::<header::RouteEntry>().expect("reads");
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].uri().host(), "p2.example.com");
}

#[test]
fn popping_the_only_entry_of_a_line_removes_the_line() {
    let popped = invite().thaw().pop_top::<Via>().expect("the top line reads");
    assert!(!popped.has(&HeaderName::Via), "the message's only Via line is gone");
}

#[test]
fn freeze_bytes_is_the_frozen_message_without_the_message() {
    let draft = invite().thaw();
    let frozen = draft.clone().freeze().expect("complete");
    assert_eq!(draft.freeze_bytes().expect("complete"), frozen.raw);

    let blank = RequestDraft::new(Method::Options, Uri::sip("biloxi.com"));
    assert!(blank.freeze_bytes().is_err(), "an incomplete draft yields no wire bytes");
}

#[test]
fn prepending_a_header_the_draft_lacks_appends_it() {
    let draft = RequestDraft::new(Method::Options, Uri::sip("biloxi.com"))
        .push(header::CallId::new("call-1@atlanta.com"))
        .prepend(Via::udp("atlanta.com", 5060).with_branch("z9hG4bKopt"));
    let names: Vec<&HeaderName> = draft.entries().iter().map(|e| e.name()).collect();
    assert_eq!(names, [&HeaderName::CallId, &HeaderName::Via]);
}

#[test]
fn a_thawed_request_uri_is_forwarded_byte_for_byte() {
    let original = invite();
    let frozen = original.thaw().freeze().expect("a thawed draft is complete");
    assert_eq!(frozen.uri, original.uri);
}

#[test]
fn keep_filters_by_header_identity_not_by_name_strings() {
    let original = invite();
    let kept = RequestDraft::keep(&original, |name| {
        !matches!(name, HeaderName::Route | HeaderName::Contact | HeaderName::Supported)
    });
    assert!(!kept.has(&HeaderName::Route));
    assert!(!kept.has(&HeaderName::Contact));
    assert!(kept.has(&HeaderName::From));
    let frozen = kept.freeze().expect("mandatory headers survive the filter");
    assert!(frozen.raw(HeaderName::Route).next().is_none());
}

#[test]
fn a_body_swap_restates_the_length_and_the_media_type() {
    let frozen = invite()
        .thaw()
        .body(Bytes::from_static(b"v=0\r\no=- 1 1 IN IP4 h\r\n"), MediaType::new("application/sdp"))
        .freeze()
        .expect("still complete");
    assert_eq!(frozen.raw(HeaderName::ContentLength).next(), Some("23"));
    assert_eq!(frozen.body.len(), 23);
    assert_eq!(frozen.raw(HeaderName::ContentType).next(), Some("application/sdp"));
}

#[test]
fn render_unchecked_emits_an_invalid_datagram_without_minting_a_message() {
    let bytes = RequestDraft::new(Method::Invite, Uri::sip_user("bob", "biloxi.com"))
        .push_raw(HeaderName::Via, "not a via at all")
        .push_raw(HeaderName::CSeq, "one INVITE")
        .render_unchecked();
    let wire = text(&bytes);
    assert!(wire.starts_with("INVITE sip:bob@biloxi.com SIP/2.0\r\n"), "{wire}");
    assert!(wire.contains("Via: not a via at all\r\n"), "{wire}");
    assert!(wire.contains("CSeq: one INVITE\r\n"), "{wire}");
}

#[test]
fn a_response_draft_rides_the_same_engine() {
    let request = invite();
    let response = ResponseDraft::new(180, "Ringing")
        .push_raw(HeaderName::Via, request.raw(HeaderName::Via).next().unwrap().to_owned())
        .push(request.from())
        .push(request.to().with_tag("b0b"))
        .push(request.call_id())
        .push(request.cseq())
        .freeze()
        .expect("a response needs no Max-Forwards");

    assert_eq!(response.status, 180);
    assert_eq!(response.reason, "Ringing");
    assert_eq!(response.to.tag.as_deref(), Some("b0b"));
    assert_eq!(response.via.first().host, "client.atlanta.com");
}

#[test]
fn an_untouched_entry_is_never_reparsed() {
    let draft = invite().thaw();
    assert!(draft.entries().iter().all(|e| e.is_raw()), "thaw parses nothing");
    let edited = draft.update::<header::MaxForwards>(|mf| mf.incremented()).expect("reads");
    let typed: Vec<&HeaderName> =
        edited.entries().iter().filter(|e| !e.is_raw()).map(|e| e.name()).collect();
    assert_eq!(typed, [&HeaderName::MaxForwards], "only the touched header became typed");
}

#[test]
fn a_draft_reads_its_own_typed_values() {
    let draft = invite().thaw();
    let supported = draft.header::<header::Supported>().expect("present").expect("reads");
    assert!(supported.contains("100rel"));
    assert_eq!(draft.header::<header::From>().unwrap().unwrap().tag(), Some("9fxced76sl"));
    assert_eq!(draft.values::<header::RouteEntry>().unwrap().len(), 2);
}

#[test]
fn a_frozen_message_answers_the_typed_read_surface() {
    let msg = invite();
    assert_eq!(msg.from().display(), Some("Alice"));
    assert_eq!(msg.to().uri().host_port(), ("biloxi.com", 5060));
    assert_eq!(msg.top_via().branch(), Some("z9hG4bK74bf9"));
    assert_eq!(msg.cseq().to_wire(), "1 INVITE");
    assert_eq!(msg.call_id().as_str(), "3848276298220188511@atlanta.com");
    assert!(msg.header::<header::Supported>().unwrap().unwrap().contains("100rel"));
    assert_eq!(msg.request_uri().user(), Some("bob"));
    assert!(msg.has(&HeaderName::Contact));
    assert_eq!(msg.raw(HeaderName::ContentType).next(), Some("application/sdp"));
    let _ = SipStr::EMPTY;
}
