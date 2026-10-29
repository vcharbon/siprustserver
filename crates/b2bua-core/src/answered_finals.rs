//! The finals a turn sends to received non-INVITE requests, kept on the
//! record while their server transactions are Completed (RFC 3261 §17.2.2):
//! a node serving the call from the record answers a copy of the request with
//! the final it holds, which the record may hold before the wire saw it.
//! CANCEL and ACK are never answered here: the transaction layer answers a
//! CANCEL itself, and an ACK draws no response.

use call::{AnsweredRequest, CallModelState, Repeated, RetainedEmission};
use sip_message::Method;

use crate::effects::{HandlerResult, OutboundBody, OutboundTxnMode};

/// `result` with every non-INVITE final it sends kept on its record, the
/// finals whose Timer J passed at `now_ms` dropped. A final on the same
/// transaction as one kept replaces it. A turn that ends the call keeps none.
pub fn retain(mut result: HandlerResult, now_ms: i64) -> HandlerResult {
    let call = &mut result.call;
    call.answered.retain(|a| a.completed_at(now_ms));
    if call.state == CallModelState::Terminated {
        call.answered.clear();
        return result;
    }
    for eff in &result.effects.outbound {
        let (OutboundTxnMode::ServerResponse, OutboundBody::Response(resp)) =
            (&eff.mode, &eff.body)
        else {
            continue;
        };
        let method = resp.cseq().method();
        if resp.status() < 200 || matches!(method, Method::Invite | Method::Cancel | Method::Ack) {
            continue;
        }
        let Some(branch) = resp.top_via().branch().filter(|b| !b.is_empty()) else { continue };
        let retained = RetainedEmission::on_trigger(
            resp.image().to_vec(),
            eff.destination.clone(),
            Repeated::response(method.as_str(), resp.status()),
        );
        call.answered.retain(|a| !(a.branch == branch && a.method == method.as_str()));
        call.answered.push(AnsweredRequest::new(method.as_str(), branch, retained, now_ms));
    }
    result
}

#[cfg(test)]
mod tests;
