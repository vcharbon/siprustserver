//! The bulk reboot reclaim and the one-call straggler reclaim: this node
//! re-serving its own `pri:{self}` partition (ADR-0011 X11 / ADR-0014).

use std::sync::Arc;

use call::{Call, TimerType};

use super::super::materialise::{has_invite_to_serve, materialise, Materialised, Origin, Reason};
use super::super::restore_hygiene::{reanchor, Smoothing};
use super::super::undecided::{is_undecided, settle_undecided};
use super::super::RouterCtx;
use crate::store::PartitionRole;

/// **Bulk reclaim** (ADR-0014): re-materialise every `pri:{self}`
/// call into the live map + re-arm its timers — what makes a rebooted primary
/// re-*serve* its partition, not just re-*store* it. The scan decodes the
/// partition to size the cohort; each body is then materialised through the
/// TTL-gated reclaim read, so a body whose TTL ran out is dead and is never
/// re-served: it counts in `scanned` and not in `materialized`.
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
pub(in crate::router) async fn reclaim_all(ctx: &Arc<RouterCtx>) {
    let start_ms = ctx.clock.now_ms();
    let now_ms = start_ms;
    let active_before = ctx.state.active_count() as u64;
    let own = &ctx.config.self_ordinal;
    // The whole turn works through the partition, one call at a time: each
    // call read, re-anchored or reclaimed is a piece of work of its own, so
    // the router's pulse shows the turn making progress, however large the
    // partition.
    let beat = || ctx.router_pulse.turning();
    let mut calls = ctx.state.scan_partition(PartitionRole::Primary, own, &beat).await;
    let scanned = calls.len() as u64;
    let l_max = largest_keepalive_gap(&mut calls, now_ms, &beat);
    let smoothing = Smoothing {
        now_ms,
        l_max,
        speedup: ctx.config.keepalive_catchup_speedup.max(1),
        cap_ms: ctx.config.max_catchup_window_sec.map(|s| s * 1000),
    };
    let mut materialized = 0u64;
    for (call, _skew) in calls {
        beat();
        if reclaim_into_live(ctx, &call.call_ref, Some(smoothing)).await {
            materialized += 1;
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
        active_after,
        l_max_ms = l_max,
        duration_ms,
        "reboot reclaim"
    );
}

/// `L_max`: the largest past-due keepalive gap across the scanned partition,
/// read on SKEW-CORRECTED deadlines — each copy is re-anchored by its own
/// persisted offset first. The copies are then only named; `materialise`
/// re-reads each body and re-anchors it once itself. `beat` runs once per call.
fn largest_keepalive_gap(calls: &mut [(call::Call, i64)], now_ms: i64, beat: &dyn Fn()) -> i64 {
    let mut l_max = 0;
    for (call, skew) in calls.iter_mut() {
        beat();
        reanchor(call, *skew);
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
/// under its per-call lock (ADR-0011 X11). `smoothing = Some(_)` is the bulk
/// reboot sweep ([`reclaim_all`]); `None` a single straggler. Neither has a
/// message to serve, so a call still undecided is settled (ADR-0047), and a
/// call with an INVITE transaction to serve is announced ([`announce_reclaim`]). Returns
/// `true` iff this pass did the work — freshly materialised, or a deferred
/// terminal discharged — so the caller meters per-pass reclaim completeness;
/// `false` for a call already resident (idempotent re-pass) or gone from the store.
pub(super) async fn reclaim_into_live(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    smoothing: Option<Smoothing>,
) -> bool {
    let _guard = ctx.state.lock(call_ref).await;
    match materialise(ctx, call_ref, Origin::Reclaim(smoothing)).await {
        Materialised::Served { call, seeded } => {
            match seeded {
                Some(seeded) if is_undecided(&call) => {
                    settle_undecided(ctx, call_ref, call, seeded).await;
                }
                _ if has_invite_to_serve(&call) => announce_reclaim(ctx, call),
                _ => {}
            }
            true
        }
        Materialised::Refused(Reason::Terminated | Reason::Unrecoverable) => true,
        _ => false,
    }
}

/// Write the reclaimed `call` once: this node serving the call again is the
/// change the write records, one `p` increment, and it reaches the backup's
/// Element as the primary's write, where a takeover copy of the call the
/// backup's departure sweep made stands down on it (ADR-0047 D7). Only a call
/// with an INVITE transaction to serve is written — a quiet call has no sweep
/// copy — so
/// the reclaim of a large partition stays write-free for those.
fn announce_reclaim(ctx: &Arc<RouterCtx>, call: Call) {
    ctx.state.update(call.clone());
    ctx.state.flush(&call);
}

#[cfg(test)]
mod tests;
