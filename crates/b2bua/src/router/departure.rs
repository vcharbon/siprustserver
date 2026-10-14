//! The departure sweep (ADR-0047): a peer that left the membership set is
//! gone, and a caller or callee it left mid-INVITE may send nothing more — a
//! caller that heard a 100 runs no Timer B (RFC 3261 §17.1.1.2), and a silent
//! callee stays silent, and nobody else repeats a 2xx the caller lost (RFC
//! 3261 §13.3.1.4). Every call this node backs up for the peer with an INVITE
//! transaction to serve — awaiting its final, or its 2xx awaiting the ACK — is
//! taken over at once: its timers run at their replicated deadlines, its
//! transactions are rebuilt, and an undecided call is answered. A call with
//! no INVITE transaction to serve is left to the reactive path
//! (ADR-0014 §1): taking every quiet dialog over at once is the stale-CSeq
//! storm that rule exists to avoid. A call a takeover copy here ended and
//! deferred to the peer, which left without reconciling it, is discharged —
//! CDR, limiter release, delete — since no node is left to (ADR-0047 D8).
//!
//! The sweep is a task of its own, never the router loop's: a departed
//! partition holds every call the peer served, read one body at a time, and
//! the router serves its own calls meanwhile. The dispatcher tracks it, so a
//! crash of the node stops it with the call work it runs. Each call is taken under its
//! per-call lock, so the sweep and the call's own turns never interleave.

use std::sync::Arc;

use call::CallModelState;

use super::materialise::{has_invite_to_serve, materialise, Materialised, Origin};
use super::reclaim::discharge_materialized_terminal;
use super::undecided::{is_undecided, settle_undecided};
use super::RouterCtx;
use crate::store::PartitionRole;

/// How many calls the sweep reads between two yields to the runtime.
const YIELD_EVERY: usize = 32;

/// Start the sweep of `ordinal`'s backup partition and return at once.
pub(super) fn take_over_departed(ctx: &Arc<RouterCtx>, ordinal: &str) {
    let refs = ctx.state.partition_refs(PartitionRole::Backup, ordinal);
    let (task_ctx, ordinal) = (ctx.clone(), ordinal.to_string());
    let task = observe::spawn_keeping_node(async move { sweep(&task_ctx, &ordinal, refs).await });
    ctx.dispatcher.track(task.abort_handle());
}

/// What the sweep did with one call.
enum Swept {
    /// Nothing in flight, already live here, or gone.
    Left,
    Taken,
    TakenAndSettled,
    /// Ended here and deferred to the peer: discharged.
    Discharged,
}

/// Take over every call of `refs` with an INVITE transaction to serve, one at
/// a time.
async fn sweep(ctx: &Arc<RouterCtx>, ordinal: &str, refs: Vec<String>) {
    let scanned = refs.len();
    let (mut taken, mut settled, mut discharged) = (0usize, 0usize, 0usize);
    for (i, call_ref) in refs.iter().enumerate() {
        if i % YIELD_EVERY == YIELD_EVERY - 1 {
            tokio::task::yield_now().await;
        }
        match take_over_one(ctx, call_ref).await {
            Swept::Left => {}
            Swept::Taken => taken += 1,
            Swept::TakenAndSettled => (taken, settled) = (taken + 1, settled + 1),
            Swept::Discharged => discharged += 1,
        }
    }
    tracing::info!(
        node = observe::node(),
        peer = ordinal,
        scanned,
        taken,
        settled,
        discharged,
        "departed peer's in-flight calls taken over"
    );
}

/// Discharge `call_ref` when its body is ended, else take it over when it
/// names an INVITE transaction to serve ([`has_invite_to_serve`]). The body is
/// read once without the lock to leave a quiet call untouched; a call already
/// live here is left as it is.
async fn take_over_one(ctx: &Arc<RouterCtx>, call_ref: &str) -> Swept {
    let Ok((call, _skew)) = ctx.state.peek_replica(call_ref).await else { return Swept::Left };
    if call.state == CallModelState::Terminated {
        // A takeover copy ended the call and deferred it to the peer, which
        // left without reconciling it: no one else discharges it now.
        let _guard = ctx.state.lock(call_ref).await;
        let now_ms = ctx.clock.now_ms();
        discharge_materialized_terminal(ctx, call_ref, call, now_ms, false).await;
        return Swept::Discharged;
    }
    if !has_invite_to_serve(&call) {
        return Swept::Left;
    }
    let guard = ctx.state.lock(call_ref).await;
    match materialise(ctx, call_ref, Origin::Takeover).await {
        Materialised::Served { call, seeded } => match seeded {
            Some(seeded) if is_undecided(&call) => {
                settle_undecided(ctx, call_ref, call, seeded).await;
                Swept::TakenAndSettled
            }
            _ => Swept::Taken,
        },
        Materialised::Resident(_) => Swept::Left,
        // Gone from the partition since it was read: the lock entry this pass
        // made is the only trace, and no release will clear it. The guard
        // goes first, as it is held on that entry.
        Materialised::Refused(_) => {
            drop(guard);
            ctx.state.discard_orphan(call_ref);
            Swept::Left
        }
    }
}

#[cfg(test)]
mod tests;
