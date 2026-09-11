//! Relaying an inbound in-dialog **request** to the peer leg: per-dialog CSeq
//! bookkeeping, the RAck rewrite, the pending-request correlation snapshot,
//! and ACK origination toward a leg. Response relay does NOT live here — see
//! [`super::relay_response`].

use call::helpers::{add_pending_request, bump_local_cseq, relay_cseq_delta, update_remote_cseq};
use call::{Call, PendingRequest, RetainedEmission};
use sip_message::generators::{self, GenerateInDialogRequestOpts, InDialogMethod};
use sip_message::header::{HeaderName, MediaType, RAck};
use sip_message::{hops, Method};
use sip_txn::TxnKind;

use crate::effects::{HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode};
use crate::rules::capabilities;
use crate::rules::model::RuleContext;
use crate::rules::relay;

use super::select::{
    dialog_identity_tag, in_dialog_method, invite_cseq_from_handle, leg_at, leg_index,
};
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// ACK `leg_id`'s confirmed dialog, carrying `body`/`content_type` through
    /// (a delayed-offer answer rides the ACK, RFC 3261 §13.2.2.4 / RFC 3264 §4).
    pub(super) fn ack_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        body: Vec<u8>,
        content_type: Option<MediaType>,
    ) {
        let leg = if leg_id == call.a_leg.leg_id {
            Some(&call.a_leg)
        } else {
            call.b_legs.iter().find(|l| l.leg_id == leg_id)
        };
        // §13.2.2.4 hands THE ACK — the one the 2xx triggered — back to the
        // transport on every retransmitted copy: when this dialog's current
        // INVITE transaction already emitted its ACK, a bodyless ack re-sends
        // those bytes as they are (answer included on a delayed offer) instead
        // of composing a fresh, answerless datagram. A body-bearing call is a
        // first emission and composes below.
        if body.is_empty() {
            if let Some(retained) =
                leg.and_then(|l| l.dialogs.first()).and_then(|d| d.ext.emitted_ack.as_ref())
            {
                fx.outbound.push(super::ladder::repeat_of(
                    retained,
                    format!("ACK (re-ACK, 2xx retransmit) → {leg_id}"),
                    leg_id,
                ));
                return;
            }
        }
        let ack = leg.and_then(|leg| {
            relay::ack_b_leg(
                &call.call_ref,
                leg,
                call.emergency == Some(true),
                self.config,
                self.id_gen,
                body,
                content_type,
            )
        });
        if let Some((e, branch)) = ack {
            // Retain THE ACK's exact bytes + destination before it leaves, so a
            // retransmitted 2xx re-passes this datagram (§13.2.2.4).
            if let OutboundBody::Request(r) = &e.body {
                *call = call::helpers::retain_emitted_ack(
                    call.clone(),
                    leg_id,
                    RetainedEmission::on_trigger(
                        r.image().to_vec(),
                        e.destination.clone(),
                        call::Repeated::request(r.method().as_str()),
                    ),
                );
            }
            fx.outbound.push(e);
            // Retain the ACK branch so a retransmitted 2xx is re-ACKed on the
            // SAME transaction (§13.2.2.4 — `re-ack-retransmitted-2xx`).
            *call = call::helpers::retain_ack_branch(call.clone(), leg_id, &branch);
            // The ACK this dialog owed is on the wire, so the §13.2.2.4
            // obligation is discharged HERE — by the emission, whoever composed
            // it. A peer ACK for the same 2xx is then absorbed rather than
            // relayed on top of it (`relay_request`'s `owed` gate). A dialog
            // whose ACK nobody composes keeps the obligation and gets the peer's.
            *call = call::helpers::set_awaited_ack_cseq(call.clone(), leg_id, None);
        }
    }

    /// Relay an inbound SIP request to `target_leg`. Replicates the source's
    /// per-dialog CSeq bookkeeping (`relay_cseq_delta` — each dialog has its own
    /// sequence, RFC 3261 §12.2.1.1), the PRACK `RAck` CSeq rewrite (RFC 3262
    /// §7.2), and the pending-request snapshot used to correlate the eventual
    /// response (RFC 3261 §8.1.3.3).
    pub(super) fn relay_request(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        target_leg: &str,
        req: &sip_message::SipRequest,
        target_to_tag: Option<String>,
    ) {
        // ACK for 2xx: reuse the INVITE CSeq (no dialog-sequence advance,
        // §13.2.2.4) — delegate to the dedicated builder, carrying the inbound
        // ACK's body through (the delayed-offer re-INVITE answer rides the ACK,
        // RFC 3264 §4). The target may be either side (a re-INVITE answered by
        // bob is ACKed toward bob; one answered by alice is ACKed toward alice).
        //
        // Relayed ONLY while the target dialog holds an armed, undischarged ACK
        // obligation for this CSeq — a 2xx in hand that nothing has ACKed yet.
        // Anything else is absorbed: an ACK the target's own UAC core already
        // composed on receipt is not owed twice, an early dialog owes no ACK (a
        // §17.1.1.3 reject-ACK is hop-local, never relayed), and a surplus peer
        // ACK — fresh Via branch or repeated — must not re-ACK an acknowledged
        // 2xx. A repeated 2xx re-ACKs via `re-ack-retransmitted-2xx`, which never
        // consults this obligation.
        if req.method() == Method::Ack {
            let owed = leg_index(call, target_leg)
                .map(|i| leg_at(call, i))
                .and_then(|leg| leg.dialogs.first())
                .is_some_and(|d| d.ext.awaited_ack_cseq == Some(req.cseq().seq() as i64));
            if !owed {
                return;
            }
            let content_type = req.raw(HeaderName::ContentType).next().and_then(relay::media_type);
            self.ack_leg(call, fx, target_leg, req.body().to_vec(), content_type);
            return;
        }
        let Some(method) = in_dialog_method(req.method()) else {
            return;
        };
        let Some(t_idx) = leg_index(call, target_leg) else {
            return;
        };
        // Forking: pick the early dialog by its callee tag (RFC 3261 §12.2.1.1 —
        // each forked early dialog is independent); else the first/only dialog.
        let target_dialog = {
            let leg = leg_at(call, t_idx);
            let picked = target_to_tag
                .as_deref()
                .and_then(|tt| leg.dialogs.iter().find(|d| d.sip.remote_tag == tt))
                .or_else(|| leg.dialogs.first());
            match picked {
                Some(d) => d.clone(),
                None => return,
            }
        };
        // A tag-less (mid-confirm / early) target dialog cannot produce a
        // well-formed in-dialog request — skip rather than panic in `make_request`
        // and leak the dialog. An X11 takeover/reclaim can surface a replica
        // dialog captured before its confirming To-tag landed.
        if target_dialog.sip.remote_tag.is_empty() {
            return;
        }

        // ── Per-dialog CSeq (§12.2.1.1): outbound = target.localCSeq + delta,
        //    delta = relay_cseq_delta(inbound, sourceDialog.remoteCSeq). ──
        let inbound_cseq = req.cseq().seq() as i64;
        let source_leg_id = ctx.source_leg_id.to_string();
        let source_dialog = ctx.source_dialog().cloned();
        let source_remote_cseq = source_dialog.as_ref().and_then(|d| d.ext.remote_cseq);
        let delta = relay_cseq_delta(inbound_cseq, source_remote_cseq);
        let target_invite_cseq =
            invite_cseq_from_handle(&target_dialog).unwrap_or(target_dialog.sip.local_cseq);
        let outbound_cseq = target_dialog.sip.local_cseq + delta;

        // Advance the sequences: source learns the inbound CSeq; target bumps.
        if let Some(sd) = &source_dialog {
            let s_id = dialog_identity_tag(&source_leg_id, sd);
            *call = update_remote_cseq(call.clone(), &source_leg_id, &s_id, inbound_cseq);
        }
        let t_id = dialog_identity_tag(target_leg, &target_dialog);
        *call = bump_local_cseq(call.clone(), target_leg, &t_id, delta);

        // RFC 3262 §7.2: RAck names the reliable 1xx and the INVITE that drew it
        // *on the target leg* — the CSeq token becomes the target's INVITE CSeq
        // and the RSeq token translates back to what the target stated
        // (`b_rseq_for`). `relay-prack` already refused what names nothing this
        // stack showed (`call::helpers::unacknowledgeable_rack`); an untranslated
        // number reaching here is one that predicate could not disprove.
        let rack = if method == InDialogMethod::Prack {
            // The number the PRACKing party names is unique to the dialog it
            // was shown in, so the translation is keyed there — and the PRACK
            // names that dialog itself: its To-tag is the tag this stack showed
            // on its face, whichever leg the provisional came from.
            let a_tag = req.to().tag().unwrap_or_default().to_string();
            req.header::<RAck>().and_then(Result::ok).map(|r| {
                let a_rseq = i64::from(r.rseq());
                let translated =
                    call::helpers::b_rseq_for(call, &a_tag, a_rseq).map(|(_, b_rseq)| b_rseq);
                // A translated RAck names a provisional this stack really
                // showed, so the matching PRACK is now received: the
                // provisional retires (RFC 3262 §3) — its ladder the engine
                // already discharged before this rule ran — and a callee
                // repeat of it is absorbed by the relay instead of reaching
                // the caller.
                if translated.is_some() {
                    *call = call::helpers::retire_a_rseq(call.clone(), &a_tag, a_rseq);
                }
                let rseq = translated.map_or(r.rseq(), |b_rseq| b_rseq.max(0) as u32);
                RAck::new(rseq, target_invite_cseq.max(0) as u32, r.method().clone())
            })
        } else {
            None
        };
        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&target_dialog.sip);
        let target_face = capabilities::Face::of_leg(target_leg);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(
                self.config,
                &call.call_ref,
                target_leg,
                call.emergency == Some(true),
                branch.clone(),
            )),
            contact: Some(relay::leg_contact(
                self.config,
                &call.call_ref,
                target_leg,
                call.emergency == Some(true),
            )),
            body: req.body().to_vec(),
            content_type: req.raw(HeaderName::ContentType).next().and_then(relay::media_type),
            cseq: Some(outbound_cseq as u32),
            // §16.6 step 3: the relayed request continues the sender's hop
            // budget. The stack's OWN in-dialog requests (teardown BYE,
            // keepalive, a re-offer it authors) state the §8.1.1.6 default —
            // only what crosses the back-to-back UA inherits a count.
            max_forwards: Some(hops::forwarded_max_forwards(req).value()),
            extra_headers: relay::relay_request_passthrough_headers(
                req,
                &capabilities::declared_advert_headers(call.features.as_ref(), target_face),
            ),
            rack,
            // The peer's own advertisement rides in `extra_headers`; this is
            // the DECLARED set for the face, else nothing of the stack's own.
            capabilities: Some(capabilities::advertised(call, target_face)),
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(method, &gen_dialog, &opts);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) = relay::apply_b_leg_egress(
            self.config,
            target_leg,
            &gen_dialog.route_set,
            res.request,
            dest,
        );
        let kind =
            if method == InDialogMethod::Invite { TxnKind::Invite } else { TxnKind::NonInvite };

        // For a re-INVITE, cache its client-transaction handle on the *target*
        // dialog so the eventual ACK-for-2xx echoes the re-INVITE CSeq
        // (RFC 3261 §13.2.2.4) and CANCEL can reuse the branch (§9.1). (The
        // initial INVITE never reaches this path — it is built by
        // `CreateLeg`/`build_b_leg`.)
        if method == InDialogMethod::Invite {
            *call = call::helpers::update_dialog(call.clone(), target_leg, &t_id, |d| {
                d.ext.pending_invite_txn = Some(call::InviteTxnHandle {
                    branch: branch.clone(),
                    original_invite: out_req.image().to_vec(),
                    destination: call::HostPort { host: dest.0.clone(), port: dest.1 },
                });
                // New INVITE transaction → drop the prior ACK branch and its
                // retained datagram (§13.2.2.4); this re-INVITE's 2xx ACK mints
                // its own, retained on first ACK — a copy of that 2xx must
                // never be re-ACKed with the PREVIOUS transaction's answer.
                // Any stale relay-ACK obligation dies with it — this 2xx arms
                // its own (`relay_response`).
                d.ext.ack_branch = None;
                d.ext.emitted_ack = None;
                d.ext.awaited_ack_cseq = None;
            });
        }

        // Snapshot the inbound request so the response can echo its Via/From/To/
        // Call-ID/CSeq (§8.1.3.3) and so glare detection on the target dialog
        // sees the in-flight re-INVITE (`reinvite-glare`). The B2BUA answers BYE
        // locally and ACK has no response, so neither needs correlation.
        if !matches!(method, InDialogMethod::Bye) {
            let pending = PendingRequest {
                method: req.method().to_string(),
                outbound_cseq,
                inbound_cseq,
                source_vias: req.raw(HeaderName::Via).map(str::to_string).collect(),
                source_call_id: req.call_id().as_str().to_string(),
                source_from: req.raw(HeaderName::From).next().unwrap_or_default().to_string(),
                source_to: req.raw(HeaderName::To).next().unwrap_or_default().to_string(),
                source_timestamp: req.raw(HeaderName::Timestamp).next().map(str::to_string),
                direction: ctx.direction,
                cancelled: false,
                offered_100rel: req.offers_100rel(),
            };
            *call = add_pending_request(call.clone(), target_leg, &t_id, pending);
        }

        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(kind),
            destination: dest,
            label: format!("relay {} → {target_leg}", req.method()),
            leg_id: Some(target_leg.to_string()),
        });
    }
}
