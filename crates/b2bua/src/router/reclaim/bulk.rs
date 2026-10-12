//! The bulk reboot reclaim and the one-call straggler reclaim: this node
//! re-serving its own `pri:{self}` partition (ADR-0011 X11 / ADR-0014).

use std::sync::Arc;

use call::TimerType;

use super::super::materialise::{materialise, Materialised, Origin, Reason};
use super::super::restore_hygiene::{reanchor_timers, Smoothing};
use super::super::undecided::{
    is_cancelled_setup, is_undecided, settle_cancelled_setup, settle_undecided,
};
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
    let mut calls = ctx.state.scan_partition(PartitionRole::Primary, own).await;
    let scanned = calls.len() as u64;
    // The cohort classification below (past-due vs future-dated) and `l_max`
    // read SKEW-CORRECTED deadlines: re-anchor every scanned copy by its own
    // persisted offset first. These copies are discarded; `materialise` re-reads
    // each body and re-anchors it once itself.
    for (call, skew) in calls.iter_mut() {
        reanchor_timers(&mut call.timers, *skew);
    }
    // L_max = the largest past-due keepalive gap across the whole partition, over
    // the now skew-corrected deadlines.
    let l_max = calls
        .iter()
        .flat_map(|(c, _)| c.timers.iter())
        .filter(|t| *t.timer_type() == TimerType::Keepalive)
        .map(|t| (now_ms - t.fire_at).max(0))
        .max()
        .unwrap_or(0);
    let smoothing = Smoothing {
        now_ms,
        l_max,
        speedup: ctx.config.keepalive_catchup_speedup.max(1),
        cap_ms: ctx.config.max_catchup_window_sec.map(|s| s * 1000),
    };
    let mut materialized = 0u64;
    for (call, _skew) in calls {
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

/// Reclaim one call of this node's `pri:{self}` partition into the live map
/// under its per-call lock (ADR-0011 X11). `smoothing = Some(_)` is the bulk
/// reboot sweep ([`reclaim_all`]); `None` a single straggler. Neither has a
/// message to serve, so a call still undecided, or a setup CANCELled before
/// its decision, is settled (ADR-0047). Returns
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
            if is_cancelled_setup(&call) {
                settle_cancelled_setup(ctx, call_ref, call).await;
            } else if let (Some(seeded), true) = (seeded, is_undecided(&call)) {
                settle_undecided(ctx, call_ref, call, seeded).await;
            }
            true
        }
        Materialised::Refused(Reason::Terminated) => true,
        _ => false,
    }
}
