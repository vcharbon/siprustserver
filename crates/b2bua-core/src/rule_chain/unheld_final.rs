//! A non-2xx final to an INVITE that answered none of this node's client
//! transactions: its transaction layer sent no hop-by-hop ACK (RFC 3261
//! §17.1.1.3), and the INVITE is one another owner sent, a leg's dialling
//! INVITE whose responses reach this node as the call's primary (ADR-0055
//! §8). When the rules sent no ACK for it, it is ACKed as a node that holds
//! no client transaction of it answers it, from its verified via-extension
//! (ADR-0055 §12, [`crate::unheld_response`]); one whose extension does not
//! verify draws nothing.

use sip_ids::TurnIds;
use sip_message::{Method, SipMessage, SipResponse};

use crate::config::B2buaConfig;
use crate::effects::{HandlerEffects, OutboundBody};
use b2bua_sdk::event::CallEvent;

/// Push onto `effects` the ACK `event` draws: a non-2xx final to an INVITE,
/// `matched_client_txn` false, that no ACK of the turn answers already.
pub(super) fn ack_unheld_final(
    config: &B2buaConfig,
    event: &CallEvent,
    matched_client_txn: bool,
    effects: &mut HandlerEffects,
    ids: &mut TurnIds,
    now_ms: i64,
) {
    let CallEvent::Sip { message, src, .. } = event else { return };
    let SipMessage::Response(resp) = message.as_ref() else { return };
    if matched_client_txn || resp.status() < 300 || resp.cseq().method() != Method::Invite {
        return;
    }
    if effects.outbound.iter().any(|e| acks(&e.body, resp)) {
        return;
    }
    let from = (src.ip().to_string(), src.port());
    effects
        .outbound
        .extend(crate::unheld_response::answer_unheld_response(config, resp, from, ids, now_ms));
}

/// Whether `body` is an ACK of `resp`'s transaction: its Call-ID and CSeq.
fn acks(body: &OutboundBody, resp: &SipResponse) -> bool {
    let OutboundBody::Request(req) = body else { return false };
    *req.method() == Method::Ack
        && req.call_id().as_str() == resp.call_id().as_str()
        && req.cseq().seq() == resp.cseq().seq()
}
