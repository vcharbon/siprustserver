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
mod tests;
