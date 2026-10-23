//! The UAS response the B2BUA mints on the a-leg's inbound INVITE (toward the
//! originator), on that INVITE's own server transaction — and the one seam
//! where a second final, or a provisional after the final, on that
//! transaction is refused.

use call::Call;
use sip_message::generators::{self, response_states_own_contact, GenerateResponseOpts};
use sip_message::header::{self, MediaType};
use sip_message::{Method, SipHeader as MsgHeader, SipRequest};

use crate::effects::{
    BufferedObservabilityEffect, HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode,
    Provenance,
};

/// Build a UAS response on the a-leg's inbound INVITE (toward alice). `to_tag`
/// pins the stable a-facing dialog tag; `contact` is stamped only where
/// [`response_states_own_contact`] states it for an INVITE response (a 3xx /
/// 485's retry targets ride in `extra_headers`); `provenance`
/// says whose response it is — a callee's forwarded, or this stack's own.
///
/// A provisional carrying `to_tag` marks the a-dialog of that tag shown
/// ([`call::Dialog::shown`]).
///
/// A final (≥ 200) is admitted once per transaction (RFC 3261 §17.2.1): the
/// first records itself as [`call::Leg::invite_final_sent`]; any later one is
/// refused — `None`, nothing built — and reported as
/// [`BufferedObservabilityEffect::SecondFinalRefused`]. A provisional passes
/// only while the transaction carries no final ([`provisional_after_final`]).
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
    provenance: Provenance,
    author: super::sdp_session::Author<'_>,
) -> Option<OutboundSipEffect> {
    // The call as it stands before the response records anything: one that
    // is not built leaves it so.
    let unsent = call.clone();
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
        // A non-2xx final ends every early dialog the INVITE opened (RFC
        // 3261 §12.1).
        if status >= 300 {
            call::helpers::reject_early_dialogs(&mut call.a_leg, status);
        }
    } else if provisional_after_final(call, fx, status) {
        return None;
    } else if let Some(tag) = to_tag.as_deref().filter(|_| status > 100) {
        // The caller now holds the early dialog this provisional names.
        if let Some(d) = call::helpers::caller_dialog_mut(call, tag) {
            d.shown = true;
        }
    }
    let a_leg = call.a_leg.leg_id.clone();
    // The caller-facing dialog the response names (RFC 3261 §12.1.1).
    let dialog = to_tag
        .clone()
        .map(|our_tag| call::DialogId { our_tag, peer_tag: call.a_leg_invite.from_tag.clone() });
    let body = super::sdp_session::continue_on_leg(
        call,
        &a_leg,
        dialog.as_ref(),
        author,
        super::sdp_session::Carried::of(&Method::Invite, Some(status), false),
        body,
        content_type.as_ref(),
        // An opening description (the INVITE's own responses) consults no policy.
        &crate::config::AsWritten,
    );
    // An authored final's stated text the wire cannot carry is left out, so
    // the caller still gets its final; a relayed one carries a peer's parsed
    // lines, which hold no line break.
    let (reason, extra_headers) = match provenance {
        Provenance::Authored => (
            super::stated_text::writable_reason("final to the caller", status, reason, fx),
            super::stated_text::writable_lines("final to the caller", extra_headers, fx),
        ),
        Provenance::Relayed | Provenance::Probe => (reason.to_string(), extra_headers),
    };
    let opts = GenerateResponseOpts {
        to_tag,
        contact: contact.filter(|_| response_states_own_contact(&Method::Invite, status)),
        body,
        content_type,
        extra_headers,
        incoming_source,
    };
    let resp = match generators::generate_response(a_leg_invite, status, &reason, &opts) {
        Ok(resp) => resp,
        Err(e) => {
            *call = unsent;
            super::unbuilt(&call.call_ref, &a_leg, "response", &e);
            return None;
        }
    };
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
        provenance,
    })
}

/// Whether a provisional of `status` toward the a-leg's initial INVITE is
/// refused: that transaction already sent its final, and a completed INVITE
/// server transaction emits no further provisional (RFC 3261 §13.3.1.1 /
/// §17.2.1). A refusal is reported as
/// [`BufferedObservabilityEffect::ProvisionalAfterFinalRefused`]. The
/// executors that would mint one ask here first, so a refused provisional
/// touches no dialog tag and no reliable-provisional ladder on the way.
pub fn provisional_after_final(call: &Call, fx: &mut HandlerEffects, status: u16) -> bool {
    let Some(carried) = call.a_leg.invite_final_sent else { return false };
    tracing::warn!(
        call_ref = %call.call_ref,
        status,
        carried,
        "provisional to the a-leg INVITE after its final refused"
    );
    fx.buffered.push(BufferedObservabilityEffect::ProvisionalAfterFinalRefused { status, carried });
    true
}
