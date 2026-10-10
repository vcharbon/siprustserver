//! Restore hygiene for replicated timer sets: the single seam every
//! failover/reclaim hydration path runs `call.timers` through before re-arming
//! them into this node's `TimerService` (clock-skew hardening + keepalive
//! cohort smoothing). Pure functions over `Vec<TimerEntry>` — the `timers.rs`
//! driver stays untouched and `(p,b)`-causal reconciliation remains the sole
//! correctness mechanism (ADR-0014).

use call::helpers::cap_keepalive_fire_at;
use call::{TimerEntry, TimerType};

/// Strip a stale `KeepaliveTimeout` from a timer set hydrated off a replica
/// snapshot (reclaim or reactive takeover).
///
/// A `KeepaliveTimeout` guards an in-flight keepalive OPTIONS *client
/// transaction* — armed when the OPTIONS is sent (`keepalive` rule) and
/// cancelled the instant its 200 lands (`absorb-options-200`). It exists on the
/// wire for only the round-trip, but a flush that catches that window
/// replicates it, so a `bak:`/`pri:` snapshot can carry an *armed*
/// `KeepaliveTimeout`. When that snapshot is hydrated onto a different node,
/// the client transaction it guarded **died with the crashed node** — its 200
/// can never arrive to cancel it, and its `fire_at` (minted on the dead node's
/// clock) is typically already past-due, so `restore` would fire it on the next
/// tick and the `keepalive-timeout` rule would BYE *both* legs of a perfectly
/// healthy long hold. The hydrated call re-probes safely on its own schedule:
/// its `Keepalive` timer fires a *fresh* OPTIONS and arms a *fresh*
/// `KeepaliveTimeout` against the live node's clock — dropping the stale guard
/// loses nothing. Purely local timer hygiene — no clock/settle, no `(p,b)`
/// interaction (ADR-0014 untouched).
fn drop_stale_keepalive_timeout(timers: &mut Vec<TimerEntry>) {
    timers.retain(|t| !matches!(t.timer_type, TimerType::KeepaliveTimeout));
}

/// Deterministic per-`callRef` hash (FNV-1a, 64-bit) used to de-correlate a
/// rehydrated keepalive cohort in [`smooth_keepalives`]. Deterministic (no random
/// seed) so the same call always lands in the same slot of its interval — a reboot
/// re-pass is idempotent — yet distinct refs spread uniformly. NOT for security.
fn stable_jitter(call_ref: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in call_ref.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Re-spread a reclaimed call's `Keepalive` deadlines for the bulk reboot sweep
/// (`reclaim::reclaim_all`) — load management only, never correctness (`(p,b)`
/// reconciliation makes any incidental keepalive overlap non-corrupting; ADR-0014).
/// Two cohorts, both de-synchronised so a rehydrated partition does not re-probe in
/// a single burst that overruns the single-task front proxy:
/// - **Past-due** (`fire_at <= now`): oldest-first over `[0, l_max/speedup]`
///   (optionally capped) so the most-overdue (most at risk of a UAC keepalive
///   timeout) re-probes first and the backlog drains bounded to `speedup`× cadence.
/// - **Future-dated** (`fire_at > now`): a *clean* reboot rehydrates ~the whole
///   partition at one instant with deadlines clustered inside one interval, so left
///   untouched they fire as a synchronised OPTIONS burst one cadence later — large
///   enough to throttle INVITE forwarding through the front proxy. Spread each call
///   into `[now, fire_at]` by [`stable_jitter`]. This only ever moves a probe
///   EARLIER (an early keepalive is harmless; *delaying* one risks the UAC
///   keepalive timeout — the loss we avoid), so there is still no settle/handback
///   floor.
///
/// Non-`Keepalive` timers keep their absolute deadline.
fn smooth_keepalives(
    timers: &mut [TimerEntry],
    call_ref: &str,
    now_ms: i64,
    l_max: i64,
    speedup: i64,
    cap_ms: Option<i64>,
) {
    for t in timers.iter_mut() {
        if !matches!(t.timer_type, TimerType::Keepalive) {
            continue;
        }
        let l = now_ms - t.fire_at;
        if l > 0 {
            let mut offset = (l_max - l) / speedup;
            if let Some(cap) = cap_ms {
                offset = offset.min(cap);
            }
            t.fire_at = now_ms + offset;
        } else {
            let window = t.fire_at - now_ms;
            if window > 0 {
                t.fire_at = now_ms + (stable_jitter(call_ref) % window as u64) as i64;
            }
        }
    }
}

/// Enforce the ledger's one-interval ceiling ([`cap_keepalive_fire_at`]) on every
/// restored `Keepalive` — the seam's half of the invariant the arming sites assert.
///
/// A deadline beyond the cadence is an origin-frame residual [`reanchor_timers`]
/// could not remove. Applies on every hydration path; moves a probe only earlier,
/// so no settle or handback is introduced (ADR-0014 untouched).
fn cap_future_keepalives(timers: &mut [TimerEntry], now_ms: i64, keepalive_interval_ms: i64) {
    for t in timers.iter_mut() {
        if matches!(t.timer_type, TimerType::Keepalive) {
            t.fire_at = cap_keepalive_fire_at(t.fire_at, now_ms, keepalive_interval_ms);
        }
    }
}

/// Re-anchor **deadband** (ms): a persisted `skew_offset_ms` whose magnitude is
/// below this is NOT applied — it is dominated by replication transit latency +
/// clock jitter, not a genuine inter-node clock disagreement worth correcting.
/// Under the single-clock harness `receiver_now − origin_now` is pure latency
/// with ZERO real skew, so the re-anchor is a *no-op* there and a keepalive
/// keeps the deadline the harness's strict SIP-transparency oracle expects. A
/// real host clock STEP is interval-sized
/// (≥ the keepalive cadence, hundreds of seconds), so it clears this deadband by
/// orders of magnitude. Correcting only skew that materially exceeds latency is
/// also strictly better in production: sub-second offsets do not meaningfully
/// move a 300 s keepalive / 150 s setup timer.
const REANCHOR_DEADBAND_MS: i64 = 1_000;

/// Add the receive-time wall-clock `skew_offset_ms` to every timer's absolute
/// `fire_at` (clock-skew hardening), when the offset clears [`REANCHOR_DEADBAND_MS`].
/// Each replicated `TimerEntry.fire_at` is an epoch-ms deadline minted on the
/// ORIGIN node's clock; the offset (`receiver_now_ms − origin_now_ms`, persisted at
/// replica-put time) shifts it into THIS node's clock frame, so the driver's
/// `fire_at − now_ms` reconstruction bounds restore skew to ~replication latency
/// instead of trusting the dead node's clock unboundedly. A sub-deadband or `0`
/// offset (locally-originated body, negligible skew, or already applied by
/// `reclaim::reclaim_all`) is a no-op. This makes ALL downstream past-due math
/// skew-corrected. No `(p,b)` interaction — accuracy only (ADR-0014 untouched).
pub(super) fn reanchor_timers(timers: &mut [TimerEntry], skew_offset_ms: i64) {
    if skew_offset_ms.abs() < REANCHOR_DEADBAND_MS {
        return;
    }
    for t in timers.iter_mut() {
        t.fire_at += skew_offset_ms;
    }
}

/// **The single restore-hygiene seam** every failover/reclaim hydration path runs
/// a replicated timer set through before re-arming it into this node's
/// `TimerService` (clock-skew hardening). Accuracy/performance only — this
/// introduces NO wall-clock correctness rule, settle window, or handback. In
/// order:
///
/// 1. **Re-anchor** by `skew_offset_ms` ([`reanchor_timers`]) so every deadline is
///    in this node's clock frame — this alone fixes the skew-ahead
///    "OPTIONS-at-takeover" artifact and makes the cohort classification in
///    [`smooth_keepalives`] correct.
/// 2. **Drop stale `KeepaliveTimeout`** ([`drop_stale_keepalive_timeout`]) — the
///    OPTIONS it guarded died with the crashed node.
/// 3. **Ceiling — always** ([`cap_future_keepalives`]), and re-asserted after step
///    5: applied before the spread it bounds that spread's window, and after it
///    bounds the past-due catch-up offset (which scales with a batch-wide `l_max`
///    one poisoned deadline can inflate). Together: no restored call waits more
///    than a cadence for its first probe.
/// 4. **Defensive floor — ONLY when the offset is UNKNOWN** (`None`: a path that
///    could not re-anchor). A `Keepalive` past-due by ≥ 1× `keepalive_interval` is
///    then treated as uncorrected skew/backlog pathology and re-based to `now +
///    (stable_jitter % interval)` rather than firing an immediate OPTIONS at
///    takeover (which would race the failed-over transaction). When the offset is
///    KNOWN (`Some`, including a well-synced `0`) it is trusted: a past-due
///    keepalive after correction is a normal catch-up and fires promptly (probe
///    the recovered peer) — this is what keeps the single-clock harness
///    transparent (skew is a known 0, so reclaim keeps the source's OPTIONS
///    timing token-for-token). Deterministic per `(call_ref, timer id)` so a
///    reboot re-pass is idempotent.
/// 5. **Cohort smoothing** ([`smooth_keepalives`]) when `smoothing` is requested
///    (the bulk reboot sweep).
pub(super) fn sanitize_restored_timers(
    timers: &mut Vec<TimerEntry>,
    call_ref: &str,
    now_ms: i64,
    skew_offset_ms: Option<i64>,
    keepalive_interval_ms: i64,
    smoothing: Option<Smoothing>,
) {
    // 1. Re-anchor into this node's clock frame when the offset is KNOWN.
    if let Some(offset) = skew_offset_ms {
        reanchor_timers(timers, offset);
    }
    // 2. Stale keepalive-timeout hygiene.
    drop_stale_keepalive_timeout(timers);
    // 3. Ceiling — every path, so the spread in step 5 is bounded to one interval.
    if keepalive_interval_ms > 0 {
        cap_future_keepalives(timers, now_ms, keepalive_interval_ms);
    }
    // 4. Defensive floor — only for an UNKNOWN offset (see the doc above). Skipped
    //    when the offset is known so a well-anchored past-due keepalive fires
    //    promptly (single-clock transparency).
    if skew_offset_ms.is_none() && keepalive_interval_ms > 0 {
        for t in timers.iter_mut() {
            if !matches!(t.timer_type, TimerType::Keepalive) {
                continue;
            }
            if now_ms - t.fire_at >= keepalive_interval_ms {
                let jitter =
                    (stable_jitter(call_ref) ^ stable_jitter(&t.id)) % keepalive_interval_ms as u64;
                t.fire_at = now_ms + jitter as i64;
            }
        }
    }
    // 5. Cohort smoothing (bulk reboot sweep only). Runs AFTER re-anchoring so its
    //    past-due/future classification keys on skew-corrected deadlines, then
    //    re-asserts the step-3 ceiling over its past-due catch-up offsets.
    if let Some(s) = smoothing {
        smooth_keepalives(timers, call_ref, s.now_ms, s.l_max, s.speedup, s.cap_ms);
        if keepalive_interval_ms > 0 {
            cap_future_keepalives(timers, now_ms, keepalive_interval_ms);
        }
    }
}

/// Cohort-smoothing parameters for the bulk reboot sweep, passed through the
/// [`sanitize_restored_timers`] seam. `None` (a single reactive straggler /
/// on-demand reclaim) skips smoothing — there is no cohort to de-correlate.
#[derive(Debug, Clone, Copy)]
pub(super) struct Smoothing {
    pub(super) now_ms: i64,
    pub(super) l_max: i64,
    pub(super) speedup: i64,
    pub(super) cap_ms: Option<i64>,
}

#[cfg(test)]
mod tests;
