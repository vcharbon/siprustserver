//! The departure log's fact: the UA sends copies of its INFO (CSeq 1) on an
//! early dialog and of its own 487 to the peer's INVITE numbered 1; a copy
//! its loss model dropped never departs, one it held departs when the hold
//! ends.

use sip_message::parser::custom::CustomParser;
use sip_message::{sniff, SipMessage, SipParser, SipResponse};

use super::Departures;
use crate::dialogs::{DialogEnd, Ending};

const UA_INFO: &str =
    "INFO sip:s@192.0.2.1 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u1\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\nCSeq: 1 INFO\r\n\
     Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
const UAS_487: &str = "SIP/2.0 487 Request Terminated\r\n\
     Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-s1\r\n\
     From: <sip:s@host>;tag=S\r\nTo: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\n\
     CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
const PEERS_200: &str = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u1\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\nCSeq: 1 INFO\r\n\
     Content-Length: 0\r\n\r\n";

const OWN_487: DialogEnd =
    DialogEnd { ending: Ending::Final { invite_cseq: 1, status: 487 }, by_peer: false };

fn peers_200() -> SipResponse {
    match CustomParser::new().parse(PEERS_200.as_bytes()).expect("a well-formed 200") {
        SipMessage::Response(r) => r,
        SipMessage::Request(_) => unreachable!(),
    }
}

/// The fact after the departures `left`, in that order.
fn fact_after(left: &[&str], end: DialogEnd) -> Option<bool> {
    let log = Departures::default();
    left.iter().for_each(|raw| log.note(&sniff::identity(raw.as_bytes())));
    log.ahead(&peers_200(), end)
}

/// An INFO copy that left before the 487's first departure reached the peer
/// ahead of it, whether that copy is the first or a re-send overtaking a held
/// 487; a later re-send changes nothing.
#[test]
fn a_copy_that_left_before_the_final_reached_ahead() {
    assert_eq!(fact_after(&[UA_INFO, UAS_487], OWN_487), Some(true));
    assert_eq!(fact_after(&[UA_INFO, UAS_487, UA_INFO], OWN_487), Some(true));
}

/// The INFO's first copy dropped, its re-send left after the 487: it reached
/// the peer behind that final. With no copy departed, or on the peer's own
/// final, the log cannot tell.
#[test]
fn a_request_whose_first_departure_followed_the_final_did_not_reach_ahead() {
    assert_eq!(fact_after(&[UAS_487, UA_INFO], OWN_487), Some(false));
    assert_eq!(fact_after(&[UAS_487], OWN_487), None);
    let peers = DialogEnd { by_peer: true, ..OWN_487 };
    assert_eq!(fact_after(&[UA_INFO, UAS_487], peers), None);
}
