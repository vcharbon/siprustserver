//! The ACK-for-2xx on a b-leg dialog (RFC 3261 §13.2.2.4): branch-stable
//! re-ACKs for a retransmitted 2xx, echoing the CSeq of the INVITE the ACK
//! acknowledges.

use call::{Dialog, Leg};
use sip_message::generators::GenerateAckFor2xxOpts;
use sip_message::generators;
use sip_message::header::MediaType;
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode};

use super::dialog::{target_dest, to_gen_dialog};
use super::egress::apply_b_leg_egress;
use super::identity::leg_via;

/// Build an ACK-for-2xx on a b-leg dialog (toward bob), sent raw. `body` carries
/// the inbound ACK's payload through (the delayed-offer re-INVITE answer rides
/// the ACK, RFC 3264 §4); pass empty for a bodyless ACK.
///
/// Returns the effect **and the Via branch it used**, so the caller can retain
/// the branch on the dialog ([`call::helpers::retain_ack_branch`]) for a
/// §13.2.2.4 re-ACK of a retransmitted 2xx.
pub fn ack_b_leg(
    call_ref: &str,
    leg: &Leg,
    is_emergency: bool,
    config: &B2buaConfig,
    id_gen: &IdGen,
    body: Vec<u8>,
    content_type: Option<MediaType>,
) -> Option<(OutboundSipEffect, String)> {
    let dialog = leg.dialogs.first()?;
    let gen_dialog = to_gen_dialog(&dialog.sip);
    // RFC 3261 §13.2.2.4: the ACK for a 2xx is a UAC-core retransmit target. The
    // answerer re-sends its 2xx end-to-end until ACKed (up to its Timer H ≈ 32 s),
    // so a retransmitted 2xx MUST be re-ACKed reusing the SAME Via branch — a
    // fresh branch would mint a *new* client transaction and never quiesce the
    // answerer's INVITE server txn, leaking / late-timing-out the confirmed call
    // when the first ACK is lost. Reuse the branch retained from the first ACK on
    // this INVITE transaction's 2xx; mint one on the first ACK. `ack_branch` is
    // reset wherever a new INVITE transaction is cached on the dialog, so a
    // `Some(_)` here always belongs to the CSeq echoed just below.
    let branch = dialog
        .ext
        .ack_branch
        .clone()
        .unwrap_or_else(|| id_gen.new_branch());
    // The ACK reuses the CSeq of the INVITE it acknowledges — not the dialog's
    // running `local_cseq`, which an intervening early PRACK/UPDATE (or a later
    // in-dialog request) has advanced past the INVITE. Recover it from the cached
    // INVITE transaction handle.
    let ack_cseq = acked_invite_cseq(dialog).unwrap_or_else(|| dialog.sip.local_cseq.max(0) as u32);
    let opts = GenerateAckFor2xxOpts {
        via: Some(leg_via(config, call_ref, &leg.leg_id, is_emergency, branch.clone())),
        cseq: Some(ack_cseq),
        body,
        content_type,
        ..Default::default()
    };
    let ack = generators::generate_ack_for_2xx(None, &gen_dialog, &opts);
    let dest = target_dest(&dialog.sip.remote_target);
    let (ack, dest) = apply_b_leg_egress(config, &leg.leg_id, &gen_dialog.route_set, ack, dest);
    Some((
        OutboundSipEffect {
            body: OutboundBody::Request(ack),
            mode: OutboundTxnMode::Raw,
            destination: dest,
            label: format!("ACK → {}", leg.leg_id),
            leg_id: Some(leg.leg_id.clone()),
        },
        branch,
    ))
}

/// The CSeq sequence number of the INVITE last sent on this dialog (initial or
/// re-INVITE), recovered from the cached client-transaction handle so the
/// 2xx ACK can echo it (RFC 3261 §13.2.2.4).
pub(crate) fn acked_invite_cseq(dialog: &Dialog) -> Option<u32> {
    use sip_message::SipParser;
    let handle = dialog.ext.pending_invite_txn.as_ref()?;
    match sip_message::parser::custom::CustomParser::new()
        .parse(&handle.original_invite)
        .ok()?
    {
        sip_message::SipMessage::Request(r) => Some(r.cseq().seq()),
        _ => None,
    }
}
