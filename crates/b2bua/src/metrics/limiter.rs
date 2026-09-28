//! [`LimiterCounters`] — the worker's view of its call limiter, every series
//! under `b2bua_limiter_*` (ADR-0038 Consequences).
//!
//! Counters end in `_total` and carry one closed label set each: the request
//! (`op`), why a request got no usable answer (`cause`), why an entry was
//! given up (`reason`), what an answer said (`outcome`), the admit's site
//! (`site`), the supervised task (`task`), the breaker's new state (`to`).
//! Every label value is rendered, zero included, so a rate never starts from
//! a missing series.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::limiter::RefreshOutcome;
use crate::limiter_refresh_batch::outcome_label;
use crate::limiter_release::{ReleaseFlush, ReleaseFlushOutcome};

/// A limiter request, as the `op` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimiterOp {
    Admit,
    Refresh,
    Release,
    Health,
}

impl LimiterOp {
    const ALL: [LimiterOp; 4] =
        [LimiterOp::Admit, LimiterOp::Refresh, LimiterOp::Release, LimiterOp::Health];

    /// The metric label.
    pub fn label(self) -> &'static str {
        match self {
            LimiterOp::Admit => "admit",
            LimiterOp::Refresh => "refresh",
            LimiterOp::Release => "release",
            LimiterOp::Health => "health",
        }
    }

    /// The causes a request of this kind can fail with: only an admit is
    /// answered by the open breaker, and a release answer has no body read.
    fn causes(self) -> &'static [LimiterFailure] {
        use LimiterFailure::*;
        match self {
            LimiterOp::Admit => &[Timeout, Transport, Status, BadAnswer, BreakerOpen],
            LimiterOp::Refresh | LimiterOp::Health => &[Timeout, Transport, Status, BadAnswer],
            LimiterOp::Release => &[Timeout, Transport, Status],
        }
    }
}

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
    pub fn label(self) -> &'static str {
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

/// A supervised limiter task, as the `task` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimiterTask {
    ReleaseSender,
    RefreshSender,
    BreakerProbe,
}

/// Every refresh outcome, in label order.
const REFRESH_OUTCOMES: [RefreshOutcome; 4] = [
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
    admit_released: [AtomicU64; 2],
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

    /// Set the number of resident calls that run `fail_open` (gauge).
    pub fn set_uncounted_calls(&self, n: u64) {
        self.uncounted_calls.store(n, Ordering::Relaxed);
    }

    /// Resident calls that run `fail_open`.
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
        let head = |s: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(s, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };
        let one = |s: &mut String, name: &str, kind: &str, help: &str, v: String| {
            head(s, name, kind, help);
            let _ = writeln!(s, "{name} {v}");
        };
        let secs = |d: Duration| format!("{}", d.as_millis() as f64 / 1000.0);

        head(
            s,
            "b2bua_limiter_requests_total",
            "counter",
            "limiter requests this worker made, by request (op=admit|refresh|release|health), a name that did not resolve included",
        );
        for op in LimiterOp::ALL {
            let _ = writeln!(
                s,
                "b2bua_limiter_requests_total{{op=\"{}\"}} {}",
                op.label(),
                self.requests_total(op)
            );
        }
        head(s, "b2bua_limiter_failures_total", "counter", "limiter requests with no usable answer, by request and cause (timeout: past the request's budget; transport: refused, reset, or a name that does not resolve; status: an answer other than 200; bad_answer: a 200 whose body could not be read, a limiter older than its workers shows here; breaker_open: an admit the open breaker answered without a request, which owes no release). An op=admit failure on an initial admit, or on an uncounted call, runs the call uncounted; one on a reroute of a counted call leaves it counted (ADR-0038)");
        for op in LimiterOp::ALL {
            for cause in op.causes() {
                let _ = writeln!(
                    s,
                    "b2bua_limiter_failures_total{{op=\"{}\",cause=\"{}\"}} {}",
                    op.label(),
                    cause.label(),
                    self.failures_total(op, *cause)
                );
            }
        }
        head(s, "b2bua_limiter_admit_released_total", "counter", "admits refused because the limiter had released the call's key (site=initial: the call runs uncounted, expected 0; site=fold: the call was released while the consult was in flight)");
        for (site, label) in [(AdmitSite::Initial, "initial"), (AdmitSite::Fold, "fold")] {
            let _ = writeln!(
                s,
                "b2bua_limiter_admit_released_total{{site=\"{label}\"}} {}",
                self.admit_released_total(site)
            );
        }
        one(s, "b2bua_limiter_uncounted_calls", "gauge", "calls resident on this worker that run uncounted although their route names limiter ids: their admit failed open (no usable answer, the breaker open, or no limiter configured, on a call not counted), the limiter refused it on a release fence, or a refresh answered dropped (ADR-0038). Every call with limiter ids is here on a worker with no limiter configured. A counted call whose refresh is answered released stays counted and is not here: the next refresh re-registers it. A call held by two nodes at once counts on each, so a fleet-wide sum can count it twice", self.uncounted_calls().to_string());

        head(s, "b2bua_limiter_refresh_answers_total", "counter", "calls the refresh requests named, by the limiter's answer (extended; reregistered: a set the limiter no longer held re-created; released: refused by a release fence, the call stays counted; dropped: an admit of the key dropped its set, the call goes uncounted and still releases its key at its end)");
        for outcome in REFRESH_OUTCOMES {
            let _ = writeln!(
                s,
                "b2bua_limiter_refresh_answers_total{{outcome=\"{}\"}} {}",
                outcome_label(outcome),
                self.refresh_answers_total(outcome)
            );
        }
        head(s, "b2bua_limiter_refresh_answers_discarded_total", "counter", "refresh answers handed back to their call that found nothing to apply to (reason=call_gone: the call ended or left this worker; reason=stale: a route fold restated the call's set since the refresh left)");
        for (reason, label) in
            [(RefreshDiscard::CallGone, "call_gone"), (RefreshDiscard::Stale, "stale")]
        {
            let _ = writeln!(
                s,
                "b2bua_limiter_refresh_answers_discarded_total{{reason=\"{label}\"}} {}",
                self.refresh_discarded_total(reason)
            );
        }
        one(s, "b2bua_limiter_refresh_keys_sent_total", "counter", "limiter keys the refresh requests named, over every request: over b2bua_limiter_requests_total{op=\"refresh\"}, the mean batch size", self.refresh_keys_sent_total().to_string());
        one(s, "b2bua_limiter_refresh_retries_total", "counter", "limiter keys a refresh request with no usable answer put back, sent again at the next round", self.refresh_retries_total().to_string());
        head(s, "b2bua_limiter_refresh_given_up_total", "counter", "due limiter refreshes given up before the limiter answered them (reason=lease_expired: due for one lease, the call's own refresh marks it again; reason=cap: the oldest entry of a full batch; reason=released: the call's release was queued)");
        for (reason, label) in [
            (RefreshGiveUp::LeaseExpired, "lease_expired"),
            (RefreshGiveUp::Cap, "cap"),
            (RefreshGiveUp::Released, "released"),
        ] {
            let _ = writeln!(
                s,
                "b2bua_limiter_refresh_given_up_total{{reason=\"{label}\"}} {}",
                self.refresh_given_up_total(reason)
            );
        }
        one(s, "b2bua_limiter_refresh_due", "gauge", "limiter keys due in this worker's refresh batch, the request in flight included (held while the breaker is open)", self.refresh_due().to_string());

        one(
            s,
            "b2bua_limiter_release_retries_total",
            "counter",
            "queued releases put back after a failed send, one per key per failed send",
            self.release_retries_total().to_string(),
        );
        head(s, "b2bua_limiter_release_given_up_total", "counter", "queued limiter releases given up before the limiter answered them, each freed by the lease (reason=lease_expired: queued longer than the lease; reason=cap: the oldest entry of a full queue; reason=shutdown: still queued when a planned exit's flush reached its bound or was held by the open breaker)");
        for (reason, label) in [
            (ReleaseGiveUp::LeaseExpired, "lease_expired"),
            (ReleaseGiveUp::Cap, "cap"),
            (ReleaseGiveUp::Shutdown, "shutdown"),
        ] {
            let _ = writeln!(
                s,
                "b2bua_limiter_release_given_up_total{{reason=\"{label}\"}} {}",
                self.release_given_up_total(reason)
            );
        }
        one(s, "b2bua_limiter_release_queue_depth", "gauge", "limiter releases waiting in this worker's release queue, in flight included (a queue that only grows means the limiter is not answering)", self.release_queue_depth().to_string());
        head(s, "b2bua_limiter_release_flushes_total", "counter", "planned exits by what their release-queue flush did (outcome=empty: nothing was queued; sent: every queued release was answered; given_up: some were given up, counted in b2bua_limiter_release_given_up_total{reason=\"shutdown\"})");
        for outcome in ReleaseFlushOutcome::ALL {
            let _ = writeln!(
                s,
                "b2bua_limiter_release_flushes_total{{outcome=\"{}\"}} {}",
                outcome.label(),
                self.release_flushes_total(outcome)
            );
        }
        one(
            s,
            "b2bua_limiter_release_flush_seconds_total",
            "counter",
            "time planned exits spent flushing the release queue",
            secs(self.release_flush_time()),
        );

        head(s, "b2bua_limiter_task_restarts_total", "counter", "supervised limiter tasks that panicked and were restarted with their state intact (task=release_sender|refresh_sender|breaker_probe) — expected 0");
        for (task, label) in [
            (LimiterTask::ReleaseSender, "release_sender"),
            (LimiterTask::RefreshSender, "refresh_sender"),
            (LimiterTask::BreakerProbe, "breaker_probe"),
        ] {
            let _ = writeln!(
                s,
                "b2bua_limiter_task_restarts_total{{task=\"{label}\"}} {}",
                self.task_restarts_total(task)
            );
        }
        one(s, "b2bua_limiter_breaker_open", "gauge", "1 while this worker's limiter circuit breaker is open: admits send no request and their calls run uncounted, releases and refreshes wait", (self.breaker_open() as u64).to_string());
        head(s, "b2bua_limiter_breaker_transitions_total", "counter", "limiter circuit breaker transitions (to=open: consecutive admits with no usable answer reached the threshold, or the limiter's address was not known at boot; to=closed: a health probe was answered)");
        for (open, label) in [(true, "open"), (false, "closed")] {
            let _ = writeln!(
                s,
                "b2bua_limiter_breaker_transitions_total{{to=\"{label}\"}} {}",
                self.breaker_transitions_total(open)
            );
        }

        one(s, "b2bua_limiter_lease_seconds", "gauge", "the limiter's lease as this worker last learnt it from an admit, refresh or health answer (the default lease before any answer)", secs(self.lease()));
        one(s, "b2bua_limiter_refresh_period_seconds", "gauge", "how often a counted call refreshes its lease: the configured period, or a third of the learnt lease when shorter", secs(self.refresh_period()));
        one(s, "b2bua_limiter_lease_too_short_total", "counter", "learnt leases the configured refresh period plus one refresh tick reaches, the first one stated and each change after it", self.lease_too_short_total().to_string());
        one(s, "b2bua_limiter_refresh_period_clamped_total", "counter", "learnt leases that set a refresh period (a third of the lease) shorter than the configured one, the first one stated and each change after it", self.refresh_period_clamped_total().to_string());
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
