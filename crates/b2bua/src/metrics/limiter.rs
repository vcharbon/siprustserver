//! [`LimiterCounters`] — the worker's view of its call limiter, every series
//! under `b2bua_limiter_*` (ADR-0040 Consequences).
//!
//! Counters end in `_total` and carry one closed label set each: the request
//! (`op`), why a request got no usable answer (`cause`), why an entry was
//! given up (`reason`), what an answer said (`outcome`), the admit's site
//! (`site`), the supervised task (`task`), the breaker's new state (`to`).
//! Every label value is rendered, zero included, so a rate never starts from
//! a missing series.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::limiter::release_queue::{ReleaseFlush, ReleaseFlushOutcome};
use crate::limiter::RefreshOutcome;

/// A limiter request, as the `op` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimiterOp {
    Admit,
    Refresh,
    Release,
    Health,
}

impl LimiterOp {
    /// Every request, in declaration order.
    pub const ALL: [LimiterOp; 4] =
        [LimiterOp::Admit, LimiterOp::Refresh, LimiterOp::Release, LimiterOp::Health];

    /// The metric label.
    pub const fn label(self) -> &'static str {
        match self {
            LimiterOp::Admit => "admit",
            LimiterOp::Refresh => "refresh",
            LimiterOp::Release => "release",
            LimiterOp::Health => "health",
        }
    }

    /// The causes a request of this kind can fail with: only an admit is
    /// answered by the open breaker, and a release answer has no body read.
    pub const fn causes(self) -> &'static [LimiterFailure] {
        match self {
            LimiterOp::Admit => &ADMIT_CAUSES,
            LimiterOp::Refresh | LimiterOp::Health => &ANSWERED_CAUSES,
            LimiterOp::Release => &RELEASE_CAUSES,
        }
    }
}

/// The causes an admit fails with.
pub const ADMIT_CAUSES: [LimiterFailure; 5] = [
    LimiterFailure::Timeout,
    LimiterFailure::Transport,
    LimiterFailure::Status,
    LimiterFailure::BadAnswer,
    LimiterFailure::BreakerOpen,
];

/// The causes a refresh or a health probe fails with: an answer read, no
/// breaker.
pub const ANSWERED_CAUSES: [LimiterFailure; 4] = [
    LimiterFailure::Timeout,
    LimiterFailure::Transport,
    LimiterFailure::Status,
    LimiterFailure::BadAnswer,
];

/// The causes a release fails with: no answer body is read.
pub const RELEASE_CAUSES: [LimiterFailure; 3] =
    [LimiterFailure::Timeout, LimiterFailure::Transport, LimiterFailure::Status];

/// Why a limiter request got no usable answer, as the `cause` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimiterFailure {
    /// No answer within the request's budget.
    Timeout,
    /// The request never reached an answer: connection refused or reset, a
    /// name that does not resolve.
    Transport,
    /// An answer other than 200.
    Status,
    /// A 200 whose body could not be read (no lease or one below the floor,
    /// a contradiction, an unknown shape).
    BadAnswer,
    /// An admit the open breaker answered without a request.
    BreakerOpen,
}

impl LimiterFailure {
    /// The metric label.
    pub const fn label(self) -> &'static str {
        match self {
            LimiterFailure::Timeout => "timeout",
            LimiterFailure::Transport => "transport",
            LimiterFailure::Status => "status",
            LimiterFailure::BadAnswer => "bad_answer",
            LimiterFailure::BreakerOpen => "breaker_open",
        }
    }
}

/// Where an admit was refused on a release fence, as the `site` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmitSite {
    /// The initial route: the call runs uncounted.
    Initial,
    /// A route fold: the call was released while the consult was in flight.
    Fold,
    /// A service's replacement of the call's admission set: the call was
    /// released while the admit was in flight.
    Service,
}

impl AdmitSite {
    /// Every site, in declaration order.
    pub const ALL: [AdmitSite; 3] = [AdmitSite::Initial, AdmitSite::Fold, AdmitSite::Service];

    /// The metric label.
    pub const fn label(self) -> &'static str {
        match self {
            AdmitSite::Initial => "initial",
            AdmitSite::Fold => "fold",
            AdmitSite::Service => "service",
        }
    }
}

/// Why a queued release was given up, as the `reason` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseGiveUp {
    /// Queued longer than the limiter's lease, which already freed the call.
    LeaseExpired,
    /// The oldest entry of a full queue.
    Cap,
    /// Still queued when a planned exit's flush reached its bound.
    Shutdown,
}

impl ReleaseGiveUp {
    /// Every reason, in declaration order.
    pub const ALL: [ReleaseGiveUp; 3] =
        [ReleaseGiveUp::LeaseExpired, ReleaseGiveUp::Cap, ReleaseGiveUp::Shutdown];

    /// The metric label.
    pub const fn label(self) -> &'static str {
        match self {
            ReleaseGiveUp::LeaseExpired => "lease_expired",
            ReleaseGiveUp::Cap => "cap",
            ReleaseGiveUp::Shutdown => "shutdown",
        }
    }
}

/// Why a due refresh was given up, as the `reason` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshGiveUp {
    /// Due for one lease; the call's own refresh marks it again.
    LeaseExpired,
    /// The oldest entry of a full batch.
    Cap,
    /// The call's release was queued.
    Released,
}

impl RefreshGiveUp {
    /// Every reason, in declaration order.
    pub const ALL: [RefreshGiveUp; 3] =
        [RefreshGiveUp::LeaseExpired, RefreshGiveUp::Cap, RefreshGiveUp::Released];

    /// The metric label.
    pub const fn label(self) -> &'static str {
        match self {
            RefreshGiveUp::LeaseExpired => "lease_expired",
            RefreshGiveUp::Cap => "cap",
            RefreshGiveUp::Released => "released",
        }
    }
}

/// Why a refresh answer handed back to its call found nothing to apply to,
/// as the `reason` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshDiscard {
    /// The call ended or left this worker.
    CallGone,
    /// A route fold restated the call's set since the refresh left, or the
    /// call is no longer counted.
    Stale,
}

impl RefreshDiscard {
    /// Every reason, in declaration order.
    pub const ALL: [RefreshDiscard; 2] = [RefreshDiscard::CallGone, RefreshDiscard::Stale];

    /// The metric label.
    pub const fn label(self) -> &'static str {
        match self {
            RefreshDiscard::CallGone => "call_gone",
            RefreshDiscard::Stale => "stale",
        }
    }
}

/// A supervised limiter task, as the `task` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimiterTask {
    ReleaseSender,
    RefreshSender,
    BreakerProbe,
}

impl LimiterTask {
    /// Every task, in declaration order.
    pub const ALL: [LimiterTask; 3] =
        [LimiterTask::ReleaseSender, LimiterTask::RefreshSender, LimiterTask::BreakerProbe];

    /// The metric label.
    pub const fn label(self) -> &'static str {
        match self {
            LimiterTask::ReleaseSender => "release_sender",
            LimiterTask::RefreshSender => "refresh_sender",
            LimiterTask::BreakerProbe => "breaker_probe",
        }
    }
}

/// Every refresh outcome, in label order.
pub const REFRESH_OUTCOMES: [RefreshOutcome; 4] = [
    RefreshOutcome::Extended,
    RefreshOutcome::Reregistered,
    RefreshOutcome::Released,
    RefreshOutcome::Dropped,
];

/// The worker's limiter counters and gauges. Lives in
/// [`B2buaMetrics`](super::B2buaMetrics), reached through
/// [`limiter`](super::B2buaMetrics::limiter).
#[derive(Debug, Default)]
pub struct LimiterCounters {
    requests: [AtomicU64; 4],
    failures: [[AtomicU64; 5]; 4],
    admit_released: [AtomicU64; 3],
    uncounted_calls: AtomicU64,
    refresh_answers: [AtomicU64; 4],
    refresh_discarded: [AtomicU64; 2],
    refresh_keys_sent: AtomicU64,
    refresh_retries: AtomicU64,
    refresh_given_up: [AtomicU64; 3],
    refresh_due: AtomicU64,
    release_retries: AtomicU64,
    release_given_up: [AtomicU64; 3],
    release_queue_depth: AtomicU64,
    release_flushes: [AtomicU64; 3],
    release_flush_ms: AtomicU64,
    task_restarts: [AtomicU64; 3],
    breaker_open: AtomicU64,
    breaker_transitions: [AtomicU64; 2],
    lease_ms: AtomicU64,
    lease_too_short: AtomicU64,
    refresh_period_ms: AtomicU64,
    refresh_period_clamped: AtomicU64,
}

fn add(a: &AtomicU64, n: u64) {
    a.fetch_add(n, Ordering::Relaxed);
}

fn get(a: &AtomicU64) -> u64 {
    a.load(Ordering::Relaxed)
}

fn outcome_slot(outcome: RefreshOutcome) -> usize {
    match outcome {
        RefreshOutcome::Extended => 0,
        RefreshOutcome::Reregistered => 1,
        RefreshOutcome::Released => 2,
        RefreshOutcome::Dropped => 3,
    }
}

impl LimiterCounters {
    /// One request of `op` left for the limiter.
    pub fn count_request(&self, op: LimiterOp) {
        add(&self.requests[op as usize], 1);
    }

    /// Requests of `op` that left for the limiter.
    pub fn requests_total(&self, op: LimiterOp) -> u64 {
        get(&self.requests[op as usize])
    }

    /// One request of `op` got no usable answer for `cause`.
    pub fn count_failure(&self, op: LimiterOp, cause: LimiterFailure) {
        add(&self.failures[op as usize][cause as usize], 1);
    }

    /// Requests of `op` that got no usable answer for `cause`.
    pub fn failures_total(&self, op: LimiterOp, cause: LimiterFailure) -> u64 {
        get(&self.failures[op as usize][cause as usize])
    }

    /// Requests of `op` that got no usable answer, every cause.
    pub fn failures_of(&self, op: LimiterOp) -> u64 {
        op.causes().iter().map(|c| self.failures_total(op, *c)).sum()
    }

    /// One admit at `site` refused because the limiter had released the key.
    pub fn count_admit_released(&self, site: AdmitSite) {
        add(&self.admit_released[site as usize], 1);
    }

    /// Admits at `site` refused on a release fence.
    pub fn admit_released_total(&self, site: AdmitSite) -> u64 {
        get(&self.admit_released[site as usize])
    }

    /// Set the number of resident calls that run uncounted (gauge).
    pub fn set_uncounted_calls(&self, n: u64) {
        self.uncounted_calls.store(n, Ordering::Relaxed);
    }

    /// Resident calls that run uncounted (`CallLimiterState::runs_uncounted`).
    pub fn uncounted_calls(&self) -> u64 {
        get(&self.uncounted_calls)
    }

    /// One call a refresh named was answered `outcome`.
    pub fn count_refresh_answer(&self, outcome: RefreshOutcome) {
        add(&self.refresh_answers[outcome_slot(outcome)], 1);
    }

    /// Calls refresh requests named that were answered `outcome`.
    pub fn refresh_answers_total(&self, outcome: RefreshOutcome) -> u64 {
        get(&self.refresh_answers[outcome_slot(outcome)])
    }

    /// One refresh answer handed back to its call found nothing to apply to.
    pub fn count_refresh_discarded(&self, reason: RefreshDiscard) {
        add(&self.refresh_discarded[reason as usize], 1);
    }

    /// Refresh answers discarded for `reason`.
    pub fn refresh_discarded_total(&self, reason: RefreshDiscard) -> u64 {
        get(&self.refresh_discarded[reason as usize])
    }

    /// One refresh request named `keys` keys.
    pub fn count_refresh_keys_sent(&self, keys: u64) {
        add(&self.refresh_keys_sent, keys);
    }

    /// Keys the refresh requests named, over every request.
    pub fn refresh_keys_sent_total(&self) -> u64 {
        get(&self.refresh_keys_sent)
    }

    /// `n` keys a refresh request with no usable answer put back.
    pub fn count_refresh_retries(&self, n: u64) {
        add(&self.refresh_retries, n);
    }

    /// Keys unanswered refresh requests put back.
    pub fn refresh_retries_total(&self) -> u64 {
        get(&self.refresh_retries)
    }

    /// One due refresh given up for `reason`.
    pub fn count_refresh_given_up(&self, reason: RefreshGiveUp) {
        add(&self.refresh_given_up[reason as usize], 1);
    }

    /// Due refreshes given up for `reason`.
    pub fn refresh_given_up_total(&self, reason: RefreshGiveUp) -> u64 {
        get(&self.refresh_given_up[reason as usize])
    }

    /// Set the number of keys due in the refresh batch (gauge).
    pub fn set_refresh_due(&self, n: u64) {
        self.refresh_due.store(n, Ordering::Relaxed);
    }

    /// Keys due in the refresh batch, the request in flight included.
    pub fn refresh_due(&self) -> u64 {
        get(&self.refresh_due)
    }

    /// `n` queued releases put back after a failed send.
    pub fn count_release_retries(&self, n: u64) {
        add(&self.release_retries, n);
    }

    /// Queued releases put back after a failed send.
    pub fn release_retries_total(&self) -> u64 {
        get(&self.release_retries)
    }

    /// `n` queued releases given up for `reason`.
    pub fn count_release_given_up(&self, reason: ReleaseGiveUp, n: u64) {
        add(&self.release_given_up[reason as usize], n);
    }

    /// Queued releases given up for `reason`.
    pub fn release_given_up_total(&self, reason: ReleaseGiveUp) -> u64 {
        get(&self.release_given_up[reason as usize])
    }

    /// Set the number of releases waiting in the release queue (gauge).
    pub fn set_release_queue_depth(&self, n: u64) {
        self.release_queue_depth.store(n, Ordering::Relaxed);
    }

    /// Releases waiting in the release queue, in flight included.
    pub fn release_queue_depth(&self) -> u64 {
        get(&self.release_queue_depth)
    }

    /// One planned exit flushed the release queue.
    pub fn count_release_flush(&self, flush: &ReleaseFlush) {
        add(&self.release_flushes[flush.outcome() as usize], 1);
        add(&self.release_flush_ms, flush.elapsed.as_millis() as u64);
    }

    /// Release flushes that ended `outcome`.
    pub fn release_flushes_total(&self, outcome: ReleaseFlushOutcome) -> u64 {
        get(&self.release_flushes[outcome as usize])
    }

    /// Time the release flushes took.
    pub fn release_flush_time(&self) -> Duration {
        Duration::from_millis(get(&self.release_flush_ms))
    }

    /// One supervised `task` panicked and was restarted.
    pub fn count_task_restart(&self, task: LimiterTask) {
        add(&self.task_restarts[task as usize], 1);
    }

    /// Restarts of `task`.
    pub fn task_restarts_total(&self, task: LimiterTask) -> u64 {
        get(&self.task_restarts[task as usize])
    }

    /// The breaker opened (`true`) or closed (`false`): the gauge and the
    /// transition.
    pub fn count_breaker_transition(&self, open: bool) {
        self.breaker_open.store(open as u64, Ordering::Relaxed);
        add(&self.breaker_transitions[open as usize], 1);
    }

    /// Set the breaker's state (gauge) with no transition: its start.
    pub fn set_breaker_open(&self, open: bool) {
        self.breaker_open.store(open as u64, Ordering::Relaxed);
    }

    /// Whether the breaker is open.
    pub fn breaker_open(&self) -> bool {
        get(&self.breaker_open) == 1
    }

    /// Breaker transitions to open (`true`) or closed (`false`).
    pub fn breaker_transitions_total(&self, open: bool) -> u64 {
        get(&self.breaker_transitions[open as usize])
    }

    /// Set the lease as the worker last learnt it (gauge).
    pub fn set_lease(&self, lease: Duration) {
        self.lease_ms.store(lease.as_millis() as u64, Ordering::Relaxed);
    }

    /// The lease as the worker last learnt it.
    pub fn lease(&self) -> Duration {
        Duration::from_millis(get(&self.lease_ms))
    }

    /// One learnt lease the configured refresh period plus a tick reaches.
    pub fn count_lease_too_short(&self) {
        add(&self.lease_too_short, 1);
    }

    /// Learnt leases the configured refresh period plus a tick reaches.
    pub fn lease_too_short_total(&self) -> u64 {
        get(&self.lease_too_short)
    }

    /// Set the refresh period the learnt lease sets (gauge).
    pub fn set_refresh_period(&self, period: Duration) {
        self.refresh_period_ms.store(period.as_millis() as u64, Ordering::Relaxed);
    }

    /// The refresh period the learnt lease sets.
    pub fn refresh_period(&self) -> Duration {
        Duration::from_millis(get(&self.refresh_period_ms))
    }

    /// One learnt lease that shortened the refresh period.
    pub fn count_refresh_period_clamped(&self) {
        add(&self.refresh_period_clamped, 1);
    }

    /// Learnt leases that shortened the refresh period.
    pub fn refresh_period_clamped_total(&self) -> u64 {
        get(&self.refresh_period_clamped)
    }

    /// Append every series as Prometheus text.
    pub fn render(&self, s: &mut String) {
        use super::catalogue::limiter as c;
        let secs = |d: Duration| d.as_millis() as f64 / 1000.0;
        c::REQUESTS.render(s, |series| self.requests_total(LimiterOp::ALL[series.index(&c::OP)]));
        c::FAILURES.render(s, |series| {
            let op = LimiterOp::ALL[series.block()];
            self.failures_total(op, op.causes()[series.at(1)])
        });
        c::ADMIT_RELEASED.render(s, |series| {
            self.admit_released_total(AdmitSite::ALL[series.index(&c::ADMIT_SITE)])
        });
        c::UNCOUNTED_CALLS.render_value(s, self.uncounted_calls());
        c::REFRESH_ANSWERS.render(s, |series| {
            self.refresh_answers_total(REFRESH_OUTCOMES[series.index(&c::REFRESH_OUTCOME)])
        });
        c::REFRESH_ANSWERS_DISCARDED.render(s, |series| {
            self.refresh_discarded_total(RefreshDiscard::ALL[series.index(&c::REFRESH_DISCARD)])
        });
        c::REFRESH_KEYS_SENT.render_value(s, self.refresh_keys_sent_total());
        c::REFRESH_RETRIES.render_value(s, self.refresh_retries_total());
        c::REFRESH_GIVEN_UP.render(s, |series| {
            self.refresh_given_up_total(RefreshGiveUp::ALL[series.index(&c::REFRESH_GIVE_UP)])
        });
        c::REFRESH_DUE.render_value(s, self.refresh_due());
        c::RELEASE_RETRIES.render_value(s, self.release_retries_total());
        c::RELEASE_GIVEN_UP.render(s, |series| {
            self.release_given_up_total(ReleaseGiveUp::ALL[series.index(&c::RELEASE_GIVE_UP)])
        });
        c::RELEASE_QUEUE_DEPTH.render_value(s, self.release_queue_depth());
        c::RELEASE_FLUSHES.render(s, |series| {
            self.release_flushes_total(ReleaseFlushOutcome::ALL[series.index(&c::RELEASE_FLUSH)])
        });
        c::RELEASE_FLUSH_SECONDS.render_value(s, secs(self.release_flush_time()));
        c::TASK_RESTARTS
            .render(s, |series| self.task_restarts_total(LimiterTask::ALL[series.index(&c::TASK)]));
        c::BREAKER_OPEN.render_value(s, self.breaker_open() as u64);
        c::BREAKER_TRANSITIONS
            .render(s, |series| self.breaker_transitions_total(series.index(&c::BREAKER_TO) == 0));
        c::LEASE_SECONDS.render_value(s, secs(self.lease()));
        c::REFRESH_PERIOD_SECONDS.render_value(s, secs(self.refresh_period()));
        c::LEASE_TOO_SHORT.render_value(s, self.lease_too_short_total());
        c::REFRESH_PERIOD_CLAMPED.render_value(s, self.refresh_period_clamped_total());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every label value is rendered, zero included, and a count lands on
    /// its own series only.
    #[test]
    fn every_series_is_rendered_with_its_closed_labels() {
        let m = LimiterCounters::default();
        m.count_failure(LimiterOp::Admit, LimiterFailure::BreakerOpen);
        m.count_failure(LimiterOp::Health, LimiterFailure::BadAnswer);
        m.count_refresh_answer(RefreshOutcome::Dropped);
        m.count_release_given_up(ReleaseGiveUp::Shutdown, 3);
        m.count_task_restart(LimiterTask::BreakerProbe);
        m.set_uncounted_calls(2);
        let mut s = String::new();
        m.render(&mut s);
        for line in [
            "b2bua_limiter_failures_total{op=\"admit\",cause=\"breaker_open\"} 1",
            "b2bua_limiter_failures_total{op=\"admit\",cause=\"timeout\"} 0",
            "b2bua_limiter_failures_total{op=\"health\",cause=\"bad_answer\"} 1",
            "b2bua_limiter_failures_total{op=\"release\",cause=\"status\"} 0",
            "b2bua_limiter_requests_total{op=\"release\"} 0",
            "b2bua_limiter_refresh_answers_total{outcome=\"dropped\"} 1",
            "b2bua_limiter_refresh_answers_total{outcome=\"extended\"} 0",
            "b2bua_limiter_release_given_up_total{reason=\"shutdown\"} 3",
            "b2bua_limiter_task_restarts_total{task=\"breaker_probe\"} 1",
            "b2bua_limiter_uncounted_calls 2",
            "b2bua_limiter_release_flushes_total{outcome=\"given_up\"} 0",
            "# TYPE b2bua_limiter_uncounted_calls gauge",
        ] {
            assert!(s.contains(&format!("{line}\n")), "{line} missing in\n{s}");
        }
        assert!(!s.contains("op=\"release\",cause=\"bad_answer\""), "a release body is never read");
        assert!(!s.contains("op=\"refresh\",cause=\"breaker_open\""), "only an admit is not sent");
        assert_eq!(m.failures_of(LimiterOp::Admit), 1);
    }
}
