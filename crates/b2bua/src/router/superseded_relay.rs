//! A pending non-INVITE relay its originator's retransmission supersedes.
//!
//! A non-INVITE request reaching the rules found no server transaction on
//! this node: the node that relayed it, and holds that transaction and the
//! relay's client transaction, is not this one. The relay's final follows its
//! Via to that node, a non-INVITE response is never failed over, and the
//! target does not repeat it on its own (RFC 3261 §17.2.2), so no node can
//! finish the relay. The record drops it, and the retransmission is relayed
//! afresh: the target sees the request twice.

use b2bua_sdk::event::CallEvent;
use call::{Call, PendingRequest};
use sip_message::header::{HeaderValue, Via};
use sip_message::{Method, SipMessage, SipRequest, SipStr};

/// `call` without the pending relay the non-INVITE request `event` carries
/// would retransmit: the relay snapshot of the same method and originator
/// CSeq, received on the same Call-ID with the same top-Via branch and
/// sent-by (RFC 3261 §17.2.3). Any other event, or a request no relay
/// snapshots, leaves `call` as it is.
pub(super) fn drop_superseded_relay(call: Call, event: &CallEvent) -> Call {
    let CallEvent::Sip { message, .. } = event else { return call };
    let SipMessage::Request(req) = message.as_ref() else { return call };
    if matches!(req.method(), Method::Invite | Method::Ack | Method::Cancel) {
        return call;
    }
    let superseded = std::iter::once(&call.a_leg).chain(&call.b_legs).find_map(|leg| {
        leg.dialogs.iter().find_map(|d| {
            d.inbound_pending_requests
                .iter()
                .find(|p| retransmits(req, p))
                .map(|p| (leg.leg_id.clone(), d.id(), p.outbound_cseq))
        })
    });
    match superseded {
        Some((leg_id, dialog, outbound_cseq)) => {
            call::helpers::remove_pending_request(call, &leg_id, &dialog, outbound_cseq)
        }
        None => call,
    }
}

/// Whether `req` is a copy of the request `pending` snapshots.
fn retransmits(req: &SipRequest, pending: &PendingRequest) -> bool {
    if !pending.method.eq_ignore_ascii_case(req.method().as_str())
        || pending.inbound_cseq != i64::from(req.cseq().seq())
        || pending.source_call_id != req.call_id().as_str()
    {
        return false;
    }
    let Some(via) = pending.source_vias.first().and_then(|v| Via::parse(&SipStr::owned(v)).ok())
    else {
        return false;
    };
    let top = req.top_via();
    via.branch().is_some_and(|b| Some(b) == top.branch()) && via.sent_by_ref() == top.sent_by_ref()
}

#[cfg(test)]
mod tests;
