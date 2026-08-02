//! The burst-aggregation state machine (ADR-0026 §1 aggregation discipline).
//!
//! A [`Wave`] turns an unbounded per-call event class — 5000 takeover
//! hydrations, a keepalive-timeout storm, a limiter outage — into three kinds of
//! line: a **rising edge** when the first event of an episode arrives, a
//! **periodic summary** while the episode stays open, and a **falling edge**
//! carrying the episode totals and duration. Volume changes the counters in
//! those lines, never the number of lines.
//!
//! Time rides `tokio::time::Instant`, so a `start_paused` test drives the
//! summary and the idle close with `advance` like any other behaviour timer.
//!
//! This file is the state machine only: it decides *when* a line is due and
//! *what* it totals. Emission (and the driver that polls a quiet episode to its
//! falling edge) is [`crate::wave_set`].

use std::fmt;
use std::time::Duration;

use tokio::time::Instant;

/// Named counters one episode can accumulate. Four covers every current call
/// site (`hydrated`/`resolved`/`self_released` is the widest).
pub const MAX_COUNTERS: usize = 4;

/// Cadence of the periodic summary while an episode is open.
pub const DEFAULT_SUMMARY_EVERY: Duration = Duration::from_secs(5);

/// Quiet time after the last event that ends an episode.
pub const DEFAULT_IDLE_CLOSE_AFTER: Duration = Duration::from_secs(5);

/// Which of the three lines an episode is producing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    /// First event of an episode.
    Rising,
    /// Periodic summary of an open episode.
    Summary,
    /// Episode over — the totals and duration.
    Falling,
}

impl fmt::Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Edge::Rising => "rising",
            Edge::Summary => "summary",
            Edge::Falling => "falling",
        })
    }
}

/// A fixed-capacity set of named counters, accumulated without allocating.
/// Formats as the `key=value` run a lifecycle line embeds (`hydrated=12
/// resolved=3`); an empty tally formats as `-`.
#[derive(Clone, Copy, Debug)]
pub struct Tally {
    names: [&'static str; MAX_COUNTERS],
    values: [u64; MAX_COUNTERS],
    len: usize,
}

impl Default for Tally {
    fn default() -> Self {
        Self { names: [""; MAX_COUNTERS], values: [0; MAX_COUNTERS], len: 0 }
    }
}

impl Tally {
    /// Add `n` to `name`, registering it on first use. Counters beyond
    /// [`MAX_COUNTERS`] are dropped — widen the constant rather than silently
    /// losing a counter at a new call site.
    pub fn add(&mut self, name: &'static str, n: u64) {
        for i in 0..self.len {
            if self.names[i] == name {
                self.values[i] = self.values[i].saturating_add(n);
                return;
            }
        }
        debug_assert!(self.len < MAX_COUNTERS, "wave tally overflow on {name}");
        if self.len < MAX_COUNTERS {
            self.names[self.len] = name;
            self.values[self.len] = n;
            self.len += 1;
        }
    }

    /// The accumulated value of `name` (`0` if never recorded).
    pub fn get(&self, name: &str) -> u64 {
        (0..self.len).find(|&i| self.names[i] == name).map(|i| self.values[i]).unwrap_or(0)
    }

    /// Sum over every counter — the episode's event count.
    pub fn total(&self) -> u64 {
        self.values[..self.len].iter().fold(0u64, |a, v| a.saturating_add(*v))
    }
}

impl fmt::Display for Tally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.len == 0 {
            return f.write_str("-");
        }
        for i in 0..self.len {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{}={}", self.names[i], self.values[i])?;
        }
        Ok(())
    }
}

/// One line an episode owes the log.
#[derive(Clone, Copy, Debug)]
pub struct WaveReport {
    /// Which of the three lines this is.
    pub edge: Edge,
    /// Time since the episode's first event.
    pub elapsed_ms: u64,
    /// Episode totals so far (final totals on [`Edge::Falling`]).
    pub tally: Tally,
}

/// One episode's accumulator.
#[derive(Clone, Copy)]
struct Episode {
    started: Instant,
    last_event: Instant,
    last_summary: Instant,
    tally: Tally,
}

/// The rising-edge / summary / falling-edge state machine for ONE event class.
///
/// [`record`](Wave::record) is the event path: it opens an episode on the first
/// event and thereafter returns a line only when the summary cadence has
/// elapsed. [`poll`](Wave::poll) is the driver path: it produces a due summary
/// (or the idle falling edge) for an episode that has gone quiet.
/// [`close`](Wave::close) ends an episode whose end is known — a recovery, a
/// completed sweep — without waiting for the idle window.
pub struct Wave {
    summary_every: Duration,
    idle_close_after: Duration,
    /// Bumped on every rising edge, so a driver spawned for a previous episode
    /// recognises that it is stale.
    generation: u64,
    open: Option<Episode>,
}

impl Default for Wave {
    fn default() -> Self {
        Self::new(DEFAULT_SUMMARY_EVERY, DEFAULT_IDLE_CLOSE_AFTER)
    }
}

impl Wave {
    /// Build with an explicit summary cadence and idle-close window.
    pub fn new(summary_every: Duration, idle_close_after: Duration) -> Self {
        Self { summary_every, idle_close_after, generation: 0, open: None }
    }

    /// The current episode generation; changes on each rising edge.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether an episode is open.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The summary cadence — the period a driver sleeps between [`poll`](Wave::poll)s.
    pub fn summary_every(&self) -> Duration {
        self.summary_every
    }

    /// Record one event. Returns the line it owes: the rising edge that opened
    /// the episode, a summary when the cadence has elapsed, otherwise nothing.
    pub fn record(&mut self, counter: &'static str, n: u64) -> Option<WaveReport> {
        let now = Instant::now();
        match &mut self.open {
            None => {
                let mut tally = Tally::default();
                tally.add(counter, n);
                self.open = Some(Episode {
                    started: now,
                    last_event: now,
                    last_summary: now,
                    tally,
                });
                self.generation = self.generation.wrapping_add(1);
                Some(WaveReport { edge: Edge::Rising, elapsed_ms: 0, tally })
            }
            Some(ep) => {
                ep.tally.add(counter, n);
                ep.last_event = now;
                if now.duration_since(ep.last_summary) >= self.summary_every {
                    ep.last_summary = now;
                    Some(WaveReport {
                        edge: Edge::Summary,
                        elapsed_ms: elapsed_ms(ep.started, now),
                        tally: ep.tally,
                    })
                } else {
                    None
                }
            }
        }
    }

    /// Driver step for an episode that may have gone quiet: the idle falling
    /// edge once no event has landed for the idle window, else a due summary,
    /// else nothing.
    pub fn poll(&mut self) -> Option<WaveReport> {
        let now = Instant::now();
        let ep = self.open.as_mut()?;
        if now.duration_since(ep.last_event) >= self.idle_close_after {
            return self.close();
        }
        if now.duration_since(ep.last_summary) >= self.summary_every {
            ep.last_summary = now;
            return Some(WaveReport {
                edge: Edge::Summary,
                elapsed_ms: elapsed_ms(ep.started, now),
                tally: ep.tally,
            });
        }
        None
    }

    /// End the open episode now and return its falling edge. `None` if no
    /// episode is open (a close is idempotent).
    pub fn close(&mut self) -> Option<WaveReport> {
        let ep = self.open.take()?;
        Some(WaveReport {
            edge: Edge::Falling,
            elapsed_ms: elapsed_ms(ep.started, Instant::now()),
            tally: ep.tally,
        })
    }
}

fn elapsed_ms(from: Instant, to: Instant) -> u64 {
    to.saturating_duration_since(from).as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paused clock throughout: every deadline below is tripped by an explicit
    /// `advance`, one deadline at a time (docs/testing/test-clock.md).
    #[tokio::test(start_paused = true)]
    async fn one_rising_line_then_a_summary_per_cadence() {
        let mut w = Wave::default();

        let rising = w.record("hydrated", 1).expect("first event opens the episode");
        assert_eq!(rising.edge, Edge::Rising);
        assert_eq!(rising.tally.total(), 1);

        // A thousand events inside the first cadence produce no further line.
        for _ in 0..1000 {
            assert!(w.record("hydrated", 1).is_none());
        }

        // Exactly to the summary deadline: the next event carries the totals.
        tokio::time::advance(DEFAULT_SUMMARY_EVERY).await;
        let summary = w.record("resolved", 1).expect("cadence elapsed → summary");
        assert_eq!(summary.edge, Edge::Summary);
        assert_eq!(summary.tally.get("hydrated"), 1001);
        assert_eq!(summary.tally.get("resolved"), 1);
        assert_eq!(summary.elapsed_ms, DEFAULT_SUMMARY_EVERY.as_millis() as u64);

        // The cadence restarts from the summary just emitted.
        assert!(w.record("hydrated", 1).is_none());
    }

    /// A wider idle window than summary cadence, so the two deadlines are
    /// tripped one at a time (never two in one advance).
    #[tokio::test(start_paused = true)]
    async fn quiet_episode_summarizes_then_closes_on_the_idle_window() {
        let mut w = Wave::new(Duration::from_secs(5), Duration::from_secs(12));
        w.record("timeouts", 3).unwrap();

        // Before either deadline: nothing due.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(w.poll().is_none());

        // Summary cadence, twice — the episode is quiet but not yet idle.
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(w.poll().expect("cadence elapsed").edge, Edge::Summary);
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(w.poll().expect("cadence elapsed again").edge, Edge::Summary);

        // Then the idle window closes it with the totals + duration.
        tokio::time::advance(Duration::from_secs(2)).await;
        let falling = w.poll().expect("idle → falling edge");
        assert_eq!(falling.edge, Edge::Falling);
        assert_eq!(falling.tally.get("timeouts"), 3);
        assert_eq!(falling.elapsed_ms, 12_000);
        assert!(!w.is_open());
        assert!(w.poll().is_none(), "a closed episode owes nothing");
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_close_is_idempotent_and_reopening_bumps_the_generation() {
        let mut w = Wave::default();
        w.record("fail_open", 1).unwrap();
        let gen_first = w.generation();

        tokio::time::advance(Duration::from_secs(2)).await;
        let falling = w.close().expect("explicit close ends the episode");
        assert_eq!(falling.edge, Edge::Falling);
        assert_eq!(falling.elapsed_ms, 2_000);
        assert!(w.close().is_none(), "closing twice owes nothing");

        // A later burst is a NEW episode: rising edge again, fresh totals.
        tokio::time::advance(Duration::from_secs(30)).await;
        let rising = w.record("fail_open", 1).expect("new episode");
        assert_eq!(rising.edge, Edge::Rising);
        assert_eq!(rising.tally.get("fail_open"), 1);
        assert_ne!(w.generation(), gen_first);
    }

    #[test]
    fn tally_formats_as_a_key_value_run() {
        let mut t = Tally::default();
        assert_eq!(t.to_string(), "-");
        t.add("hydrated", 12);
        t.add("resolved", 3);
        t.add("hydrated", 1);
        assert_eq!(t.to_string(), "hydrated=13 resolved=3");
        assert_eq!(t.total(), 16);
    }
}
