use super::*;
use call::{Direction, LegState};
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;

use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::test_call as test_support;

const BRANCH: &str = "z9hG4bK-info-1";
const SENT_BY: &str = "10.0.0.5:5080";

/// The caller's in-dialog request of `method`, CSeq `cseq`, top Via `sent_by`
/// on `branch`.
fn request(method: &str, cseq: u32, sent_by: &str, branch: &str) -> CallEvent {
    request_on("acid@alice", method, cseq, sent_by, branch)
}

/// [`request`] on Call-ID `call_id`.
fn request_on(call_id: &str, method: &str, cseq: u32, sent_by: &str, branch: &str) -> CallEvent {
    let raw = format!(
        "{method} sip:svc@10.0.0.9 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {sent_by};branch={branch}\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-alice-1\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@10.0.0.1>;tag=atag\r\n\
         To: <sip:bob@10.0.0.9>;tag=svca\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} {method}\r\n\
         Content-Length: 0\r\n\r\n"
    );
    let message = CustomParser::new().parse(raw.as_bytes()).expect("a request");
    CallEvent::Sip {
        message: Box::new(message),
        src: test_support::src(),
        matched_client_txn: false,
    }
}

/// What the b-leg's confirmed dialog holds of the caller's `method` request
/// (CSeq 5, on `BRANCH` from `SENT_BY`), relayed under outbound CSeq
/// `outbound_cseq`.
fn relay(method: &str, outbound_cseq: i64) -> PendingRequest {
    PendingRequest {
        method: method.into(),
        outbound_cseq,
        inbound_cseq: 5,
        source_vias: vec![
            format!("SIP/2.0/UDP {SENT_BY};branch={BRANCH}"),
            "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-alice-1".into(),
        ],
        source_call_id: "acid@alice".into(),
        source_from: "<sip:alice@10.0.0.1>;tag=atag".into(),
        source_to: "<sip:bob@10.0.0.9>;tag=svca".into(),
        direction: Direction::FromA,
        cancelled: false,
        offered_100rel: false,
        offered: false,
        source_timestamp: None,
        relayed_by: call::RelayOwner::new("w1", "w1-boot-1"),
    }
}

/// An established call whose b-leg dialog holds `relays`.
fn call_holding(relays: Vec<PendingRequest>) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let invite = test_support::invite("w0", "w1", "acid@alice");
    let call = build_initial_call(
        &invite,
        test_support::src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut leg = call::testkit::dialled_leg("b-1", "svc");
    call::testkit::stage_leg(&mut leg, LegState::Confirmed);
    leg.dialogs.confirmed_mut().expect("confirmed").inbound_pending_requests = relays;
    call::helpers::add_b_leg(call, leg)
}

fn relays_left(call: &Call) -> Vec<(String, i64)> {
    call.b_legs[0]
        .dialogs
        .confirmed()
        .expect("confirmed")
        .inbound_pending_requests
        .iter()
        .map(|p| (p.method.clone(), p.outbound_cseq))
        .collect()
}

/// A copy of the request a relay snapshots drops that relay, and only it.
#[test]
fn a_copy_of_a_relayed_request_drops_its_relay() {
    let call = call_holding(vec![relay("INFO", 3), relay("UPDATE", 4)]);
    let call = drop_superseded_relay(call, &request("INFO", 5, SENT_BY, BRANCH));
    assert_eq!(relays_left(&call), [("UPDATE".to_string(), 4)]);
}

/// A request differing from the snapshot in method, CSeq, branch, sent-by or
/// Call-ID is another request, and an INVITE is never superseded here: the
/// relay stays.
#[test]
fn another_request_leaves_the_relay() {
    let held = vec![relay("INFO", 3), relay("INVITE", 4)];
    for event in [
        request("MESSAGE", 5, SENT_BY, BRANCH),
        request("INFO", 6, SENT_BY, BRANCH),
        request("INFO", 5, SENT_BY, "z9hG4bK-info-2"),
        request("INFO", 5, "10.0.0.6:5080", BRANCH),
        request_on("other@alice", "INFO", 5, SENT_BY, BRANCH),
        request("INVITE", 5, SENT_BY, BRANCH),
    ] {
        let call = drop_superseded_relay(call_holding(held.clone()), &event);
        assert_eq!(relays_left(&call).len(), 2, "{event:?}");
    }
}
