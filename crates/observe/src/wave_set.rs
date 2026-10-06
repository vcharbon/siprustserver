//! Keyed burst logging: one [`Wave`] per episode key, plus the driver that
//! carries a quiet episode to its falling edge.
//!
//! A call site records against a key — the dead peer, the failing target, the
//! shed reason — and the set emits through the closure it was built with, so
//! each site owns its own field vocabulary while the aggregation rules stay in
//! one place. A site that names an example in its lines records it as the
//! episode's payload, so every line reads its own episode's example and the
//! example is freed with the key. On a rising edge the set spawns ONE task
//! for that episode which polls at the summary cadence and exits at the
//! falling edge; a process with no runtime (a synchronous unit test) simply
//! loses the periodic and the falling line, never the rising or the recorded
//! totals.
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

/// What a [`WaveSet`] does with a due line: the key, the line, and the
/// payload of the episode that owes it.
type Emit<P> = dyn Fn(&str, &WaveReport, &P) + Send + Sync;

/// One key's episode: its state machine and the payload of its latest event,
/// set before the event reaches the state machine, so an open episode always
/// has one. A slot holds exactly one episode; the driver retires it at the
/// falling edge, in the same critical section that removes it from the map.
struct Slot<P> {
    wave: Wave,
    payload: Option<P>,
    retired: bool,
}

type SlotRef<P> = Arc<Mutex<Slot<P>>>;

/// What one driver step found.
enum Step<P> {
    /// Nothing due yet.
    Quiet,
    /// A line is due, with the payload of the episode that owes it.
    Due(WaveReport, Option<P>),
    /// The episode the driver was spawned for is over.
    Gone,
}

/// A keyed family of [`Wave`]s sharing one emission shape. Each episode
/// carries a payload `P` (the latest one recorded), which every line of that
/// episode is emitted with and which is freed with the episode's key.
pub struct WaveSet<P = ()> {
    summary_every: Duration,
    idle_close_after: Duration,
    emit: Arc<Emit<P>>,
    waves: Mutex<HashMap<String, SlotRef<P>>>,
    /// Open episodes that have not yet been told they recovered — the cheap
    /// "is anything still burning?" flag.
    active: Arc<AtomicUsize>,
}

impl WaveSet<()> {
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
        Self::build(summary_every, idle_close_after, move |key, report, _: &()| emit(key, report))
    }

    /// Record one event of `counter` against `key`; see
    /// [`record_with`](WaveSet::record_with).
    pub fn record(self: &Arc<Self>, key: &str, counter: &'static str, n: u64) {
        self.record_with(key, counter, n, ());
    }
}

impl<P: Clone + Send + 'static> WaveSet<P> {
    /// Build a set whose episodes carry a payload, with the default 5 s
    /// cadence and idle window.
    pub fn with_payload(emit: impl Fn(&str, &WaveReport, &P) + Send + Sync + 'static) -> Arc<Self> {
        Self::build(DEFAULT_SUMMARY_EVERY, DEFAULT_IDLE_CLOSE_AFTER, emit)
    }

    fn build(
        summary_every: Duration,
        idle_close_after: Duration,
        emit: impl Fn(&str, &WaveReport, &P) + Send + Sync + 'static,
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

    /// Record one event of `counter` against `key`, `payload` becoming the
    /// episode's payload, and emit whatever line that makes due. An event
    /// landing on a key whose close is armed revives that episode silently.
    /// Ignored once [`MAX_KEYS`] episodes are open.
    pub fn record_with(self: &Arc<Self>, key: &str, counter: &'static str, n: u64, payload: P) {
        let mut payload = Some(payload);
        loop {
            let Some(slot) = self.slot_for(key) else { return };
            if self.record_into(key, &slot, counter, n, &mut payload) {
                return;
            }
        }
    }

    /// The key's live slot, inserted fresh when absent; `None` once
    /// [`MAX_KEYS`] episodes are open.
    fn slot_for(&self, key: &str) -> Option<SlotRef<P>> {
        let mut waves = self.waves.lock().unwrap();
        if let Some(slot) = waves.get(key) {
            return Some(slot.clone());
        }
        if waves.len() >= MAX_KEYS {
            return None;
        }
        let slot = Arc::new(Mutex::new(Slot {
            wave: Wave::new(self.summary_every, self.idle_close_after),
            payload: None,
            retired: false,
        }));
        waves.insert(key.to_string(), slot.clone());
        Some(slot)
    }

    /// Apply one event to `slot`, emitting the line it owes. Returns `false`,
    /// with `payload` untouched, when the driver retired the slot after
    /// [`slot_for`](WaveSet::slot_for) returned it: the event belongs to the
    /// key's next episode.
    fn record_into(
        self: &Arc<Self>,
        key: &str,
        slot: &SlotRef<P>,
        counter: &'static str,
        n: u64,
        payload: &mut Option<P>,
    ) -> bool {
        // Both `active` increments happen under the slot lock, so a concurrent
        // recovery on this key cannot decrement a count this call has not
        // added yet: every `+1` here is matched by exactly one `-1` in
        // `recovered` or `finish`.
        let (report, payload) = {
            let mut s = slot.lock().unwrap();
            if s.retired {
                return false;
            }
            if s.wave.is_close_requested() {
                self.active.fetch_add(1, Ordering::Relaxed);
            }
            s.payload = payload.take();
            let report = s.wave.record(counter, n);
            if matches!(&report, Some(r) if r.edge == Edge::Rising) {
                self.active.fetch_add(1, Ordering::Relaxed);
            }
            let Some(report) = report else { return true };
            (report, s.payload.clone())
        };
        if report.edge == Edge::Rising {
            self.spawn_driver(key.to_string(), slot.clone());
        }
        if let Some(payload) = payload {
            (self.emit)(key, &report, &payload);
        }
        true
    }

    /// Report that `key` recovered: arm its falling edge without emitting one,
    /// so the episode ends only once the key stays quiet for the idle window.
    /// No-op when nothing is burning for `key`.
    pub fn recovered(&self, key: &str) {
        if !self.is_active() {
            return;
        }
        let slot = self.waves.lock().unwrap().get(key).cloned();
        let Some(slot) = slot else { return };
        let mut s = slot.lock().unwrap();
        if s.wave.request_close() {
            self.release_active();
        }
    }

    /// Report a recovery for every burning key — for a hot path that knows the
    /// system is healthy again but not which episodes are open.
    pub fn recovered_all(&self) {
        if !self.is_active() {
            return;
        }
        let slots: Vec<SlotRef<P>> = self.waves.lock().unwrap().values().cloned().collect();
        for slot in slots {
            let mut s = slot.lock().unwrap();
            if s.wave.request_close() {
                self.release_active();
            }
        }
    }

    /// One driver step for `slot`. On the falling edge the slot is retired
    /// and its key removed in one critical section (map lock, then slot lock),
    /// so a concurrent record either revives this episode or opens the next
    /// one, never both.
    fn drive(&self, key: &str, slot: &SlotRef<P>) -> Step<P> {
        let mut waves = self.waves.lock().unwrap();
        let mut s = slot.lock().unwrap();
        if s.retired || !s.wave.is_open() {
            return Step::Gone;
        }
        let was_active = !s.wave.is_close_requested();
        let Some(report) = s.wave.poll() else { return Step::Quiet };
        if report.edge != Edge::Falling {
            return Step::Due(report, s.payload.clone());
        }
        s.retired = true;
        waves.remove(key);
        if was_active {
            self.release_active();
        }
        Step::Due(report, s.payload.take())
    }

    /// Give back one burning episode. Saturating: `is_active` must never wrap
    /// to "always burning" and pin the hot paths on the map lock.
    fn release_active(&self) {
        let _ =
            self.active.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }

    /// Drive one episode's periodic summary and idle close. Exits at the
    /// falling edge, or as soon as the episode it was spawned for is gone.
    fn spawn_driver(self: &Arc<Self>, key: String, slot: SlotRef<P>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let set = self.clone();
        let period = self.summary_every;
        handle.spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let (report, payload) = match set.drive(&key, &slot) {
                    Step::Quiet => continue,
                    Step::Gone => return,
                    Step::Due(report, payload) => (report, payload),
                };
                if let Some(payload) = payload {
                    (set.emit)(&key, &report, &payload);
                }
                if report.edge == Edge::Falling {
                    return;
                }
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

    /// Lines of a payload-carrying set: key, edge, the episode's payload.
    type PayloadLines = Arc<Mutex<Vec<(String, Edge, &'static str)>>>;

    fn capturing_payloads() -> (Arc<WaveSet<&'static str>>, PayloadLines) {
        let seen = PayloadLines::default();
        let sink = seen.clone();
        let set = WaveSet::with_payload(move |key, report, payload: &&'static str| {
            sink.lock().unwrap().push((key.to_string(), report.edge, payload));
        });
        (set, seen)
    }

    /// A record that looked its key's slot up just before the driver closed
    /// that episode does not land on the closed episode: it is refused there
    /// and opens the key's next episode, which owns its payload and its slot.
    #[tokio::test(start_paused = true)]
    async fn a_record_racing_the_close_opens_the_next_episode_with_its_own_payload() {
        let (set, seen) = capturing_payloads();
        set.record_with("k", "events", 1, "old");
        tokio::task::yield_now().await;

        let raced = set.slot_for("k").expect("the key is open");
        tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;
        let mut payload = Some("new");
        assert!(
            !set.record_into("k", &raced, "events", 1, &mut payload),
            "a closed episode refuses the racing event",
        );
        set.record_with("k", "events", 1, payload.expect("a refused event keeps its payload"));
        set.record_with("k", "events", 1, "newer");
        tokio::task::yield_now().await;
        tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;

        let lines = seen.lock().unwrap().clone();
        let k = || "k".to_string();
        assert_eq!(
            lines,
            [
                (k(), Edge::Rising, "old"),
                (k(), Edge::Falling, "old"),
                (k(), Edge::Rising, "new"),
                (k(), Edge::Falling, "newer"),
            ],
        );
        assert!(!set.is_active());
        assert!(set.waves.lock().unwrap().is_empty(), "a closed episode frees its key");
    }

    /// An event recorded from the falling line itself — the earliest a record
    /// can follow a close — opens a new episode with its own payload.
    #[tokio::test(start_paused = true)]
    async fn a_record_from_the_falling_line_opens_a_new_episode() {
        let seen = PayloadLines::default();
        let sink = seen.clone();
        let this: Arc<std::sync::OnceLock<std::sync::Weak<WaveSet<&'static str>>>> = Arc::default();
        let reenter = this.clone();
        let set = WaveSet::with_payload(move |key, report, payload: &&'static str| {
            sink.lock().unwrap().push((key.to_string(), report.edge, payload));
            if report.edge == Edge::Falling && *payload == "old" {
                if let Some(set) = reenter.get().and_then(std::sync::Weak::upgrade) {
                    set.record_with(key, "events", 1, "new");
                }
            }
        });
        let _ = this.set(Arc::downgrade(&set));

        set.record_with("k", "events", 1, "old");
        tokio::task::yield_now().await;
        tokio::time::advance(DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;

        let lines = seen.lock().unwrap().clone();
        let k = || "k".to_string();
        assert_eq!(
            lines,
            [(k(), Edge::Rising, "old"), (k(), Edge::Falling, "old"), (k(), Edge::Rising, "new")],
        );
        assert!(set.is_active(), "the new episode is burning");
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
