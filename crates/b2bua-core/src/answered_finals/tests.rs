use super::*;
use crate::effects::{HandlerEffects, OutboundSipEffect, Provenance};
use crate::test_call::{invite, src};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipResponse};

fn response(status: u16, method: &str, branch: &str) -> SipResponse {
    let raw = format!(
        "SIP/2.0 {status} OK\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch={branch}\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>;tag=bobtag\r\n\
         Call-ID: answered@10.0.0.9\r\n\
         CSeq: 2 {method}\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()) {
        Ok(SipMessage::Response(r)) => r,
        _ => unreachable!("a fixed response parses"),
    }
}

fn turn_sending(responses: Vec<SipResponse>) -> HandlerResult {
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    let call =
        crate::initial_invite::build_initial_call(&invite("answered"), src(), &config, &mut ids, 0);
    let mut fx = HandlerEffects::new();
    for resp in responses {
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Response(resp),
            mode: OutboundTxnMode::ServerResponse,
            destination: ("10.0.0.9".into(), 5060),
            label: "final".into(),
            leg_id: Some("a".into()),
            provenance: Provenance::Authored,
        });
    }
    HandlerResult { call, effects: fx }
}

/// A non-INVITE final is kept, keyed by its transaction, until Timer J; a
/// provisional, an INVITE's final and a CANCEL's are not.
#[test]
fn a_non_invite_final_is_kept_until_timer_j() {
    let result = retain(
        turn_sending(vec![
            response(200, "INFO", "z9hG4bK-info"),
            response(100, "REFER", "z9hG4bK-trying"),
            response(200, "INVITE", "z9hG4bK-invite"),
            response(200, "CANCEL", "z9hG4bK-cancel"),
        ]),
        1_000,
    );
    let kept: Vec<_> =
        result.call.answered.iter().map(|a| (a.method.as_str(), a.branch.as_str())).collect();
    assert_eq!(kept, [("INFO", "z9hG4bK-info")]);
    assert_eq!(result.call.answered[0].until_ms, 1_000 + AnsweredRequest::TIMER_J_MS);

    let later = HandlerResult { call: result.call, effects: HandlerEffects::new() };
    let later = retain(later, 1_000 + AnsweredRequest::TIMER_J_MS);
    assert!(later.call.answered.is_empty(), "Timer J drops it");
}

/// A turn that ends the call keeps nothing.
#[test]
fn a_terminal_turn_keeps_no_final() {
    let mut turn = turn_sending(vec![response(200, "BYE", "z9hG4bK-bye")]);
    turn.call.state = CallModelState::Terminated;
    assert!(retain(turn, 0).call.answered.is_empty());
}
