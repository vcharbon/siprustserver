//! Response synthesis toward a leg: answering the current request in place
//! (`Respond`), decision-authored a-leg finals (reject / redirect / failure),
//! the a-side fork-confirm (`AnswerALegNewDialog`), brokered early-media
//! provisionals. Relaying a *peer's* response does NOT live here — see
//! [`super::relay_response`]; the ladder an a-facing 2xx arms lives in
//! [`super::ladder`].

use call::helpers::{set_leg_state, Scope};
use call::{Call, LegState, TimerType};
use sip_message::draft::Entry;
use sip_message::generators::{self, GenerateResponseOpts};
use sip_message::header::{HeaderClass, HeaderName};
use sip_message::{SipHeader, SipStr};

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::capabilities::{self, Face};
use crate::rules::model::RuleContext;
use crate::rules::relay;
use crate::rules::RelayedFinal;

use super::select::dialog_identity_tag;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Answer the current request event in place with `status` (no relay) —
    /// the response goes back to the request's top-Via sent-by on the source
    /// leg's server transaction. A locally answered **in-dialog** request still
    /// advances the source dialog's highest-seen CSeq (RFC 3261 §12.2.2), so
    /// the next relayed request's `relay_cseq_delta` does not reproduce the
    /// gap on the peer dialog (§12.2.1.1 — its CSeq increments by exactly one).
    pub(super) fn respond(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        status: u16,
        reason: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) {
        if let Some(req) = ctx.request() {
            if req.to().tag().is_some() {
                if let Some(sd) = ctx.source_dialog() {
                    let inbound_cseq = req.cseq().seq() as i64;
                    if sd.ext.remote_cseq.map_or(true, |c| inbound_cseq > c) {
                        let s_id = dialog_identity_tag(ctx.source_leg_id, sd);
                        *call = call::helpers::update_remote_cseq(
                            call.clone(),
                            ctx.source_leg_id,
                            &s_id,
                            inbound_cseq,
                        );
                    }
                }
            }
            // A request naming no dialog is answered under the leg's own tag
            // (RFC 3261 §8.2.6.2), never a minted one; a tagged To is echoed.
            let to_tag = req
                .to()
                .tag()
                .is_none()
                .then(|| call::helpers::b2bua_tag(call, ctx.source_leg_id))
                .flatten();
            let opts = GenerateResponseOpts {
                to_tag,
                body: body.to_vec(),
                content_type: content_type.and_then(relay::media_type),
                ..Default::default()
            };
            let resp = generators::generate_response(req, status, reason, &opts);
            // RFC 3261 §18.2.2 — a response goes back to the request's top-Via
            // sent-by.
            let hop = req.top_via();
            let (host, port) = hop.sent_by().pair();
            let dest = (host.to_string(), port);
            fx.outbound.push(OutboundSipEffect {
                body: OutboundBody::Response(resp),
                mode: OutboundTxnMode::ServerResponse,
                destination: dest,
                label: format!("{status} (respond)"),
                leg_id: Some(ctx.source_leg_id.to_string()),
                provenance: Provenance::Authored,
            });
        }
    }

    /// Answer the a-leg INVITE with a failure final under the B2BUA's own
    /// a-dialog tag ([`crate::rules::model::RuleAction::RelayFailureToALeg`]);
    /// the Contact rides only where
    /// [`sip_message::generators::response_states_contact`] states it.
    /// A final that answers the `/call/failure` consult restates the failing
    /// b-leg final's relayable headers (RFC 3261 §16.6), so what the refusing
    /// peer stated — its `Warning`, charging correlation, vendor annotations —
    /// reaches the caller; a final answering anything else speaks only for
    /// itself.
    pub(super) fn relay_failure_to_a_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        status: u16,
        reason: &str,
    ) {
        // A failure final ends the setup: every §3 ladder stops with it
        // (RFC 3261 §17.2.1 — no 1xx after the transaction's final).
        self.retire(call, fx, Scope::Provisionals);
        let a_tag = self.ensure_a_dialog(call);
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(
            self.config,
            &call.call_ref,
            &call.a_leg.leg_id,
            call.emergency == Some(true),
        );
        let extra = failure_headers_answering(ctx, call);
        if let Some(effect) = relay::response_to_a_leg(
            call,
            fx,
            &a_invite,
            status,
            reason,
            Some(a_tag),
            Some(contact),
            vec![],
            None,
            None,
            extra,
            Provenance::Relayed,
        ) {
            fx.outbound.push(effect);
        }
    }

    /// Answer the a-leg INVITE with a decision-authored Reject/Redirect final
    /// ([`crate::rules::model::RuleAction::RespondToALeg`]). No B2BUA Contact: a
    /// redirect carries its own Contact list (via the built headers), a reject
    /// carries none (ADR-0017 header-ownership X2,
    /// [`sip_message::generators::response_states_contact`]).
    /// When this final answers the `/call/failure` consult, the failing b-leg
    /// final's relayable headers ride UNDER the decision's own statements: a
    /// `header_updates` entry naming a header — set or removal — owns that name
    /// (X2 precedence). A redirect (3xx) and a refused redirect are new
    /// instructions, not relayed refusals, so they carry none of them.
    pub(super) fn respond_to_a_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        authored: AuthoredFinal<'_>,
    ) {
        let AuthoredFinal { status, reason, header_updates, contacts } = authored;
        // A decision-authored final ends the setup: every §3 ladder stops with
        // it (RFC 3261 §17.2.1 — no 1xx after the transaction's final).
        self.retire(call, fx, Scope::Provisionals);
        let a_tag = self.ensure_a_dialog(call);
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        // A redirect target that does not read is refused, not invented: the
        // caller dials what a 3xx Contact names (055). The caller still gets a
        // final — the plain server error, with no Contact list.
        let (status, reason, mut extra, refused) =
            match build_a_leg_response_headers(header_updates, contacts) {
                Ok(headers) => (status, reason.to_string(), headers, false),
                Err(err) => {
                    tracing::warn!(
                        call_ref = %call.call_ref,
                        detail = %err.detail(),
                        "redirect refused"
                    );
                    (500, err.to_string(), Vec::new(), true)
                }
            };
        // A 3xx is a new instruction, not a relayed refusal: RFC 3261 §20.33
        // gives `Retry-After` a per-status meaning (on a 3xx it declares the
        // redirect Contact's validity, not when the refusing callee frees up),
        // so a plan-authored redirect carries none of the peer's image — like
        // the refused-redirect 500, it speaks only for itself.
        if !refused && !(300..400).contains(&status) {
            for h in failure_headers_answering(ctx, call) {
                let name = HeaderName::from(h.name.as_str());
                if !header_updates.iter().any(|(n, _)| name.matches(n)) {
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
            None,
            extra,
            Provenance::Authored,
        ) {
            fx.outbound.push(effect);
        }
    }

    /// The one seam every a-facing initial-INVITE **2xx** leaves through — the
    /// plain relay, the masked `relayFirst18xTo180` relays, and the
    /// B2BUA-minted answer alike. The final ends every caller-facing
    /// provisional at once (RFC 3261 §17.2.1 — a §3 rung after it would put a
    /// 1xx behind the transaction's final, a losing fork's included) and no
    /// other ladder: a b-leg's re-INVITE 2xx still awaiting its ACK keeps
    /// repeating. Then the answer is retained and its §13.3.1.4 ladder armed
    /// (`retain_a_leg_answer`) — every path that answers the caller owes that,
    /// so no rule arms it.
    pub(super) fn send_a_leg_answer(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        effect: OutboundSipEffect,
    ) {
        self.retire(call, fx, Scope::Provisionals);
        self.retain_a_leg_answer(call, fx, &effect);
        fx.outbound.push(effect);
    }

    /// Broker an unadopted leg's SDP onto the a-leg as an unreliable provisional
    /// (RFC 3262 §3 early media). Only the a-leg has a stored UAS INVITE to
    /// answer; a non-a target or a non-1xx status is skipped. `to_tag` set ⇒ the
    /// caller's early identity, stated by the service; absent ⇒ the B2BUA's own
    /// (reuse/mint).
    ///
    /// Either way the tag the caller is SHOWN becomes the a-dialog's, so the
    /// non-2xx final ending this transaction answers under it (§17.2.1) rather
    /// than under a tag the caller was never in — a b-leg confirm may have minted
    /// one before she saw anything. Only a 2xx supersedes it, under A2
    /// ([`Self::answer_a_leg_new_dialog`]).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_provisional_to_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        status: u16,
        reason: &str,
        body: &[u8],
        content_type: Option<&str>,
        to_tag: Option<&str>,
        p_early_media: Option<&str>,
    ) {
        if !(100..200).contains(&status) || leg_id != call.a_leg.leg_id {
            return;
        }
        // The a-leg INVITE already sent its final: refused at the seam before
        // the caller's early identity is touched (RFC 3261 §17.2.1).
        if relay::provisional_after_final(call, fx, status) {
            return;
        }
        // `to_tag` provided → the service states the caller's early identity: it
        // seeds the a-dialog, and re-stamps one a b-leg confirm minted before any
        // caller-facing response carried it. Absent → the B2BUA's own early
        // identity: reuse the existing a-dialog tag or mint and persist one.
        let to_tag = match to_tag {
            Some(t) => {
                self.ensure_a_dialog_with(call, Some(t.to_string()));
                if let Some(d) = call.a_leg.dialogs.first_mut() {
                    d.sip.local_tag = t.to_string();
                }
                t.to_string()
            }
            None => self.ensure_a_dialog(call),
        };
        // SDP early-media body defaults to application/sdp (mirrors the request path).
        let content_type = content_type
            .and_then(relay::media_type)
            .or_else(|| (!body.is_empty()).then(relay::sdp));
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(
            self.config,
            &call.call_ref,
            &call.a_leg.leg_id,
            call.emergency == Some(true),
        );
        let mut extra_headers = Vec::new();
        if let Some(pem) = p_early_media {
            extra_headers.push(SipHeader {
                name: SipStr::owned(HeaderName::PEarlyMedia.as_wire_str()),
                value: SipStr::owned(pem),
            });
        }
        if let Some(effect) = relay::response_to_a_leg(
            call,
            fx,
            &a_invite,
            status,
            reason,
            Some(to_tag),
            Some(contact),
            body.to_vec(),
            content_type,
            None,
            extra_headers,
            Provenance::Authored,
        ) {
            fx.outbound.push(effect);
        }
    }

    /// A-side fork-confirm ([`crate::rules::model::RuleAction::AnswerALegNewDialog`]):
    /// answer the a-leg INVITE with a final **2xx** under a fresh (or supplied)
    /// To-tag A2 that becomes the confirmed a-dialog, **superseding** an
    /// early-media dialog A1 the caller saw on a prior `18x` (RFC 3261 §12.1
    /// forked-request dialog establishment; the tag change is the RFC 3264 §5.1
    /// one-answer-per-dialog way to deliver MRF-early-media-then-callee-media
    /// when the two SDPs differ).
    ///
    /// Only a `2xx` establishes a dialog — a non-2xx status is a no-op (the
    /// abandoned early dialog / the ADR-0022 `invariants::enforce` unanswered-a-leg
    /// funnel own the failure paths). Steps: mint/adopt A2, OVERWRITE the a-dialog
    /// `local_tag` to A2 (the MRF `ConfirmDialog` / an earlier 18x pinned A1),
    /// relay the final/SDP under A2, and confirm the a-leg. The answer SDP is
    /// kept as the dialog's `cached_sdp` (the relayFirst18x / fake-PRACK
    /// cache, mirroring `confirm_dialog`); the §13.3.1.4 repeat needs none of
    /// it — it re-sends the retained datagram.
    ///
    /// The 2xx carries what the plain relay would (RFC 3261 §16.6): every line of
    /// `relayed` — the callee final this answer delivers — and, for the
    /// `Allow`/`Supported`/`Accept` advert, the face's DECLARED halves over the
    /// callee's relayed ones (RFC 3261 §13.2.1). A `header_updates` entry naming a
    /// header owns it over both: a set value is kept verbatim, a removal keeps it
    /// absent. A [`RelayedFinal::none`] answer is the B2BUA's own and relays
    /// nothing.
    ///
    /// The sip-txn layer only *stores* `uas_to_tag` from the first >100 response
    /// (the 183's A1) and never rewrites a later final's `to.tag`, so the `200`
    /// leaves under A2 verbatim. A CANCEL before the 2xx is answered 200 + 487
    /// under A1 by the layer; after it, 200 under A2 and no 487 (RFC 3261 §9.2).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn answer_a_leg_new_dialog(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        status: u16,
        reason: &str,
        body: &[u8],
        content_type: Option<&str>,
        to_tag: Option<&str>,
        header_updates: &[(String, Option<String>)],
        relayed: &RelayedFinal,
    ) {
        // Only a 2xx establishes the new a-dialog (RFC 3261 §12.1). A non-2xx
        // final does not create a dialog and is not this primitive's job.
        if !(200..300).contains(&status) {
            return;
        }
        // A2: the caller-supplied tag verbatim, else a freshly minted one.
        // Minting via the IdGen guarantees A2 ≠ A1 (distinct from the pinned
        // early-media tag), the RFC 3264 §5.1 requirement.
        let a2 = to_tag.map(str::to_string).unwrap_or_else(|| self.id_gen.new_tag());
        // SDP answer defaults to application/sdp (mirrors the provisional path).
        let content_type = content_type
            .and_then(relay::media_type)
            .or_else(|| (!body.is_empty()).then(relay::sdp));
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(
            self.config,
            &call.call_ref,
            &call.a_leg.leg_id,
            call.emergency == Some(true),
        );
        // §16.6: the delivered final's lines ride as received, except the names
        // the service's `header_updates` state — those are the service's, set
        // or removed, and a relayed line of the same name never competes.
        let mut extra_headers: Vec<SipHeader> = relayed
            .headers()
            .iter()
            .filter(|h| {
                !header_updates.iter().any(|(n, _)| HeaderName::from(n.as_str()).matches(&h.name))
            })
            .cloned()
            .collect();
        // The advert (RFC 3261 §13.2.1): a DECLARED half stands, an undeclared
        // one states what the delivered final advertised, and none where
        // neither does — the ordinary relay's rule. A `header_updates` entry
        // naming a half owns it: a set value is kept verbatim, a removal keeps
        // it absent (`header_update_lines` already dropped it).
        let advert = capabilities::relaying(call, Face::Originator, &extra_headers);
        extra_headers.extend(header_update_lines(header_updates));
        let service_owned: Vec<Entry> =
            [HeaderName::Allow, HeaderName::Supported, HeaderName::Accept]
                .into_iter()
                .filter_map(|name| {
                    header_updates
                        .iter()
                        .find(|(n, _)| name.matches(n))
                        .map(|(_, v)| Entry::raw(name, SipStr::owned(v.as_deref().unwrap_or(""))))
                })
                .collect();
        relay::stamp_a_facing_invite_advert(&mut extra_headers, &service_owned, &advert);
        // Built before the a-dialog moves to A2: a refused answer (the INVITE
        // already carries its final) leaves the dialog the caller holds intact.
        let Some(effect) = relay::response_to_a_leg(
            call,
            fx,
            &a_invite,
            status,
            reason,
            Some(a2.clone()),
            Some(contact),
            body.to_vec(),
            content_type,
            None,
            extra_headers,
            Provenance::Authored,
        ) else {
            return;
        };
        // Seed the a-dialog if absent (fresh minting adopts A2 directly); when it
        // already exists under the early-media A1, `ensure_a_dialog_with` returns
        // A1 unchanged, so re-stamp local_tag to A2 explicitly — the early dialog
        // is superseded, not kept. The answer SDP becomes the dialog's
        // `cached_sdp`, as `confirm_dialog` keeps it.
        self.ensure_a_dialog_with(call, Some(a2.clone()));
        if let Some(d) = call.a_leg.dialogs.first_mut() {
            d.sip.local_tag = a2;
            if !body.is_empty() {
                d.ext.cached_sdp = Some(body.to_vec());
            }
        }
        // This 2xx answers the caller: it goes through the one seam that
        // retains the datagram + arms the §13.3.1.4 ladder.
        self.send_a_leg_answer(call, fx, effect);
        // The caller now holds a confirmed dialog under A2 — confirm the a-leg.
        *call = set_leg_state(call.clone(), &call.a_leg.leg_id.clone(), LegState::Confirmed);
        // An answer the B2BUA authors on its own behalf is an answer the caller
        // receives: under the `Answer` anchor the overall call ceiling runs from
        // it (`MaxDurationAnchor`), as `confirm-dialog` anchors it at a 2xx it
        // relays. Under `Creation` the creation-time arm stands.
        if let Some(platform) = call.features.as_ref().map(|f| f.platform.clone()) {
            if platform.max_duration_anchor == call::features::MaxDurationAnchor::Answer {
                self.schedule(
                    call,
                    fx,
                    TimerType::GlobalDuration,
                    platform.max_duration_sec * 1000,
                    None,
                );
            }
        }
    }
}

/// Build the extra a-leg response headers for a decision-authored Reject/Redirect
/// ([`crate::rules::model::RuleAction::RespondToALeg`]): non-structural
/// `header_updates` *sets* plus one `Contact: <uri>;q=…` per redirect target.
/// Removals and structural keys drop — the response generator owns the
/// stack-owned set (ADR-0017 X2), including the Contact a redirect authors from
/// its typed target list.
/// The non-structural `header_updates` *sets*. Removals and structural keys
/// drop — the response generator owns the stack-owned set (ADR-0017 X2).
fn header_update_lines(header_updates: &[(String, Option<String>)]) -> Vec<SipHeader> {
    header_updates
        .iter()
        .filter_map(|(name, val)| {
            let named = HeaderName::from(name.as_str());
            match (val, named.class()) {
                (Some(v), HeaderClass::EndToEnd) => {
                    Some(SipHeader { name: SipStr::owned(name), value: SipStr::owned(v) })
                }
                _ => None,
            }
        })
        .collect()
}

/// What the decision authored for the caller's final, as
/// [`crate::rules::model::RuleAction::RespondToALeg`] states it: the status line
/// plus the two lists that own their own names.
pub(super) struct AuthoredFinal<'a> {
    pub status: u16,
    pub reason: &'a str,
    pub header_updates: &'a [(String, Option<String>)],
    pub contacts: &'a [(String, Option<f32>)],
}

/// The failing peer's relayable headers, but ONLY on a final that answers the
/// `/call/failure` consult the image belongs to — the `call-failure-result`
/// event. A setup deadline, a capacity refusal or a media-service failure mints
/// its own diagnosis about a peer that is not the one being answered, so it
/// carries none of them: a fold whose `origin` is `call_limiter` resolved a
/// limiter refusal (the router's re-consult / terminal 486), not the peer's
/// final, and folds nothing.
fn failure_headers_answering(ctx: &RuleContext, call: &Call) -> Vec<SipHeader> {
    match ctx.event {
        crate::event::CallEvent::InternalEvent { topic, payload, .. }
            if topic == "call-failure-result"
                && payload.get("origin").and_then(|v| v.as_str()) != Some("call_limiter") =>
        {
            relay::relayed_failure_headers(call.ext.as_ref())
        }
        _ => Vec::new(),
    }
}

/// Errs when a redirect target does not read — the whole redirect is refused,
/// never partially authored.
fn build_a_leg_response_headers(
    header_updates: &[(String, Option<String>)],
    contacts: &[(String, Option<f32>)],
) -> Result<Vec<SipHeader>, relay::UnreadableAddress> {
    let mut out = header_update_lines(header_updates);
    for (uri, q) in contacts {
        out.push(relay::redirect_contact(uri, *q)?);
    }
    Ok(out)
}
