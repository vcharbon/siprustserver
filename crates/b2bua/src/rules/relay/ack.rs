//! The ACK-for-2xx on a b-leg dialog (RFC 3261 §13.2.2.4): composing it from
//! dialog state, and the branch-stable re-ACKs for a retransmitted 2xx, echoing
//! the CSeq of the INVITE the ACK acknowledges.
//!
//! The ACK a relayed INVITE's 2xx owes is the acknowledging peer's own ACK,
//! relayed: §13.2.2.4 gives one ACK per 2xx received, so the body that peer put
//! on it travels with it and a repeated 2xx re-passes that same datagram. Only
//! an INVITE this stack originated is ACKed on its own account. A non-2xx
//! final's hop-by-hop ACK (§17.1.1.3) is the transaction layer's, whichever node
//! sent the INVITE (a taken-over call seeds that transaction,
//! `router::materialise`).

use call::{Dialog, Leg, LegState};
use sip_message::generators;
use sip_message::generators::GenerateAckFor2xxOpts;
use sip_message::header::MediaType;
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance};

use super::dialog::{target_dest, to_gen_dialog};
use super::egress::apply_b_leg_egress;
use super::identity::leg_via;

/// Build an ACK-for-2xx on a b-leg dialog (toward bob), sent raw. `body` carries
/// the inbound ACK's payload through (the delayed-offer re-INVITE answer rides
/// the ACK, RFC 3264 §4); pass empty for a bodyless ACK.
///
/// Returns the effect **and the Via branch it used**, so the caller can retain
/// the branch on the dialog ([`call::helpers::retain_ack_branch`]) for a
/// §13.2.2.4 re-ACK of a retransmitted 2xx. `provenance` says whose ACK it
/// is: the peer's relayed, or one this stack composes on its own account.
#[allow(clippy::too_many_arguments)]
pub fn ack_b_leg(
    call_ref: &str,
    leg: &Leg,
    is_emergency: bool,
    config: &B2buaConfig,
    id_gen: &IdGen,
    body: Vec<u8>,
    content_type: Option<MediaType>,
    provenance: Provenance,
) -> Option<(OutboundSipEffect, String)> {
    let dialog = leg.dialogs.first()?;
    // A leg with no 2xx in hand cannot be ACKed — the ACK would acknowledge a
    // final response that does not exist — and a tag-less dialog cannot form the
    // in-dialog request (§12.2.1.1): refuse rather than mint a branch toward a
    // provisional Contact. Same defensive shape as `relay_request`'s tag guard.
    if !matches!(leg.state, LegState::Confirmed | LegState::Terminated)
        || dialog.sip.remote_tag.is_empty()
    {
        return None;
    }
    let gen_dialog = to_gen_dialog(&dialog.sip);
    // RFC 3261 §13.2.2.4: the ACK for a 2xx is a UAC-core retransmit target. The
    // answerer re-sends its 2xx end-to-end until ACKed (up to its Timer H ≈ 32 s),
    // so a retransmitted 2xx MUST be re-ACKed reusing the SAME Via branch — a
    // fresh branch would mint a *new* client transaction and never quiesce the
    // answerer's INVITE server txn, leaking / late-timing-out the confirmed call
    // when the first ACK is lost. Reuse the branch this INVITE transaction's 2xx
    // armed (`confirm_dialog`), else mint one here. `ack_branch` is reset wherever
    // a new INVITE transaction is cached on the dialog, so a `Some(_)` here always
    // belongs to the CSeq echoed just below.
    let branch = dialog.ext.ack_branch.clone().unwrap_or_else(|| id_gen.new_branch());
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
            provenance,
        },
        branch,
    ))
}

/// The CSeq sequence number of the INVITE last sent on this dialog (initial or
/// re-INVITE), recovered from the cached client-transaction handle so the
/// 2xx ACK can echo it (RFC 3261 §13.2.2.4).
pub(crate) fn acked_invite_cseq(dialog: &Dialog) -> Option<u32> {
    acked_invite(dialog).map(|r| r.cseq().seq())
}

/// `true` when the INVITE last sent on this dialog carried the offer, so the ACK
/// for its 2xx owes no answer body (RFC 3264 §4) and this stack can compose it
/// from the dialog alone. A delayed-offer INVITE answers this `false`: its ACK
/// carries the answer, which only the acknowledging peer's own ACK supplies.
pub(crate) fn acked_invite_carries_offer(dialog: &Dialog) -> bool {
    acked_invite(dialog).is_some_and(|r| !r.body().is_empty())
}

/// The INVITE last sent on this dialog, re-parsed from its cached
/// client-transaction handle.
fn acked_invite(dialog: &Dialog) -> Option<sip_message::SipRequest> {
    parse_request(&dialog.ext.pending_invite_txn.as_ref()?.original_invite)
}

/// A request re-parsed from the bytes a client-transaction handle caches it as.
fn parse_request(bytes: &[u8]) -> Option<sip_message::SipRequest> {
    use sip_message::SipParser;
    match sip_message::parser::custom::CustomParser::new().parse(bytes).ok()? {
        sip_message::SipMessage::Request(r) => Some(r),
        _ => None,
    }
}
