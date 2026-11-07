//! A non-2xx final to a leg's dialling INVITE another owner sent reaches this
//! node, the call's primary (ADR-0055 §8), on a call it holds: its transaction
//! layer holds no client transaction of the INVITE and sent no ACK, so the
//! turn ACKs it hop by hop from the verified via-extension (RFC 3261
//! §17.1.1.3, ADR-0055 §12). A final the layer matched, or one whose Via names
//! this node, draws no ACK of the turn's.

use b2bua_sdk::event::CallEvent;
use call::{Call, Direction, LegState};
use sip_message::header::HeaderValue;
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;

use crate::config::B2buaConfig;
use crate::effects::OutboundBody;
use crate::initial_invite::build_initial_call;
use crate::obligations::ObligationSet;
use crate::rule_chain::{rule_chain_turn, Framework, TurnInput};
use crate::rules::default_rules;
use crate::stack_identity::{build_call_via, RequestFacts, StackIdentityOpts};
use crate::test_call::{invite, src, turn};

fn key() -> sip_ids::SigningKey {
    sip_ids::SigningKey::new(&[9u8; 32]).expect("a 32-byte key")
}

fn config() -> B2buaConfig {
    B2buaConfig {
        self_ordinal: "w0".into(),
        sip_local_ip: "127.0.0.1".into(),
        sip_local_port: 5091,
        cluster_key: Some(key()),
        ..B2buaConfig::default()
    }
}

/// The callee the other owner dialled.
const CALLEE_URI: &str = "sip:carol@192.0.2.3:5070";

/// A call `w0` serves, holding the callee leg `c-1` its backup `w1` dialled,
/// ended by the CANCEL that leg's INVITE drew.
fn call_with_an_ended_dial() -> Call {
    let mut call = build_initial_call(
        &invite("unheld-final"),
        src(),
        &config(),
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

/// The callee's 487 to the INVITE sent on `c-1` from `sender`, the
/// via-extension signed under the cluster key.
fn refused(call: &Call, sender: (&str, u16)) -> CallEvent {
    let k = key();
    let facts = RequestFacts { method: "INVITE", uri: CALLEE_URI, route: "", offer: true };
    let via = build_call_via(
        &StackIdentityOpts {
            local_ip: sender.0,
            local_port: sender.1,
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

/// The Request-URIs of the ACKs the turn on the callee's 487, sent from
/// `sender`, sends, `matched_client_txn` as the layer read it.
fn acks_sent(sender: (&str, u16), matched_client_txn: bool) -> Vec<String> {
    let config = config();
    let rules = default_rules();
    let obligations = ObligationSet::core();
    let fw = Framework { config: &config, rules: &rules, obligations: &obligations };
    let call = call_with_an_ended_dial();
    let event = refused(&call, sender);
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

/// The other owner, `w1`.
const OTHER: (&str, u16) = ("127.0.0.2", 5092);

#[test]
fn an_unmatched_non_2xx_to_another_owners_dial_is_acked_from_its_extension() {
    assert_eq!(acks_sent(OTHER, false), vec![CALLEE_URI.to_string()]);
}

#[test]
fn a_non_2xx_the_layer_matched_draws_no_second_ack() {
    assert_eq!(acks_sent(OTHER, true), Vec::<String>::new());
}

#[test]
fn a_non_2xx_whose_via_names_this_node_draws_no_ack() {
    assert_eq!(acks_sent(("127.0.0.1", 5091), false), Vec::<String>::new());
}
