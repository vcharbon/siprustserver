//! [`WindowBreaker`] — the rule that opens a backend's circuit breaker
//! (ADR-0043): it opens when the last [`BreakerConfig::window`] saw at least
//! [`BreakerConfig::min_sent`] requests sent and not one HTTP response, of
//! any status. One dead peer among several never opens it (the others
//! answer); a total outage or a hung application does. The rule is evaluated
//! each time a request ends without a response. Pure: the caller feeds it
//! the instants.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

/// When a backend's breaker opens and how it probes while open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakerConfig {
    /// The window the rule looks back over.
    pub window: Duration,
    /// Requests sent inside the window, none answered, that open it.
    pub min_sent: usize,
    /// The probe period while open.
    pub probe_period: Duration,
    /// The budget of one probe's request, after its lookup.
    pub probe_timeout: Duration,
    /// The path-and-query the probe asks with `GET`.
    pub health_path: String,
}

impl BreakerConfig {
    /// The default rule (3 requests, 2 s, a probe every 1 s under 1 s)
    /// probing `GET health_path`.
    pub fn probing(health_path: impl Into<String>) -> Self {
        Self {
            window: Duration::from_secs(2),
            min_sent: 3,
            probe_period: Duration::from_secs(1),
            probe_timeout: Duration::from_secs(1),
            health_path: health_path.into(),
        }
    }
}

/// The breaker's state under the window rule.
#[derive(Debug)]
pub struct WindowBreaker {
    window: Duration,
    min_sent: usize,
    /// When each request inside the window was sent, oldest first.
    sent: VecDeque<Instant>,
    /// The last HTTP response.
    answered: Option<Instant>,
    open: bool,
}

impl WindowBreaker {
    /// A closed breaker under `config`'s rule.
    pub fn new(config: &BreakerConfig) -> Self {
        Self {
            window: config.window,
            min_sent: config.min_sent.max(1),
            sent: VecDeque::new(),
            answered: None,
            open: false,
        }
    }

    /// Whether it is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// A request left at `now`.
    pub fn sent(&mut self, now: Instant) {
        self.forget_before(now);
        self.sent.push_back(now);
    }

    /// An HTTP response, of any status, arrived at `now`.
    pub fn answered(&mut self, now: Instant) {
        self.answered = Some(now);
    }

    /// A request ended at `now` without a response: whether this opens the
    /// breaker.
    pub fn unanswered(&mut self, now: Instant) -> bool {
        if self.open {
            return false;
        }
        self.forget_before(now);
        let answered_inside = self.answered.is_some_and(|t| now.duration_since(t) <= self.window);
        if self.sent.len() < self.min_sent || answered_inside {
            return false;
        }
        self.open = true;
        true
    }

    /// Open it now, whatever the window holds: whether it was closed.
    pub fn trip(&mut self) -> bool {
        !std::mem::replace(&mut self.open, true)
    }

    /// A probe answered at `now`: close it, the window starting afresh.
    /// Whether it was open.
    pub fn close(&mut self, now: Instant) -> bool {
        self.sent.clear();
        self.answered = Some(now);
        std::mem::replace(&mut self.open, false)
    }

    fn forget_before(&mut self, now: Instant) {
        while self.sent.front().is_some_and(|t| now.duration_since(*t) > self.window) {
            self.sent.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker() -> WindowBreaker {
        WindowBreaker::new(&BreakerConfig::probing("/"))
    }

    fn at(ms: u64) -> Instant {
        // Instants on one paused timeline; the base is arbitrary.
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    /// Three requests sent inside 2 s, none answered: it opens.
    #[test]
    fn three_unanswered_inside_the_window_open_it() {
        let mut b = breaker();
        b.sent(at(0));
        b.sent(at(100));
        assert!(!b.unanswered(at(150)), "two sent so far");
        b.sent(at(200));
        assert!(b.unanswered(at(250)), "the third inside 2 s opens it");
        assert!(b.is_open());
    }

    /// A `503` inside the window is an answer: it stays closed.
    #[test]
    fn any_response_inside_the_window_keeps_it_closed() {
        let mut b = breaker();
        b.sent(at(0));
        b.answered(at(10));
        for ms in [100, 200, 300] {
            b.sent(at(ms));
            assert!(!b.unanswered(at(ms + 5)));
        }
        assert!(!b.is_open());
    }

    /// Three requests spread over 2.2 s never fill one window.
    #[test]
    fn requests_spread_wider_than_the_window_keep_it_closed() {
        let mut b = breaker();
        for ms in [0, 1_100, 2_200] {
            b.sent(at(ms));
            assert!(!b.unanswered(at(ms + 1)), "at {ms}");
        }
    }

    /// One peer of two dead, ten requests a second alternating answered and
    /// failed: it never opens.
    #[test]
    fn one_dead_peer_among_several_never_opens_it() {
        let mut b = breaker();
        for n in 0..100u64 {
            let now = at(n * 100);
            b.sent(now);
            if n % 2 == 0 {
                b.answered(now + Duration::from_millis(5));
            } else {
                assert!(!b.unanswered(now + Duration::from_millis(5)), "request {n}");
            }
        }
    }

    /// A close starts the window afresh; a trip opens it at once.
    #[test]
    fn a_close_starts_afresh_and_a_trip_opens_at_once() {
        let mut b = breaker();
        for ms in [0, 10, 20] {
            b.sent(at(ms));
        }
        assert!(b.unanswered(at(30)));
        assert!(b.close(at(1_000)));
        assert!(!b.is_open());
        b.sent(at(3_100));
        assert!(!b.unanswered(at(3_101)), "one request since the close");
        assert!(b.trip());
        assert!(!b.trip(), "already open");
    }
}
