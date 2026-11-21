//! The lines a final the stack mints for the caller's INVITE takes beside its
//! own: what the decision authored for it (status, statements, redirect
//! targets), and the failing peer's headers a final answering the
//! `/call/failure` consult restates.

use call::Call;
use sip_message::generators::{RelayDirection, RelaySituation};
use sip_message::{Method, SipHeader};

use crate::config::B2buaConfig;
use crate::rules::relay;
use b2bua_sdk::model::RuleContext;

/// What the decision authored for the caller's final, as
/// [`b2bua_sdk::model::RuleAction::RespondToALeg`] states it: the status line
/// plus the two lists that own their own names.
pub(super) struct AuthoredFinal<'a> {
    pub status: u16,
    pub reason: &'a str,
    pub header_updates: &'a [(String, Option<String>)],
    pub contacts: &'a [b2bua_sdk::redirect_contact::RedirectContact],
}

/// Errs when a redirect target does not read — the whole redirect is refused,
/// never partially authored.
pub(super) fn build_a_leg_response_headers(
    header_updates: &[(String, Option<String>)],
    contacts: &[b2bua_sdk::redirect_contact::RedirectContact],
) -> Result<Vec<SipHeader>, relay::UnreadableAddress> {
    let mut out = relay::set_lines(header_updates);
    for contact in contacts {
        out.push(relay::redirect_contact(contact)?);
    }
    Ok(out)
}

/// The failing peer's relayable headers, but ONLY on a final that answers the
/// `/call/failure` consult the image belongs to — the `call-failure-result`
/// event — and only when that failure had a peer final: `None` when the final
/// restates no peer's. A setup deadline, a capacity refusal or a media-service failure mints
/// its own diagnosis about a peer that is not the one being answered, so it
/// carries none of them, nor does a fold that answers no peer's final
/// ([`crate::failure_unavailable::answers_the_peer`]). The deployment's relay policy is read for the
/// final being minted: a `status` to the INVITE, toward the caller.
pub(super) fn failure_headers_answering(
    ctx: &RuleContext,
    call: &Call,
    config: &B2buaConfig,
    status: u16,
) -> Option<Vec<SipHeader>> {
    match ctx.event {
        b2bua_sdk::event::CallEvent::InternalEvent { topic, payload, .. }
            if topic == "call-failure-result"
                && crate::failure_unavailable::answers_the_peer(payload) =>
        {
            let minted =
                RelaySituation::response(status, &Method::Invite, RelayDirection::TowardCaller);
            let mut image = relay::relayed_failure_headers(call.ext.as_ref())?;
            image.retain(|h| !config.relay_policy.drops(&h.name, minted));
            Some(image)
        }
        _ => None,
    }
}
