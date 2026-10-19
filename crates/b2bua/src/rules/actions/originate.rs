//! Requests the B2BUA originates itself in a leg's dialog: the resync
//! re-INVITE, NOTIFY and the generic in-dialog request (`SendRequestToLeg`);
//! the PRACKs it owes are [`super::prack`]'s. The new leg's INVITE is [`super::create_leg`]'s;
//! relaying an *inbound* request is [`super::relay_request`]'s.

use call::helpers::bump_our_cseq;
use call::TxnKind;
use call::{Call, DialogId, HeldRequest};
use sip_message::generators::{self, GenerateInDialogRequestOpts, InDialogMethod};
use sip_message::header::{Event, HeaderValue, SubscriptionState};
use sip_message::{Method, SipStr};

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::capabilities;
use crate::rules::relay;

use super::select::{in_dialog_method, leg_at, leg_index};
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Originate a NOTIFY on `leg_id`'s confirmed dialog (toward the referrer)
    /// carrying the REFER implicit-subscription state (RFC 3515 §2.4.4): `Event:
    /// refer`, `Subscription-State`, and a `message/sipfrag` body. The B2BUA is
    /// the UAS of the referrer leg, so the NOTIFY rides that dialog's local
    /// sequence.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_notify(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        event: &str,
        subscription_state: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) {
        let Some(id) = leg_index(call, leg_id)
            .and_then(|i| call::helpers::carrying_dialog(leg_at(call, i)))
            .map(|d| d.id())
        else {
            return;
        };
        if self.held_behind_notify(call, leg_id, &id) {
            let notify = HeldRequest::Notify {
                event: event.to_string(),
                subscription_state: subscription_state.to_string(),
                content_type: content_type.map(str::to_string),
                body: body.to_vec(),
            };
            self.hold(call, fx, leg_id, &id, notify);
            return;
        }
        self.notify_now(call, fx, leg_id, &id, event, subscription_state, content_type, body);
    }

    /// Compose and send the NOTIFY on `leg_id`'s dialog `id`, the dialog's next
    /// CSeq: what this stack asks for there next waits for its final.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn notify_now(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        id: &DialogId,
        event: &str,
        subscription_state: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) {
        let Some(idx) = leg_index(call, leg_id) else { return };
        let Some(dialog) = leg_at(call, idx).dialogs.by_tags(&id.our_tag, &id.peer_tag).cloned()
        else {
            return;
        };
        let outbound_cseq = dialog.our_cseq + 1;
        let branch = self.turn.ids.new_branch();
        let gen_dialog = relay::to_gen_dialog(leg_at(call, idx), &dialog);
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
        // The dialog's number advances only for a NOTIFY that is built.
        let res = match generators::generate_in_dialog_request(
            InDialogMethod::Notify,
            &gen_dialog,
            &opts,
        ) {
            Ok(res) => res,
            Err(e) => return relay::unbuilt(&call.call_ref, leg_id, "NOTIFY", &e),
        };
        *call = bump_our_cseq(call.clone(), leg_id, id, 1);
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
        self.notify_left(call, leg_id, id, outbound_cseq);
    }

    /// Originate a re-INVITE on `leg_id` carrying `body` as the new offer, typed
    /// `content_type` (`None`: SDP) and described by `descriptors`, plus
    /// `add_headers` (Allow/Supported), the leg's capability set
    /// ([`capabilities::for_reinvite`]) filling what they leave unstated.
    /// CSeq = the dialog's `our_cseq` + 1. Used by
    /// `promote18xPemTo200` to resync Alice when bob's final SDP differs from the
    /// early-media SDP promoted into the synthetic 200 OK. The response comes back
    /// classified from-a (the B2BUA's stamped Via cr/lg) and is claimed by the
    /// `promote-resync-reinvite-response` rule.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_reinvite(
        &mut self,
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
        let dialog = match call::helpers::carrying_dialog(leg_at(call, idx)) {
            Some(d) => d.clone(),
            None => return,
        };
        let t_id = dialog.id();
        // The INVITE that created this leg's dialog: the originator's own on
        // hers, the one the stack sent on a leg it dialled.
        let creating = if leg_id == call.a_leg.leg_id {
            Some(relay::rebuild_a_leg_invite(&call.a_leg_invite))
        } else {
            relay::dialling_invite(leg_at(call, idx))
        };
        let outbound_cseq = dialog.our_cseq + 1;
        // The call as it stands before the re-INVITE records anything: one
        // that is not built leaves it so.
        let unsent = call.clone();
        *call = bump_our_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.turn.ids.new_branch();
        let gen_dialog = relay::to_gen_dialog(leg_at(call, idx), &dialog);
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
                Some(&dialog.peer_tag),
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
        let res = match generators::generate_in_dialog_request(
            InDialogMethod::Invite,
            &gen_dialog,
            &opts,
        ) {
            Ok(res) => res,
            Err(e) => {
                *call = unsent;
                return relay::unbuilt(&call.call_ref, leg_id, "re-INVITE", &e);
            }
        };
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
            d.pending_invite_txn = Some(call::InviteTxnHandle {
                branch: branch.clone(),
                cseq: i64::from(out_req.cseq().seq()),
                original_invite: out_req.image().to_vec(),
                destination: call::HostPort { host: dest.0.clone(), port: dest.1 },
            });
            d.ack_branch = None;
            d.emitted_ack = None;
            d.awaited_ack_cseq = None;
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
        &mut self,
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
        // Probe the dialog the leg's requests ride ([`call::helpers::carrying_dialog`]).
        // Skip when the leg holds none (nothing to probe).
        let dialog = match call::helpers::carrying_dialog(leg_at(call, idx)) {
            Some(d) => d.clone(),
            None => {
                // An early/mid-confirm leg legitimately has no confirmed dialog
                // yet — skip quietly. A CONFIRMED leg with none is a broken
                // invariant that silently drops every in-dialog request to
                // this leg — never swallow it.
                let leg = leg_at(call, idx);
                if leg.state() == call::LegState::Confirmed {
                    tracing::error!(
                        call_ref = %call.call_ref,
                        %leg_id,
                        dialogs = leg.dialogs.len(),
                        %method,
                        "INVARIANT VIOLATION: leg is Confirmed but holds no confirmed dialog — \
                         cannot originate the in-dialog request, so keepalive will never fire \
                         and this leg is permanently un-probeable"
                    );
                }
                return;
            }
        };
        // Advance + persist the dialog CSeq by exactly one (§12.2.1.1), exactly as
        // every other in-dialog originator here. Without this the in-dialog
        // keepalive OPTIONS re-derives the same CSeq every cycle and the later
        // relayed BYE collides on it — an RFC 3261 violation a real UAS rejects.
        let t_id = dialog.id();
        // `KeepaliveCseqReuse` (the wire-fault seam) emits the keepalive at the
        // CSeq the dialog already used and leaves the dialog un-advanced: the
        // exact defect `cseq-in-dialog-order` gates on, on purpose.
        let reuse = m == InDialogMethod::Options
            && self.turn.faults.is_armed(crate::wire_fault_set::WireFaultPoint::KeepaliveCseqReuse);
        let outbound_cseq = if reuse { dialog.our_cseq } else { dialog.our_cseq + 1 };
        // The call as it stands before the request records anything: one that
        // is not built leaves it so.
        let unsent = call.clone();
        if !reuse {
            *call = bump_our_cseq(call.clone(), leg_id, &t_id, 1);
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
        let branch = self.turn.ids.new_branch();
        let gen_dialog = relay::to_gen_dialog(leg_at(call, idx), &dialog);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, relay::CallMarks::of(call), leg_id, branch)),
            contact: Some(relay::leg_contact(self.config, relay::CallMarks::of(call), leg_id)),
            cseq: Some(outbound_cseq as u32),
            body: relay::continue_on_leg(
                call,
                leg_id,
                Some(&dialog.peer_tag),
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
        let res = match generators::generate_in_dialog_request(m, &gen_dialog, &opts) {
            Ok(res) => res,
            Err(e) => {
                *call = unsent;
                return relay::unbuilt(&call.call_ref, leg_id, method, &e);
            }
        };
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

    /// The answer to `offer` rejecting every stream (RFC 3264 §6, port 0) —
    /// the least a dialog about to end, or a party that gave no answer, can
    /// commit to.
    pub(super) fn rejecting_answer(&mut self, offer: &[u8]) -> Option<Vec<u8>> {
        sip_message::sdp_answer::reject_offer(
            offer,
            &sip_message::BuildHeldSdpOptions {
                local_ip: self.config.sip_local_ip.clone(),
                now_ms: self.now_ms,
            },
        )
    }
}
