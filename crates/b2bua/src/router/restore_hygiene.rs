//! Restore hygiene for replicated timer sets: the single seam every
//! failover/reclaim hydration path runs `call.timers` through before re-arming
//! them into this node's `TimerService` (clock-skew hardening + keepalive
//! cohort smoothing). Pure functions over `Vec<TimerEntry>` — the `timers.rs`
//! driver stays untouched and `(p,b)`-causal reconciliation remains the sole
//! correctness mechanism (ADR-0014).

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

/// Re-anchor **deadband** (ms): a persisted `skew_offset_ms` whose magnitude is
/// below this is NOT applied — it is dominated by replication transit latency +
/// clock jitter, not a genuine inter-node clock disagreement worth correcting.
/// This is what keeps the re-anchor a *no-op* under the single-clock harness,
/// whose coarse `advance` (100 ms chunks + settles between replication hops)
/// inflates `receiver_now − origin_now` to a few hundred ms of pure latency with
/// ZERO real skew — perturbing a keepalive by that latency breaks the harness's
/// strict SIP-transparency oracle. A real host clock STEP is interval-sized
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
/// 3. **Defensive floor — ONLY when the offset is UNKNOWN** (`None`: a path that
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
/// 4. **Cohort smoothing** ([`smooth_keepalives`]) when `smoothing` is requested
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
    // 3. Defensive floor — only for an UNKNOWN offset (see the doc above). Skipped
    //    when the offset is known so a well-anchored past-due keepalive fires
    //    promptly (single-clock transparency).
    if skew_offset_ms.is_none() && keepalive_interval_ms > 0 {
        for t in timers.iter_mut() {
            if !matches!(t.timer_type, TimerType::Keepalive) {
                continue;
            }
            if now_ms - t.fire_at >= keepalive_interval_ms {
                let jitter = (stable_jitter(call_ref) ^ stable_jitter(&t.id))
                    % keepalive_interval_ms as u64;
                t.fire_at = now_ms + jitter as i64;
            }
        }
    }
    // 4. Cohort smoothing (bulk reboot sweep only). Runs AFTER re-anchoring so its
    //    past-due/future classification keys on skew-corrected deadlines.
    if let Some(s) = smoothing {
        smooth_keepalives(timers, call_ref, s.now_ms, s.l_max, s.speedup, s.cap_ms);
    }
}

/// Cohort-smoothing parameters for the bulk reboot sweep, passed through the
/// [`sanitize_restored_timers`] seam. `None` (a single reactive straggler /
/// on-demand reclaim) skips smoothing — there is no cohort to de-correlate.
#[derive(Clone, Copy)]
pub(super) struct Smoothing {
    pub(super) now_ms: i64,
    pub(super) l_max: i64,
    pub(super) speedup: i64,
    pub(super) cap_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::CallEvent;
    use crate::timers::TimerService;
    use sip_clock::Clock;
    use std::time::Duration;

    fn keepalive(fire_at: i64) -> TimerEntry {
        TimerEntry { id: "Keepalive".into(), timer_type: TimerType::Keepalive, fire_at, leg_id: None }
    }
    fn keepalive_timeout(leg: &str, fire_at: i64) -> TimerEntry {
        TimerEntry {
            id: format!("KeepaliveTimeout:{leg}"),
            timer_type: TimerType::KeepaliveTimeout,
            fire_at,
            leg_id: Some(leg.into()),
        }
    }

    // The reclaim/takeover hygiene: a snapshot caught mid-keepalive-round-trip
    // carries an armed `KeepaliveTimeout`; restoring it verbatim onto the
    // reclaiming/taking-over node fires it (its guarded OPTIONS died with the
    // crashed node) and BYEs a healthy long hold. The fix strips it; the next
    // `Keepalive` re-probes fresh. Asserts both the stripping AND that the
    // remaining `Keepalive` survives.
    #[test]
    fn drop_stale_keepalive_timeout_strips_only_the_timeout() {
        let mut timers = vec![
            keepalive(300_000),
            keepalive_timeout("a", 35_000),
            keepalive_timeout("b-1", 35_000),
            TimerEntry { id: "GlobalDuration".into(), timer_type: TimerType::GlobalDuration, fire_at: 3_600_000, leg_id: None },
        ];
        drop_stale_keepalive_timeout(&mut timers);
        assert!(
            timers.iter().all(|t| !matches!(t.timer_type, TimerType::KeepaliveTimeout)),
            "every KeepaliveTimeout is stripped from the reclaimed snapshot",
        );
        assert!(
            timers.iter().any(|t| matches!(t.timer_type, TimerType::Keepalive)),
            "the Keepalive (re-probe) timer is kept — the call re-probes on its own schedule",
        );
        assert!(
            timers.iter().any(|t| matches!(t.timer_type, TimerType::GlobalDuration)),
            "unrelated timers (GlobalDuration) are kept",
        );
    }

    /// A service-owned timer (`TimerType::Service`) rides the restore-hygiene
    /// seam GENERICALLY: it is kept (never stripped like `KeepaliveTimeout`),
    /// re-anchored by the persisted skew offset like every timer, and left out
    /// of the keepalive-only floor/smoothing — so a takeover node restores a
    /// downstream service's watchdog (e.g. the 18x deadline) with the same
    /// clock-skew bounds as core timers.
    #[test]
    fn service_timer_survives_restore_hygiene_and_is_reanchored() {
        let svc = TimerType::service(call::MachineId::new("routing"), "timer18x");
        let now = 1_000_000;
        let skew = 45_000; // receiver_now − origin_now (clears the deadband)
        let raw_fire_at = now - skew + 5_000; // origin-frame deadline
        let mut timers = vec![
            TimerEntry {
                id: svc.timer_id(None),
                timer_type: svc.clone(),
                fire_at: raw_fire_at,
                leg_id: None,
            },
            keepalive_timeout("b-1", now - skew + 1_000),
        ];
        let smoothing = Some(Smoothing { now_ms: now, l_max: 0, speedup: 10, cap_ms: None });
        sanitize_restored_timers(&mut timers, "w1|svc|svc", now, Some(skew), 300_000, smoothing);
        assert_eq!(timers.len(), 1, "service timer kept; stale KeepaliveTimeout stripped");
        assert_eq!(timers[0].timer_type, svc);
        assert_eq!(
            timers[0].fire_at,
            now + 5_000,
            "re-anchored into this node's clock frame exactly like a core timer",
        );
    }

    // REPRO of the residual endurance loss at the timer layer: a *past-due*
    // `KeepaliveTimeout` restored from a pre-crash snapshot fires IMMEDIATELY
    // (`restore` clamps `fire_at <= now` to a next-tick fire) — this is the
    // event that drives `keepalive-timeout` → BYE on a healthy reclaimed call.
    // With the fix the stripped set restores NOTHING that fires at now=reclaim.
    #[tokio::test(start_paused = true)]
    async fn past_due_keepalive_timeout_fires_on_restore_without_the_fix() {
        let clock = Clock::test_at(0);
        // now is well past the snapshot's KeepaliveTimeout deadline (the dead
        // node armed it +120 s before its clock, which is in our past).
        tokio::time::advance(Duration::from_millis(200_000)).await;

        // WITHOUT the fix: the verbatim snapshot includes the past-due timeout.
        let (timers, mut fire_rx) = TimerService::spawn(clock.clone());
        let snapshot = vec![keepalive(500_000), keepalive_timeout("b-1", 120_000)];
        timers.restore(snapshot.clone(), "w0|cid|tag".into()).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        let fired = fire_rx.recv().await.unwrap();
        match fired {
            CallEvent::Timer { timer_type, .. } => assert_eq!(
                timer_type,
                TimerType::KeepaliveTimeout,
                "BUG: the stale past-due KeepaliveTimeout fires on reclaim → spurious BYE",
            ),
            _ => panic!("expected a timer event"),
        }

        // WITH the fix: the same snapshot, stripped, fires NOTHING at reclaim time
        // (the future Keepalive is the only survivor and is far off).
        let (timers2, mut fire_rx2) = TimerService::spawn(clock);
        let mut fixed = snapshot;
        drop_stale_keepalive_timeout(&mut fixed);
        timers2.restore(fixed, "w0|cid|tag2".into()).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(
            fire_rx2.try_recv().is_err(),
            "FIX: no spurious keepalive-timeout fires on reclaim; the call survives",
        );
    }

    // REPRO of the endurance throughput collapse at the smoothing layer: a clean
    // reboot rehydrates the whole partition at one instant with future-dated
    // keepalive deadlines clustered in one interval. WITHOUT the future-dated
    // branch they all keep the SAME `fire_at` and fire as one burst a cadence
    // later (an OPTIONS spike that saturates the front proxy); WITH it each
    // call's keepalive is spread into `[now, fire_at]` by a per-call hash, so a
    // 1000-call cohort no longer shares a single deadline.
    #[test]
    fn future_dated_keepalive_cohort_is_de_correlated() {
        let now = 1_000_000;
        let deadline = now + 300_000; // whole cohort clustered at +300 s (one interval)
        let speedup = 10;
        let mut fire_ats = std::collections::HashSet::new();
        for i in 0..1000 {
            // Distinct call_ref per call, identical clustered keepalive deadline.
            let call_ref = format!("w1|call-{i}|tag-{i}");
            let mut timers = vec![keepalive(deadline)];
            smooth_keepalives(&mut timers, &call_ref, now, 0, speedup, None);
            let fa = timers[0].fire_at;
            assert!(
                (now..=deadline).contains(&fa),
                "spread keepalive stays in [now, original deadline] (never delayed past it): {fa}",
            );
            fire_ats.insert(fa);
        }
        // De-correlation: a synchronised cohort would collapse to ONE deadline;
        // the fix scatters them across the interval (allow a few hash collisions).
        assert!(
            fire_ats.len() > 900,
            "cohort de-correlated: {} distinct fire_at over 1000 calls (was 1 before the fix)",
            fire_ats.len(),
        );
        // Determinism: re-running the same ref yields the SAME slot (idempotent
        // reboot re-pass — a second reclaim scan must not re-scatter live timers).
        let mut a = vec![keepalive(deadline)];
        let mut b = vec![keepalive(deadline)];
        smooth_keepalives(&mut a, "w1|call-7|tag-7", now, 0, speedup, None);
        smooth_keepalives(&mut b, "w1|call-7|tag-7", now, 0, speedup, None);
        assert_eq!(a[0].fire_at, b[0].fire_at, "stable_jitter is deterministic per call_ref");
    }

    // The past-due (overdue) path: oldest-first, bounded to speedup× cadence —
    // most-overdue fires first (smallest offset).
    #[test]
    fn past_due_keepalives_keep_oldest_first_schedule() {
        let now = 1_000_000;
        let l_max = 200_000; // most-overdue gap across the batch
        let speedup = 10;
        // Most-overdue (fire_at = now - 200s, l = l_max): offset (l_max-l)/speedup = 0.
        let mut oldest = vec![keepalive(now - 200_000)];
        smooth_keepalives(&mut oldest, "w1|a|a", now, l_max, speedup, None);
        assert_eq!(oldest[0].fire_at, now, "most-overdue re-probes first (offset 0)");
        // Least-overdue of the batch (fire_at = now - 100s, l = 100s): offset
        // (l_max - l)/speedup = (200s - 100s)/10 = 10s later.
        let mut newer = vec![keepalive(now - 100_000)];
        smooth_keepalives(&mut newer, "w1|b|b", now, l_max, speedup, None);
        assert_eq!(newer[0].fire_at, now + 10_000, "less-overdue drains later, bounded by speedup");
    }

    // ── clock-skew hardening: the restore-hygiene seam ──────────────────────

    /// smooth_keepalives / l_max classification over SKEW-CORRECTED offsets. A
    /// reclaimer anchored +45 s AHEAD of the origin reads a keepalive that is
    /// genuinely FUTURE-dated (fire_at = origin_now + 300 s) as if it were
    /// past-due, because the raw `fire_at` is in the reclaimer's past-frame. WITHOUT
    /// re-anchoring, `smooth_keepalives` would classify it past-due and compress it
    /// into the catch-up band. The seam re-anchors first (+45 s), so it is correctly
    /// seen as future-dated and de-correlated into `[now, fire_at]` — never crushed
    /// to `now`.
    #[test]
    fn seam_reanchor_keeps_future_cohort_out_of_the_catchup_band() {
        // The reclaimer's clock frame.
        let now = 1_000_000;
        // Origin minted the keepalive 300 s out on ITS clock, which is 45 s BEHIND
        // the reclaimer → the reclaimer received it with skew_offset = +45_000, and
        // the raw fire_at (origin frame) reads as `now - 45_000 + 300_000` once we
        // subtract the offset back out. Model the raw (pre-correction) fire_at:
        let skew = 45_000; // receiver_now − origin_now
        let raw_fire_at = now - skew + 300_000; // origin-frame deadline as stored
        // Before correction this is `now + 255_000` → looks 45 s "closer" but still
        // future; a LARGER skew would flip it past-due. Use a skew big enough to
        // flip it: origin minted it only 30 s out.
        let raw_fire_at_flip = now - skew + 30_000; // = now - 15_000 → PAST-DUE raw!
        let mut timers = vec![keepalive(raw_fire_at_flip)];
        // l_max computed over the CORRECTED deadline (as reclaim_all now does):
        // corrected = raw + skew = now + 30_000 (future) → not past-due → l_max 0.
        let smoothing = Some(Smoothing { now_ms: now, l_max: 0, speedup: 10, cap_ms: None });
        sanitize_restored_timers(&mut timers, "w1|c|c", now, Some(skew), 300_000, smoothing);
        let fa = timers[0].fire_at;
        assert!(
            fa >= now,
            "corrected future keepalive is NOT crushed to a past-due catch-up slot: {fa} < {now}",
        );
        assert!(
            fa <= now + 30_000,
            "de-correlated within [now, corrected deadline], not the raw past-due frame: {fa}",
        );
        // Control: the SAME raw timers WITHOUT re-anchoring (offset None) would be
        // seen past-due and smoothed toward the catch-up band at `now`.
        let mut raw_timers = vec![keepalive(raw_fire_at_flip)];
        smooth_keepalives(&mut raw_timers, "w1|c|c", now, 15_000, 10, None);
        assert!(
            raw_timers[0].fire_at <= now,
            "uncorrected: the future keepalive IS mis-compressed to the catch-up band",
        );
        let _ = raw_fire_at;
    }

    /// The defensive floor. When the offset is UNKNOWN (`None`, a path that
    /// could not re-anchor), a keepalive past-due by ≥ 1× interval is re-based
    /// to within one interval of `now` (not fired immediately, which would race a
    /// failed-over transaction). Deterministic per (call_ref, timer id).
    #[test]
    fn defensive_floor_rebases_deep_past_due_keepalive_within_one_interval() {
        let now = 1_000_000;
        let interval = 300_000;
        // Past-due by 2× interval — deep skew/backlog pathology.
        let make = || vec![keepalive(now - 2 * interval)];

        let mut t = make();
        // Unknown offset (None) → floor engages; no smoothing.
        sanitize_restored_timers(&mut t, "w1|d|d", now, None, interval, None);
        let fa = t[0].fire_at;
        assert!(
            (now..now + interval).contains(&fa),
            "deep-past-due keepalive re-based into [now, now+interval): {fa}",
        );
        assert!(fa > now, "not fired immediately at now (which would race the failed-over txn)");

        // Determinism: same (call_ref, id) → same slot (idempotent reboot re-pass).
        let mut t2 = make();
        sanitize_restored_timers(&mut t2, "w1|d|d", now, None, interval, None);
        assert_eq!(t2[0].fire_at, fa, "floor is deterministic per call_ref/id");
        // Distinct call_ref → (very likely) a different slot.
        let mut t3 = make();
        sanitize_restored_timers(&mut t3, "w1|different|x", now, None, interval, None);
        // (Not asserting inequality hard — hash collisions are possible — but the
        // slot must still be within the interval.)
        assert!((now..now + interval).contains(&t3[0].fire_at));

        // KNOWN offset (Some, even 0) → floor SKIPPED: a well-anchored past-due
        // keepalive fires promptly (single-clock transparency).
        let mut t4 = make();
        sanitize_restored_timers(&mut t4, "w1|d|d", now, Some(0), interval, None);
        assert_eq!(
            t4[0].fire_at,
            now - 2 * interval,
            "known offset trusts the deadline — past-due fires promptly, floor does NOT engage",
        );
    }

    /// SetupTimeout (a non-keepalive policy deadline) under skew. A ringing call
    /// partway through its setup window fails over; the restored SetupTimeout
    /// must land at its TRUE remaining time in the takeover node's clock frame,
    /// NOT reaped early (skew-ahead) nor extended by the skew (skew-behind). The
    /// seam re-anchors ALL timer classes, so this holds for the SetupTimeout
    /// ledger timer exactly as for the keepalive.
    #[test]
    fn setup_timeout_is_reanchored_not_reaped_early_or_extended() {
        // Origin minted SetupTimeout 150 s out at ring-start; the call is 100 s in,
        // so 50 s of setup window remains (origin-frame fire_at = origin_ring + 150).
        // Model the restored entry as it arrives on the takeover node, whose `now`
        // is 100 s past the (origin-frame) ring start.
        let setup = |fire_at: i64| TimerEntry {
            id: format!("{:?}", TimerType::SetupTimeout),
            timer_type: TimerType::SetupTimeout,
            fire_at,
            leg_id: None,
        };

        // ── skew-AHEAD (+40 s): the takeover node's clock is 40 s ahead of the
        //    origin. Raw fire_at (origin frame) = now_local − 40_000 + 50_000 (still
        //    50 s of true window). WITHOUT re-anchor it would read 10 s out (reaped
        //    40 s early); WITH +40 s re-anchor it lands at the true 50 s. ──────────
        let now = 1_000_000;
        let skew_ahead = 40_000;
        let raw_fire_at = now - skew_ahead + 50_000; // origin-frame deadline
        let mut t = vec![setup(raw_fire_at)];
        sanitize_restored_timers(&mut t, "w1|s|s", now, Some(skew_ahead), 300_000, None);
        assert_eq!(
            t[0].fire_at,
            now + 50_000,
            "skew-ahead: SetupTimeout lands at the TRUE remaining 50 s, not reaped 40 s early",
        );

        // ── skew-BEHIND (−40 s): the takeover node's clock is 40 s behind the
        //    origin. Raw fire_at (origin frame) = now_local + 40_000 + 50_000.
        //    WITHOUT re-anchor it would read 90 s out (extended by the skew); WITH
        //    −40 s re-anchor it lands at the true 50 s. ─────────────────────────────
        let skew_behind = -40_000;
        let raw_fire_at_b = now - skew_behind + 50_000; // = now + 90_000 (origin frame)
        let mut tb = vec![setup(raw_fire_at_b)];
        sanitize_restored_timers(&mut tb, "w1|s|s", now, Some(skew_behind), 300_000, None);
        assert_eq!(
            tb[0].fire_at,
            now + 50_000,
            "skew-behind: SetupTimeout lands at the TRUE remaining 50 s, not extended by 40 s",
        );
    }

    /// The re-anchor itself: a known offset shifts every timer's absolute deadline
    /// into the local clock frame; a sub-deadband offset is a no-op (latency, not
    /// skew — keeps the single-clock harness transparent).
    #[test]
    fn reanchor_applies_above_deadband_and_ignores_below() {
        let mut t = vec![keepalive(1_000_000), keepalive_timeout("a", 900_000)];
        reanchor_timers(&mut t, 30_000); // +30 s real skew
        assert_eq!(t[0].fire_at, 1_030_000);
        assert_eq!(t[1].fire_at, 930_000, "ALL timer classes re-anchored, not just keepalive");

        let mut small = vec![keepalive(1_000_000)];
        reanchor_timers(&mut small, 200); // 200 ms — below the 1 s deadband
        assert_eq!(small[0].fire_at, 1_000_000, "sub-deadband latency offset is a no-op");
    }
}
