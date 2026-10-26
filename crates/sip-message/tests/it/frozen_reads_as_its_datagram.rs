//! A frozen message is what a lenient read of its own datagram yields,
//! whatever typed values its draft was built from — including values whose
//! render reads back as something else — and a value the lenient read
//! refuses refuses the freeze.

use sip_message::draft::{RequestDraft, ResponseDraft};
use sip_message::header::{self, CSeq, CallId, Contact, MaxForwards, ParamValue, To, Uri, Via};
use sip_message::{
    CustomParser, Method, SipMessage, SipParser, SipParserLimits, SipRequest, SipResponse,
};

fn reread(image: &[u8]) -> SipMessage {
    let lenient = SipParserLimits { wire_grammar: false, ..SipParserLimits::default() };
    CustomParser::with_limits(lenient).parse(image).expect("its own datagram reads back")
}

fn invite() -> RequestDraft {
    RequestDraft::new(Method::Invite, Uri::sip_user("bob", "example.com"))
        .push(Via::udp("10.0.0.1", 5060).with_branch("z9hG4bK-a").with_received("192.0.2.1"))
        .push(Via::tcp("2001:db8::1", 5070).with_branch("z9hG4bK-b").requesting_rport())
        .push(MaxForwards::new(70))
        .push(
            header::From::from_uri(Uri::sip_user("alice", "example.com").with_port(5062))
                .with_display("Alice \"A\" Smith")
                .with_tag("1928")
                .with_param("x", ParamValue::text("y z")),
        )
        .push(To::from_uri(Uri::sip_user("bob", "example.com")).with_display("Bob"))
        .push(CallId::new("a84b4c76e66710@pc33.example.com"))
        .push(CSeq::new(314159, Method::Invite))
        .push(Contact::from_uri(
            Uri::sip_user("b2bua", "10.0.0.1")
                .with_port(5060)
                .with_param("callRef", ParamValue::text("c1")),
        ))
}

fn same_core_request(frozen: &SipRequest) {
    let SipMessage::Request(read) = reread(frozen.image()) else { panic!("a request") };
    assert_eq!(frozen.from(), read.from());
    assert_eq!(frozen.to(), read.to());
    assert_eq!(frozen.cseq(), read.cseq());
    assert_eq!(frozen.via(), read.via());
    assert_eq!(frozen.contacts(), read.contacts());
}

fn same_core_response(frozen: &SipResponse) {
    let SipMessage::Response(read) = reread(frozen.image()) else { panic!("a response") };
    assert_eq!(frozen.from(), read.from());
    assert_eq!(frozen.to(), read.to());
    assert_eq!(frozen.cseq(), read.cseq());
    assert_eq!(frozen.via(), read.via());
    assert_eq!(frozen.contacts(), read.contacts());
}

#[test]
fn a_frozen_request_holds_what_its_datagram_reads_as() {
    same_core_request(&invite().freeze().expect("it freezes"));
}

#[test]
fn a_frozen_response_holds_what_its_datagram_reads_as() {
    let response = ResponseDraft::new(200, "OK")
        .push(Via::udp("10.0.0.1", 5060).with_branch("z9hG4bK-a"))
        .push(header::From::from_uri(Uri::sip_user("alice", "example.com")).with_tag("1928"))
        .push(To::from_uri(Uri::sip_user("bob", "example.com")).with_tag("as83kf"))
        .push(CallId::new("a84b4c76e66710@pc33.example.com"))
        .push(CSeq::new(314159, Method::Invite))
        .push(Contact::from_uri(Uri::sip_user("bob", "192.0.2.4")))
        .freeze()
        .expect("it freezes");
    same_core_response(&response);
}

#[test]
fn a_thawed_request_with_typed_edits_holds_what_its_datagram_reads_as() {
    let request = invite().freeze().expect("it freezes");
    let edited = request
        .thaw()
        .update_top::<Via>(|via| via.stamped_from("192.0.2.10", 33000))
        .expect("the top Via reads")
        .push_front(Via::udp("proxy.example.com", 5060).with_branch("z9hG4bK-p"))
        .freeze()
        .expect("it freezes");
    same_core_request(&edited);
}

/// An OPTIONS whose mandatory set is complete apart from the values given.
fn options(via: Via, from: header::From, to: To) -> RequestDraft {
    RequestDraft::new(Method::Options, Uri::sip("example.com"))
        .push(via)
        .push(MaxForwards::new(70))
        .push(from)
        .push(to)
        .push(CallId::new("c1"))
        .push(CSeq::new(1, Method::Options))
}

fn plain_from() -> header::From {
    header::From::from_uri(Uri::sip("example.com")).with_tag("1")
}

fn plain_via() -> Via {
    Via::udp("10.0.0.1", 5060).with_branch("z9hG4bK-1")
}

#[test]
fn a_via_host_holding_a_comma_freezes_as_the_hops_its_render_reads_as() {
    let draft = options(
        Via::udp("h1, SIP/2.0/UDP h2", 5060).with_branch("z9hG4bK-1"),
        plain_from(),
        To::from_uri(Uri::sip("example.com")),
    );
    let frozen = draft.freeze().expect("it freezes");
    assert_eq!(frozen.via().len(), 2, "the render carries two hops");
    same_core_request(&frozen);
}

#[test]
fn a_user_holding_an_angle_bracket_freezes_as_its_render_reads() {
    let draft =
        options(plain_via(), plain_from(), To::from_uri(Uri::sip_user("x>y", "example.com")));
    same_core_request(&draft.freeze().expect("it freezes"));
}

#[test]
fn a_user_holding_a_semicolon_and_a_space_freezes_as_its_render_reads() {
    let draft = options(
        plain_via(),
        header::From::from_uri(Uri::sip_user("a;b=c d", "example.com")).with_tag("1"),
        To::from_uri(Uri::sip("example.com")),
    );
    same_core_request(&draft.freeze().expect("it freezes"));
}

#[test]
fn a_value_the_lenient_read_refuses_refuses_the_freeze() {
    let empty_branch = RequestDraft::new(Method::Options, Uri::sip("example.com"))
        .push(Via::udp("10.0.0.1", 5060).with_branch(""))
        .push(MaxForwards::new(70))
        .push(header::From::from_uri(Uri::sip("example.com")).with_tag("1"))
        .push(To::from_uri(Uri::sip("example.com")))
        .push(CallId::new("c1"))
        .push(CSeq::new(1, Method::Options));
    assert!(empty_branch.freeze().is_err(), "an empty branch is refused");

    let empty_tag = RequestDraft::new(Method::Options, Uri::sip("example.com"))
        .push(Via::udp("10.0.0.1", 5060).with_branch("z9hG4bK-1"))
        .push(MaxForwards::new(70))
        .push(header::From::from_uri(Uri::sip("example.com")).with_tag(""))
        .push(To::from_uri(Uri::sip("example.com")))
        .push(CallId::new("c1"))
        .push(CSeq::new(1, Method::Options));
    assert!(empty_tag.freeze().is_err(), "an empty tag is refused");
}
