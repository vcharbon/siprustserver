//! The final a decision authors for the caller's INVITE (reject, redirect,
//! failure treatment): the headers it states, the failing peer's it restates,
//! and the deployment's advertisement under them.

use call::helpers::Scope;
use call::Call;
use sip_message::generators;
use sip_message::header::{HeaderName, Minted};
use sip_message::SipHeader;

use crate::effects::{BufferedObservabilityEffect, HandlerEffects, Provenance};
use crate::rules::capabilities;
use crate::rules::relay;
use b2bua_sdk::model::RuleContext;

use super::respond::failure_headers_answering;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Answer the a-leg INVITE with a decision-authored Reject/Redirect final
    /// ([`b2bua_sdk::model::RuleAction::RespondToALeg`]). No B2BUA Contact: a
    /// redirect carries its own Contact list (via the built headers), a reject
    /// carries none (ADR-0017 header-ownership X2,
    /// [`sip_message::generators::response_states_own_contact`]).
    /// When this final answers the `/call/failure` consult, the failing b-leg
    /// final's relayable headers ride UNDER the decision's own statements: a
    /// `header_updates` entry naming a header — set or removal — owns that name
    /// (X2 precedence). The peer's Contact lines never ride: the decision
    /// authors this final and names its own targets, if any. A redirect (3xx)
    /// and a refused redirect are new instructions, not relayed refusals, so
    /// they carry none of the peer's headers. A failure final that restates
    /// no peer's carries the deployment's advertisement for a minted final,
    /// under the decision's statements. A statement of a header the final
    /// does not take from the decision is left out and reported
    /// ([`relay::admitted`]), so the peer's line of it rides as relayed.
    pub(super) fn respond_to_a_leg(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        authored: AuthoredFinal<'_>,
    ) {
        let AuthoredFinal { status, reason, header_updates, contacts } = authored;
        let mut skipped = Vec::new();
        let header_updates = &relay::admitted(header_updates, Minted::Final(status), &mut skipped);
        fx.buffered.extend(BufferedObservabilityEffect::skipped("final to the caller", skipped));
        // A decision-authored final ends the setup: every §3 ladder stops with
        // it (RFC 3261 §17.2.1 — no 1xx after the transaction's final).
        self.retire(call, fx, Scope::Provisionals);
        let a_tag = self.ensure_a_dialog(call);
        let Some(a_invite) = self.caller_invite(call) else { return };
        // A redirect target that does not read is refused, not invented: the
        // caller dials what a 3xx Contact names. The caller gets the plain
        // server error, with the decision's headers and no Contact list.
        let (status, reason, mut extra, refused) =
            match build_a_leg_response_headers(header_updates, contacts) {
                Ok(headers) => (status, reason.to_string(), headers, false),
                Err(err) => {
                    tracing::warn!(
                        call_ref = %call.call_ref,
                        detail = %err.detail(),
                        "redirect refused"
                    );
                    (500, err.to_string(), relay::set_lines(header_updates), true)
                }
            };
        // A 3xx is a new instruction, not a relayed refusal: RFC 3261 §20.33
        // gives `Retry-After` a per-status meaning (on a 3xx it declares the
        // redirect Contact's validity, not when the refusing callee frees up),
        // so a plan-authored redirect carries none of the peer's image — like
        // the refused-redirect 500, it speaks only for itself.
        // The peer's Contact lines never ride: the decision authors this final
        // and states its own targets, if any (RFC 3261 §21.4.22 on a 485). Nor
        // do the refusal's clock stamps: the final is the stack's own message,
        // sent after the consult (RFC 3261 §20.17 / §8.2.6.1).
        let image = (!refused && !(300..400).contains(&status))
            .then(|| failure_headers_answering(ctx, call, self.config, status))
            .flatten();
        if image.is_none() && capabilities::is_minted_failure(status) {
            capabilities::stamp_own_advertisement(
                &self.config.minted_final_advertisement,
                |name| header_updates.iter().any(|(n, _)| name.matches(n)),
                &mut extra,
            );
        }
        if let Some(image) = image {
            for h in image {
                let name = HeaderName::from(h.name.as_str());
                if !HeaderName::Contact.matches(&h.name)
                    && !header_updates.iter().any(|(n, _)| name.matches(n))
                    && !generators::states_send_time(&h.name)
                {
                    extra.push(h);
                }
            }
        }
        if let Some(effect) = relay::response_to_a_leg(
            call,
            fx,
            &a_invite,
            status,
            &reason,
            Some(a_tag),
            None,
            vec![],
            None,
            extra,
            Provenance::Authored,
            relay::Author::Stack,
        ) {
            fx.outbound.push(effect);
        }
    }
}

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
fn build_a_leg_response_headers(
    header_updates: &[(String, Option<String>)],
    contacts: &[b2bua_sdk::redirect_contact::RedirectContact],
) -> Result<Vec<SipHeader>, relay::UnreadableAddress> {
    let mut out = relay::set_lines(header_updates);
    for contact in contacts {
        out.push(relay::redirect_contact(contact)?);
    }
    Ok(out)
}
