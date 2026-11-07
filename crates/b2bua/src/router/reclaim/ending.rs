//! The other owner's ending of the call, met in the record that ends it
//! here (ADR-0055 §7, ADR-0020 X3): its terminal body is discharged through
//! the one enforcement funnel, and every re-INVITE this node relayed draws
//! one final for its originator.

use std::sync::Arc;

use b2bua_sdk::model::RuleAction;
use call::helpers::{keep_dialogs, raise_sequences};
use call::{Call, RelayOwner, RequestRecord};

use super::super::commit::{behind_the_write, Rider};
use super::super::materialise::relayed_server_key;
use super::super::off_wire::{self, Actions};
use super::super::RouterCtx;
use crate::store::Enveloped;

/// Discharge the other owner's ended body met here off the wire
/// ([`off_wire::Ending::Met`]), from `before`, the live record, so its edge
/// is this turn's: the CDR, the limiter release and the propagated delete
/// ride the commit as a primary-served BYE's would. The discharged body
/// keeps the higher of both views' dialog sequences, and the dialogs only
/// this node's copy holds.
///
/// Every re-INVITE this node relayed and the record holds open draws one
/// final for its originator (RFC 3261 §15.1.2), from the server transaction
/// this node holds:
/// - the ending body holds it CANCELled — the other copy's teardown answered
///   it — so this node relinquishes its server transaction with nothing sent;
///   its client transaction stays to absorb the target's final;
/// - otherwise this node answers it as its own teardown does: a 487 to its
///   originator, a CANCEL to its target.
///
/// A round the other copy relayed has its server transaction there, and its
/// ending answers it. The record takes the ending body's envelope.
pub(super) async fn discharge_met_ending(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    before: &Call,
    terminal: Enveloped,
    now_ms: i64,
) {
    let Enveloped { call: terminal, topology } = terminal;
    let by = &ctx.relay_owner;
    let (answered_there, ours) = own_open_rounds(before, &terminal, by);
    // Behind the sends the call's outbox still holds (ADR-0055 §3).
    let keys: Vec<_> = answered_there.iter().filter_map(relayed_server_key).collect();
    if !keys.is_empty() {
        behind_the_write(ctx, call_ref, Rider::Relinquish(keys)).await;
    }
    let body = keep_dialogs(raise_sequences(terminal, before), before);
    let actions = Actions {
        topic: "folded-ending",
        list: rounds_answered(ours),
        owners: ctx.owners(call_ref),
    };
    ctx.state.update_enveloped(Enveloped::new(before.clone(), topology));
    let ending = off_wire::Ending::Met { before, body, actions };
    let label = "the discharge of the other copy's ending";
    off_wire::end(ctx, call_ref, ending, off_wire::Caller::Leave, now_ms, label).await;
}

/// The actions answering the record's re-INVITE relays `rounds` — `(leg,
/// outbound CSeq)`: a 487 to the originator and a CANCEL to the target.
fn rounds_answered(rounds: Vec<(String, i64)>) -> Vec<RuleAction> {
    rounds
        .into_iter()
        .map(|(leg_id, outbound_cseq)| RuleAction::RejectPendingReinvite {
            leg_id,
            outbound_cseq,
            status: 487,
            reason: "Request Terminated".into(),
        })
        .collect()
}

/// The re-INVITE relays worker `by` made that `record` holds open, not
/// CANCELled: those `ending` holds CANCELled, and the `(leg, outbound CSeq)`
/// of the others.
fn own_open_rounds(
    record: &Call,
    ending: &Call,
    by: &RelayOwner,
) -> (Vec<RequestRecord>, Vec<(String, i64)>) {
    let legs = || std::iter::once(&record.a_leg).chain(record.b_legs.iter());
    let (mut answered_there, mut ours) = (Vec::new(), Vec::new());
    for leg in legs() {
        let theirs = call::helpers::find_leg(ending, &leg.leg_id);
        for d in leg.dialogs.iter() {
            let held = theirs.and_then(|l| l.dialogs.by_tags(&d.our_tag, &d.peer_tag));
            let open =
                call::helpers::invite_relay(d).filter(|p| !p.cancelled && p.relayed_by == *by);
            if let Some(p) = open {
                let cancelled_there = held.is_some_and(|h| {
                    call::helpers::invite_relay(h)
                        .is_some_and(|t| t.outbound_cseq == p.outbound_cseq && t.cancelled)
                });
                if cancelled_there {
                    answered_there.push(p.clone());
                } else {
                    ours.push((leg.leg_id.clone(), p.outbound_cseq));
                }
            }
        }
    }
    (answered_there, ours)
}
