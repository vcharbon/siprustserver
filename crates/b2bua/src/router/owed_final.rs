//! The final an ended call may still owe its caller (ADR-0055 §3): a turn
//! that ends the call with the non-2xx it answers her INVITE with writes the
//! ended body before that final leaves, and deletes the call once it left. A
//! node that meets the ended body with no live copy of the call — the
//! backup at a departure or a dead verdict, a restarted primary at its
//! reclaim — cannot tell whether it left, so it sends it again on a
//! Completed server transaction that repeats it until her ACK (RFC 3261
//! §17.2.1); a copy that did reach her draws only her re-ACK. The ACK of a
//! callee's non-2xx may have died held with the node too: each copy of that
//! final the callee repeats is ACKed (`materialise::rejected_dialled`).

use std::sync::Arc;

use call::Call;
use sip_txn::TxnSeed;

use super::materialise::{rejected_dialled_seeds, rejected_seed};
use super::RouterCtx;

/// Hand the layer the finals `call` may still owe its caller, if any.
pub(super) async fn deliver(ctx: &Arc<RouterCtx>, call_ref: &str, call: &Call) {
    let seeds = owed_finals(call, ctx.clock.now_ms());
    if !seeds.is_empty() {
        let _ = ctx.txn.seed(call_ref, seeds).await;
    }
}

/// The finals the ended `call` may still owe its caller at `now_ms`: the
/// non-2xx her INVITE was answered with, within Timer H of the call's end;
/// and the ACKs it may still owe a callee's non-2xx.
fn owed_finals(call: &Call, now_ms: i64) -> Vec<TxnSeed> {
    let timer_h = i64::try_from(sip_txn::timers::TIMER_H).unwrap_or(i64::MAX);
    let ended_at = call.termination.as_ref().map(|t| t.at_ms);
    if ended_at.is_none_or(|at| now_ms >= at.saturating_add(timer_h)) {
        return Vec::new();
    }
    let mut seeds: Vec<TxnSeed> = rejected_seed(&call.a_leg).into_iter().collect();
    seeds.extend(rejected_dialled_seeds(call, now_ms));
    seeds
}
