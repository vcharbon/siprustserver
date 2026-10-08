//! Requests the B2BUA originates itself: the new-b-leg INVITE (`CreateLeg`,
//! behind the destination allow-list), the resync re-INVITE, NOTIFY, PRACK and
//! the generic in-dialog request (`SendRequestToLeg`). Relaying an *inbound*
//! request does NOT live here — see [`super::relay_request`].

use call::helpers::{add_cdr_event, add_originated_b_leg, bump_local_cseq};
use call::{Call, CdrEvent, LegKind, TerminationCause, TimerType};
use sip_message::generators::{self, GenerateInDialogRequestOpts, InDialogMethod};
use sip_message::header::HeaderName;
use sip_message::header::{Event, HeaderValue, RAck, SubscriptionState};
use sip_message::{Method, SipStr};
use sip_txn::TxnKind;

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::capabilities;
use crate::rules::relay;
use b2bua_sdk::model::{Body, RuleContext};

use super::select::{dialog_identity_tag, in_dialog_method, leg_at, leg_index};
use super::teardown::terminate_all;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Build and send a new b-leg INVITE toward `destination`
    /// ([`b2bua_sdk::model::RuleAction::CreateLeg`]). Admission gate — same
    /// policy as `apply_route`: a rule-driven destination that doesn't pass the
    /// suffix allow-list is a config bug; surface it as a terminate so the call
    /// doesn't hang waiting for an answer that will never come. No leg /
    /// outbound is built; the call is torn down and a `Reject` CDR records the
    /// cause. An admitted leg is recorded with its `InviteSent` as its INVITE
    /// leaves, under the decision current at this turn.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn create_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        destination: &(String, u16),
        new_ruri: Option<&str>,
        new_from: Option<&str>,
        new_to: Option<&str>,
        no_answer_timeout_sec: Option<i64>,
        callback_context: Option<&str>,
        body_override: Option<&Body>,
        header_updates: &[(String, Option<String>)],
        header_adds: &[(String, Vec<String>)],
        kind: Option<LegKind>,
    ) {
        if crate::destination_allowlist::classify_admission(
            &destination.0,
            &self.config.worker_allowed_target_suffixes,
        ) == crate::destination_allowlist::AdmissionVerdict::Reject
        {
            *call = add_cdr_event(
                call.clone(),
                CdrEvent {
                    event_type: call::CdrEventType::Reject,
                    timestamp: self.now_ms,
                    leg_id: ctx.source_leg_id.to_string(),
                    status_code: Some(503),
                    reason: Some(format!("admission_reject host={}", destination.0)),
                    decision_ordinal: 0,
                },
            );
            terminate_all(call, self.now_ms, TerminationCause::Admission, None);
            return;
        }
        let n = call.b_legs.len() + 1;
        let leg_id = format!("b-{n}");
        // A rule-supplied ring deadline above `bound − margin` is held under
        // the configured transaction bound so the CANCEL→487 exchange
        // completes inside the live b-leg client transaction. After the
        // admission gate: a rejected leg is never armed, so it takes no
        // clamp note either.
        let no_answer_timeout_sec = no_answer_timeout_sec
            .map(|secs| relay::clamp_no_answer(self.config, &call.call_ref, secs));
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let offers_sdp = relay::mints_offer(&a_invite, body_override);
        // The originator's `Accept` states what she takes; a media leg answers
        // the service that dialled it, so that line stays behind unless an
        // update states one.
        let mut header_updates = header_updates.to_vec();
        if kind == Some(LegKind::Media)
            && !header_updates.iter().any(|(n, _)| HeaderName::Accept.matches(n))
        {
            header_updates.push((HeaderName::Accept.as_wire_str().to_string(), None));
        }
        let header_updates = header_updates.as_slice();
        let advertised = capabilities::relaying_for_leg(call, &leg_id, a_invite.headers());
        // Same refusal as the admission reject above, for the other way a
        // decision can name no destination: an address field that does not read.
        // The leg is not created and no INVITE goes out — originating on
        // a fabricated target would dial an address the decision never stated.
        let (mut leg, mut effect) = match relay::build_b_leg(
            relay::CallMarks::of(call),
            &leg_id,
            &a_invite,
            destination.clone(),
            new_ruri,
            new_from,
            new_to,
            no_answer_timeout_sec,
            self.config,
            self.id_gen,
            body_override,
            header_updates,
            &advertised,
            crate::rules::charging::minting_arm(call, &leg_id, kind),
            &capabilities::withheld_option_tags(call, kind, offers_sdp),
            &capabilities::offered_option_tags(call, kind),
            kind,
            call.a_leg.invite_final_sent.is_none(),
            self.now_ms,
        ) {
            Ok(built) => built,
            Err(err) => {
                tracing::warn!(
                    call_ref = %call.call_ref,
                    %leg_id,
                    detail = %err.detail(),
                    "leg not created"
                );
                *call = add_cdr_event(
                    call.clone(),
                    CdrEvent {
                        event_type: call::CdrEventType::Reject,
                        timestamp: self.now_ms,
                        leg_id: ctx.source_leg_id.to_string(),
                        status_code: Some(500),
                        reason: Some(format!("unreadable_address field={}", err.field)),
                        decision_ordinal: 0,
                    },
                );
                terminate_all(call, self.now_ms, TerminationCause::Admission, None);
                return;
            }
        };
        crate::rules::charging::uncharge_media_leg(
            call,
            kind,
            header_updates,
            &mut leg,
            &mut effect,
        );
        crate::rules::stated_headers::add_to_minted(&mut leg, &mut effect, header_adds);
        let author = match body_override {
            Some(body) => relay::Author::from(&body.author),
            None => relay::Author::Leg(&call.a_leg.leg_id),
        };
        leg.sdp_session = relay::opened(&effect, author);
        if let Some(ctx_str) = callback_context {
            call.callback_context = Some(ctx_str.to_string());
        }
        *call = add_originated_b_leg(call.clone(), leg, self.now_ms);
        fx.outbound.push(effect);
        if let Some(secs) = no_answer_timeout_sec {
            self.schedule(call, fx, TimerType::NoAnswer, secs * 1000, Some(leg_id));
        }
    }

    /// Originate a NOTIFY on `leg_id`'s confirmed dialog (toward the referrer)
    /// carrying the REFER implicit-subscription state (RFC 3515 §2.4.4): `Event:
    /// refer`, `Subscription-State`, and a `message/sipfrag` body. The B2BUA is
    /// the UAS of the referrer leg, so the NOTIFY rides that dialog's local
    /// sequence.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_notify(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        event: &str,
        subscription_state: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) {
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        let dialog = match leg_at(call, idx).dialogs.iter().find(|d| !d.sip.remote_tag.is_empty()) {
            Some(d) => d.clone(),
            None => return,
        };
        let t_id = dialog_identity_tag(leg_id, &dialog);
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, relay::CallMarks::of(call), leg_id, branch)),
            contact: Some(relay::leg_contact(self.config, relay::CallMarks::of(call), leg_id)),
            body: body.to_vec(),
            content_type: content_type.and_then(relay::media_type),
            cseq: Some(outbound_cseq as u32),
            // A value the reader rejects still reaches the peer as the policy
            // stated it — this stack does not invent an event package.
            event: Some(
                Event::parse(&SipStr::owned(event))
                    .unwrap_or_else(|_| Event::new(SipStr::owned(event))),
            ),
            subscription_state: Some(
                SubscriptionState::parse(&SipStr::owned(subscription_state))
                    .unwrap_or_else(|_| SubscriptionState::new(SipStr::owned(subscription_state))),
            ),
            ..Default::default()
        };
        let res =
            generators::generate_in_dialog_request(InDialogMethod::Notify, &gen_dialog, &opts);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) = relay::apply_b_leg_egress(
            self.config,
            leg_id,
            &gen_dialog.route_set,
            res.request,
            dest,
        );
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("NOTIFY → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance: Provenance::Authored,
        });
    }

    /// Originate a re-INVITE on `leg_id` carrying `body` as the new offer, typed
    /// `content_type` (`None`: SDP) and described by `descriptors`, plus
    /// `add_headers` (Allow/Supported), the leg's capability set
    /// ([`capabilities::for_reinvite`]) filling what they leave unstated.
    /// CSeq = dialog.localCSeq + 1. Used by
    /// `promote18xPemTo200` to resync Alice when bob's final SDP differs from the
    /// early-media SDP promoted into the synthetic 200 OK. The response comes back
    /// classified from-a (the B2BUA's stamped Via cr/lg) and is claimed by the
    /// `promote-resync-reinvite-response` rule.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_reinvite(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        body: &[u8],
        content_type: Option<&str>,
        descriptors: &[sip_message::SipHeader],
        add_headers: &[sip_message::draft::Entry],
        author: relay::Author<'_>,
    ) {
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        let dialog = match leg_at(call, idx).dialogs.iter().find(|d| !d.sip.remote_tag.is_empty()) {
            Some(d) => d.clone(),
            None => return,
        };
        let t_id = dialog_identity_tag(leg_id, &dialog);
        // The INVITE that created this leg's dialog: the originator's own on
        // hers, the one the stack sent on a leg it dialled.
        let creating = if leg_id == call.a_leg.leg_id {
            Some(relay::rebuild_a_leg_invite(&call.a_leg_invite))
        } else {
            relay::dialling_invite(leg_at(call, idx))
        };
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let mut extra: Vec<sip_message::SipHeader> = add_headers
            .iter()
            .map(|e| sip_message::SipHeader {
                name: sip_message::SipStr::owned(e.name().as_wire_str()),
                value: e.text(),
            })
            .collect();
        // RFC 7315 §5.6: the arm's vector for the leg, unless the action states one.
        let vector = sip_message::header::ChargingVector::header_name();
        if !extra.iter().any(|h| vector.matches(&h.name)) {
            if let Some(line) = crate::rules::charging::in_dialog_invite_vector(call, leg_id) {
                extra.push(sip_message::SipHeader {
                    name: sip_message::SipStr::owned(vector.as_wire_str()),
                    value: sip_message::SipStr::owned(&line),
                });
            }
        }
        relay::describe_body(&mut extra, body, descriptors);
        let content_type = (!body.is_empty())
            .then(|| content_type.and_then(relay::media_type).unwrap_or_else(relay::sdp));
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(
                self.config,
                relay::CallMarks::of(call),
                leg_id,
                branch.clone(),
            )),
            contact: Some(relay::leg_contact(self.config, relay::CallMarks::of(call), leg_id)),
            body: relay::continue_on_leg(
                call,
                leg_id,
                Some(&dialog.sip.remote_tag),
                author,
                relay::Carried::InDialog,
                body.to_vec(),
                content_type.as_ref(),
                self.config.sdp_form.as_ref(),
            ),
            content_type,
            cseq: Some(outbound_cseq as u32),
            extra_headers: extra,
            capabilities: Some(capabilities::for_reinvite(call, leg_id, creating.as_ref())),
            ..Default::default()
        };
        let res =
            generators::generate_in_dialog_request(InDialogMethod::Invite, &gen_dialog, &opts);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) = relay::apply_b_leg_egress(
            self.config,
            leg_id,
            &gen_dialog.route_set,
            res.request,
            dest,
        );

        // Cache the re-INVITE's client-transaction handle so the ACK-for-2xx
        // echoes its CSeq (§13.2.2.4). Reset the retained ACK branch, its
        // retained datagram, and any armed ACK obligation: all belonged to the
        // prior CSeq — this new INVITE transaction's 2xx mints/arms its own.
        *call = call::helpers::update_dialog(call.clone(), leg_id, &t_id, |d| {
            d.ext.pending_invite_txn = Some(call::InviteTxnHandle {
                branch: branch.clone(),
                original_invite: out_req.image().to_vec(),
                destination: call::HostPort { host: dest.0.clone(), port: dest.1 },
            });
            d.ext.ack_branch = None;
            d.ext.emitted_ack = None;
            d.ext.awaited_ack_cseq = None;
        });

        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::Invite),
            destination: dest,
            label: format!("resync re-INVITE → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance: Provenance::Authored,
        });
    }

    /// Originate an in-dialog request on `leg_id`'s confirmed dialog
    /// ([`b2bua_sdk::model::RuleAction::SendRequestToLeg`]): keepalive
    /// OPTIONS, opaque-body INFO (MSCML), deferred-relay re-emission.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_request_to_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        method: &str,
        body: &[u8],
        content_type: Option<&str>,
        headers: &[(String, String)],
        descriptors: &[sip_message::SipHeader],
        author: relay::Author<'_>,
    ) {
        let m = match in_dialog_method(&Method::from_wire(method)) {
            Some(m) => m,
            None => return,
        };
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        // Probe the CONFIRMED dialog (non-empty remote tag). A leg can hold an
        // early/forked dialog with no remote tag as its first entry, and a
        // failover takeover/reclaim (ADR-0011 X11) can materialise a replica
        // dialog captured mid-confirm; building an in-dialog `To` from an empty
        // remote tag yields a tag-less header that panics in `make_request` and
        // leaks the dialog. Skip when no dialog is confirmed (nothing to probe).
        let dialog = match leg_at(call, idx).dialogs.iter().find(|d| !d.sip.remote_tag.is_empty()) {
            Some(d) => d.clone(),
            None => {
                // An early/mid-confirm leg legitimately has no confirmed dialog
                // yet — skip quietly. A CONFIRMED leg with only tag-less dialogs
                // is a broken invariant (an established dialog always knows its
                // peer's tag; a tag-less INVITE is rejected at ingest) that
                // silently drops every in-dialog request to this leg — never
                // swallow it.
                let leg = leg_at(call, idx);
                if leg.state == call::LegState::Confirmed {
                    tracing::error!(
                        call_ref = %call.call_ref,
                        %leg_id,
                        dialogs = leg.dialogs.len(),
                        %method,
                        "INVARIANT VIOLATION: leg is Confirmed but has no dialog with a remote \
                         tag (all tag-less) — cannot originate the in-dialog request, so \
                         keepalive will never fire and this leg is permanently un-probeable"
                    );
                }
                return;
            }
        };
        // Advance + persist the dialog CSeq by exactly one (§12.2.1.1), exactly as
        // every other in-dialog originator here. Without this the in-dialog
        // keepalive OPTIONS re-derives the same CSeq every cycle and the later
        // relayed BYE collides on it — an RFC 3261 violation a real UAS rejects.
        let t_id = dialog_identity_tag(leg_id, &dialog);
        // `KeepaliveCseqReuse` (the wire-fault seam) emits the keepalive at the
        // CSeq the dialog already used and leaves the dialog un-advanced: the
        // exact defect `cseq-in-dialog-order` gates on, on purpose.
        let reuse = m == InDialogMethod::Options
            && self.wire_faults.is_armed(crate::wire_faults::WireFaultPoint::KeepaliveCseqReuse);
        let outbound_cseq = if reuse { dialog.sip.local_cseq } else { dialog.sip.local_cseq + 1 };
        if !reuse {
            *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);
        }

        // Opaque body carrier (MSCML INFO rides here): default the content type
        // to `application/sdp` when a body is present and none was given.
        let content_type = content_type
            .and_then(relay::media_type)
            .or_else(|| (!body.is_empty()).then(relay::sdp));
        // Forward the service-nominated application headers verbatim (e.g. a held
        // `User-To-User` re-emitted toward the peer on a deferred User-to-User relay).
        // Body-owned headers are dropped: `body`/`content_type` own
        // Content-Type/Content-Length via `append_body_headers`, so listing them
        // here would duplicate them.
        let mut extra_headers: Vec<sip_message::SipHeader> = headers
            .iter()
            .filter(|(n, _)| {
                let named = sip_message::HeaderName::from(n.as_str());
                named != sip_message::HeaderName::ContentType
                    && named != sip_message::HeaderName::ContentLength
            })
            .map(|(name, value)| sip_message::SipHeader {
                name: name.clone().into(),
                value: value.clone().into(),
            })
            .collect();
        relay::describe_body(&mut extra_headers, body, descriptors);
        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, relay::CallMarks::of(call), leg_id, branch)),
            contact: Some(relay::leg_contact(self.config, relay::CallMarks::of(call), leg_id)),
            cseq: Some(outbound_cseq as u32),
            body: relay::continue_on_leg(
                call,
                leg_id,
                Some(&dialog.sip.remote_tag),
                author,
                relay::Carried::of(&Method::from_wire(method), None, true),
                body.to_vec(),
                content_type.as_ref(),
                self.config.sdp_form.as_ref(),
            ),
            content_type,
            extra_headers,
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(m, &gen_dialog, &opts);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) = relay::apply_b_leg_egress(
            self.config,
            leg_id,
            &gen_dialog.route_set,
            res.request,
            dest,
        );
        let kind = if m == InDialogMethod::Invite { TxnKind::Invite } else { TxnKind::NonInvite };
        // The one in-dialog OPTIONS this stack originates is the keepalive.
        let provenance =
            if m == InDialogMethod::Options { Provenance::Probe } else { Provenance::Authored };
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(kind),
            destination: dest,
            label: format!("{method} → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance,
        });
    }

    /// Originate a PRACK toward the responder's early dialog (selected by its
    /// tag) acknowledging a reliable 1xx (RFC 3262 §4). The RAck is
    /// `<rseq> <invite_cseq> INVITE`; the dialog's local CSeq advances by one.
    /// This stack PRACKs itself wherever the originator never saw the reliable
    /// provisional (a masking policy, no `100rel` offered, a provisional
    /// crossing this stack's CANCEL) or will not acknowledge it any more (its
    /// INVITE CANCELled here). Once per provisional: the acknowledgement and
    /// its client branch are recorded, and a repeat of it — the responder's
    /// §3 retransmission — sends nothing (§4), so no second PRACK names the
    /// same `RAck` on a fresh CSeq. `answer`: the session description the
    /// PRACK carries, answering an offer the provisional carried (RFC 3262 §5).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_prack_to_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        rseq: i64,
        invite_cseq: i64,
        b_tag: &str,
        responder_sdp: bool,
        answer: Option<Vec<u8>>,
    ) {
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        // Pick the early dialog STRICTLY by callee tag (forking → independent
        // dialogs). No first-dialog fallback: falling back would stamp another
        // fork's To-tag on this fork's RAck (a PRACK the callee rejects with
        // 481, and the fork keeps retransmitting its reliable 1xx). The
        // `ensure_b_early_dialog` seam registers the dialog before this runs.
        let dialog = {
            let leg = leg_at(call, idx);
            match leg.dialogs.iter().find(|d| d.sip.remote_tag == b_tag) {
                Some(d) => d.clone(),
                None => return,
            }
        };
        // No confirmed remote tag (an early/mid-confirm dialog, e.g. a takeover
        // replica captured before its provisional To-tag landed) → skip rather
        // than build a tag-less in-dialog PRACK and panic in `make_request`.
        if dialog.sip.remote_tag.is_empty() {
            return;
        }
        let (updated, first) = call::helpers::record_pracked_provisional(
            call.clone(),
            leg_id,
            b_tag,
            invite_cseq,
            rseq,
            responder_sdp,
        );
        *call = updated;
        if !first {
            return;
        }
        let t_id = dialog_identity_tag(leg_id, &dialog);
        // Per-dialog CSeq (§12.2.1.1): each forked early dialog has its OWN
        // sequence, so its first PRACK is the INVITE CSeq + 1 independent of the
        // other forks — two forks' PRACKs at the same CSeq is correct (distinct
        // dialogs, distinct To-tags), not a collision.
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        *call = call::helpers::note_own_prack_branch(
            call.clone(),
            leg_id,
            b_tag,
            invite_cseq,
            rseq,
            &branch,
        );
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, relay::CallMarks::of(call), leg_id, branch)),
            contact: Some(relay::leg_contact(self.config, relay::CallMarks::of(call), leg_id)),
            rack: Some(RAck::new(rseq.max(0) as u32, invite_cseq.max(0) as u32, Method::Invite)),
            cseq: Some(outbound_cseq as u32),
            content_type: answer.is_some().then(relay::sdp),
            body: answer.unwrap_or_default(),
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Prack, &gen_dialog, &opts);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) = relay::apply_b_leg_egress(
            self.config,
            leg_id,
            &gen_dialog.route_set,
            res.request,
            dest,
        );
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("PRACK → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance: Provenance::Authored,
        });
    }

    /// As the INVITE final `resp` arrives from `leg_id`, PRACKs each relayed
    /// reliable provisional of that INVITE still unacknowledged, on its own
    /// early dialog (RFC 3262 §4), an offer it carried answered rejecting every
    /// stream — except an offer in the dialog a 2xx confirms, whose PRACK
    /// carries the caller's answer at her ACK (RFC 3264).
    pub fn prack_owed_at_final(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        resp: &sip_message::SipResponse,
    ) {
        if resp.status() < 200 || resp.cseq().method() != Method::Invite {
            return;
        }
        let confirmed = (200..300).contains(&resp.status()).then(|| resp.to().tag()).flatten();
        let invite_cseq = i64::from(resp.cseq().seq());
        let owed =
            call::helpers::unacknowledged_relayed_provisionals(call, leg_id, Some(invite_cseq));
        let deferred = |o: &call::helpers::OwedPrack| {
            o.offer.is_some() && confirmed.is_some_and(|tag| tag == o.b_tag)
        };
        for o in owed.into_iter().filter(|o| !deferred(o)) {
            let answer = o.offer.as_deref().and_then(|offer| self.rejecting_answer(offer));
            self.send_prack_to_leg(
                call,
                fx,
                leg_id,
                o.rseq,
                o.invite_cseq,
                &o.b_tag,
                o.responder_sdp,
                answer,
            );
        }
    }

    /// PRACKs `owed` — the provisionals whose offer the 2xx left to the caller's
    /// ACK ([`crate::rules::relay::offers_owed_at_ack`]) — on `leg_id`, each
    /// offer answered with `answer`, or rejecting every stream where the ACK
    /// gives none (RFC 3262 §5, RFC 3264 §6).
    pub(super) fn prack_offers_owed_at_ack(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        owed: Vec<call::helpers::OwedPrack>,
        answer: Option<Vec<u8>>,
    ) {
        for o in owed {
            let Some(offer) = o.offer.as_deref() else { continue };
            let body = answer.clone().or_else(|| self.rejecting_answer(offer));
            self.send_prack_to_leg(
                call,
                fx,
                leg_id,
                o.rseq,
                o.invite_cseq,
                &o.b_tag,
                o.responder_sdp,
                body,
            );
        }
    }

    /// PRACK every reliable provisional `leg_id`'s responder sent on the INVITE
    /// `invite_cseq` that was relayed and is still unacknowledged, as this
    /// stack ends that INVITE with a CANCEL: the party it was shown to will
    /// not PRACK it any more, and the CANCEL does not end the transaction, so
    /// this stack — the leg's UAC — owes the acknowledgement (RFC 3262 §4),
    /// answering an offer one carried rejecting every stream (§5). `None`
    /// reads every INVITE of the leg (a leg still ringing has one).
    pub(super) fn prack_relayed_unacknowledged(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        invite_cseq: Option<i64>,
    ) {
        let owed = call::helpers::unacknowledged_relayed_provisionals(call, leg_id, invite_cseq);
        for o in owed {
            let answer = o.offer.as_deref().and_then(|offer| self.rejecting_answer(offer));
            self.send_prack_to_leg(
                call,
                fx,
                leg_id,
                o.rseq,
                o.invite_cseq,
                &o.b_tag,
                o.responder_sdp,
                answer,
            );
        }
    }

    /// The answer to `offer` rejecting every stream (RFC 3264 §6, port 0) —
    /// the least a dialog about to end, or a party that gave no answer, can
    /// commit to.
    pub(super) fn rejecting_answer(&self, offer: &[u8]) -> Option<Vec<u8>> {
        sip_message::sdp_answer::reject_offer(
            offer,
            &sip_message::BuildHeldSdpOptions {
                local_ip: self.config.sip_local_ip.clone(),
                now_ms: self.now_ms,
            },
        )
    }
}
