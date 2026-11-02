//! Restore hygiene for replicated calls: the re-anchor that moves every
//! instant of a copy into this node's clock frame (ADR-0055 §9), the keepalive
//! probes a restored copy forgets, the single seam every failover/reclaim
//! hydration path runs `call.timers` through before re-arming them into this
//! node's `TimerService` (keepalive ceiling + cohort smoothing), and the
//! ladders a meeting takes going on from where they stand.
//! Pure functions — the `timers.rs` driver stays untouched and
//! `(p,b)`-causal reconciliation remains the sole correctness mechanism
//! (ADR-0014).

use b2bua_core::config::B2buaConfig;
use b2bua_core::rules::actions::ladder_bound;
use call::helpers::instants::reanchor_offset;
use call::helpers::{advance_ladder, cap_keepalive_fire_at, cease_shown_ladder, shift_instants};
use call::{Call, Ceased, Obligation, TimerEntry, TimerType};

/// Forget the keepalive probes of `call`, a copy restored on this node at
/// `now_ms`, that it cannot carry on. A probe inside its Timer F is its client
/// transaction, re-sent under its CSeq and branch so the dialog's sequence
/// stays contiguous whether or not it left (RFC 3261 §12.2.1.1, §17.1.2.2),
/// its answer steered to this node with the dead sender's; it is kept unless
/// `this` is not its sender and `sender_alive` (a reclaim's or a parked call's
/// copy carrying the live backup's probe, whose answer reaches the backup).
/// A probe past its Timer F is forgotten either way: its answer went to the
/// node that sent it, and the copy probes again on its interval. Whether any
/// was forgotten.
pub(super) fn forget_keepalive_probes(
    call: &mut Call,
    now_ms: i64,
    this: &str,
    sender_alive: bool,
) -> bool {
    call::helpers::drop_keepalive_records(call, |r| {
        let spent = r.deadline_ms.is_some_and(|at| at <= now_ms);
        spent || (sender_alive && r.relayed_by.ordinal != this)
    })
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

/// `seed` brought into `0..span` ms; zero for a span that is not positive.
fn jitter_within(seed: u64, span: i64) -> i64 {
    match u64::try_from(span) {
        // Below `span`, so it fits an i64.
        Ok(span) if span > 0 => i64::try_from(seed % span).unwrap_or(0),
        _ => 0,
    }
}

/// Re-spread a reclaimed call's `Keepalive` deadlines for the bulk reboot sweep
/// (`reclaim::arm_parked`) — load management only, never correctness (`(p,b)`
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
        if *t.timer_type() != TimerType::Keepalive {
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
                t.fire_at = now_ms + jitter_within(stable_jitter(call_ref), window);
            }
        }
    }
}

/// Enforce the ledger's one-interval ceiling ([`cap_keepalive_fire_at`]) on every
/// restored `Keepalive` — the seam's half of the invariant the arming sites assert.
///
/// A deadline beyond the cadence is an origin-frame residual [`reanchor`] could
/// not remove. Applies on every hydration path; moves a probe only earlier,
/// so no settle or handback is introduced (ADR-0014 untouched).
fn cap_future_keepalives(timers: &mut [TimerEntry], now_ms: i64, keepalive_interval_ms: i64) {
    for t in timers.iter_mut() {
        if *t.timer_type() == TimerType::Keepalive {
            t.fire_at = cap_keepalive_fire_at(t.fire_at, now_ms, keepalive_interval_ms);
        }
    }
}

/// Move every absolute instant of `call`, a copy read from the replicating
/// store, into this node's clock frame (ADR-0055 §9): `skew_offset_ms` is the
/// store's offset for the body (this node's clock minus the clock its instants
/// were written on, composed across every hop), as far as
/// [`reanchor_offset`] applies it. Run once per copy read, before any of its
/// instants is compared or re-armed, so every downstream past-due reading is
/// skew-corrected. No `(p,b)` interaction — accuracy only (ADR-0014 untouched).
pub(super) fn reanchor(call: &mut Call, skew_offset_ms: i64) {
    shift_instants(call, reanchor_offset(skew_offset_ms));
}

/// **The single restore-hygiene seam** every failover/reclaim hydration path runs
/// a replicated timer set through before re-arming it into this node's
/// `TimerService`. The set is already in this node's clock frame ([`reanchor`]),
/// so a past-due keepalive is a normal catch-up and fires promptly.
/// Accuracy/performance only — this introduces NO wall-clock correctness rule,
/// settle window, or handback. In order:
///
/// 1. **Ceiling — always** ([`cap_future_keepalives`]), and re-asserted after step
///    2: applied before the spread it bounds that spread's window, and after it
///    bounds the past-due catch-up offset (which scales with a batch-wide `l_max`
///    one poisoned deadline can inflate). Together: no restored call waits more
///    than a cadence for its first probe.
/// 2. **Cohort smoothing** ([`smooth_keepalives`]) when `smoothing` is requested
///    (the bulk reboot sweep).
pub(super) fn sanitize_restored_timers(
    timers: &mut [TimerEntry],
    call_ref: &str,
    now_ms: i64,
    keepalive_interval_ms: i64,
    smoothing: Option<Smoothing>,
) {
    // 1. Ceiling — every path, so the spread in step 2 is bounded to one interval.
    if keepalive_interval_ms > 0 {
        cap_future_keepalives(timers, now_ms, keepalive_interval_ms);
    }
    // 2. Cohort smoothing (bulk reboot sweep only), on deadlines already in this
    //    node's frame, then the step-1 ceiling over its past-due catch-up offsets.
    if let Some(s) = smoothing {
        smooth_keepalives(timers, call_ref, s.now_ms, s.l_max, s.speedup, s.cap_ms);
        if keepalive_interval_ms > 0 {
            cap_future_keepalives(timers, now_ms, keepalive_interval_ms);
        }
    }
}

/// Step every rung in `fresh` — ladder timers a meeting takes from the other
/// owner's copy of `call` — whose deadline is at or before `now_ms` along its
/// ladder to its first rung after `now_ms`, `call`'s rung index with it, so
/// the ladder goes on from where it stands and is never repeated at once
/// (RFC 3261 §13.3.1.4, RFC 3262 §3). This assumes the other owner sent the
/// rungs due before the meeting; one it did not (it stopped between its
/// write and the rung) is skipped, a wider gap between two retransmissions,
/// which no RFC requirement forbids. A ladder with no rung left inside
/// `config`'s bound leaves `fresh` as a ladder ceasing does; a 2xx's give-up
/// stands.
pub(super) fn resume_met_ladders(
    call: &mut Call,
    fresh: &mut Vec<TimerEntry>,
    now_ms: i64,
    config: &B2buaConfig,
) {
    fresh.retain_mut(|t| {
        if *t.timer_type() != TimerType::Rung || t.fire_at > now_ms {
            return true;
        }
        let Some(obligation) = t.key.obligation_of().cloned() else { return true };
        let bound = ladder_bound(config, &obligation);
        while t.fire_at <= now_ms {
            let Some(next) = advance_ladder(call, &obligation, bound) else {
                if let Obligation::AwaitPrack { tag, rseq } = &obligation {
                    cease_shown_ladder(call, tag, *rseq, Ceased::Bound);
                }
                return false;
            };
            t.fire_at += call::millis_i64(next);
        }
        true
    });
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
