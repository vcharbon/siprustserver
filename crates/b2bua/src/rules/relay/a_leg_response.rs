//! The UAS response the B2BUA mints on the a-leg's inbound INVITE (toward the
//! originator), on that INVITE's own server transaction.

use sip_message::generators::{self, GenerateResponseOpts};
use sip_message::header::{self, MediaType};
use sip_message::{SipHeader as MsgHeader, SipRequest};

use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode};

/// Whether a response of this status carries the B2BUA's own `Contact`
/// (RFC 3261 Table 3): a 1xx keeps the early dialog reachable for in-dialog
/// requests, a 2xx to INVITE MUST carry one, a 3xx and a 485 name where to
/// retry. Every other final ends the transaction and names no reachable
/// dialog, so it carries none.
pub fn stamps_contact(status: u16) -> bool {
    matches!(status, 100..=399 | 485)
}

/// Build a UAS response on a leg's inbound INVITE (toward alice). `to_tag` pins
/// the stable a-facing dialog tag; `contact` is stamped only on the statuses
/// [`stamps_contact`] names.
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
        contact: contact.filter(|_| stamps_contact(status)),
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
