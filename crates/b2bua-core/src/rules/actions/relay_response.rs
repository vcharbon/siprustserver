//! Relaying an inbound SIP **response** toward the peer (normally the a-leg):
//! the per-fork caller-facing dialog on b-leg INVITE 1xx/2xx, the a-leg
//! server-txn regeneration default, and the bare-180 downgrade
//! (`relayFirst18xTo180`). A response to a relayed request is relayed by
//! [`super::relay_pending_response`]; request relay does NOT live here — see
//! [`super::relay_request`].

use call::helpers::Scope;
use call::Call;
use sip_message::generators::{RelayDirection, RelayScope, RelaySituation, SourceBody};
use sip_message::header::{HeaderName, MediaType};
use sip_message::{SipHeader, SipResponse, SipStr};

use crate::effects::{HandlerEffects, Provenance};
use crate::rules::capabilities::{self, Face};
use crate::rules::relay;
use b2bua_sdk::model::{MessageTransform, RuleContext};
use b2bua_sdk::relayed_final::invite_timestamp;

use super::select::resolve_peer;
use super::ActionExecutor;

/// A response on its way to `target_leg`, as every relay path reads it.
pub(super) struct Relaying<'a> {
    pub resp: &'a SipResponse,
    pub transform: &'a MessageTransform,
    pub target_leg: &'a str,
    /// The leg the response arrived on.
    pub source_leg_id: String,
    pub status: u16,
    pub reason: String,
    /// The body relayed toward alice: dropped (bare-180 downgrade), replaced
    /// by a staged policy body (fake-prack cached SDP on the 200 OK), or the
    /// response's own body verbatim.
    pub body: Vec<u8>,
    pub content_type: Option<MediaType>,
    /// What the relayed message carries where THIS response had a body, which
    /// decides how much of the set describing it travels (§16.6): a staged
    /// body of the same role keeps the header stating that role, a dropped
    /// body keeps none.
    pub source_body: SourceBody,
    /// Whether the relayed message carries a description: on a reliable
    /// provisional, the description its INVITE's sender holds (RFC 3311 §5.1).
    pub carried_sdp: bool,
    pub cseq_num: i64,
    pub cseq_method: String,
    pub to_tag: String,
}

impl<'a> Relaying<'a> {
    /// `resp`, arrived on `source_leg_id`, relayed toward `target_leg` under
    /// `transform`. The staged body is ONE-SHOT: armed per 2xx by the staging
    /// rule and taken here, so it never substitutes itself into a later
    /// relayed response. `ConfirmDialog` runs before this relay, so the
    /// retransmit copy it stashes still sees it.
    fn of(
        call: &mut Call,
        source_leg_id: &str,
        target_leg: &'a str,
        transform: &'a MessageTransform,
        resp: &'a SipResponse,
    ) -> Self {
        let (body, content_type, source_body) = if transform.drop_body {
            (vec![], None, SourceBody::Dropped)
        } else if let Some(call::PolicyUpdateBody::Bytes(b)) = call.policy_update_body.take() {
            (b, Some(relay::sdp()), SourceBody::Replaced)
        } else {
            (
                resp.body().to_vec(),
                resp.raw(HeaderName::ContentType).next().and_then(relay::media_type),
                SourceBody::Verbatim,
            )
        };
        let carried_sdp = content_type
            .as_ref()
            .is_some_and(|ct| sip_message::multipart::sdp_range(ct, &body).is_some());
        let cseq = resp.cseq();
        let cseq_method = if cseq.method().as_str().is_empty() {
            "INVITE".to_string()
        } else {
            cseq.method().to_string()
        };
        Relaying {
            resp,
            transform,
            target_leg,
            source_leg_id: source_leg_id.to_string(),
            status: transform.status.unwrap_or(resp.status()),
            reason: transform.reason.clone().unwrap_or_else(|| resp.reason().to_string()),
            body,
            content_type,
            source_body,
            carried_sdp,
            cseq_num: cseq.seq() as i64,
            cseq_method,
            to_tag: resp.to().tag().unwrap_or_default().to_string(),
        }
    }

    /// Passthrough headers minus any the transform suppresses (e.g.
    /// Require/RSeq on a bare-180 downgrade), plus any the transform stamps
    /// with replace semantics (Allow/Supported on the synthetic 200 / resync
    /// re-INVITE, `promote18xPemTo200`).
    pub(super) fn passthrough(&self, hs: Vec<SipHeader>) -> Vec<SipHeader> {
        let add_headers = &self.transform.add_headers;
        let mut out: Vec<SipHeader> = hs
            .into_iter()
            .filter(|h| {
                !self.transform.remove_headers.iter().any(|r| r.matches(&h.name))
                    && !add_headers.iter().any(|e| e.name().matches(&h.name))
            })
            .collect();
        for entry in add_headers {
            out.push(SipHeader {
                name: SipStr::owned(entry.name().as_wire_str()),
                value: entry.text(),
            });
        }
        out
    }
}

impl ActionExecutor<'_> {
    /// Relay an inbound SIP response toward `target_leg` (normally the a-leg).
    /// Two paths, mirroring the source:
    ///   - **pending-correlated** (any relayed in-dialog request: re-INVITE/
    ///     PRACK/OPTIONS/INFO/UPDATE/…): rebuild from the snapshot captured
    ///     when the request was relayed ([`Self::relay_pending_response`]).
    ///   - **default** (initial-INVITE 1xx/2xx): regenerate on the a-leg server
    ///     transaction, establishing the b-leg early dialog + the caller-facing
    ///     dialog fronting it on a reliable 1xx so a later PRACK can target the callee.
    pub(super) fn relay_response(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        target_leg: &str,
        transform: &MessageTransform,
        resp: &sip_message::SipResponse,
    ) {
        let r = Relaying::of(call, ctx.source_leg_id, target_leg, transform, resp);

        // ── Pending transparent-relay correlation (§8.1.3.3) ──
        // The relay this response answers, as the engine's dialog gate
        // resolved it: kept on the dialog the response's tags name (our
        // From-tag, the responder's To-tag), live or retired by the answer,
        // for the same method and CSeq. Never another dialog's: two dialogs
        // may carry the same CSeq, and a final delivered to a transaction it
        // does not answer — or regenerated below as an INVITE response —
        // would turn a `200 (PRACK)` into a spurious `200 (INVITE)`.
        let relayed = ctx.resolved.record.as_deref().filter(|(_, r)| r.is_relay()).cloned();
        if let Some((holder, pending)) = relayed {
            self.relay_pending_response(call, fx, &r, holder, pending);
            return;
        }
        let status = r.status;
        let source_leg_id = r.source_leg_id.as_str();
        let to_tag = r.to_tag.as_str();

        // A provisional to the originator's initial INVITE after its final:
        // the seam refuses it before any a-facing tag or RSeq is minted for
        // it (RFC 3261 §17.2.1), and the ringing leg keeps its early dialog;
        // a reliable one, shown to no one, the engine PRACKs (RFC 3262 §4).
        if r.cseq_method == "INVITE"
            && status < 200
            && relay::provisional_after_final(call, fx, status)
        {
            if !to_tag.is_empty() && call::helpers::find_b_leg(call, source_leg_id).is_some() {
                self.ensure_b_early_dialog(call, ctx, source_leg_id, to_tag);
            }
            return;
        }

        // ── b-leg INVITE 1xx/2xx → per-fork caller-facing dialog ──
        // Each callee early dialog (forking → several per b-leg) gets its own
        // a-facing tag so the caller sees independent early dialogs; the response
        // is relayed under that per-fork tag (RFC 3261 §12; source confirm/relay).
        // The 2xx is included so that when a *non-first* fork wins, the confirmed
        // dialog the caller sees carries the WINNING fork's a-tag — not the first
        // fork's primary (RFC 3261 §13.2.2.4) — so the caller's ACK/in-dialog
        // requests address the dialog the B2BUA actually established.
        // The range IS the split: the answering fork's tag on a 2xx, the leg's
        // primary on a non-2xx final — which ends the transaction and
        // establishes nothing, so it names no dialog the caller keeps.
        if r.cseq_method == "INVITE"
            && (100..300).contains(&resp.status())
            && !to_tag.is_empty()
            && call::helpers::find_b_leg(call, source_leg_id).is_some()
        {
            self.track_b_early_dialog(call, source_leg_id, resp, to_tag);
            let answering = (200..300).contains(&status);
            let a_face = self.a_face_tag(call, source_leg_id, to_tag, answering);
            call::helpers::note_shown(call, source_leg_id, to_tag);
            let Some(a_invite) = self.caller_invite(call) else { return };
            let contact = relay::leg_contact(call, &call.a_leg.leg_id);
            let mut passthrough = r.passthrough(relay::relay_response_passthrough_headers(
                resp,
                self.config
                    .relay_scope(
                        RelayScope::response_carrying(r.source_body),
                        RelaySituation::response(
                            status,
                            resp.cseq().method(),
                            RelayDirection::TowardCaller,
                        ),
                    )
                    .stamped(invite_timestamp(&call.a_leg_invite)),
            ));
            relay::withhold_from_relayed_provisional(call, status, &mut passthrough);
            // A 2xx INVITE answer the B2BUA mints toward the caller advertises the
            // capability set of the originator face (RFC 3261 §13.2.1/§20.37):
            // the callee's own, carried through, unless the call declares one.
            // Provisionals keep verbatim passthrough so reliable-1xx
            // (Supported:100rel) negotiation survives.
            if (200..300).contains(&status) {
                let caps = capabilities::relaying(call, Face::Originator, &passthrough);
                relay::stamp_advertisement(&mut passthrough, &transform.add_headers, &caps);
            }
            let shown = self.own_relayed_rseq(
                call,
                a_invite.offers_100rel(),
                &a_face,
                i64::from(a_invite.cseq().seq()),
                source_leg_id,
                resp,
                &mut passthrough,
                r.carried_sdp,
            );
            let Some(mut effect) = relay::response_to_a_leg(
                call,
                fx,
                &a_invite,
                status,
                &r.reason,
                Some(a_face.clone()),
                contact,
                r.body,
                r.content_type,
                passthrough,
                Provenance::Relayed,
                relay::Author::Leg(source_leg_id),
            ) else {
                return;
            };
            // A 2xx answers the caller: it goes through the one seam that
            // retains the datagram + arms the §13.3.1.4 ladder.
            if (200..300).contains(&status) {
                self.send_a_leg_answer(call, fx, effect);
            } else {
                // A reliable provisional leaving under our own number is ours
                // to repeat until PRACKed (RFC 3262 §3): retain it + arm the
                // caller-facing ladder.
                if let Some(rseq) = shown.filter(|_| (101..200).contains(&status)) {
                    self.arm_reliable_provisional_ladder(call, fx, &mut effect, &a_face, rseq);
                }
                fx.outbound.push(effect);
            }
            return;
        }

        // A non-INVITE response that reached here failed pending correlation
        // (§8.1.3.3): there is no a-leg transaction to answer. Regenerating it as
        // an INVITE response would emit a spurious `200 (INVITE)` toward the
        // caller (the exact forking corruption above). Drop it instead — only an
        // INVITE response legitimately regenerates on the a-leg server txn.
        if r.cseq_method != "INVITE" {
            return;
        }

        // ── Default: regenerate the INVITE response on the a-leg server txn ──
        // The a-dialog's tag, whoever showed it first: a caller already put in an
        // early dialog is answered under that same tag (§8.2.6.2), including the
        // owned bare 180's under `relayFirst18xTo180`.
        let a_tag = self.ensure_a_dialog(call);
        let Some(a_invite) = self.caller_invite(call) else { return };
        let contact = relay::leg_contact(call, &call.a_leg.leg_id);
        // Reliable-provisional negotiation (Require/Supported) passes through
        // transparently so end-to-end PRACK keeps working (RFC 3262); the RSeq
        // it rides on is this transaction's own (`own_relayed_rseq`).
        let mut passthrough = r.passthrough(relay::relay_response_passthrough_headers(
            resp,
            self.config
                .relay_scope(
                    RelayScope::response_carrying(r.source_body),
                    RelaySituation::response(
                        status,
                        resp.cseq().method(),
                        RelayDirection::TowardCaller,
                    ),
                )
                .stamped(invite_timestamp(&call.a_leg_invite)),
        ));
        // A compliant callee's provisional carries a To tag (RFC 3261 §8.2.6.2)
        // and takes the per-fork seam above, where the call's provisional
        // withhold applies; this seam relays finals and a tagless 1xx.
        // A 2xx INVITE answer carries the originator face's Allow/Supported —
        // the callee's own, carried through, unless the call declares a set
        // (RFC 3261 §13.2.1/§20.37); provisionals keep passthrough.
        if (200..300).contains(&status) {
            let caps = capabilities::relaying(call, Face::Originator, &passthrough);
            relay::stamp_advertisement(&mut passthrough, &transform.add_headers, &caps);
        }
        let shown = self.own_relayed_rseq(
            call,
            a_invite.offers_100rel(),
            &a_tag,
            i64::from(a_invite.cseq().seq()),
            source_leg_id,
            resp,
            &mut passthrough,
            r.carried_sdp,
        );
        let Some(mut effect) = relay::response_to_a_leg(
            call,
            fx,
            &a_invite,
            status,
            &r.reason,
            Some(a_tag.clone()),
            contact,
            r.body,
            r.content_type,
            passthrough,
            Provenance::Relayed,
            relay::Author::Leg(source_leg_id),
        ) else {
            return;
        };
        // A 2xx answers the caller: it goes through the one seam that retains
        // the datagram + arms the §13.3.1.4 ladder.
        if (200..300).contains(&status) {
            self.send_a_leg_answer(call, fx, effect);
        } else {
            // A relayed non-2xx final ends the setup: no 1xx may follow it on
            // the a-leg INVITE transaction (RFC 3261 §17.2.1), so every §3
            // ladder stops with it.
            if status >= 300 {
                self.retire(call, fx, Scope::Provisionals);
            }
            // A reliable provisional leaving under our own number is ours to
            // repeat until PRACKed (RFC 3262 §3): retain it + arm the
            // caller-facing ladder.
            if let Some(rseq) = shown.filter(|_| (101..200).contains(&status)) {
                self.arm_reliable_provisional_ladder(call, fx, &mut effect, &a_tag, rseq);
            }
            fx.outbound.push(effect);
        }
    }

    /// Bare-180 downgrade relay ([`b2bua_sdk::model::RuleAction::RelayFirstBare180`]).
    /// The bare 180 ESTABLISHES the a-leg dialog it shows the caller, so that
    /// dialog's local tag is the owned a-facing To-tag: every later relayed 18x,
    /// every non-2xx final and the transaction layer's own 487 answer under it
    /// (RFC 3261 §8.2.6.2, §17.2.1), and so does the 2xx of a callee dialog it
    /// mapped. A LATER 18x the `Relay18xMessages` policy relays again (ALL /
    /// ONE_PER_VALUE) reads back the same tag, so the caller holds ONE early
    /// dialog regardless of which fork rings; the 2xx of a dialog it never
    /// showed her opens a second (`answering-dialog-identity`). The a-facing
    /// dialog fronts this b-leg dialog; record it (+ the upstream status value
    /// for ONE_PER_VALUE dedupe), then relay the current 1xx as a bare 180. The
    /// relay path resolves the a-facing tag from the fronting dialog
    /// (`fronting_dialog`).
    pub(super) fn relay_first_bare_180(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
        b_tag: &str,
    ) {
        // The a-leg INVITE already sent its final: refused at the seam before
        // the mask records anything (RFC 3261 §17.2.1).
        if relay::provisional_after_final(call, fx, 180) {
            return;
        }
        let stored = call::helpers::relay_first_18x_stored_a_tag(call).map(str::to_string);
        let a_facing_tag = self.ensure_a_dialog_with(call, stored);
        self.map_a_face(call, &a_facing_tag, leg_id, b_tag);
        call::helpers::set_relay_first_18x_relayed(call, &a_facing_tag);
        if let Some(resp) = ctx.response() {
            call::helpers::record_relay_first_18x_value(call, resp.status());
        }
        // A bare 180 states ringing and nothing else: the reliable-provisional
        // negotiation goes with the reliability the downgrade removes (RFC
        // 3262), and the RFC 5009 early-media authorization goes with the media
        // description — authorizing a stream the caller is given no description
        // of leaves it listening to a gate it cannot open.
        let transform = MessageTransform {
            status: Some(180),
            reason: Some("Ringing".to_string()),
            drop_body: true,
            remove_headers: vec![HeaderName::Require, HeaderName::RSeq, HeaderName::PEarlyMedia],
            add_headers: vec![],
        };
        if let call::helpers::RelayPeer::Leg { leg_id, dialog } = resolve_peer(call, ctx) {
            self.relay_to(call, fx, ctx, &leg_id, &transform, dialog);
        }
    }
}
