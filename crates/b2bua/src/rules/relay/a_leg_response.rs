//! The UAS response the B2BUA mints on the a-leg's inbound INVITE (toward the
//! originator), on that INVITE's own server transaction.

use sip_message::generators::{self, response_states_contact, GenerateResponseOpts};
use sip_message::header::{self, MediaType};
use sip_message::{Method, SipHeader as MsgHeader, SipRequest};

use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode};

/// Build a UAS response on a leg's inbound INVITE (toward alice). `to_tag` pins
/// the stable a-facing dialog tag; `contact` is stamped only where
/// [`response_states_contact`] states it for an INVITE response.
#[allow(clippy::too_many_arguments)]
pub fn response_to_a_leg(
    a_leg_invite: &SipRequest,
    status: u16,
    reason: &str,
    to_tag: Option<String>,
    contact: Option<header::Contact>,
    body: Vec<u8>,
    content_type: Option<MediaType>,
    incoming_source: Option<(String, u16)>,
    extra_headers: Vec<MsgHeader>,
) -> OutboundSipEffect {
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
    OutboundSipEffect {
        body: OutboundBody::Response(resp),
        mode: OutboundTxnMode::ServerResponse,
        destination: dest,
        label: format!("{status} → a-leg"),
        leg_id: Some("a".to_string()),
    }
}
