//! Real msgpack call bodies for the replication tests whose code under test
//! reads a stored body's lifecycle state (the `Delete` guard, ADR-0031 D3):
//! a call `pri` owns and `bak` backs up, ringing or answered, live or ending.

use std::net::SocketAddr;

use call::{Call, CallModelState, LegState, MsgpackCodec};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;

fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

/// A proxied INVITE keyed by `cid`, carrying the stickiness cookie so the call
/// it builds is replicable (`topology = {pri, bak}`).
fn invite(pri: &str, bak: &str, cid: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{cid}\r\n\
         Record-Route: <sip:10.0.0.1:5060;v=3;w_pri={pri};w_bak={bak};e=0;kid=k1;sig=abc;lr>\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {cid}@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}

/// A call `pri` owns and `bak` backs up, in `state` with its a-leg in
/// `a_state`. `a_state == Confirmed` is the caller-answered body an acting
/// backup writes; `Early` the ringing one its primary still holds, whose own
/// teardown ends in the ring deadline's `480`. Whether the caller was answered
/// is a DURABLE fact of the body — the final the a-leg's initial INVITE server
/// transaction took — so it is written here beside the leg state, as the live
/// path writes it.
pub(super) fn call_in(
    pri: &str,
    bak: &str,
    cid: &str,
    a_state: LegState,
    state: CallModelState,
) -> Call {
    let config = B2buaConfig { self_ordinal: pri.into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    let mut call = build_initial_call(&invite(pri, bak, cid), src(), &config, &mut ids, 0);
    let answered = a_state == LegState::Confirmed;
    *call.a_leg.invite_final_sent_mut() = match (answered, state) {
        (true, _) => Some(200),
        (false, CallModelState::Active) => None,
        (false, _) => Some(480),
    };
    call::testkit::stage_leg(&mut call.a_leg, a_state);
    call.state = state;
    call
}

/// The msgpack body of `call` in its placed envelope — what a flush puts on the wire.
pub(super) fn body(call: &Call) -> Vec<u8> {
    crate::store::envelope::placed_body(call).unwrap()
}

/// The body of `call` written at `(p, b)` by a write that changed a compared
/// field: the topology a flush embeds states that version and that stamp.
pub(super) fn body_at(call: &Call, p: i64, b: i64) -> Vec<u8> {
    let mut topology = crate::router::topology_cookie::placement(call);
    if let Some(t) = topology.as_mut() {
        (t.gen, t.bak_gen) = (p, b);
        t.stamp_vector();
    }
    crate::store::envelope::encode(&MsgpackCodec::new(), topology.as_ref(), call).unwrap()
}
