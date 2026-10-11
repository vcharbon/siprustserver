//! [`WindowBreaker`] — the rule that opens a backend's circuit breaker
//! (ADR-0043): it opens when at least [`BreakerConfig::min_sent`] requests
//! were in flight during the last [`BreakerConfig::window`] (sent inside it,
//! or sent before it and still unanswered when it began) and not one HTTP
//! response, of any status, arrived inside it. One dead peer among several
//! never opens it (the others answer); a total outage or a hung application
//! does, whatever the attempt budget against the window. The rule is
//! evaluated each time a request ends without a response. Pure: the caller
//! feeds it the instants.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

/// When a backend's breaker opens and how it probes while open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakerConfig {
    /// The window the rule looks back over.
    pub window: Duration,
    /// Requests in flight during the window, none answered, that open it.
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

/// One request the breaker counts, from [`WindowBreaker::sent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sent(u64);

/// A request counted, and when it ended once over.
#[derive(Debug)]
struct Request {
    id: u64,
    ended: Option<Instant>,
}

/// The breaker's state under the window rule.
#[derive(Debug)]
pub struct WindowBreaker {
    window: Duration,
    min_sent: usize,
    /// Every request in flight, and every one that ended inside the window.
    requests: VecDeque<Request>,
    next: u64,
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
            requests: VecDeque::new(),
            next: 0,
            answered: None,
            open: false,
        }
    }

    /// Whether it is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// A request left at `now`.
    pub fn sent(&mut self, now: Instant) -> Sent {
        self.forget_before(now);
        let id = self.next;
        self.next += 1;
        self.requests.push_back(Request { id, ended: None });
        Sent(id)
    }

    /// `request` got an HTTP response, of any status, at `now`.
    pub fn answered(&mut self, request: Sent, now: Instant) {
        self.answered = Some(now);
        self.end(request, now);
    }

    /// `request` ended at `now`, its caller having given up on it: it stays
    /// counted inside the window, and is not evaluated.
    pub fn dropped(&mut self, request: Sent, now: Instant) {
        self.end(request, now);
    }

    /// `request` ended at `now` without a response: whether this opens the
    /// breaker.
    pub fn unanswered(&mut self, request: Sent, now: Instant) -> bool {
        self.end(request, now);
        if self.open {
            return false;
        }
        self.forget_before(now);
        let answered_inside = self.answered.is_some_and(|t| now.duration_since(t) <= self.window);
        if self.requests.len() < self.min_sent || answered_inside {
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
        self.requests.clear();
        self.answered = Some(now);
        std::mem::replace(&mut self.open, false)
    }

    fn end(&mut self, request: Sent, now: Instant) {
        if let Some(r) = self.requests.iter_mut().find(|r| r.id == request.0) {
            r.ended.get_or_insert(now);
        }
    }

    /// Drop the requests that ended before the window.
    fn forget_before(&mut self, now: Instant) {
        let window = self.window;
        self.requests.retain(|r| r.ended.is_none_or(|t| now.duration_since(t) <= window));
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
        let first = b.sent(at(0));
        let second = b.sent(at(100));
        assert!(!b.unanswered(first, at(150)), "two sent so far");
        let _third = b.sent(at(200));
        assert!(b.unanswered(second, at(250)), "three in flight inside 2 s open it");
        assert!(b.is_open());
    }

    /// Three requests sent at once to a hung backend, each timing out at
    /// 2.5 s, past the window: they were in flight inside it, so it opens.
    #[test]
    fn a_hung_burst_longer_than_the_window_opens_it() {
        let mut b = breaker();
        let burst: Vec<_> = (0..3).map(|_| b.sent(at(0))).collect();
        assert!(b.unanswered(burst[0], at(2_500)));
    }

    /// A `503` inside the window is an answer: it stays closed.
    #[test]
    fn any_response_inside_the_window_keeps_it_closed() {
        let mut b = breaker();
        let first = b.sent(at(0));
        b.answered(first, at(10));
        for ms in [100, 200, 300] {
            let request = b.sent(at(ms));
            assert!(!b.unanswered(request, at(ms + 5)));
        }
        assert!(!b.is_open());
    }

    /// Three requests spread over 2.2 s, each ending at once, never fill one
    /// window.
    #[test]
    fn requests_spread_wider_than_the_window_keep_it_closed() {
        let mut b = breaker();
        for ms in [0, 1_100, 2_200] {
            let request = b.sent(at(ms));
            assert!(!b.unanswered(request, at(ms + 1)), "at {ms}");
        }
    }

    /// Requests their callers gave up on stay counted but are not evaluated.
    #[test]
    fn a_dropped_request_is_counted_but_never_evaluated() {
        let mut b = breaker();
        let requests: Vec<_> = (0..3).map(|n| b.sent(at(n * 10))).collect();
        b.dropped(requests[0], at(40));
        b.dropped(requests[1], at(50));
        assert!(!b.is_open());
        assert!(b.unanswered(requests[2], at(60)), "the three were in flight inside 2 s");
    }

    /// One peer of two dead, ten requests a second alternating answered and
    /// failed: it never opens.
    #[test]
    fn one_dead_peer_among_several_never_opens_it() {
        let mut b = breaker();
        for n in 0..100u64 {
            let now = at(n * 100);
            let request = b.sent(now);
            if n % 2 == 0 {
                b.answered(request, now + Duration::from_millis(5));
            } else {
                assert!(!b.unanswered(request, now + Duration::from_millis(5)), "request {n}");
            }
        }
    }

    /// A close starts the window afresh; a trip opens it at once.
    #[test]
    fn a_close_starts_afresh_and_a_trip_opens_at_once() {
        let mut b = breaker();
        let requests: Vec<_> = [0, 10, 20].into_iter().map(|ms| b.sent(at(ms))).collect();
        assert!(b.unanswered(requests[0], at(30)), "three in flight inside the window");
        assert!(b.close(at(1_000)));
        assert!(!b.is_open());
        let request = b.sent(at(3_100));
        assert!(!b.unanswered(request, at(3_101)), "one request since the close");
        assert!(b.trip());
        assert!(!b.trip(), "already open");
    }
}
