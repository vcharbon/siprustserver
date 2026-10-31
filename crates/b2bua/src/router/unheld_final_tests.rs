//! A non-2xx final to a leg's dialling INVITE another owner sent reaches this
//! node, the call's primary (ADR-0055 §8), on a call it holds: its transaction
//! layer holds no client transaction of the INVITE and sent no ACK, so the
//! turn ACKs it hop by hop from the verified via-extension (RFC 3261
//! §17.1.1.3, ADR-0055 §12). A final the layer matched draws no second ACK.

use b2bua_sdk::event::CallEvent;
use call::{Call, Direction, LegState};
use sip_message::header::HeaderValue;
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;

use super::test_support::{invite, node_with, src, turn};
use crate::config::B2buaConfig;
use crate::effects::OutboundBody;
use crate::initial_invite::build_initial_call;
use crate::rule_chain::{rule_chain_turn, TurnInput};
use crate::stack_identity::{build_call_via, RequestFacts, StackIdentityOpts};

fn key() -> sip_ids::SigningKey {
    sip_ids::SigningKey::new(&[9u8; 32]).expect("a 32-byte key")
}

/// The callee the other owner dialled.
const CALLEE_URI: &str = "sip:carol@192.0.2.3:5070";

/// A call `w0` serves, holding the callee leg `c-1` its backup `w1` dialled,
/// ended by the CANCEL that leg's INVITE drew.
fn call_with_an_ended_dial() -> Call {
    let mut call = build_initial_call(
        &invite("w0", "w1", "unheld-final"),
        src(),
        &B2buaConfig { self_ordinal: "w0".into(), ..Default::default() },
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut leg = call::testkit::dialled_leg("c-1", "w1-tag");
    if let Some(record) = leg.invite.as_mut() {
        record.relayed_by = call::RelayOwner::new("w1", "w1-boot-1");
    }
    call::testkit::stage_leg(&mut leg, LegState::Terminated);
    call::helpers::add_b_leg(&mut call, leg);
    call
}

/// The callee's 487 to the INVITE `w1` sent on `c-1`, its Via `w1`'s, the
/// via-extension signed under the cluster key.
fn refused(call: &Call) -> CallEvent {
    let k = key();
    let facts = RequestFacts { method: "INVITE", uri: CALLEE_URI, offer: true };
    let via = build_call_via(
        &StackIdentityOpts {
            local_ip: "127.0.0.2",
            local_port: 5092,
            call_ref: &call.call_ref,
            leg: "c-1",
            incarnation_mark: call.incarnation_mark(),
            is_emergency: false,
            backup: Some("w1"),
            primary: Some("w0"),
            request: Some((facts, &k)),
        },
        "z9hG4bKdialled".to_string(),
    );
    let raw = format!(
        "SIP/2.0 487 Request Terminated\r\nVia: {}\r\nFrom: <sip:svc@127.0.0.2>;tag=w1-tag\r\n\
         To: <sip:carol@192.0.2.3>;tag=caroltag\r\nCall-ID: c-1-call-id\r\nCSeq: 1 INVITE\r\n\
         Content-Length: 0\r\n\r\n",
        via.to_wire()
    );
    CallEvent::sip(CustomParser::new().parse(raw.as_bytes()).expect("a response"), src())
}

/// The ACKs the turn on `event` sends, `matched_client_txn` as the layer read
/// it, each with its Request-URI.
async fn acks_sent(matched_client_txn: bool) -> Vec<String> {
    let node = node_with("w0", |c| c.cluster_key = Some(key())).await;
    let call = call_with_an_ended_dial();
    let event = refused(&call);
    let call_ref = call.call_ref.clone();
    let input = TurnInput {
        event: &event,
        call_ref: &call_ref,
        source_leg_id: "c-1",
        direction: Direction::FromB,
        resolved: Default::default(),
        matched_client_txn,
        now_ms: 1_000,
        owners: Default::default(),
    };
    let fw = node.core.router_ctx().framework();
    let result = rule_chain_turn(&fw, call, input, &mut turn(1));
    result
        .effects
        .outbound
        .iter()
        .filter_map(|e| match &e.body {
            OutboundBody::Request(r) if r.method().as_str() == "ACK" => {
                Some(r.request_uri().text().into_owned())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_unmatched_non_2xx_to_another_owners_dial_is_acked_from_its_extension() {
    assert_eq!(acks_sent(false).await, vec![CALLEE_URI.to_string()]);
}

#[tokio::test(start_paused = true)]
async fn a_non_2xx_the_layer_matched_draws_no_second_ack() {
    assert_eq!(acks_sent(true).await, Vec::<String>::new());
}
