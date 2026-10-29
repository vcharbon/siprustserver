//! The bulk reboot reclaim and the one-call straggler reclaim: this node
//! re-serving its own `pri:{self}` partition (ADR-0011 X11 / ADR-0014).

use std::sync::Arc;

use call::{Call, TimerType};

use super::super::materialise::{
    arm, has_invite_to_serve, materialise, Materialised, Origin, Reason,
};
use super::super::restore_hygiene::Smoothing;
use super::super::undecided::{is_undecided, settle_undecided};
use super::super::RouterCtx;
use crate::store::PartitionRole;

/// **Bulk reclaim, first pass** (ADR-0014, ADR-0055 §8): make every
/// `pri:{self}` call resident and park it, nothing armed, so nothing of it
/// leaves the node; a call with an INVITE to serve, or one its backup wrote,
/// is written once ([`announce_reclaim`]), so the backup's takeover copy of
/// it meets that write before the proxy routes anything to this node, and a
/// copy holding an INVITE it dialled plays on (ADR-0047 D7). It runs on bootstrap
/// completion, whether or not the node's own endpoint reads ready; the
/// parked calls are armed once it does ([`arm_parked`]). Each body is read
/// through the TTL-gated reclaim read, so a body whose TTL ran out is dead and
/// is never re-served: it counts in `scanned` and not in `materialized`.
pub(in crate::router) async fn park_all(ctx: &Arc<RouterCtx>) {
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
    for (copy, _skew) in calls {
        beat();
        let call_ref = &copy.call.call_ref;
        let _guard = ctx.state.lock(call_ref).await;
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
        active_after,
        duration_ms,
        "reboot reclaim"
    );
}

/// **Bulk reclaim, second pass**: arm every parked call once the node's own
/// endpoint reads ready (ADR-0055 §8, `router::readmission`) — its timers
/// re-armed, its transactions seeded, an undecided call refused (ADR-0047 D4).
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
pub(in crate::router) async fn arm_parked(ctx: &Arc<RouterCtx>) {
    let start_ms = ctx.clock.now_ms();
    let beat = || ctx.router_pulse.turning();
    let parked = ctx.state.parked();
    let calls: Vec<Call> = parked.iter().filter_map(|call_ref| ctx.state.peek(call_ref)).collect();
    let l_max = largest_keepalive_gap(&calls, start_ms, &beat);
    let smoothing = Smoothing {
        now_ms: start_ms,
        l_max,
        speedup: ctx.replica.keepalive_catchup_speedup.max(1),
        cap_ms: ctx.replica.max_catchup_window_sec.map(|s| s * 1000),
    };
    let mut armed = 0u64;
    for call_ref in &parked {
        beat();
        let _guard = ctx.state.lock(call_ref).await;
        let Some((call, seeded)) = arm(ctx, call_ref, Some(smoothing)).await else { continue };
        armed += 1;
        if let Some(seeded) = seeded.filter(|_| is_undecided(&call)) {
            settle_undecided(ctx, call_ref, call, seeded).await;
        }
    }
    let duration_ms = ctx.clock.now_ms() - start_ms;
    tracing::info!(
        node = observe::node(),
        armed,
        l_max_ms = l_max,
        duration_ms,
        "reclaimed calls armed"
    );
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
/// no message to serve, so a call still undecided is settled (ADR-0047), and a
/// call with an INVITE transaction to serve, or one its backup wrote, is
/// announced ([`announce_reclaim`]).
/// Returns `true` iff this pass did the work — freshly materialised, or a
/// deferred terminal discharged; `false` for a call already resident
/// (idempotent re-pass) or gone from the store.
pub(super) async fn reclaim_into_live(ctx: &Arc<RouterCtx>, call_ref: &str) -> bool {
    let _guard = ctx.state.lock(call_ref).await;
    match materialise(ctx, call_ref, Origin::Reclaim).await {
        Materialised::Served { call, seeded } => {
            match seeded {
                Some(seeded) if is_undecided(&call) => {
                    settle_undecided(ctx, call_ref, call, seeded).await;
                }
                _ => {
                    announce_reclaim(ctx, call);
                }
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
mod tests;
