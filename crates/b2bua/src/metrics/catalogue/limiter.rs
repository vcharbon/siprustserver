//! The worker's view of its call limiter: every `b2bua_limiter_*` family
//! (ADR-0040).

use metric_catalogue::{assert_exposition_order, label_values, Dim, Family, Labels};

use crate::limiter::refresh_batch::outcome_label;
use crate::limiter::release_queue::ReleaseFlushOutcome;
use crate::metrics::limiter::{ADMIT_CAUSES, ANSWERED_CAUSES, REFRESH_OUTCOMES, RELEASE_CAUSES};
use crate::metrics::{
    AdmitSite, LimiterFailure, LimiterOp, LimiterTask, RefreshDiscard, RefreshGiveUp, ReleaseGiveUp,
};

const OP_VALUES: [&str; 4] = label_values!(LimiterOp::ALL, LimiterOp::label);
/// A limiter request, indexed like [`LimiterOp::ALL`].
pub const OP: Dim = Dim::new("op", &OP_VALUES);

const ADMIT_CAUSE_VALUES: [&str; 5] = label_values!(ADMIT_CAUSES, LimiterFailure::label);
/// Why an admit got no usable answer, indexed like `ADMIT_CAUSES`.
pub const ADMIT_CAUSE: Dim = Dim::new("cause", &ADMIT_CAUSE_VALUES);

const ANSWERED_CAUSE_VALUES: [&str; 4] = label_values!(ANSWERED_CAUSES, LimiterFailure::label);
/// Why a refresh or a health probe got no usable answer, indexed like
/// `ANSWERED_CAUSES`.
pub const ANSWERED_CAUSE: Dim = Dim::new("cause", &ANSWERED_CAUSE_VALUES);

const RELEASE_CAUSE_VALUES: [&str; 3] = label_values!(RELEASE_CAUSES, LimiterFailure::label);
/// Why a release got no usable answer, indexed like `RELEASE_CAUSES`.
pub const RELEASE_CAUSE: Dim = Dim::new("cause", &RELEASE_CAUSE_VALUES);

const ADMIT_SITE_VALUES: [&str; 3] = label_values!(AdmitSite::ALL, AdmitSite::label);
/// Where an admit was refused on a release fence, indexed like [`AdmitSite::ALL`].
pub const ADMIT_SITE: Dim = Dim::new("site", &ADMIT_SITE_VALUES);

const REFRESH_OUTCOME_VALUES: [&str; 4] = label_values!(REFRESH_OUTCOMES, outcome_label);
/// What a refresh answer said, indexed like `REFRESH_OUTCOMES`.
pub const REFRESH_OUTCOME: Dim = Dim::new("outcome", &REFRESH_OUTCOME_VALUES);

const REFRESH_DISCARD_VALUES: [&str; 2] = label_values!(RefreshDiscard::ALL, RefreshDiscard::label);
/// Why a refresh answer found nothing to apply to, indexed like
/// [`RefreshDiscard::ALL`].
pub const REFRESH_DISCARD: Dim = Dim::new("reason", &REFRESH_DISCARD_VALUES);

const REFRESH_GIVE_UP_VALUES: [&str; 3] = label_values!(RefreshGiveUp::ALL, RefreshGiveUp::label);
/// Why a due refresh was given up, indexed like [`RefreshGiveUp::ALL`].
pub const REFRESH_GIVE_UP: Dim = Dim::new("reason", &REFRESH_GIVE_UP_VALUES);

const RELEASE_GIVE_UP_VALUES: [&str; 3] = label_values!(ReleaseGiveUp::ALL, ReleaseGiveUp::label);
/// Why a queued release was given up, indexed like [`ReleaseGiveUp::ALL`].
pub const RELEASE_GIVE_UP: Dim = Dim::new("reason", &RELEASE_GIVE_UP_VALUES);

const RELEASE_FLUSH_VALUES: [&str; 3] =
    label_values!(ReleaseFlushOutcome::ALL, ReleaseFlushOutcome::label);
/// What a planned exit's release flush did, indexed like
/// `ReleaseFlushOutcome::ALL`.
pub const RELEASE_FLUSH: Dim = Dim::new("outcome", &RELEASE_FLUSH_VALUES);

const TASK_VALUES: [&str; 3] = label_values!(LimiterTask::ALL, LimiterTask::label);
/// A supervised limiter task, indexed like [`LimiterTask::ALL`].
pub const TASK: Dim = Dim::new("task", &TASK_VALUES);

/// The breaker's new state: `open`, then `closed`.
pub const BREAKER_TO: Dim = Dim::new("to", &["open", "closed"]);

assert_exposition_order!(LimiterOp: Admit, Refresh, Release, Health);
assert_exposition_order!(AdmitSite: Initial, Fold, Service);
assert_exposition_order!(RefreshDiscard: CallGone, Stale);
assert_exposition_order!(RefreshGiveUp: LeaseExpired, Cap, Released);
assert_exposition_order!(ReleaseGiveUp: LeaseExpired, Cap, Shutdown);
assert_exposition_order!(LimiterTask: ReleaseSender, RefreshSender, BreakerProbe);
assert_exposition_order!(ReleaseFlushOutcome: Empty, Sent, GivenUp);

pub const REQUESTS: Family = Family::counter(
    "b2bua_limiter_requests_total",
    Labels::Product(&[OP]),
    "limiter requests this worker made, by request (op=admit|refresh|release|health), a name that did not resolve included",
);

pub const FAILURES: Family = Family::counter(
    "b2bua_limiter_failures_total",
    Labels::Union(&[
        &[Dim::new("op", &["admit"]), ADMIT_CAUSE],
        &[Dim::new("op", &["refresh"]), ANSWERED_CAUSE],
        &[Dim::new("op", &["release"]), RELEASE_CAUSE],
        &[Dim::new("op", &["health"]), ANSWERED_CAUSE],
    ]),
    "limiter requests with no usable answer, by request and cause (timeout: past the request's budget; transport: refused, reset, or a name that does not resolve; status: an answer other than 200; bad_answer: a 200 whose body could not be read, a limiter older than its workers shows here; breaker_open: an admit the open breaker answered without a request, which owes no release). An op=admit failure on an initial admit, or on an uncounted call, runs the call uncounted; one on a reroute of a counted call leaves it counted (ADR-0040)",
);

pub const ADMIT_RELEASED: Family = Family::counter(
    "b2bua_limiter_admit_released_total",
    Labels::Product(&[ADMIT_SITE]),
    "admits refused because the limiter had released the call's key (site=initial: the call runs uncounted, expected 0; site=fold: the call was released while the consult was in flight; site=service: the call was released while a service's admit was in flight)",
);

pub const UNCOUNTED_CALLS: Family = Family::gauge(
    "b2bua_limiter_uncounted_calls",
    Labels::None,
    "calls resident on this worker running on a limiter id the limiter does not count for them: their target names an id the limiter does not hold (their latest admit failed open: no usable answer, the breaker open, or no limiter configured; the limiter refused it on a release fence; or a refresh answered dropped; a service's change adding an id counts for its round trip), or they run on a set the limiter does not hold all of, a service's move or a failover or reroute route (refused, superseded, unanswered or in flight), until a statement of the limiter holds it, another set is run on, or a resolution after a refused route runs them on their target (ADR-0040). Every call with limiter ids is here on a worker with no limiter configured. A counted call whose refresh is answered released stays counted and is not here: the next refresh re-registers it. A call held by two nodes at once counts on each, so a fleet-wide sum can count it twice",
);

pub const REFRESH_ANSWERS: Family = Family::counter(
    "b2bua_limiter_refresh_answers_total",
    Labels::Product(&[REFRESH_OUTCOME]),
    "calls the refresh requests named, by the limiter's answer (extended; reregistered: a set the limiter no longer held re-created; released: refused by a release fence, the call stays counted; dropped: an admit of the key dropped its set, the call goes uncounted and still releases its key at its end)",
);

pub const REFRESH_ANSWERS_DISCARDED: Family = Family::counter(
    "b2bua_limiter_refresh_answers_discarded_total",
    Labels::Product(&[REFRESH_DISCARD]),
    "refresh answers handed back to their call that found nothing to apply to (reason=call_gone: the call ended or left this worker; reason=stale: a route fold restated the call's set since the refresh left)",
);

pub const REFRESH_KEYS_SENT: Family = Family::counter(
    "b2bua_limiter_refresh_keys_sent_total",
    Labels::None,
    "limiter keys the refresh requests named, over every request: over b2bua_limiter_requests_total{op=\"refresh\"}, the mean batch size",
);

pub const REFRESH_RETRIES: Family = Family::counter(
    "b2bua_limiter_refresh_retries_total",
    Labels::None,
    "limiter keys a refresh request with no usable answer put back, sent again at the next round",
);

pub const REFRESH_GIVEN_UP: Family = Family::counter(
    "b2bua_limiter_refresh_given_up_total",
    Labels::Product(&[REFRESH_GIVE_UP]),
    "due limiter refreshes given up before the limiter answered them (reason=lease_expired: due for one lease, the call's own refresh marks it again; reason=cap: the oldest entry of a full batch; reason=released: the call's release was queued)",
);

pub const REFRESH_DUE: Family = Family::gauge(
    "b2bua_limiter_refresh_due",
    Labels::None,
    "limiter keys due in this worker's refresh batch, the request in flight included (held while the breaker is open)",
);

pub const RELEASE_RETRIES: Family = Family::counter(
    "b2bua_limiter_release_retries_total",
    Labels::None,
    "queued releases put back after a failed send, one per key per failed send",
);

pub const RELEASE_GIVEN_UP: Family = Family::counter(
    "b2bua_limiter_release_given_up_total",
    Labels::Product(&[RELEASE_GIVE_UP]),
    "queued limiter releases given up before the limiter answered them, each freed by the lease (reason=lease_expired: queued longer than the lease; reason=cap: the oldest entry of a full queue; reason=shutdown: still queued when a planned exit's flush reached its bound or was held by the open breaker)",
);

pub const RELEASE_QUEUE_DEPTH: Family = Family::gauge(
    "b2bua_limiter_release_queue_depth",
    Labels::None,
    "limiter releases waiting in this worker's release queue, in flight included (a queue that only grows means the limiter is not answering)",
);

pub const RELEASE_FLUSHES: Family = Family::counter(
    "b2bua_limiter_release_flushes_total",
    Labels::Product(&[RELEASE_FLUSH]),
    "planned exits by what their release-queue flush did (outcome=empty: nothing was queued; sent: every queued release was answered; given_up: some were given up, counted in b2bua_limiter_release_given_up_total{reason=\"shutdown\"})",
);

pub const RELEASE_FLUSH_SECONDS: Family = Family::counter(
    "b2bua_limiter_release_flush_seconds_total",
    Labels::None,
    "time planned exits spent flushing the release queue",
);

pub const TASK_RESTARTS: Family = Family::counter(
    "b2bua_limiter_task_restarts_total",
    Labels::Product(&[TASK]),
    "supervised limiter tasks that panicked and were restarted with their state intact (task=release_sender|refresh_sender|breaker_probe) — expected 0",
);

pub const BREAKER_OPEN: Family = Family::gauge(
    "b2bua_limiter_breaker_open",
    Labels::None,
    "1 while this worker's limiter circuit breaker is open: admits send no request and their calls run uncounted, releases and refreshes wait",
);

pub const BREAKER_TRANSITIONS: Family = Family::counter(
    "b2bua_limiter_breaker_transitions_total",
    Labels::Product(&[BREAKER_TO]),
    "limiter circuit breaker transitions (to=open: consecutive admits with no usable answer reached the threshold, or the limiter's address was not known at boot; to=closed: a health probe was answered)",
);

pub const LEASE_SECONDS: Family = Family::gauge(
    "b2bua_limiter_lease_seconds",
    Labels::None,
    "the limiter's lease as this worker last learnt it from an admit, refresh or health answer (the default lease before any answer)",
);

pub const REFRESH_PERIOD_SECONDS: Family = Family::gauge(
    "b2bua_limiter_refresh_period_seconds",
    Labels::None,
    "how often a counted call refreshes its lease: the configured period, or a third of the learnt lease when shorter",
);

pub const LEASE_TOO_SHORT: Family = Family::counter(
    "b2bua_limiter_lease_too_short_total",
    Labels::None,
    "learnt leases the configured refresh period plus one refresh tick reaches, the first one stated and each change after it",
);

pub const REFRESH_PERIOD_CLAMPED: Family = Family::counter(
    "b2bua_limiter_refresh_period_clamped_total",
    Labels::None,
    "learnt leases that set a refresh period (a third of the lease) shorter than the configured one, the first one stated and each change after it",
);
