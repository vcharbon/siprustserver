//! The UAS response the B2BUA mints on the a-leg's inbound INVITE (toward the
//! originator), on that INVITE's own server transaction — and the one seam
//! where a second final on that transaction is refused.

use call::Call;
use sip_message::generators::{self, response_states_contact, GenerateResponseOpts};
use sip_message::header::{self, MediaType};
use sip_message::{Method, SipHeader as MsgHeader, SipRequest};

use crate::effects::{
    BufferedObservabilityEffect, HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode,
    Provenance,
};

/// Build a UAS response on the a-leg's inbound INVITE (toward alice). `to_tag`
/// pins the stable a-facing dialog tag; `contact` is stamped only where
/// [`response_states_contact`] states it for an INVITE response.
///
/// A final (≥ 200) is admitted once per transaction (RFC 3261 §17.2.1): the
/// first records itself as [`call::Leg::invite_final_sent`]; any later one is
/// refused — `None`, nothing built — and reported as
/// [`BufferedObservabilityEffect::SecondFinalRefused`]. A provisional passes.
#[allow(clippy::too_many_arguments)]
pub fn response_to_a_leg(
    call: &mut Call,
    fx: &mut HandlerEffects,
    a_leg_invite: &SipRequest,
    status: u16,
    reason: &str,
    to_tag: Option<String>,
    contact: Option<header::Contact>,
    body: Vec<u8>,
    content_type: Option<MediaType>,
    incoming_source: Option<(String, u16)>,
    extra_headers: Vec<MsgHeader>,
) -> Option<OutboundSipEffect> {
    if status >= 200 {
        if let Some(carried) = call.a_leg.invite_final_sent {
            tracing::warn!(
                call_ref = %call.call_ref,
                status,
                carried,
                "second final to the a-leg INVITE refused"
            );
            fx.buffered.push(BufferedObservabilityEffect::SecondFinalRefused { status, carried });
            return None;
        }
        call.a_leg.invite_final_sent = Some(status);
    }
    let opts = GenerateResponseOpts {
        to_tag,
        contact: contact.filter(|_| response_states_contact(&Method::Invite, status)),
        body,
        content_type,
        extra_headers,
        incoming_source,
    };
    let resp = generators::generate_response(a_leg_invite, status, reason, &opts);
    // Routed by the txn layer to the a-leg server transaction; dest is alice
    // (top Via sent-by of her INVITE, RFC 3261 §18.2.2).
    let hop = a_leg_invite.top_via();
    let (host, port) = hop.sent_by().pair();
    let dest = (host.to_string(), port);
    Some(OutboundSipEffect {
        body: OutboundBody::Response(resp),
        mode: OutboundTxnMode::ServerResponse,
        destination: dest,
        label: format!("{status} → a-leg"),
        leg_id: Some("a".to_string()),
        // The stack's own unless the caller marks it a relayed one.
        provenance: Provenance::Authored,
    })
}
