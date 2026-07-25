//! Response synthesis toward a leg: answering the current request in place
//! (`Respond`), decision-authored a-leg finals (reject / redirect / failure),
//! the a-side fork-confirm (`AnswerALegNewDialog`), brokered early-media
//! provisionals, and the §13.3.1.4 un-ACKed-2xx retransmits. Relaying a
//! *peer's* response does NOT live here — see [`super::relay_response`].

use call::helpers::set_leg_state;
use call::{Call, LegState};
use sip_message::generators::{self, GenerateResponseOpts};
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::effects::{HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode};
use crate::rules::model::RuleContext;
use crate::rules::relay;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Answer the current request event in place with `status` (no relay, no
    /// dialog bookkeeping) — the response goes back to the request's top-Via
    /// sent-by on the source leg's server transaction.
    pub(super) fn respond(
        &self,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        status: u16,
        reason: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) {
        if let Some(req) = ctx.request() {
            let opts = GenerateResponseOpts {
                body: body.to_vec(),
                content_type: content_type.map(str::to_string),
                ..Default::default()
            };
            let resp = generators::generate_response(req, status, reason, &opts);
            let dest = top_via_dest(req);
            fx.outbound.push(OutboundSipEffect {
                body: OutboundBody::Response(resp),
                mode: OutboundTxnMode::ServerResponse,
                destination: dest,
                label: format!("{status} (respond)"),
                leg_id: Some(ctx.source_leg_id.to_string()),
            });
        }
    }

    /// Answer the a-leg INVITE with a failure final under the B2BUA's own
    /// a-dialog tag and Contact ([`crate::rules::model::RuleAction::RelayFailureToALeg`]).
    pub(super) fn relay_failure_to_a_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        status: u16,
        reason: &str,
    ) {
        let a_tag = self.ensure_a_dialog(call);
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
        fx.outbound.push(relay::response_to_a_leg(
            &a_invite,
            status,
            reason,
            Some(a_tag),
            Some(contact),
            vec![],
            None,
            None,
            vec![],
        ));
    }

    /// Answer the a-leg INVITE with a decision-authored Reject/Redirect final
    /// ([`crate::rules::model::RuleAction::RespondToALeg`]). No B2BUA Contact: a
    /// redirect carries its own Contact list (via the built headers), a reject
    /// carries none (ADR-0017 header-ownership X2).
    pub(super) fn respond_to_a_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        status: u16,
        reason: &str,
        header_updates: &[(String, Option<String>)],
        contacts: &[(String, Option<f32>)],
    ) {
        let a_tag = self.ensure_a_dialog(call);
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let extra = build_a_leg_response_headers(header_updates, contacts);
        fx.outbound.push(relay::response_to_a_leg(
            &a_invite,
            status,
            reason,
            Some(a_tag),
            None,
            vec![],
            None,
            None,
            extra,
        ));
    }

    /// RFC 3261 §13.3.1.4 — re-send the a-leg INVITE 2xx toward the caller while
    /// its ACK is missing. The a-leg INVITE server txn is already `Completed`, so
    /// the txn layer would DROP a second final on the `ServerResponse` path; we
    /// send a faithful copy **raw** instead (same confirmed To-tag, same cached
    /// answer SDP, the B2BUA Contact). No-op until the a-dialog is confirmed.
    pub(super) fn retransmit_a_leg_2xx(&self, call: &Call, fx: &mut HandlerEffects) {
        let Some(d) = call.a_leg.dialogs.first() else { return };
        let a_tag = d.sip.local_tag.clone();
        if a_tag.is_empty() {
            return;
        }
        let body = d.ext.cached_sdp.clone().unwrap_or_default();
        let content_type = if body.is_empty() {
            None
        } else {
            Some("application/sdp".to_string())
        };
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
        // A 2xx INVITE answer carries the B2BUA's own Allow/Supported (RFC 3261
        // §13.2.1/§20.37), exactly as the original confirm-dialog relay stamped —
        // so the retransmit is byte-faithful and the RFC audit stays clean.
        let mut extra: Vec<sip_message::SipHeader> = Vec::new();
        relay::stamp_a_facing_invite_advert(&mut extra, &[]);
        let mut effect = relay::response_to_a_leg(
            &a_invite,
            200,
            "OK",
            Some(a_tag),
            Some(contact),
            body,
            content_type,
            None,
            extra,
        );
        // Bypass the (Completed) a-leg server txn — it would drop a second final.
        effect.mode = OutboundTxnMode::Raw;
        effect.label = "200 (2xx retransmit, no ACK) → a-leg".to_string();
        fx.outbound.push(effect);
    }

    /// RFC 3261 §13.3.1.4 (in-dialog) — re-send the a-leg **re-INVITE** 2xx
    /// toward the originator while its ACK is missing. Unlike the initial-INVITE
    /// twin above (which rebuilds from the never-mutated `a_leg_invite` +
    /// `cached_sdp`), a re-INVITE 2xx cannot be reconstructed from the initial
    /// snapshot, so the exact bytes captured at relay time
    /// (`pending_reinvite_2xx`, on the a-leg dialog) are re-parsed and re-emitted
    /// **raw** — byte-faithful to the 2xx the originator must ACK. No-op when no
    /// a-leg re-INVITE awaits an ACK.
    pub(super) fn retransmit_a_leg_reinvite_2xx(&self, call: &Call, fx: &mut HandlerEffects) {
        let Some(pending) = call
            .a_leg
            .dialogs
            .first()
            .and_then(|d| d.ext.pending_reinvite_2xx.as_ref())
        else {
            return;
        };
        let parsed = CustomParser::new().parse(&pending.response).ok();
        let Some(SipMessage::Response(resp)) = parsed else { return };
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Response(resp),
            // Bypass the (Completed) a-leg re-INVITE server txn — a second final
            // on the ServerResponse path would be dropped.
            mode: OutboundTxnMode::Raw,
            destination: (pending.dest_host.clone(), pending.dest_port),
            label: "200 (re-INVITE 2xx retransmit, no ACK) → a-leg".to_string(),
            leg_id: Some(call.a_leg.leg_id.clone()),
        });
    }

    /// Broker an unadopted leg's SDP onto the a-leg as an unreliable provisional
    /// (RFC 3262 §3 early media). Only the a-leg has a stored UAS INVITE to
    /// answer; a non-a target or a non-1xx status is skipped. `to_tag` set ⇒
    /// ephemeral forked early dialog (verbatim, not persisted); absent ⇒ the
    /// B2BUA's own early identity (reuse/mint+persist).
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
        // `to_tag` provided → an ephemeral forked early dialog, used verbatim and
        // NOT persisted onto the a-dialog. Absent → the B2BUA's own early identity:
        // reuse the existing a-dialog tag or mint and persist one.
        let to_tag = match to_tag {
            Some(t) => t.to_string(),
            None => self.ensure_a_dialog(call),
        };
        // SDP early-media body defaults to application/sdp (mirrors the request path).
        let content_type = content_type
            .map(str::to_string)
            .or_else(|| (!body.is_empty()).then(|| "application/sdp".to_string()));
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
        let mut extra_headers = Vec::new();
        if let Some(pem) = p_early_media {
            extra_headers.push(sip_message::SipHeader {
                name: "P-Early-Media".to_string().into(),
                value: pem.to_string().into(),
            });
        }
        fx.outbound.push(relay::response_to_a_leg(
            &a_invite,
            status,
            reason,
            Some(to_tag),
            Some(contact),
            body.to_vec(),
            content_type,
            None,
            extra_headers,
        ));
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
    /// relay the final/SDP under A2, confirm the a-leg, and cache the answer SDP
    /// for a §13.3.1.4 un-ACKed-2xx retransmit (mirrors `confirm_dialog`,
    /// including the B2BUA's own `Allow`/`Supported` advert on the 2xx — a
    /// `header_updates` entry naming either overrides it).
    ///
    /// The sip-txn layer only *stores* `uas_to_tag` from the first >100 response
    /// (the 183's A1) and never rewrites a later final's `to.tag`, so the `200`
    /// leaves under A2 verbatim; a late CANCEL's autonomous 487 still carries the
    /// pinned A1, which harmlessly matches the caller's abandoned early dialog.
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
        // Seed the a-dialog if absent (fresh minting adopts A2 directly); when it
        // already exists under the early-media A1, `ensure_a_dialog_with` returns
        // A1 unchanged, so re-stamp local_tag to A2 explicitly — the early dialog
        // is superseded, not kept. Also cache the answer SDP under A2 for a
        // §13.3.1.4 un-ACKed-2xx retransmit.
        self.ensure_a_dialog_with(call, Some(a2.clone()));
        if let Some(d) = call.a_leg.dialogs.first_mut() {
            d.sip.local_tag = a2.clone();
            if !body.is_empty() {
                d.ext.cached_sdp = Some(body.to_vec());
            }
        }
        // SDP answer defaults to application/sdp (mirrors the provisional path).
        let content_type = content_type
            .map(str::to_string)
            .or_else(|| (!body.is_empty()).then(|| "application/sdp".to_string()));
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
        let mut extra_headers = build_a_leg_response_headers(header_updates, &[]);
        // An a-facing INVITE 2xx carries the B2BUA's own Allow/Supported (RFC
        // 3261 §13.2.1/§20.37), same as the confirm-dialog relay. A
        // `header_updates` entry naming either owns it: a set value is kept
        // verbatim, a removal keeps it absent (`build_a_leg_response_headers`
        // already dropped it).
        let service_owned: Vec<(&'static str, String)> = ["Allow", "Supported"]
            .into_iter()
            .filter_map(|name| {
                header_updates
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, v)| (name, v.clone().unwrap_or_default()))
            })
            .collect();
        relay::stamp_a_facing_invite_advert(&mut extra_headers, &service_owned);
        fx.outbound.push(relay::response_to_a_leg(
            &a_invite,
            status,
            reason,
            Some(a2),
            Some(contact),
            body.to_vec(),
            content_type,
            None,
            extra_headers,
        ));
        // The caller now holds a confirmed dialog under A2 — confirm the a-leg.
        *call = set_leg_state(call.clone(), &call.a_leg.leg_id.clone(), LegState::Confirmed);
    }
}

fn top_via_dest(req: &sip_message::SipRequest) -> (String, u16) {
    if let Some(via) = get_header(&req.headers, "via") {
        if let Some(after) = via.split_whitespace().nth(1) {
            if let Some(sent_by) = after.split(';').next() {
                return relay::dest_of(sent_by.trim());
            }
        }
    }
    ("127.0.0.1".to_string(), 5060)
}

/// Structural headers the response generator owns — never settable via the flat
/// header map (ADR-0017 X2). `Contact` is excluded because a redirect authors it
/// from the typed contact list and a reject carries none.
const A_LEG_RESPONSE_STRUCTURAL: &[&str] = &[
    "from", "to", "via", "call-id", "cseq", "max-forwards", "content-length", "record-route",
    "contact",
];

/// Build the extra a-leg response headers for a decision-authored Reject/Redirect
/// ([`crate::rules::model::RuleAction::RespondToALeg`]): non-structural
/// `header_updates` *sets* plus one `Contact: <uri>;q=…` per redirect target.
/// Removals and structural keys drop.
fn build_a_leg_response_headers(
    header_updates: &[(String, Option<String>)],
    contacts: &[(String, Option<f32>)],
) -> Vec<sip_message::SipHeader> {
    let mut out: Vec<sip_message::SipHeader> = Vec::new();
    for (name, val) in header_updates {
        let is_structural =
            A_LEG_RESPONSE_STRUCTURAL.contains(&name.to_ascii_lowercase().as_str());
        if let (Some(v), false) = (val, is_structural) {
            out.push(sip_message::SipHeader { name: name.clone().into(), value: v.clone().into() });
        }
    }
    for (uri, q) in contacts {
        let value = match q {
            Some(q) => format!("<{uri}>;q={q}"),
            None => format!("<{uri}>"),
        };
        out.push(sip_message::SipHeader { name: "Contact".to_string().into(), value: value.into() });
    }
    out
}
