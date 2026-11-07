//! A single-valued header (`header::SINGLE_VALUED`) is on one line of every
//! message, whatever made it (RFC 3261 §7.3.1): each reading of the wire, a
//! stored image, a hydrated request and a frozen draft refuse a second line,
//! so the one value a reader gets is the only one there is.

use sip_message::draft::{IncompleteDraft, RequestDraft};
use sip_message::header::{self, CSeq, CallId, Expires, HeaderValue, MaxForwards, To, Uri, Via};
use sip_message::parser::ToolReading;
use sip_message::{
    hydrate_request, read_stored, CustomParser, HeaderName, Method, SipHeader, SipMessage,
    SipParser, SipParserLimits, SipStr,
};

const HEAD: &str = "SUBSCRIBE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: c1\r\n\
CSeq: 1 SUBSCRIBE\r\n\
Contact: <sip:alice@192.0.2.5>\r\n";

fn with(extra: &str) -> Vec<u8> {
    format!("{HEAD}{extra}Content-Length: 0\r\n\r\n").into_bytes()
}

/// One reading of a datagram, its refusal's reason as the error.
type Reading = Box<dyn Fn(&[u8]) -> Result<SipMessage, String>>;

/// Every reading a datagram can meet.
fn readings() -> Vec<(&'static str, Reading)> {
    let parse = |parser: CustomParser| {
        Box::new(move |raw: &[u8]| parser.parse(raw).map_err(|e| e.reason)) as Reading
    };
    let lenient = SipParserLimits { wire_grammar: false, ..SipParserLimits::DEFAULT };
    vec![
        ("built", parse(CustomParser::built())),
        ("tool", parse(CustomParser::tool(ToolReading::plain_rfc().limits().clone()))),
        ("lenient", parse(CustomParser::with_limits(lenient))),
        ("stored", Box::new(|raw: &[u8]| read_stored(raw).map_err(|e| e.reason))),
    ]
}

#[test]
fn every_reading_refuses_a_second_line_of_a_single_valued_header() {
    for extra in ["Event: presence\r\nEvent: presence\r\n", "Event: presence\r\no: dialog\r\n"] {
        for (name, read) in readings() {
            let refused = read(&with(extra)).expect_err(name);
            assert!(refused.starts_with("Multiple Event headers"), "{name}: {refused}");
        }
    }
    let charging = "P-Charging-Vector: icid-value=1\r\np-charging-vector: icid-value=1\r\n";
    for (name, read) in readings() {
        let refused = read(&with(charging)).expect_err(name);
        assert!(refused.starts_with("Multiple p-charging-vector"), "{name}: {refused}");
    }
}

#[test]
fn every_reading_reads_the_one_value() {
    for (name, read) in readings() {
        let SipMessage::Request(request) = read(&with("Event: presence;id=7\r\n")).expect(name)
        else {
            panic!("{name}: a request")
        };
        let event = request.header::<header::Event>().expect("carried").expect("reads");
        assert_eq!(event.to_wire(), "presence;id=7", "{name}");
    }
}

#[test]
fn a_list_header_keeps_every_line_in_every_reading() {
    let extra = "Supported: timer\r\nSupported: 100rel\r\n";
    for (name, read) in readings() {
        let SipMessage::Request(request) = read(&with(extra)).expect(name) else {
            panic!("{name}: a request")
        };
        assert_eq!(request.list::<header::Supported>().expect("reads").len(), 2, "{name}");
        let set = request.token_set::<header::Supported>().expect("carried").expect("reads");
        assert_eq!(set.to_wire(), "timer, 100rel", "{name}");
    }
}

fn line(name: &str, value: &str) -> SipHeader {
    SipHeader::new(SipStr::owned(name), SipStr::owned(value))
}

#[test]
fn a_hydrated_request_refuses_a_second_line() {
    let lines = |extra: Vec<SipHeader>| {
        let mut headers = vec![
            line("Via", "SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-h"),
            line("Max-Forwards", "70"),
            line("From", "<sip:alice@example.com>;tag=a1"),
            line("To", "<sip:bob@example.com>"),
            line("Call-ID", "h1"),
            line("CSeq", "1 OPTIONS"),
        ];
        headers.extend(extra);
        headers
    };
    let one = hydrate_request("OPTIONS", "sip:bob@example.com", lines(vec![]), Vec::new());
    assert!(one.is_ok());
    let extra = vec![line("Expires", "60"), line("Expires", "60")];
    let refused = hydrate_request("OPTIONS", "sip:bob@example.com", lines(extra), Vec::new())
        .expect_err("refused");
    assert!(refused.reason.starts_with("Multiple Expires headers"), "{}", refused.reason);
}

fn options() -> RequestDraft {
    RequestDraft::new(Method::Options, Uri::sip("example.com"))
        .push(Via::udp("192.0.2.1", 5060).with_branch("z9hG4bK-1"))
        .set(MaxForwards::new(70))
        .set(header::From::from_uri(Uri::sip("example.com")).with_tag("1"))
        .set(To::from_uri(Uri::sip("example.com")))
        .set(CallId::new("c1"))
        .set(CSeq::new(1, Method::Options))
}

#[test]
fn a_draft_holding_a_second_line_reads_no_value_and_never_freezes() {
    let draft = options().push_raw(HeaderName::Expires, "60").push_raw(HeaderName::Expires, "60");
    let read = draft.header::<Expires>().expect("carried").expect_err("two lines");
    assert!(read.reason.starts_with("Multiple Expires headers"), "{}", read.reason);
    assert!(draft.clone().update::<Expires>(|e| e).is_err(), "an update reads the one line");
    let refused = |result: Result<(), IncompleteDraft>| {
        matches!(result, Err(IncompleteDraft::Repeated(HeaderName::Expires)))
    };
    assert!(refused(draft.clone().freeze().map(drop)));
    assert!(refused(draft.freeze_bytes().map(drop)));
}

#[test]
fn a_relayed_message_given_a_second_line_never_freezes() {
    let SipMessage::Request(received) = CustomParser::built().parse(&with("")).expect("reads")
    else {
        panic!("a request")
    };
    let draft = received.thaw().push_raw(HeaderName::CallId, "c2");
    assert!(matches!(draft.freeze(), Err(IncompleteDraft::Repeated(HeaderName::CallId))));
}

#[test]
fn set_writes_a_single_valued_header_on_its_one_line() {
    let draft = options().set(Expires::new(60)).set(Expires::new(30));
    assert_eq!(draft.raw(&HeaderName::Expires).len(), 1);
    let frozen = draft.update::<Expires>(|e| Expires::new(e.value() + 1)).expect("reads");
    let request = frozen.freeze().expect("one line each");
    assert_eq!(request.header::<Expires>().expect("carried").expect("reads").value(), 31);
}
