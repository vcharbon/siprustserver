//! The bulk reboot reclaim and the one-call straggler reclaim: this node
//! re-serving its own `pri:{self}` partition (ADR-0011 X11 / ADR-0014).

use std::collections::HashSet;
use std::sync::Arc;

use call::{Call, CallModelState, TimerType};

use super::super::materialise::{
    arm, has_invite_to_serve, materialise, Materialised, Origin, Reason,
};
use super::super::restore_hygiene::{reanchor, Smoothing};
use super::super::undecided::{is_undecided, settle_undecided};
use super::super::unrecoverable::record_reads;
use super::super::RouterCtx;
use super::deferred;
use crate::store::PartitionRole;

/// **Bulk reclaim, first pass** (ADR-0014, ADR-0055 §8): make every
/// `pri:{self}` call resident and park it, nothing armed, so nothing of it
/// leaves the node; a call with an INVITE to serve, or one its backup wrote,
/// is written once ([`announce_reclaim`]), so the backup's takeover copy of
/// it meets that write before the proxy routes anything to this node, and
/// stands down on it when the write covers it (ADR-0047 D7): not when the
/// copy wrote after this node's pull, nor while it holds a request of its own
/// open. It runs on bootstrap
/// completion, whether or not the node's own endpoint reads ready; the
/// parked calls are armed once it does ([`arm_parked`]). Each body is read
/// through the TTL-gated reclaim read, so a body whose TTL ran out is dead and
/// is never re-served: it counts in `scanned` and not in `materialized`.
///
/// The pass waits on no call's lock: a call whose lock a turn holds, and every
/// ended or unrecoverable body, whose discharge waits on its write's
/// replication, is parked on a turn of its own queue ([`deferred`]) and
/// returned, to be armed only by a turn queued behind that park. Those calls
/// count in neither `materialized` nor `announced`.
pub(in crate::router) async fn park_all(ctx: &Arc<RouterCtx>) -> Vec<Deferral> {
    let start_ms = ctx.clock.now_ms();
    let active_before = ctx.state.active_count() as u64;
    let own = &ctx.config.self_ordinal;
    // The whole turn works through the partition, one call at a time: each
    // call read or parked is a piece of work of its own, so the router's
    // pulse shows the turn making progress, however large the partition.
    let beat = || ctx.router_pulse.turning();
    let calls = ctx.state.scan_partition(PartitionRole::Primary, own, &beat).await;
    let scanned = calls.len() as u64;
    let mut materialized = 0u64;
    let mut announced = 0u64;
    let mut deferrals = Vec::new();
    for (mut copy, skew) in calls {
        beat();
        let call_ref = copy.call.call_ref.clone();
        reanchor(&mut copy.call, skew);
        let ends = ends_on_its_turn(&copy.call, ctx.clock.now_ms());
        let Some(_guard) = (!ends).then(|| ctx.state.try_lock(&call_ref)).flatten() else {
            if deferred::offer_park(ctx, &call_ref) {
                deferrals.push(Deferral { scanned: (!ends).then_some(copy.call), call_ref });
            }
            continue;
        };
        let call_ref = &call_ref;
        match materialise(ctx, call_ref, Origin::Park).await {
            Materialised::Parked(call) => {
                materialized += 1;
                announced += u64::from(announce_reclaim(ctx, call));
            }
            Materialised::Refused(Reason::Terminated | Reason::Unrecoverable) => materialized += 1,
            _ => {}
        }
    }
    // Per-reboot completeness telemetry. The gauges expose the pass's
    // denominator/numerator; the structured lifecycle line (visible in
    // `kubectl logs`) records the per-pass triple.
    ctx.metrics.set_repl_reclaim_pass(scanned, materialized);
    let active_after = ctx.state.active_count() as u64;
    let duration_ms = ctx.clock.now_ms() - start_ms;
    tracing::info!(
        node = observe::node(),
        active_before,
        scanned,
        materialized,
        announced,
        deferred = deferrals.len(),
        active_after,
        duration_ms,
        "reboot reclaim"
    );
    deferrals
}

/// A call the bulk park deferred to a turn of its own queue.
pub(in crate::router) struct Deferral {
    pub(in crate::router) call_ref: String,
    /// The body the pass scanned, for a call deferred because a turn held its
    /// lock; `None` for an ended or unrecoverable body.
    pub(in crate::router) scanned: Option<Call>,
}

/// Whether the scanned `call` ends on its park: ended or ending, or a record
/// that does not serve a copy at `now_ms` ([`record_reads`]).
fn ends_on_its_turn(call: &Call, now_ms: i64) -> bool {
    matches!(call.state, CallModelState::Terminating | CallModelState::Terminated)
        || record_reads(call, now_ms).is_err()
}

/// The keepalive smoothing of a bulk cohort whose largest past-due gap is
/// `l_max` ms, read at `now_ms`.
pub(super) fn smoothing(ctx: &RouterCtx, now_ms: i64, l_max: i64) -> Smoothing {
    Smoothing {
        now_ms,
        l_max,
        speedup: ctx.replica.keepalive_catchup_speedup.max(1),
        cap_ms: ctx.replica.max_catchup_window_sec.map(|s| s * 1000),
    }
}

/// **Bulk reclaim, second pass**: arm every parked call once the node's own
/// endpoint reads ready (ADR-0055 §8, `router::readmission`) — its timers
/// re-armed, its transactions seeded, an undecided call asked again (ADR-0047 D4).
///
/// **Keepalive smoothing (ADR-0014, performance-only).** Many keepalive timers in
/// a just-rehydrated partition are past-due; firing them all at once floods the
/// node with a synchronized OPTIONS burst. So we stagger the past-due keepalives
/// oldest-first: with `L = now - fire_at` the overdue gap and `L_max` the largest
/// over the batch, a keepalive's new `fire_at` is `now + (L_max - L)/speedup`, so
/// the most-overdue (most at-risk of a UAC keepalive timeout) fires first and the
/// backlog drains over `L_max/speedup`, bounded to `speedup`× the normal cadence
/// (optionally capped by `max_catchup_window_sec`). After the burst each call
/// re-arms `+interval`, naturally re-spreading load. This is **load management
/// only** — `(p,b)` reconciliation makes any incidental keepalive overlap
/// non-corrupting, so there is no settle/handback floor. `fire_at` is pre-computed
/// here, in the reclaim handler — never inside the timer driver (CLAUDE.md).
///
/// The pass waits on no call's lock: a parked call whose lock a turn holds,
/// and every call of `deferred` (the parks [`park_all`] deferred, which may
/// not have run yet), is armed by a turn queued on its own queue, its
/// smoothing read when that turn runs. Returns, for each call of `deferred`,
/// whether its arming was queued: one discarded met a released queue.
pub(in crate::router) async fn arm_parked(
    ctx: &Arc<RouterCtx>,
    deferred: &[String],
) -> Vec<(String, bool)> {
    let start_ms = ctx.clock.now_ms();
    let beat = || ctx.router_pulse.turning();
    let parked = ctx.state.parked();
    let calls: Vec<Call> = parked
        .iter()
        .filter_map(|call_ref| {
            beat();
            ctx.state.peek(call_ref)
        })
        .collect();
    let l_max = largest_keepalive_gap(&calls, start_ms, &beat);
    let smoothing = smoothing(ctx, start_ms, l_max);
    let later: HashSet<&str> = deferred.iter().map(String::as_str).collect();
    let (mut armed, mut queued) = (0u64, 0u64);
    for call_ref in parked.iter().filter(|r| !later.contains(r.as_str())) {
        beat();
        let Some(_guard) = ctx.state.try_lock(call_ref) else {
            queued += u64::from(deferred::offer_arm(ctx, call_ref, l_max));
            continue;
        };
        let Some((call, seeded)) = arm(ctx, call_ref, Some(smoothing)).await else { continue };
        armed += 1;
        if let Some(seeded) = seeded.filter(|_| is_undecided(&call)) {
            settle_undecided(ctx, call_ref, call, seeded).await;
        }
    }
    let arming: Vec<(String, bool)> =
        deferred.iter().map(|r| (r.clone(), deferred::offer_arm(ctx, r, l_max))).collect();
    queued += arming.iter().filter(|(_, q)| *q).count() as u64;
    let duration_ms = ctx.clock.now_ms() - start_ms;
    tracing::info!(
        node = observe::node(),
        armed,
        queued,
        l_max_ms = l_max,
        duration_ms,
        "reclaimed calls armed"
    );
    arming
}

/// `L_max`: the largest past-due keepalive gap across `calls`, read on
/// deadlines already in this node's clock frame (each was re-anchored when it
/// was parked). `beat` runs once per call.
fn largest_keepalive_gap<'a>(
    calls: impl IntoIterator<Item = &'a Call>,
    now_ms: i64,
    beat: &dyn Fn(),
) -> i64 {
    let mut l_max = 0;
    for call in calls {
        beat();
        let gap = call
            .timers
            .iter()
            .filter(|t| *t.timer_type() == TimerType::Keepalive)
            .map(|t| (now_ms - t.fire_at).max(0))
            .max()
            .unwrap_or(0);
        l_max = l_max.max(gap);
    }
    l_max
}

/// Reclaim one call of this node's `pri:{self}` partition into the live map
/// under its per-call lock (ADR-0011 X11), armed at once: a straggler. It has
/// no message to serve: a call with an INVITE transaction to serve, or one its
/// backup wrote, is announced ([`announce_reclaim`]), and a call still
/// undecided then has its decision asked again (ADR-0047 D4).
/// Returns `true` iff this pass did the work — freshly materialised, or a
/// deferred terminal discharged; `false` for a call already resident
/// (idempotent re-pass) or gone from the store.
pub(super) async fn reclaim_into_live(ctx: &Arc<RouterCtx>, call_ref: &str) -> bool {
    let _guard = ctx.state.lock(call_ref).await;
    match materialise(ctx, call_ref, Origin::Reclaim).await {
        Materialised::Served { call, seeded } => {
            // The write goes first: a takeover copy asking the same decision
            // stands down on it (ADR-0047 D7).
            announce_reclaim(ctx, call.clone());
            if let Some(seeded) = seeded.filter(|_| is_undecided(&call)) {
                settle_undecided(ctx, call_ref, call, seeded).await;
            }
            true
        }
        Materialised::Refused(Reason::Terminated | Reason::Unrecoverable) => true,
        _ => false,
    }
}

/// Write the reclaimed `call` once when the backup may hold a takeover copy of
/// it: the call has an INVITE transaction to serve, or the backup wrote it as
/// its acting owner (its `b` is past zero). This node serving the call again is
/// the change the write records, one `p` increment, and it reaches the
/// backup's Element as the primary's write, where a takeover copy of the call
/// stands down on it (ADR-0047 D7). Every reclaim path writes it, the bulk
/// pass, a straggler and an on-demand reclaim alike. A quiet call the backup
/// never wrote has no copy and is not written, so the reclaim of a large
/// partition stays write-free for those. Whether it wrote.
pub(in crate::router) fn announce_reclaim(ctx: &Arc<RouterCtx>, call: Call) -> bool {
    let backup_wrote = ctx
        .state
        .peek_enveloped(&call.call_ref)
        .and_then(|held| held.topology)
        .is_some_and(|topology| topology.bak_gen > 0);
    let written = backup_wrote || has_invite_to_serve(&call);
    if written {
        ctx.state.update(call.clone());
        ctx.state.flush(&call);
    }
    written
}

#[cfg(test)]
mod deferral_tests;
#[cfg(test)]
mod tests;
