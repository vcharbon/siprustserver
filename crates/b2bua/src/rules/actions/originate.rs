//! Requests the B2BUA originates itself: the new-b-leg INVITE (`CreateLeg`,
//! behind the target-admission gate), the resync re-INVITE, NOTIFY, PRACK and
//! the generic in-dialog request (`SendRequestToLeg`). Relaying an *inbound*
//! request does NOT live here — see [`super::relay_request`].

use call::helpers::{add_b_leg, add_cdr_event, bump_local_cseq};
use call::{Call, CdrEvent, LegKind, TimerType};
use sip_message::generators::{
    self, GenerateInDialogRequestOpts, InDialogMethod,
};
use sip_message::header::{Event, HeaderValue, RAck, SubscriptionState};
use sip_message::{Method, SipStr};
use sip_txn::TxnKind;

use crate::effects::{HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode};
use crate::rules::model::RuleContext;
use crate::rules::relay;

use super::select::{dialog_identity_tag, in_dialog_method, leg_at, leg_index};
use super::teardown::terminate_all;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Build and send a new b-leg INVITE toward `destination`
    /// ([`crate::rules::model::RuleAction::CreateLeg`]). Admission gate — same
    /// policy as `apply_route`: a rule-driven destination that doesn't pass the
    /// suffix allow-list is a config bug; surface it as a terminate so the call
    /// doesn't hang waiting for an answer that will never come. No leg /
    /// outbound is built; the call is torn down and a `Reject` CDR records the
    /// cause.
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
        body_override: Option<&[u8]>,
        header_updates: &[(String, Option<String>)],
        kind: Option<LegKind>,
    ) {
        if crate::target_admission::classify_admission(
            &destination.0,
            &self.config.worker_allowed_target_suffixes,
        ) == crate::target_admission::AdmissionVerdict::Reject
        {
            *call = add_cdr_event(
                call.clone(),
                CdrEvent {
                    event_type: call::CdrEventType::Reject,
                    timestamp: self.now_ms,
                    leg_id: ctx.source_leg_id.to_string(),
                    status_code: Some(503),
                    reason: Some(format!(
                        "admission_reject host={}",
                        destination.0
                    )),
                },
            );
            terminate_all(call);
            return;
        }
        let n = call.b_legs.len() + 1;
        let leg_id = format!("b-{n}");
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        // Same refusal as the admission reject above, for the other way a
        // decision can name no destination: an address field that does not read
        // (055). The leg is not created and no INVITE goes out — originating on
        // a fabricated target would dial an address the decision never stated.
        let (leg, effect) = match relay::build_b_leg(
            &call.call_ref,
            &leg_id,
            call.emergency == Some(true),
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
            kind,
        ) {
            Ok(built) => built,
            Err(err) => {
                eprintln!(
                    "WARN: call {}: leg {leg_id} not created — {}",
                    call.call_ref,
                    err.detail()
                );
                *call = add_cdr_event(
                    call.clone(),
                    CdrEvent {
                        event_type: call::CdrEventType::Reject,
                        timestamp: self.now_ms,
                        leg_id: ctx.source_leg_id.to_string(),
                        status_code: Some(500),
                        reason: Some(format!("unreadable_address field={}", err.field)),
                    },
                );
                terminate_all(call);
                return;
            }
        };
        if let Some(ctx_str) = callback_context {
            call.callback_context = Some(ctx_str.to_string());
        }
        *call = add_b_leg(call.clone(), leg);
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
        let dialog = match leg_at(call, idx)
            .dialogs
            .iter()
            .find(|d| !d.sip.remote_tag.is_empty())
        {
            Some(d) => d.clone(),
            None => return,
        };
        let t_id = dialog_identity_tag(leg_id, &dialog);
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, call.emergency == Some(true), branch)),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id, call.emergency == Some(true))),
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
        let res = generators::generate_in_dialog_request(InDialogMethod::Notify, &gen_dialog, &opts);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &gen_dialog.route_set, res.request, dest);
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("NOTIFY → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
    }

    /// Originate a re-INVITE on `leg_id` carrying `body` as the new offer plus
    /// `add_headers` (Allow/Supported). CSeq = dialog.localCSeq + 1. Used by
    /// `promote18xPemTo200` to resync Alice when bob's final SDP differs from the
    /// early-media SDP promoted into the synthetic 200 OK. The response comes back
    /// classified from-a (the B2BUA's stamped Via cr/lg) and is claimed by the
    /// `promote-resync-reinvite-response` rule.
    pub(super) fn send_reinvite(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        body: &[u8],
        add_headers: &[sip_message::draft::Entry],
    ) {
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        let dialog = match leg_at(call, idx)
            .dialogs
            .iter()
            .find(|d| !d.sip.remote_tag.is_empty())
        {
            Some(d) => d.clone(),
            None => return,
        };
        let t_id = dialog_identity_tag(leg_id, &dialog);
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let extra: Vec<sip_message::SipHeader> = add_headers
            .iter()
            .map(|e| sip_message::SipHeader {
                name: sip_message::SipStr::owned(e.name().as_wire_str()),
                value: e.text(),
            })
            .collect();
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, call.emergency == Some(true), branch.clone())),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id, call.emergency == Some(true))),
            body: body.to_vec(),
            content_type: (!body.is_empty()).then(relay::sdp),
            cseq: Some(outbound_cseq as u32),
            extra_headers: extra,
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Invite, &gen_dialog, &opts);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &gen_dialog.route_set, res.request, dest);

        // Cache the re-INVITE's client-transaction handle so the ACK-for-2xx
        // echoes its CSeq (§13.2.2.4). Reset the retained ACK branch: this new
        // INVITE transaction's 2xx will mint its own (the old branch belonged to
        // the prior CSeq).
        *call = call::helpers::update_dialog(call.clone(), leg_id, &t_id, |d| {
            d.ext.pending_invite_txn = Some(call::InviteTxnHandle {
                branch: branch.clone(),
                original_invite: out_req.image().to_vec(),
                destination: call::HostPort { host: dest.0.clone(), port: dest.1 },
            });
            d.ext.ack_branch = None;
        });

        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::Invite),
            destination: dest,
            label: format!("resync re-INVITE → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
    }

    /// Originate an in-dialog request on `leg_id`'s confirmed dialog
    /// ([`crate::rules::model::RuleAction::SendRequestToLeg`]): keepalive
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
        let dialog = match leg_at(call, idx)
            .dialogs
            .iter()
            .find(|d| !d.sip.remote_tag.is_empty())
        {
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
                    eprintln!(
                        "B2BUA INVARIANT VIOLATION: call_ref={} leg={} is Confirmed but has \
                         no dialog with a remote tag ({} dialog(s), all tag-less) — cannot \
                         originate in-dialog {} (keepalive will never fire for this leg). \
                         A tag-less INVITE is rejected at ingest, so an established dialog \
                         must never reach this state; the call context preserves the empty \
                         tag across hydration, leaving this leg permanently un-probeable.",
                        call.call_ref,
                        leg_id,
                        leg.dialogs.len(),
                        method,
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
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        // Opaque body carrier (MSCML INFO rides here): default the content type
        // to `application/sdp` when a body is present and none was given.
        let content_type =
            content_type.and_then(relay::media_type).or_else(|| (!body.is_empty()).then(relay::sdp));
        // Forward the service-nominated application headers verbatim (e.g. a held
        // `User-To-User` re-emitted toward the peer on a deferred INFO_UUI relay).
        // Body-owned headers are dropped: `body`/`content_type` own
        // Content-Type/Content-Length via `append_body_headers`, so listing them
        // here would duplicate them.
        let extra_headers: Vec<sip_message::SipHeader> = headers
            .iter()
            .filter(|(n, _)| {
                let named = sip_message::HeaderName::from(n.as_str());
                named != sip_message::HeaderName::ContentType
                    && named != sip_message::HeaderName::ContentLength
            })
            .map(|(name, value)| sip_message::SipHeader { name: name.clone().into(), value: value.clone().into() })
            .collect();
        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, call.emergency == Some(true), branch)),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id, call.emergency == Some(true))),
            cseq: Some(outbound_cseq as u32),
            body: body.to_vec(),
            content_type,
            extra_headers,
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(m, &gen_dialog, &opts);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &gen_dialog.route_set, res.request, dest);
        let kind = if m == InDialogMethod::Invite { TxnKind::Invite } else { TxnKind::NonInvite };
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(kind),
            destination: dest,
            label: format!("{method} → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
    }

    /// Originate a PRACK toward the b-leg early dialog (selected by callee tag)
    /// acknowledging a reliable 1xx (RFC 3262 §4). The RAck is
    /// `<rseq> <invite_cseq> INVITE`; the dialog's local CSeq advances by one.
    /// Used by `relayFirst18xTo180` (B2BUA PRACKs bob since alice never saw the
    /// reliable provisional).
    pub(super) fn send_prack_to_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        rseq: i64,
        invite_cseq: i64,
        b_tag: &str,
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
        let t_id = dialog_identity_tag(leg_id, &dialog);
        // Per-dialog CSeq (§12.2.1.1): each forked early dialog has its OWN
        // sequence, so its first PRACK is the INVITE CSeq + 1 independent of the
        // other forks — two forks' PRACKs at the same CSeq is correct (distinct
        // dialogs, distinct To-tags), not a collision.
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, call.emergency == Some(true), branch)),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id, call.emergency == Some(true))),
            rack: Some(RAck::new(
                rseq.max(0) as u32,
                invite_cseq.max(0) as u32,
                Method::Invite,
            )),
            cseq: Some(outbound_cseq as u32),
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Prack, &gen_dialog, &opts);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &gen_dialog.route_set, res.request, dest);
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("PRACK → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
    }
}
