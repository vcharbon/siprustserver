//! The BYE this stack sends on a confirmed dialog (RFC 3261 §15): what the
//! leg is owed before it, its composition with the `Reason` and the lines of
//! the peer's it restates, and its order behind the ACK of the dialog's own
//! 2xx and behind this stack's own NOTIFY there ([`super::subscription`]).

use call::helpers::TERMINATING_TIMEOUT_MS;
use call::TxnKind;
use call::{Call, Dialog, DialogId, Leg, Obligation, QueuedRequest, TimerKey, TimerType};
use sip_message::generators::{self, GenerateInDialogRequestOpts, InDialogMethod};
use sip_message::header::HeaderName;
use sip_message::{Method, SipHeader};

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::method_table::row;
use crate::rules::relay;

use super::teardown::{is_media, without_reason, Relayed};
use super::{ActionExecutor, Owed, Settle};

impl ActionExecutor<'_> {
    /// BYE `leg_id`'s confirmed dialog carrying `reason` and `relayed`
    /// (RFC 3261 §15): the PRACKs the leg's responders are owed leave first,
    /// an offer left to the ACK answered rejecting (RFC 3262 §4, §5), then a
    /// 2xx still unACKed is ACKed (§13.2.2.4) — bare where the ACK owes no
    /// answer, and where it owes one and `answer_lost` (the teardown answers a
    /// peer's BYE or gives up a 2xx's ACK, so no ACK will bring that answer)
    /// answering every stream rejected (RFC 3264 §6) — then the BYE. The BYE waits for the ACK of the leg's own
    /// 2xx to its dialog-creating INVITE (§15), and behind this stack's NOTIFY
    /// on the dialog awaiting its final.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_bye(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        reason: Option<&str>,
        relayed: Relayed,
        hops: Option<u32>,
        answer_lost: bool,
    ) {
        let ack_owed = relay::owes_bare_ack(call, leg_id);
        self.settle_owed_pracks(call, fx, Owed::Leg(leg_id), Settle::Ending);
        if ack_owed {
            self.ack_own_bare(call, fx, leg_id);
        } else if answer_lost {
            self.ack_rejecting(call, fx, leg_id);
        }
        let Some(leg) = call::helpers::find_leg(call, leg_id) else { return };
        let (reason, relayed) = if is_media(leg) {
            (None, Relayed { headers: without_reason(&relayed.headers), ..relayed })
        } else {
            (reason, relayed)
        };
        let Some(id) = leg.dialogs.confirmed().map(|d| d.id()) else { return };
        let row = row(&Method::Bye);
        if row.waits_behind_unacked_answer && answer_unacked(call, leg_id) {
            hold_behind_ack(call, leg_id, &id, relayed.queued_bye(reason, hops));
            return;
        }
        if row.waits_behind_own_notify && self.waits_behind_notify(call, leg_id, &id) {
            self.queue(call, fx, leg_id, &id, relayed.queued_bye(reason, hops));
            return;
        }
        self.bye_now(call, fx, leg_id, &id, reason, relayed, hops);
    }

    /// Send `bye`, a BYE queued on `leg_id`'s dialog `id` behind an ACK or this
    /// stack's NOTIFY, composed now. A terminating call's backstop runs again
    /// from this BYE, so its transaction has its whole bound.
    pub(super) fn send_queued_bye(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        id: &DialogId,
        bye: QueuedRequest,
    ) {
        let QueuedRequest::Bye { reason, headers, body, content_type, max_forwards } = bye else {
            return;
        };
        let headers = headers
            .into_iter()
            .map(|(name, value)| SipHeader { name: name.into(), value: value.into() })
            .collect();
        let content_type = content_type.as_deref().and_then(relay::media_type);
        let relayed = Relayed { headers, body, content_type };
        self.bye_now(call, fx, leg_id, id, reason.as_deref(), relayed, max_forwards);
        if call.state == call::CallModelState::Terminating {
            let backstop = TimerKey::call(TimerType::TerminatingTimeout);
            self.schedule(call, fx, backstop, TERMINATING_TIMEOUT_MS);
        }
    }

    /// Compose and send the BYE on `leg_id`'s dialog `id`: it takes the
    /// dialog's next CSeq (§12.2.1.1) and is recorded until it settles.
    #[allow(clippy::too_many_arguments)]
    fn bye_now(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        id: &DialogId,
        reason: Option<&str>,
        relayed: Relayed,
        hops: Option<u32>,
    ) {
        let Some(leg) = call::helpers::find_leg(call, leg_id) else { return };
        let Some(d) = leg.dialogs.by_tags(&id.our_tag, &id.peer_tag) else { return };
        let marks = self.marks(call);
        let Some(bye) = self.bye_on_dialog(marks, leg, d, reason, relayed, hops) else { return };
        *call = call::helpers::bump_our_cseq(call.clone(), leg_id, id, 1);
        self.record_own_effect(call, leg_id, id, &bye);
        fx.outbound.push(bye);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn bye_on_dialog(
        &mut self,
        marks: relay::CallMarks,
        leg: &Leg,
        d: &Dialog,
        reason: Option<&str>,
        relayed: Relayed,
        hops: Option<u32>,
    ) -> Option<OutboundSipEffect> {
        let leg_id = leg.leg_id.as_str();
        let dialog = relay::to_gen_dialog(leg, d);
        let branch = self.turn.ids.new_branch();
        let mut extra_headers: Vec<SipHeader> = reason
            .map(|r| {
                vec![SipHeader { name: "Reason".to_string().into(), value: r.to_string().into() }]
            })
            .unwrap_or_default();
        // The firing rule's own statement is the more specific one and stands;
        // every other name the releasing peer sent rides beside it, EVERY line
        // of it: a set-like header the peer split over several lines (`Allow`,
        // RFC 3261 §7.3.1) is one set, and keeping only its first line would
        // relay a one-token set the peer never stated.
        let stated = extra_headers.clone();
        for header in relayed.headers {
            let name = HeaderName::from(header.name.as_str());
            if !stated.iter().any(|h| name.matches(&h.name)) {
                extra_headers.push(header);
            }
        }
        // Per-dialog CSeq (§12.2.1.1): `generate_in_dialog_request` defaults to
        // this dialog's `our_cseq + 1`, the next sequence number within THIS
        // dialog (a forked sibling's CSeq is irrelevant — distinct dialog).
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(
                self.config,
                marks,
                leg_id,
                Some(relay::RequestFacts {
                    method: "BYE",
                    uri: &dialog.remote_target,
                    offer: false,
                }),
                branch,
            )),
            extra_headers,
            body: relayed.body,
            content_type: relayed.content_type,
            max_forwards: hops,
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Bye, &dialog, &opts)
            .inspect_err(|e| relay::unbuilt(marks.call_ref, leg_id, "BYE", e))
            .ok()?;
        let dest = relay::target_dest(&dialog.remote_target);
        let (req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &dialog.route_set, res.request, dest);
        Some(OutboundSipEffect {
            body: OutboundBody::Request(req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("BYE → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance: Provenance::Authored,
        })
    }
}

/// Whether `leg_id`'s confirmed dialog holds this stack's 2xx to the leg's
/// dialog-creating INVITE still awaiting its ACK: the call's answer to the
/// caller (RFC 3261 §13.3.1.4).
fn answer_unacked(call: &Call, leg_id: &str) -> bool {
    let Some(d) = call::helpers::find_leg(call, leg_id).and_then(|l| l.dialogs.confirmed()) else {
        return false;
    };
    call::helpers::answered_2xx(d).is_some_and(|u| {
        let key = Obligation::AckOf2xx {
            leg: leg_id.to_string(),
            dialog_tag: u.dialog_tag.clone(),
            cseq: u.cseq,
        };
        call::helpers::answers_initial_invite(call, &key)
    })
}

/// Hold `bye` behind the ACK of the 2xx `leg_id`'s dialog `id` answered
/// the leg's dialog-creating INVITE with (RFC 3261 §15).
fn hold_behind_ack(call: &mut Call, leg_id: &str, id: &DialogId, bye: QueuedRequest) {
    *call = call::helpers::update_dialog(call.clone(), leg_id, id, |d| {
        if let Some(x) = d.answered_invite.as_mut().and_then(|r| r.invite.as_mut()) {
            x.bye_behind_ack = Some(bye);
        }
    });
}
