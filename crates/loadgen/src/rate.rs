//! The **runtime rate handle** — a shared, atomically-mutable offered-call-rate
//! target the CPS governor reads on EVERY slot, so the load rate can be re-targeted
//! live (`POST /rate?cps=<float>`) without restarting the run.
//!
//! The rate is stored as **milli-cps** (`cps × 1000`) in an [`AtomicU64`], so a
//! fractional rate survives and the whole handle is `Clone` + `Send + Sync` with
//! no lock. `0` is a distinguished value: it **pauses** new-call admission (the
//! governor parks until the rate is raised) while in-flight calls run untouched.
//!
//! The governor's re-anchoring (a rate change resets the fixed grid so a cut fires
//! no catch-up burst and a raise takes effect within one slot) lives in
//! [`crate::driver::Driver::run`], which owns the scheduling loop; this type is
//! just the shared cell + the `cps ⇄ millicps` conversion and clamping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

/// A shared, live-tunable offered-rate target (milli-cps under the hood). Cheap to
/// clone (an `Arc<AtomicU64>`); the governor holds one clone and the `/rate` HTTP
/// handler another.
#[derive(Clone, Debug)]
pub struct RateHandle {
    /// The target rate in **milli-cps** (`cps × 1000`), rounded. `0` = paused.
    millicps: Arc<AtomicU64>,
}

impl RateHandle {
    /// A handle initialized to `cps` (clamped to `>= 0`; NaN → 0).
    pub fn new(cps: f64) -> Self {
        let h = RateHandle { millicps: Arc::new(AtomicU64::new(0)) };
        h.set(cps);
        h
    }

    /// The current target, in calls per second.
    pub fn cps(&self) -> f64 {
        self.millicps.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// The current target in milli-cps (the raw stored form; `0` = paused).
    pub fn millicps(&self) -> u64 {
        self.millicps.load(Ordering::Relaxed)
    }

    /// Whether new-call admission is currently paused (`cps == 0`).
    pub fn is_paused(&self) -> bool {
        self.millicps.load(Ordering::Relaxed) == 0
    }

    /// Re-target the rate. A negative or NaN value is clamped to `0` (pause).
    /// Returns the value actually applied, in cps (so the HTTP handler can echo
    /// the clamped result).
    pub fn set(&self, cps: f64) -> f64 {
        let applied = if cps.is_finite() && cps > 0.0 { cps } else { 0.0 };
        // Round to the nearest milli-cps; a huge value saturates rather than wraps.
        let milli = (applied * 1000.0).round();
        let milli = if milli >= u64::MAX as f64 { u64::MAX } else { milli as u64 };
        self.millicps.store(milli, Ordering::Relaxed);
        milli as f64 / 1000.0
    }
}

/// The inter-call slot period for a milli-cps rate. `0` (paused) has no period —
/// the governor never schedules a slot while paused — so it maps to a large
/// sentinel that is simply never used (the pause branch is taken first).
fn period_for(millicps: u64) -> Duration {
    if millicps == 0 {
        Duration::from_secs(3600)
    } else {
        Duration::from_secs_f64(1000.0 / millicps as f64)
    }
}

/// The **re-anchoring fixed-grid CPS governor** — the async scheduler the load
/// driver pulls admission slots from. It reads the shared [`RateHandle`] on every
/// slot, so `POST /rate` re-targets it live:
///
///   * Within one *rate epoch* the nth slot is at `epoch_start + n*period`; sleep
///     until it, and if it is already past (wake/scheduling jitter) fire NOW —
///     within an epoch this holds the offered rate at exactly `cps`.
///   * A rate CHANGE opens a NEW epoch (`epoch_start = now`, `n = 0`, fresh
///     `period`). Re-anchoring is what makes a change clean: a CUT leaves no
///     backlog of past-due slots to fire as a catch-up burst (the new grid starts
///     now); a RAISE takes effect within one slot of the shorter period.
///   * `cps == 0` PAUSES: park on a short poll until the rate is raised (or the
///     run window ends), then re-anchor and resume. In-flight calls are untouched.
///
/// The run window is a WALL-time `deadline`, independent of the grid (re-anchoring
/// resets the slot counter, so the stop condition can no longer be
/// `n*period < duration`). [`Governor::next_slot`] returns `Some(())` per admitted
/// slot and `None` once the window closes — the driver spawns one call per `Some`.
pub struct Governor {
    rate: RateHandle,
    deadline: Instant,
    epoch_start: Instant,
    epoch_milli: u64,
    period: Duration,
    n: u64,
    pause_poll: Duration,
}

impl Governor {
    /// A governor that offers slots at `rate` until `duration` of wall time has
    /// elapsed from now.
    pub fn new(rate: RateHandle, duration: Duration) -> Self {
        let start = Instant::now();
        let epoch_milli = rate.millicps();
        Governor {
            rate,
            deadline: start + duration,
            epoch_start: start,
            epoch_milli,
            period: period_for(epoch_milli),
            n: 0,
            // How often we re-check the rate while paused; small so a raise resumes
            // promptly, and only runs while parked (not hot).
            pause_poll: Duration::from_millis(20),
        }
    }

    /// Await the next admission slot: `Some(())` to spawn a call, `None` once the
    /// run window has closed. Re-anchors on a rate change and parks while paused.
    pub async fn next_slot(&mut self) -> Option<()> {
        loop {
            if Instant::now() >= self.deadline {
                return None;
            }

            // PAUSED: park until the rate is raised or the window closes, then
            // re-anchor a fresh epoch so we do not fire a burst on resume.
            if self.epoch_milli == 0 {
                tokio::time::sleep(self.pause_poll).await;
                self.reanchor(self.rate.millicps());
                continue;
            }

            // The nth slot of the current epoch. `n*period` is computed in Duration
            // space (checked) so a huge n cannot overflow Instant arithmetic; on
            // overflow we re-anchor (equivalent to a rate refresh).
            let target = match u32::try_from(self.n).ok().and_then(|k| self.period.checked_mul(k)) {
                Some(off) => self.epoch_start + off,
                None => {
                    self.reanchor(self.epoch_milli);
                    continue;
                }
            };

            // Never sleep past the run window; wake at the earlier of the slot and
            // the deadline so the loop exits promptly on a paused/slow run.
            let wake = target.min(self.deadline);
            let now = Instant::now();
            if wake > now {
                tokio::time::sleep_until(wake).await;
            }
            if Instant::now() >= self.deadline {
                return None;
            }

            // Re-read the rate: a change (including →0 pause) opens a new epoch. This
            // slot's target was already honoured (we slept to it); re-anchor so the
            // grid restarts at the new rate immediately after.
            let current = self.rate.millicps();
            if current != self.epoch_milli {
                self.reanchor(current);
                // →0 loops back into the pause branch WITHOUT admitting this slot.
                // A nonzero change admits THIS slot at the new grid's slot 0 (== now),
                // so a raise takes effect within one slot.
                if self.epoch_milli == 0 {
                    continue;
                }
            }
            self.n += 1;
            return Some(());
        }
    }

    /// Open a fresh rate epoch anchored at `now` with the given milli-cps.
    fn reanchor(&mut self, millicps: u64) {
        self.epoch_milli = millicps;
        self.epoch_start = Instant::now();
        self.period = period_for(millicps);
        self.n = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_stores_and_reads_back_a_fractional_rate() {
        let h = RateHandle::new(12.5);
        assert_eq!(h.cps(), 12.5);
        assert_eq!(h.millicps(), 12_500);
        assert!(!h.is_paused());
    }

    #[test]
    fn zero_is_paused_and_a_raise_resumes() {
        let h = RateHandle::new(0.0);
        assert!(h.is_paused());
        assert_eq!(h.cps(), 0.0);
        let applied = h.set(40.0);
        assert_eq!(applied, 40.0);
        assert!(!h.is_paused());
        // A cut back to 0 re-pauses.
        assert_eq!(h.set(0.0), 0.0);
        assert!(h.is_paused());
    }

    #[test]
    fn negative_and_nan_clamp_to_zero() {
        let h = RateHandle::new(10.0);
        assert_eq!(h.set(-5.0), 0.0);
        assert!(h.is_paused());
        assert_eq!(h.set(f64::NAN), 0.0);
        assert_eq!(h.set(f64::INFINITY), 0.0, "an infinite rate clamps to paused, not a wrap");
    }

    #[test]
    fn a_clone_sees_writes_through_the_shared_cell() {
        let a = RateHandle::new(10.0);
        let b = a.clone();
        b.set(30.0);
        assert_eq!(a.cps(), 30.0, "both handles share one atomic");
    }
}
