//! Keyed burst logging: one [`Wave`] per episode key, plus the driver that
//! carries a quiet episode to its falling edge.
//!
//! A call site records against a key — the dead peer, the failing target, the
//! shed reason — and the set emits through the closure it was built with, so
//! each site owns its own field vocabulary while the aggregation rules stay in
//! one place. On a rising edge the set spawns ONE task for that episode which
//! polls at the summary cadence and exits at the falling edge; a process with
//! no runtime (a synchronous unit test) simply loses the periodic and the
//! falling line, never the rising or the recorded totals.
//!
//! [`WaveSet::is_active`] is a single relaxed atomic load: a hot path that only
//! needs to report a *recovery* (an admit after a shed, a success after an
//! outage) pays one predicted branch while everything is quiet — and pays it
//! again only after the next failure, since a recovery clears the flag.
//!
//! A recovery ARMS the falling edge, it does not emit one: the episode ends
//! after the idle window with no further failure. This is the hysteresis that
//! keeps the lines traffic-independent — a source flapping at its cap (reject,
//! admit, reject, …) is one episode with periodic summaries, not a
//! rising/falling pair per event.
//!
//! Under a paused clock the driver arms its timer only once it has been polled,
//! so a test yields once after the rising edge before advancing — otherwise the
//! driver arms from the post-advance instant and the deadline moves with it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::wave::{Edge, Wave, WaveReport, DEFAULT_IDLE_CLOSE_AFTER, DEFAULT_SUMMARY_EVERY};

/// Maximum concurrently-open keys. Keys can be wire-influenceable (a resolver
/// name, a peer address), so the set refuses new ones past this bound rather
/// than growing without limit; closed keys free their slot.
pub const MAX_KEYS: usize = 128;

/// What a [`WaveSet`] does with a due line.
type Emit = dyn Fn(&str, &WaveReport) + Send + Sync;

/// A keyed family of [`Wave`]s sharing one emission shape.
pub struct WaveSet {
    summary_every: Duration,
    idle_close_after: Duration,
    emit: Arc<Emit>,
    waves: Mutex<HashMap<String, Arc<Mutex<Wave>>>>,
    /// Open episodes that have not yet been told they recovered — the cheap
    /// "is anything still burning?" flag.
    active: Arc<AtomicUsize>,
}

impl WaveSet {
    /// Build with the default 5 s cadence and idle window.
    pub fn new(emit: impl Fn(&str, &WaveReport) + Send + Sync + 'static) -> Arc<Self> {
        Self::with_intervals(DEFAULT_SUMMARY_EVERY, DEFAULT_IDLE_CLOSE_AFTER, emit)
    }

    /// Build with an explicit cadence and idle window.
    pub fn with_intervals(
        summary_every: Duration,
        idle_close_after: Duration,
        emit: impl Fn(&str, &WaveReport) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            summary_every,
            idle_close_after,
            emit: Arc::new(emit),
            waves: Mutex::new(HashMap::new()),
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Whether any episode is open and has not yet been told it recovered —
    /// one relaxed load, for hot paths that only need to act when something is
    /// still burning.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed) > 0
    }

    /// Record one event of `counter` against `key`, emitting whatever line that
    /// makes due. An event landing on a key whose close is armed revives that
    /// episode silently. Ignored once [`MAX_KEYS`] episodes are open.
    pub fn record(self: &Arc<Self>, key: &str, counter: &'static str, n: u64) {
        let wave = {
            let mut waves = self.waves.lock().unwrap();
            match waves.get(key) {
                Some(w) => w.clone(),
                None => {
                    if waves.len() >= MAX_KEYS {
                        return;
                    }
                    let w =
                        Arc::new(Mutex::new(Wave::new(self.summary_every, self.idle_close_after)));
                    waves.insert(key.to_string(), w.clone());
                    w
                }
            }
        };
        // Both `active` increments happen under the wave lock, so a concurrent
        // recovery on this key cannot decrement a count this call has not
        // added yet: every `+1` here is matched by exactly one `-1` in
        // `recovered` or `finish`.
        let (report, generation) = {
            let mut w = wave.lock().unwrap();
            if w.is_close_requested() {
                self.active.fetch_add(1, Ordering::Relaxed);
            }
            let report = w.record(counter, n);
            if matches!(&report, Some(r) if r.edge == Edge::Rising) {
                self.active.fetch_add(1, Ordering::Relaxed);
            }
            (report, w.generation())
        };
        let Some(report) = report else { return };
        if report.edge == Edge::Rising {
            self.spawn_driver(key.to_string(), wave, generation);
        }
        (self.emit)(key, &report);
    }

    /// Report that `key` recovered: arm its falling edge without emitting one,
    /// so the episode ends only once the key stays quiet for the idle window.
    /// No-op when nothing is burning for `key`.
    pub fn recovered(&self, key: &str) {
        if !self.is_active() {
            return;
        }
        let wave = self.waves.lock().unwrap().get(key).cloned();
        let Some(wave) = wave else { return };
        let mut w = wave.lock().unwrap();
        if w.request_close() {
            self.release_active();
        }
    }

    /// Report a recovery for every burning key — for a hot path that knows the
    /// system is healthy again but not which episodes are open.
    pub fn recovered_all(&self) {
        if !self.is_active() {
            return;
        }
        let waves: Vec<Arc<Mutex<Wave>>> = self.waves.lock().unwrap().values().cloned().collect();
        for wave in waves {
            let mut w = wave.lock().unwrap();
            if w.request_close() {
                self.release_active();
            }
        }
    }

    /// Emit a falling edge and free the key's slot. `was_active` says whether
    /// the episode still counted against [`is_active`](WaveSet::is_active).
    fn finish(&self, key: &str, report: &WaveReport, was_active: bool) {
        self.waves.lock().unwrap().remove(key);
        if was_active {
            self.release_active();
        }
        (self.emit)(key, report);
    }

    /// Give back one burning episode. Saturating: `is_active` must never wrap
    /// to "always burning" and pin the hot paths on the map lock.
    fn release_active(&self) {
        let _ =
            self.active.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }

    /// Drive one episode's periodic summary and idle close. Exits at the
    /// falling edge, or as soon as the episode it was spawned for is gone.
    fn spawn_driver(self: &Arc<Self>, key: String, wave: Arc<Mutex<Wave>>, generation: u64) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let set = self.clone();
        let period = self.summary_every;
        handle.spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let (report, was_active) = {
                    let mut w = wave.lock().unwrap();
                    if w.generation() != generation || !w.is_open() {
                        return;
                    }
                    let was_active = !w.is_close_requested();
                    (w.poll(), was_active)
                };
                let Some(report) = report else { continue };
                if report.edge == Edge::Falling {
                    set.finish(&key, &report, was_active);
                    return;
                }
                (set.emit)(&key, &report);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wave::Edge;

    /// Captured lines, so the tests assert on the aggregation rather than on a
    /// subscriber.
    #[derive(Default)]
    struct Captured(Mutex<Vec<(String, Edge, u64)>>);

    fn capturing() -> (Arc<WaveSet>, Arc<Captured>) {
        let seen = Arc::new(Captured::default());
        let sink = seen.clone();
        let set = WaveSet::new(move |key, report| {
            sink.0.lock().unwrap().push((key.to_string(), report.edge, report.tally.total()));
        });
        (set, seen)
    }

    /// The headline contract: a 5000-event burst on one key produces a handful
    /// of lines — rising, periodic summaries, falling — never one per event.
    #[tokio::test(start_paused = true)]
    async fn a_five_thousand_event_burst_produces_a_handful_of_lines() {
        let (set, seen) = capturing();

        for _ in 0..5_000 {
            set.record("w0", "hydrated", 1);
        }
        // Let the episode's driver task arm its timer BEFORE moving the clock
        // (it was spawned, not yet polled — advancing first would arm it from
        // the later instant), then advance exactly to the idle window that
        // closes the episode.
        tokio::task::yield_now().await;
        tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;

        let lines = seen.0.lock().unwrap().clone();
        assert!(lines.len() <= 4, "5000 events must not print 5000 lines: {lines:?}");
        assert_eq!(lines.first().map(|l| l.1), Some(Edge::Rising));
        let last = lines.last().expect("an episode always closes");
        assert_eq!(last.1, Edge::Falling);
        assert_eq!(last.2, 5_000, "the falling edge carries the episode totals");
        assert!(!set.is_active());
    }

    /// Keys are independent episodes: one peer's burst does not fold into
    /// another's totals, and each closes on its own quiet.
    #[tokio::test(start_paused = true)]
    async fn keys_are_independent_episodes() {
        let (set, seen) = capturing();
        set.record("w0", "hydrated", 1);
        set.record("w1", "hydrated", 1);
        set.recovered("w0");
        assert!(set.is_active(), "w1's episode is still burning");
        set.recovered_all();
        assert!(!set.is_active(), "every episode has now recovered");

        tokio::task::yield_now().await;
        tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;

        let lines = seen.0.lock().unwrap().clone();
        assert_eq!(lines[0], ("w0".into(), Edge::Rising, 1));
        assert_eq!(lines[1], ("w1".into(), Edge::Rising, 1));
        let falling: Vec<_> = lines[2..].iter().map(|l| (l.0.clone(), l.1, l.2)).collect();
        assert_eq!(falling.len(), 2, "one falling edge each: {lines:?}");
        assert!(falling.iter().all(|l| l.1 == Edge::Falling && l.2 == 1));
    }

    /// The headline flap contract at the set level: a source alternating
    /// failure/recovery at its cap owes ONE rising edge and ONE falling edge,
    /// not a pair per event — and spawns ONE driver task, not one per event.
    #[tokio::test(start_paused = true)]
    async fn a_flapping_key_stays_one_episode() {
        let (set, seen) = capturing();

        // First shed opens the episode; yield so its driver arms its cadence
        // timer at t=0 rather than from a post-advance instant.
        set.record("cps", "shed", 1);
        tokio::task::yield_now().await;

        // 1999 further admit/reject flaps, 1 ms apart — t = 1.999 s, well
        // inside the first cadence.
        for _ in 0..1_999 {
            set.recovered_all();
            tokio::time::advance(Duration::from_millis(1)).await;
            set.record("cps", "shed", 1);
        }
        let during = seen.0.lock().unwrap().clone();
        assert_eq!(during.len(), 1, "a flap owes one rising edge only: {during:?}");
        assert_eq!(during[0].1, Edge::Rising);

        // The offered load stops. The cadence deadline lands first (summary),
        // then the idle window that ends the episode — one advance each.
        tokio::time::advance(Duration::from_millis(3_001)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(DEFAULT_SUMMARY_EVERY).await;
        tokio::task::yield_now().await;

        let lines = seen.0.lock().unwrap().clone();
        assert_eq!(lines.len(), 3, "rising + one summary + falling: {lines:?}");
        assert_eq!(lines[1].1, Edge::Summary);
        assert_eq!(lines[2].1, Edge::Falling);
        assert_eq!(lines[2].2, 2_000, "the falling edge totals every shed");
        assert!(!set.is_active());
    }

    /// The key bound holds against an unbounded key source, and a key that has
    /// actually fallen frees its slot.
    #[tokio::test(start_paused = true)]
    async fn open_keys_are_bounded() {
        let (set, seen) = capturing();
        for i in 0..(MAX_KEYS + 50) {
            set.record(&format!("name-{i}"), "resolve_failed", 1);
        }
        assert_eq!(seen.0.lock().unwrap().len(), MAX_KEYS);

        // A recovery alone does NOT free the slot — quiet does.
        set.recovered("name-0");
        set.record("fresh", "resolve_failed", 1);
        assert_eq!(
            seen.0.lock().unwrap().len(),
            MAX_KEYS,
            "an armed-but-open key still holds its slot",
        );

        tokio::task::yield_now().await;
        tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;
        set.record("fresh", "resolve_failed", 1);
        assert!(
            seen.0.lock().unwrap().iter().any(|l| l.0 == "fresh"),
            "a freed slot admits a new key",
        );
    }
}
