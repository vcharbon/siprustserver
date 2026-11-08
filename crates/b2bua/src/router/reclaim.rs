//! Replication-driven reclaim and the terminal-discharge funnels (ADR-0011 X11
//! / ADR-0014 / ADR-0020 X3): the reactive reverse-flush reconcile, on-reboot
//! discharge of deferred terminals; the bulk reboot reclaim and the straggler
//! reclaim live in `bulk`, how two copies of a call meet in `meeting` and how
//! two diverged copies end in `diverged` (ADR-0055 §7), the periodic
//! replica-store reap in `reap`, and the steps those sweeps defer to a turn
//! of the call's own queue rather than wait on its lock in `deferred`.

use std::sync::Arc;

use call::CallModelState;

use super::off_wire;
use super::restore_hygiene::reanchor;
use super::RouterCtx;
use crate::store::{role_of, CallTopology, Enveloped, PartitionRole};

mod bulk;
pub(super) mod deferred;
mod diverged;
mod ending;
mod meeting;
mod reap;
use bulk::reclaim_into_live;
pub(super) use bulk::{announce_reclaim, arm_parked, park_all, Deferral};
pub(super) use meeting::meet_live;
pub(crate) use reap::reap_expired_replicas;

/// Apply a backup's reverse-flushed copy that the puller just landed in our
/// `pri:{self}` partition (ADR-0014 Reclaim-tail reconcile, extended to the
/// live copy), on a turn of the call (`flush_turn`). The acting backup served
/// the call while we still own it live: its copy meets our live copy
/// ([`meet_live`], ADR-0055 §7). A copy whose primary is a member defers the
/// CDR to it (ADR-0055 §5): the live primary discharges the call.
///
/// Not live: a **Terminated** body is a reboot-reclaimed deferral — materialise +
/// discharge (no keepalive arming); anything else is the existing reactive
/// straggler, materialised + re-armed by [`reclaim_into_live`].
pub(super) async fn reconcile_reverse_flush(ctx: &Arc<RouterCtx>, call_ref: &str) {
    // A read that sees an expired body too: an expired reverse-flushed terminal
    // is still discharged here.
    let Some((Enveloped { call: first, .. }, _)) = ctx.state.peek_reclaimable_raw(call_ref).await
    else {
        return;
    };
    // Not-live + non-terminal is the reactive-straggler path; it manages
    // its own per-call lock, so route it BEFORE taking the lock here (the guard is
    // not reentrant).
    if !ctx.state.contains(call_ref) && first.state != CallModelState::Terminated {
        reclaim_into_live(ctx, call_ref).await;
        return;
    }
    let _guard = ctx.state.lock(call_ref).await;
    // Read again under the lock: a turn that held it may have ended the call
    // and deleted the body, its CDR written; the body read before is stale.
    let Some((mut replica, skew_offset_ms)) = ctx.state.peek_reclaimable_raw(call_ref).await else {
        return;
    };
    reanchor(&mut replica.call, skew_offset_ms);
    let now_ms = ctx.clock.now_ms();
    match ctx.state.peek_enveloped(call_ref) {
        // The stored body is another call than the live one (the release of
        // the stored call is still queued): it is no copy of this call.
        Some(live) if replica.call.incarnation() != live.call.incarnation() => {}
        Some(live) => {
            meet_live(ctx, FlushDirection::Reverse, call_ref, &live, replica, now_ms).await;
        }
        // A reverse-flushed deferral for a call we no longer hold live (the
        // reactive straggler equivalent of the reboot-reclaim terminal).
        None => {
            let cause = super::ended_body::Cause::ReverseFlush;
            super::ended_body::pay(ctx, call_ref, cause, replica, now_ms).await;
        }
    }
}

/// A call's `(p,b)` version vector rendered for a lifecycle line (`-` when
/// the call is placed under no topology and is therefore non-replicable).
pub(super) fn format_pb(topology: Option<&CallTopology>) -> String {
    match topology {
        Some(t) => format!("({},{})", t.gen, t.bak_gen),
        None => "-".to_string(),
    }
}

/// Whose copy of the call meets the live copy here. The meeting is ONE rule
/// in both directions; the direction says which copy is the primary's.
#[derive(Clone, Copy, Debug)]
pub(super) enum FlushDirection {
    /// An acting backup's flush toward the primary that owns the call.
    Reverse,
    /// A primary's flush toward a backup that holds its Element (ADR-0031 D3).
    Forward,
}

impl FlushDirection {
    /// The label an operator correlates the meeting line by.
    pub(super) fn label(self) -> &'static str {
        match self {
            FlushDirection::Reverse => "reverse",
            FlushDirection::Forward => "forward",
        }
    }

    /// The role this node must play for the ref for a flush of this direction to
    /// be meaningful: a Reverse flush reaches the ref's PRIMARY, a Forward flush
    /// its backup. A command that names the other role is a misrouted frame and
    /// the meeting does nothing with it.
    fn expected_role(self) -> PartitionRole {
        match self {
            FlushDirection::Reverse => PartitionRole::Primary,
            FlushDirection::Forward => PartitionRole::Backup,
        }
    }
}

/// Whether a refused flush of `direction` may meet a copy of `call_ref` here: a
/// flush of the role this node plays for the ref, on a call live here or with a
/// queue, whose earlier turn may make it live. An Element alone opens no queue.
pub(super) fn refused_flush_may_meet(
    ctx: &RouterCtx,
    direction: FlushDirection,
    call_ref: &str,
) -> bool {
    role_of(&ctx.config.self_ordinal, call_ref) == direction.expected_role()
        && (ctx.state.contains(call_ref) || ctx.dispatcher.has_queue(call_ref))
}

/// Meet a copy the store did not take with the live copy (the body rides the
/// command, the store kept its own), on a turn of the call (`flush_turn`): the
/// store's copy contains it, or the two diverged, and the live copy may hold
/// more than the store's ([`meet_live`], ADR-0055 §7). **A live copy is
/// required**: an Element alone is kept as it is, and a call we do not serve
/// live is never resurrected here.
pub(super) async fn meet_refused_flush(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    call_ref: &str,
    body: &[u8],
    origin_now_ms: i64,
) {
    if role_of(&ctx.config.self_ordinal, call_ref) != direction.expected_role() {
        return;
    }
    let Some(mut replica) = ctx.state.decode_body(body) else {
        return;
    };
    // No live copy ⇒ nothing to meet, and an Element with no live copy is the
    // common case: it takes no lock.
    if !ctx.state.contains(call_ref) {
        return;
    }
    let _guard = ctx.state.lock(call_ref).await;
    let now_ms = ctx.clock.now_ms();
    // The call may have been released between the peek and the lock.
    let Some(live) = ctx.state.peek_enveloped(call_ref) else {
        return;
    };
    // Another call on the ref (the one the live call retried, or one a
    // takeover copy still serves) is never a copy of this call.
    if replica.call.incarnation() != live.call.incarnation() {
        return;
    }
    reanchor(&mut replica.call, now_ms.saturating_sub(origin_now_ms));
    meet_live(ctx, direction, call_ref, &live, replica, now_ms).await;
}

/// Discharge the live `call_ref` the rules failed on twice, off the wire
/// ([`off_wire::Ending::Forced`]), its caller answered if she still waits on
/// this node. Reached only by the reaper's discharge outcome; a takeover copy
/// defers its discharge to the primary (`commit_turn`). The caller holds the
/// per-call lock.
pub(super) async fn discharge_as_own(ctx: &Arc<RouterCtx>, call_ref: &str, now_ms: i64) {
    let Some(call) = ctx.state.peek(call_ref) else { return };
    let ending = off_wire::Ending::Forced { before: &call };
    let label = &"the reaper discharge";
    off_wire::end(ctx, call_ref, ending, off_wire::Caller::Answer, now_ms, label).await;
}

#[cfg(test)]
mod parked_probe_tests;
#[cfg(test)]
mod tests;
