//! Pins the three ways the front proxy's Record-Route reads for the
//! stickiness cookie, and that only one of them is quiet.
//!
//! A recorded route no reader accepts must NOT pass for "no proxy recorded
//! one": both end at non-replicating placement (`PutOpts::default()`), but
//! the first is a call that silently stops surviving a takeover, so it is
//! named on stderr. Pure builder, no clock.

use super::build_initial_call;
use super::topology_cookie::{read_topology_cookie, CookieRead};
use crate::config::B2buaConfig;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};
use std::net::SocketAddr;

/// A port no reader accepts (RFC 3261 §19.1.1 `port` is 16-bit): the message
/// parses, the recorded route does not.
const UNREADABLE_RR: &str = "<sip:10.0.0.9:70596;v=3;w_pri=w0;w_bak=w1;lr>";

/// The proxy's double-record-route as it arrives on an inbound INVITE: the
/// direction-only `;outbound` half topmost, the cookie half below it.
const COOKIE_RR: &str =
    "<sip:10.0.0.9:5060;outbound;lr>,<sip:10.0.0.9:5060;v=3;w_pri=w7;w_bak=w9;lr>";

fn invite_with_record_route(rr: Option<&str>) -> SipRequest {
    let rr_line = match rr {
        Some(v) => format!("Record-Route: {v}\r\n"),
        None => String::new(),
    };
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-cookie-iih\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: cookie-iih@10.0.0.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.9:5060>\r\n\
             {rr_line}\
             Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("fixture INVITE should parse") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

fn call_for(invite: &SipRequest) -> call::Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    build_initial_call(
        invite,
        SocketAddr::from(([10, 0, 0, 9], 5060)),
        &config,
        &sip_txn::IdGen::seeded(1),
        0,
    )
}

#[test]
fn a_cookie_record_route_places_the_call_on_the_proxys_choice() {
    let invite = invite_with_record_route(Some(COOKIE_RR));
    assert!(
        matches!(read_topology_cookie(&invite, "w0"), CookieRead::Topology(_)),
        "the cookie half must be found below the `;outbound` half"
    );
    let topology = call_for(&invite).topology.expect("a cookie INVITE gets a topology");
    assert_eq!((topology.pri.as_str(), topology.bak.as_str()), ("w7", "w9"));
    assert_eq!((topology.gen, topology.bak_gen), (1, 0), "a brand-new call is (p=1, b=0)");
}

#[test]
fn no_record_route_and_a_cookieless_one_are_both_ordinary() {
    for rr in [None, Some("<sip:10.0.0.9:5060;outbound;lr>")] {
        let invite = invite_with_record_route(rr);
        assert!(
            matches!(read_topology_cookie(&invite, "w0"), CookieRead::NoCookie),
            "readable, cookie-less recorded routes ({rr:?}) are not a failed read"
        );
        assert!(call_for(&invite).topology.is_none(), "non-proxied INVITE stays local-only");
    }
}

#[test]
fn an_unreadable_record_route_is_not_mistaken_for_an_absent_cookie() {
    // Same placement as a cookie-less INVITE — inventing pri/bak from a
    // header we cannot read would have the backup disagree with the proxy's
    // rendezvous choice — but it takes the warning branch, not the quiet one.
    let invite = invite_with_record_route(Some(UNREADABLE_RR));
    assert!(
        matches!(read_topology_cookie(&invite, "w0"), CookieRead::Unreadable(_)),
        "an out-of-range port must fail the recorded-route read, not the message parse"
    );
    assert!(
        call_for(&invite).topology.is_none(),
        "an unreadable cookie falls back to non-replicating placement"
    );
}

#[test]
fn one_unreadable_recorded_route_hides_a_readable_cookie() {
    // The read is all-or-nothing (`list::<RecordRouteEntry>`), so a bad
    // `;outbound` half takes the cookie half down with it. Pinned because it
    // is the reason the warning names the *route*, not the cookie.
    let invite =
        invite_with_record_route(Some(&format!("<sip:10.0.0.9:70596;outbound;lr>,{COOKIE_RR}")));
    assert!(matches!(read_topology_cookie(&invite, "w0"), CookieRead::Unreadable(_)));
}

#[test]
fn an_empty_w_pri_falls_back_to_this_worker() {
    // `w_pri` present but empty means the proxy named no primary; the call
    // is placed here, and `w_bak` still steers the backup.
    let invite = invite_with_record_route(Some("<sip:10.0.0.9:5060;v=3;w_pri=;w_bak=w9;lr>"));
    let topology = call_for(&invite).topology.expect("the cookie is still a cookie");
    assert_eq!((topology.pri.as_str(), topology.bak.as_str()), ("w0", "w9"));
}
