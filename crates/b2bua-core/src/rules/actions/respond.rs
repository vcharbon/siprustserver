//! Response synthesis toward a leg: answering the current request in place
//! (`Respond`), decision-authored a-leg finals (reject / redirect / failure),
//! the a-side fork-confirm (`AnswerALegNewDialog`), brokered early-media
//! provisionals. Relaying a *peer's* response does NOT live here — see
//! [`super::relay_response`]; the ladder an a-facing 2xx arms lives in
//! [`super::ladder`].

use call::helpers::Scope;
use call::{Call, TimerKey, TimerType};
use sip_message::draft::Entry;
use sip_message::generators::{self, GenerateResponseOpts, RelayDirection, RelaySituation};
use sip_message::header::{HeaderClass, HeaderName};
use sip_message::{Method, SipHeader, SipStr};

use crate::config::{B2buaConfig, RetryAfterRange};
use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::capabilities::{self, Face};
use crate::rules::relay;
use crate::rules::RelayedFinal;
use b2bua_sdk::model::{GlareRefusal, RuleContext};

use super::ActionExecutor;

/// The `Retry-After` range RFC 3261 §14.2 and RFC 3311 §5.2 set on the 500
/// refusing a request whose sender's earlier INVITE or UPDATE is unanswered:
/// 0 to 10 s.
const GLARE_500_RETRY_AFTER: RetryAfterRange = RetryAfterRange { min_sec: 0, max_sec: 10 };

impl ActionExecutor<'_> {
    /// Answer the current request event in place with `status` (no relay) —
    /// the response goes to the request's top-Via response target on the
    /// source leg's server transaction, under the source face's Contact where
    /// [`generators::response_states_own_contact`] states one (the 2xx to
    /// UPDATE, the 202 to REFER). `extra_headers` ride the response as given; on a failure final to the
    /// initial INVITE the deployment's minted-final advertisement fills a half
    /// they leave unnamed.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn respond(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        status: u16,
        reason: &str,
        body: &[u8],
        content_type: Option<&str>,
        mut extra_headers: Vec<SipHeader>,
    ) {
        if let Some(req) = ctx.request() {
            if req.method() == Method::Invite
                && req.to().tag().is_none()
                && capabilities::is_minted_failure(status)
            {
                capabilities::stamp_own_advertisement(
                    &self.config.minted_final_advertisement,
                    |_| false,
                    &mut extra_headers,
                );
            }
            // A request naming no dialog is answered under the leg's own tag
            // (RFC 3261 §8.2.6.2), never a minted one; a tagged To is echoed.
            let to_tag = req
                .to()
                .tag()
                .is_none()
                .then(|| {
                    call::helpers::find_leg(call, ctx.source_leg_id)
                        .and_then(call::helpers::leg_own_tag)
                        .map(str::to_string)
                })
                .flatten();
            let content_type = content_type.and_then(relay::media_type);
            let dialog = relay::received_on(req);
            let body = relay::continue_on_leg(
                call,
                ctx.source_leg_id,
                dialog.as_ref(),
                relay::Author::Stack,
                relay::Carried::of(req.method(), Some(status), req.to().tag().is_some()),
                body.to_vec(),
                content_type.as_ref(),
                self.config.sdp_form.as_ref(),
            );
            relay::note_answered_here(call, ctx.source_leg_id, req);
            // A 2xx or 1xx to a target refresh or a subscription-creating
            // request names this face as the dialog's remote target
            // (`response_states_own_contact`); anything else states none. The
            // response echoes the request's To tag, else carries the leg's own
            // (`leg_own_tag`; none on the a-leg before its dialog exists). No
            // rule answers locally with a 1xx, so the policy's tagged-1xx
            // clause needs no check here.
            let contact = relay::leg_contact(call, ctx.source_leg_id)
                .filter(|_| generators::response_states_own_contact(req.method(), status));
            let opts = GenerateResponseOpts {
                to_tag,
                contact,
                body,
                content_type,
                extra_headers,
                ..Default::default()
            };
            let resp = match generators::generate_response(req, status, reason, &opts) {
                Ok(resp) => resp,
                Err(e) => return relay::unbuilt(&call.call_ref, ctx.source_leg_id, "response", &e),
            };
            // RFC 3261 §18.2.2: to the request's top-Via response target.
            let (host, port) = req.top_via().response_target();
            fx.outbound.push(OutboundSipEffect {
                body: OutboundBody::Response(resp),
                mode: OutboundTxnMode::ServerResponse,
                destination: (host.to_string(), port),
                label: format!("{status} (respond)"),
                leg_id: Some(ctx.source_leg_id.to_string()),
                provenance: Provenance::Authored,
            });
        }
    }

    /// Refuse the current INVITE that meets glare, or UPDATE over an open
    /// offer, in place (RFC 3261 §14.2, RFC 3311 §5.2): 500 with a
    /// `Retry-After` drawn from [`GLARE_500_RETRY_AFTER`] when the sender's
    /// own INVITE or UPDATE is unanswered; 491 otherwise, with one from
    /// [`glare_retry_after`](crate::config::B2buaConfig::glare_retry_after)
    /// only when that INVITE's 2xx awaits its ACK.
    pub(super) fn refuse_glare(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        refusal: GlareRefusal,
    ) {
        let (status, reason, range) = match refusal {
            GlareRefusal::Unanswered => (500, "Server Internal Error", Some(GLARE_500_RETRY_AFTER)),
            GlareRefusal::Unacknowledged => (491, "Request Pending", self.config.glare_retry_after),
            GlareRefusal::Crossing => (491, "Request Pending", None),
        };
        let extra = range
            .map(|range| SipHeader {
                name: SipStr::owned(HeaderName::RetryAfter.as_wire_str()),
                value: SipStr::owned(&range.pick(self.turn.ids.draw()).to_string()),
            })
            .into_iter()
            .collect();
        self.respond(call, fx, ctx, status, reason, &[], None, extra);
    }

    /// Answer the a-leg INVITE with a failure final under the B2BUA's own
    /// a-dialog tag ([`b2bua_sdk::model::RuleAction::RelayFailureToALeg`]);
    /// the B2BUA's Contact rides only where
    /// [`sip_message::generators::response_states_own_contact`] states it, and
    /// a 3xx / 485 carries the failing peer's retry targets instead.
    /// A final that answers the `/call/failure` consult restates the failing
    /// b-leg final's relayable headers (RFC 3261 §16.6), so what the refusing
    /// peer stated — its `Warning`, charging correlation, vendor annotations —
    /// reaches the caller; a final answering anything else speaks only for
    /// itself, under the deployment's advertisement for a minted final.
    pub(super) fn relay_failure_to_a_leg(
        &mut self,
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
        let Some(a_invite) = self.caller_invite(call) else { return };
        let contact = relay::leg_contact(call, &call.a_leg.leg_id);
        // Guard: the status relayed here is the failing peer's own, so the
        // image already holds retry targets only on a 3xx / 485. Any other
        // status keeps no Contact line, whatever the image holds.
        let image = failure_headers_answering(ctx, call, self.config, status);
        let restates = image.is_some();
        let mut extra = image.unwrap_or_default();
        generators::retain_retry_targets_for(status, &mut extra);
        if !restates && capabilities::is_minted_failure(status) {
            capabilities::stamp_own_advertisement(
                &self.config.minted_final_advertisement,
                |_| false,
                &mut extra,
            );
        }
        if let Some(effect) = relay::response_to_a_leg(
            call,
            fx,
            &a_invite,
            status,
            reason,
            Some(a_tag),
            contact,
            vec![],
            None,
            extra,
            Provenance::Relayed,
            relay::Author::Stack,
        ) {
            fx.outbound.push(effect);
        }
    }

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
    /// under the decision's statements.
    pub(super) fn respond_to_a_leg(
        &mut self,
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
                    (500, err.to_string(), header_update_lines(header_updates), true)
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
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        mut effect: OutboundSipEffect,
    ) {
        self.retire(call, fx, Scope::Provisionals);
        // The 2xx confirms the caller-facing dialog its To-tag names (RFC 3261
        // §12.1.1) and retires her other early dialogs.
        if let OutboundBody::Response(resp) = &effect.body {
            if let Some(tag) = resp.to().tag().filter(|t| !t.is_empty()) {
                super::dialog_track::adopt_a_tag(call, tag);
            }
        }
        self.retain_a_leg_answer(call, fx, &mut effect);
        fx.outbound.push(effect);
    }

    /// Broker an unadopted leg's SDP onto the a-leg as an unreliable provisional
    /// (RFC 3262 §3 early media). Only the a-leg has a stored UAS INVITE to
    /// answer; a non-a target or a non-1xx status is skipped. `to_tag` set ⇒ the
    /// caller's early identity, stated by the service; absent ⇒ the B2BUA's own
    /// ([`Self::media_face_tag`]): the early media of the leg `author` names
    /// rides an early dialog of its own, apart from the callee's.
    ///
    /// Either way the tag the caller is SHOWN has an a-dialog: the a-dialog's
    /// own where it held none the caller saw — a b-leg confirm may have minted
    /// one before she saw anything — so the non-2xx final ending this
    /// transaction answers under a tag she is in (§17.2.1); an early dialog of
    /// its own beside a tag she already holds, which keeps its record
    /// (§12.1.2). A 2xx confirms the callee's dialog and ends the others
    /// (`adopt_a_tag`), under A2 ([`Self::answer_a_leg_new_dialog`]) where the
    /// service states it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_provisional_to_leg(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        status: u16,
        reason: &str,
        body: &[u8],
        content_type: Option<&str>,
        descriptors: &[SipHeader],
        to_tag: Option<&str>,
        p_early_media: Option<&str>,
        author: relay::Author<'_>,
    ) {
        if !(100..200).contains(&status) || leg_id != call.a_leg.leg_id {
            return;
        }
        // The a-leg INVITE already sent its final: refused at the seam before
        // the caller's early identity is touched (RFC 3261 §17.2.1).
        if relay::provisional_after_final(call, fx, status) {
            return;
        }
        let to_tag = self.early_media_tag(call, to_tag, author);
        // SDP early-media body defaults to application/sdp (mirrors the request path).
        let content_type = content_type
            .and_then(relay::media_type)
            .or_else(|| (!body.is_empty()).then(relay::sdp));
        let Some(a_invite) = self.caller_invite(call) else { return };
        let contact = relay::leg_contact(call, &call.a_leg.leg_id);
        let mut extra_headers = Vec::new();
        if let Some(pem) = p_early_media {
            extra_headers.push(SipHeader {
                name: SipStr::owned(HeaderName::PEarlyMedia.as_wire_str()),
                value: SipStr::owned(pem),
            });
        }
        relay::describe_body(&mut extra_headers, body, descriptors);
        if let Some(effect) = relay::response_to_a_leg(
            call,
            fx,
            &a_invite,
            status,
            reason,
            Some(to_tag),
            contact,
            body.to_vec(),
            content_type,
            extra_headers,
            Provenance::Authored,
            author,
        ) {
            fx.outbound.push(effect);
        }
    }

    /// A-side fork-confirm ([`b2bua_sdk::model::RuleAction::AnswerALegNewDialog`]):
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
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        status: u16,
        reason: &str,
        body: &[u8],
        content_type: Option<&str>,
        descriptors: &[SipHeader],
        to_tag: Option<&str>,
        header_updates: &[(String, Option<String>)],
        relayed: &RelayedFinal,
        author: relay::Author<'_>,
    ) {
        // Only a 2xx establishes the new a-dialog (RFC 3261 §12.1). A non-2xx
        // final does not create a dialog and is not this primitive's job.
        if !(200..300).contains(&status) {
            return;
        }
        // A2: the caller-supplied tag verbatim, else the one the call derives
        // for the answering dialog, distinct from every early-media tag (RFC
        // 3264 §5.1).
        let a2 = to_tag.map(str::to_string).unwrap_or_else(|| crate::caller_tags::answering(call));
        // SDP answer defaults to application/sdp (mirrors the provisional path).
        let content_type = content_type
            .and_then(relay::media_type)
            .or_else(|| (!body.is_empty()).then(relay::sdp));
        let Some(a_invite) = self.caller_invite(call) else { return };
        let contact = relay::leg_contact(call, &call.a_leg.leg_id);
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
        let undecided: Vec<SipHeader> = descriptors
            .iter()
            .filter(|d| {
                !header_updates.iter().any(|(n, _)| HeaderName::from(n.as_str()).matches(&d.name))
            })
            .cloned()
            .collect();
        relay::describe_body(&mut extra_headers, body, &undecided);
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
        relay::stamp_advertisement(&mut extra_headers, &service_owned, &advert);
        // Built before the a-dialog moves to A2: a refused answer (the INVITE
        // already carries its final) leaves the dialog the caller holds intact.
        let Some(effect) = relay::response_to_a_leg(
            call,
            fx,
            &a_invite,
            status,
            reason,
            Some(a2.clone()),
            contact,
            body.to_vec(),
            content_type,
            extra_headers,
            Provenance::Authored,
            author,
        ) else {
            return;
        };
        // Seed the a-dialog if absent (fresh minting adopts A2 directly); when it
        // already exists under the early-media A1, `adopt_a_tag` confirms a
        // dialog of its own under A2 and retires A1 — the early dialog is
        // superseded, not kept. The answer SDP becomes the dialog's
        // `cached_sdp`, as `confirm_dialog` keeps it.
        self.ensure_a_dialog_with(call, Some(a2.clone()));
        super::dialog_track::adopt_a_tag(call, &a2);
        if let Some(d) = super::dialog_track::own_a_dialog_mut(call) {
            if !body.is_empty() {
                d.cached_sdp = Some(body.to_vec());
            }
        }
        // This 2xx answers the caller: it goes through the one seam that
        // retains the datagram + arms the §13.3.1.4 ladder.
        self.send_a_leg_answer(call, fx, effect);
        // An answer the B2BUA authors on its own behalf is an answer the caller
        // receives: under the `Answer` anchor the overall call ceiling runs from
        // it (`MaxDurationAnchor`), as `confirm-dialog` anchors it at a 2xx it
        // relays. Under `Creation` the creation-time arm stands.
        if let Some(platform) = call.features.as_ref().map(|f| f.platform.clone()) {
            if platform.max_duration_anchor == call::features::MaxDurationAnchor::Answer {
                self.schedule(
                    call,
                    fx,
                    TimerKey::call(TimerType::GlobalDuration),
                    platform.max_duration_ms,
                );
            }
        }
    }
}

/// Build the extra a-leg response headers for a decision-authored Reject/Redirect
/// ([`b2bua_sdk::model::RuleAction::RespondToALeg`]): non-structural
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
/// [`b2bua_sdk::model::RuleAction::RespondToALeg`] states it: the status line
/// plus the two lists that own their own names.
pub(super) struct AuthoredFinal<'a> {
    pub status: u16,
    pub reason: &'a str,
    pub header_updates: &'a [(String, Option<String>)],
    pub contacts: &'a [(String, Option<f32>)],
}

/// The failing peer's relayable headers, but ONLY on a final that answers the
/// `/call/failure` consult the image belongs to — the `call-failure-result`
/// event — and only when that failure had a peer final: `None` when the final
/// restates no peer's. A setup deadline, a capacity refusal or a media-service failure mints
/// its own diagnosis about a peer that is not the one being answered, so it
/// carries none of them, nor does a fold that answers no peer's final
/// ([`crate::failure_unavailable::answers_the_peer`]). The deployment's relay policy is read for the
/// final being minted: a `status` to the INVITE, toward the caller.
fn failure_headers_answering(
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
