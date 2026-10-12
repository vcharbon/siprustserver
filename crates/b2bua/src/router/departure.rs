//! The departure sweep (ADR-0046): a peer that left the membership set is
//! gone, and a caller or callee it left mid-INVITE may send nothing more — a
//! caller that heard a 100 runs no Timer B (RFC 3261 §17.1.1.2), and a callee
//! that is silent stays silent. Every call this node backs up for the peer
//! with an INVITE transaction in flight is taken over at once: its timers run
//! at their replicated deadlines, its transactions are rebuilt, and an
//! undecided call is answered. A call with no INVITE in flight is left to the
//! reactive path (ADR-0014 §1): taking every quiet dialog over at once is the
//! stale-CSeq storm that rule exists to avoid.

use std::sync::Arc;

use call::CallModelState;

use super::materialise::{has_invite_in_flight, materialise, Materialised, Origin};
use super::undecided::{is_undecided, settle_undecided};
use super::RouterCtx;
use crate::store::PartitionRole;

/// Take over every call of `ordinal`'s backup partition with an INVITE in
/// flight, each under its per-call lock. A call already live here is left as
/// it is; a terminal body is not read further.
pub(super) async fn take_over_departed(ctx: &Arc<RouterCtx>, ordinal: &str) {
    let calls = ctx.state.scan_partition(PartitionRole::Backup, ordinal).await;
    let scanned = calls.len();
    let mut taken = 0usize;
    let mut settled = 0usize;
    for (call, _skew) in calls {
        if call.state == CallModelState::Terminated || !has_invite_in_flight(&call) {
            continue;
        }
        let call_ref = call.call_ref;
        let guard = ctx.state.lock(&call_ref).await;
        match materialise(ctx, &call_ref, Origin::Takeover).await {
            Materialised::Served { call, seeded } => {
                taken += 1;
                if is_undecided(&call) {
                    settle_undecided(ctx, &call_ref, call, seeded).await;
                    settled += 1;
                }
            }
            Materialised::Resident(_) => {}
            // Gone from the partition since the scan: the lock entry this pass
            // made is the only trace, and no release will clear it. The guard
            // goes first, as it is held on that entry.
            Materialised::Refused(_) => {
                drop(guard);
                ctx.state.discard_orphan(&call_ref);
            }
        }
    }
    tracing::info!(
        node = observe::node(),
        peer = ordinal,
        scanned,
        taken,
        settled,
        "departed peer's in-flight calls taken over"
    );
}

#[cfg(test)]
mod tests;
