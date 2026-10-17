//! The BYE this stack sends on a confirmed dialog (RFC 3261 §15): what the
//! leg is owed before it, its composition with the `Reason` and the lines of
//! the peer's it restates, and its order behind this stack's own NOTIFY there
//! ([`super::own_notify`]).

use call::{Call, Dialog, DialogId, HeldRequest, Leg};
use sip_message::generators::{self, GenerateInDialogRequestOpts, InDialogMethod};
use sip_message::header::HeaderName;
use sip_message::SipHeader;
use sip_txn::TxnKind;

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::relay;

use super::teardown::{is_media, without_reason, Relayed};
use super::{ActionExecutor, Owed, Settle};

impl ActionExecutor<'_> {
    /// BYE `leg_id`'s confirmed dialog carrying `reason` and `relayed`
    /// (RFC 3261 §15): the PRACKs the leg's responders are owed leave first,
    /// an offer left to the ACK answered rejecting (RFC 3262 §4, §5), then a
    /// 2xx still unACKed is ACKed bare (§13.2.2.4), then the BYE — held while
    /// this stack's NOTIFY on the dialog awaits its final.
    pub(super) fn send_bye(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        reason: Option<&str>,
        relayed: Relayed,
        hops: Option<u32>,
    ) {
        let ack_owed = relay::owes_bare_ack(call, leg_id);
        self.settle_owed_pracks(call, fx, Owed::Leg(leg_id), Settle::Ending);
        if ack_owed {
            self.ack_own_bare(call, fx, leg_id);
        }
        let Some(leg) = call::helpers::find_leg(call, leg_id) else { return };
        let (reason, relayed) = if is_media(leg) {
            (None, Relayed { headers: without_reason(&relayed.headers), ..relayed })
        } else {
            (reason, relayed)
        };
        let Some(id) = leg.dialogs.confirmed().map(|d| d.id()) else { return };
        if self.held_behind_notify(call, leg_id, &id) {
            self.hold(call, fx, leg_id, &id, relayed.held_bye(reason, hops));
            return;
        }
        self.bye_now(call, fx, leg_id, &id, reason, relayed, hops);
    }

    /// Send `bye`, a BYE held behind this stack's NOTIFY on `leg_id`'s dialog
    /// `id`, composed now.
    pub(super) fn send_held_bye(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        id: &DialogId,
        bye: HeldRequest,
    ) {
        let HeldRequest::Bye { reason, headers, body, content_type, max_forwards } = bye else {
            return;
        };
        let headers = headers
            .into_iter()
            .map(|(name, value)| SipHeader { name: name.into(), value: value.into() })
            .collect();
        let content_type = content_type.as_deref().and_then(relay::media_type);
        let relayed = Relayed { headers, body, content_type };
        self.bye_now(call, fx, leg_id, id, reason.as_deref(), relayed, max_forwards);
    }

    /// Compose and send the BYE on `leg_id`'s dialog `id`: it takes the
    /// dialog's next CSeq (§12.2.1.1).
    #[allow(clippy::too_many_arguments)]
    fn bye_now(
        &self,
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
        let marks = relay::CallMarks::of(call);
        let Some(bye) = self.bye_on_dialog(marks, leg, d, reason, relayed, hops) else { return };
        *call = call::helpers::bump_our_cseq(call.clone(), leg_id, id, 1);
        fx.outbound.push(bye);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn bye_on_dialog(
        &self,
        marks: relay::CallMarks,
        leg: &Leg,
        d: &Dialog,
        reason: Option<&str>,
        relayed: Relayed,
        hops: Option<u32>,
    ) -> Option<OutboundSipEffect> {
        let leg_id = leg.leg_id.as_str();
        let dialog = relay::to_gen_dialog(leg, d);
        let branch = self.id_gen.new_branch();
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
            via: Some(relay::leg_via(self.config, marks, leg_id, branch)),
            extra_headers,
            body: relayed.body,
            content_type: relayed.content_type,
            max_forwards: hops,
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Bye, &dialog, &opts);
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
