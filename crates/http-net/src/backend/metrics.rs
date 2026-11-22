//! Backend client counters, by backend (`peer`, the target as configured, or
//! the address of an addressed request; a share's slot counters
//! `<peer>/<name>`, [`super::BackendClient::share`]): the in-flight cap's
//! saturation and wait, retries, breaker transitions, the 5xx answers by
//! what became of their connection, the connections a client closed by
//! cause ([`crate::ejection`]) and the closes its budget refused, and the
//! answer bodies refused past the client's cap. Counters only, never a
//! per-request log line (ADR-0026). A runner appends [`prometheus_text`] to
//! its `/metrics` body. Capped like [`crate::failures`]: each family has its
//! overflow counter.

use std::sync::OnceLock;
use std::time::Duration;

use metric_catalogue::{label_values, Dim, Family, Labels};
use metric_values::OpenRows;

use crate::ejection::CloseCause;

/// Requests that found every in-flight slot taken and waited for one.
pub const SLOT_WAITS: Family = Family::counter(
    "http_request_slot_waits_total",
    Labels::Product(&[Dim::new("peer", &[])]),
    "Backend requests that found every in-flight slot taken and waited for one.",
)
.capped(&SLOT_WAITS_OVERFLOW, &["peer"]);

/// Observations of [`SLOT_WAITS`] past its cap.
pub const SLOT_WAITS_OVERFLOW: Family = Family::counter(
    "http_request_slot_waits_overflow_total",
    Labels::None,
    "observations of http_request_slot_waits_total past its cap, each counted on its series whose peer reads _overflow",
);

/// Time spent waiting for an in-flight slot.
pub const SLOT_WAIT_MS: Family = Family::counter(
    "http_request_slot_wait_ms_total",
    Labels::Product(&[Dim::new("peer", &[])]),
    "Milliseconds backend requests spent waiting for an in-flight slot.",
)
.capped(&SLOT_WAIT_MS_OVERFLOW, &["peer"]);

/// Observations of [`SLOT_WAIT_MS`] past its cap.
pub const SLOT_WAIT_MS_OVERFLOW: Family = Family::counter(
    "http_request_slot_wait_ms_overflow_total",
    Labels::None,
    "observations of http_request_slot_wait_ms_total past its cap, each counted on its series whose peer reads _overflow",
);

/// Requests whose budget ran out waiting for a slot: never sent.
pub const SLOT_EXPIRED: Family = Family::counter(
    "http_request_slot_expired_total",
    Labels::Product(&[Dim::new("peer", &[])]),
    "Backend requests whose budget ran out waiting for an in-flight slot, never sent.",
)
.capped(&SLOT_EXPIRED_OVERFLOW, &["peer"]);

/// Observations of [`SLOT_EXPIRED`] past its cap.
pub const SLOT_EXPIRED_OVERFLOW: Family = Family::counter(
    "http_request_slot_expired_overflow_total",
    Labels::None,
    "observations of http_request_slot_expired_total past its cap, each counted on its series whose peer reads _overflow",
);

/// Second attempts sent after a connection-level failure or a timeout.
pub const RETRIES: Family = Family::counter(
    "http_request_retries_total",
    Labels::Product(&[Dim::new("peer", &[])]),
    "Backend requests sent a second time after a connection-level failure or a timeout.",
)
.capped(&RETRIES_OVERFLOW, &["peer"]);

/// Observations of [`RETRIES`] past its cap.
pub const RETRIES_OVERFLOW: Family = Family::counter(
    "http_request_retries_overflow_total",
    Labels::None,
    "observations of http_request_retries_total past its cap, each counted on its series whose peer reads _overflow",
);

/// The `connection` label values of [`ERROR_ANSWERS`].
const CONNECTION: &[&str] = &["closed", "pooled"];

/// 5xx answers, by what became of the connection they came on.
pub const ERROR_ANSWERS: Family = Family::counter(
    "http_request_error_answers_total",
    Labels::Product(&[Dim::new("peer", &[]), Dim::new("connection", CONNECTION)]),
    "Backend 5xx answers, by what became of their connection: closed, or pooled (a status that closes none, a shed run not yet long enough, or the client's close budget spent).",
)
.capped(&ERROR_ANSWERS_OVERFLOW, &["peer"]);

/// Observations of [`ERROR_ANSWERS`] past its cap.
pub const ERROR_ANSWERS_OVERFLOW: Family = Family::counter(
    "http_request_error_answers_overflow_total",
    Labels::None,
    "observations of http_request_error_answers_total past its cap, each counted on its series whose peer reads _overflow",
);

/// The `cause` label values of [`CONNECTION_CLOSES`].
const CLOSE_CAUSES: [&str; 5] = label_values!(CloseCause::ALL, CloseCause::label);

/// Connections a client closed, by cause.
pub const CONNECTION_CLOSES: Family = Family::counter(
    "http_request_connection_closes_total",
    Labels::Product(&[Dim::new("peer", &[]), Dim::new("cause", &CLOSE_CAUSES)]),
    "Connections a backend client closed, by cause: status (a close status), shed (a run of shed statuses), timeout (a request abandoned before its answer, a reset), rotation (its client retired), idle (past the idle timeout or the idle cap). A connection its peer ended, or that a failed exchange or a refused body ended, is not counted.",
)
.capped(&CONNECTION_CLOSES_OVERFLOW, &["peer"]);

/// Observations of [`CONNECTION_CLOSES`] past its cap.
pub const CONNECTION_CLOSES_OVERFLOW: Family = Family::counter(
    "http_request_connection_closes_overflow_total",
    Labels::None,
    "observations of http_request_connection_closes_total past its cap, each counted on its series whose peer reads _overflow",
);

/// Closes the client's budget refused: the connection pooled instead.
pub const CLOSES_BUDGET_EXHAUSTED: Family = Family::counter(
    "http_request_connection_closes_budget_exhausted_total",
    Labels::Product(&[Dim::new("peer", &[])]),
    "Answers whose status or shed run would close their connection, pooled instead: the client's close budget was spent.",
)
.capped(&CLOSES_BUDGET_EXHAUSTED_OVERFLOW, &["peer"]);

/// Observations of [`CLOSES_BUDGET_EXHAUSTED`] past its cap.
pub const CLOSES_BUDGET_EXHAUSTED_OVERFLOW: Family = Family::counter(
    "http_request_connection_closes_budget_exhausted_overflow_total",
    Labels::None,
    "observations of http_request_connection_closes_budget_exhausted_total past its cap, each counted on its series whose peer reads _overflow",
);

/// Why an answer body was refused: the `cause` label of [`BODY_REFUSED`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyRefusal {
    /// Its declared length passed the cap: none of it was read.
    Declared,
    /// The bytes read passed the cap: the rest was left unread.
    Read,
}

impl BodyRefusal {
    /// Every refusal, in declaration order.
    pub const ALL: [BodyRefusal; 2] = [BodyRefusal::Declared, BodyRefusal::Read];

    /// The `cause` label value.
    pub const fn label(self) -> &'static str {
        match self {
            BodyRefusal::Declared => "declared",
            BodyRefusal::Read => "read",
        }
    }
}

metric_catalogue::assert_exposition_order!(BodyRefusal: Declared, Read);

/// The `cause` label values of [`BODY_REFUSED`].
const BODY_REFUSALS: [&str; 2] = label_values!(BodyRefusal::ALL, BodyRefusal::label);

/// Answer bodies refused past the client's cap.
pub const BODY_REFUSED: Family = Family::counter(
    "http_request_body_refused_total",
    Labels::Product(&[Dim::new("peer", &[]), Dim::new("cause", &BODY_REFUSALS)]),
    "Answers a backend client refused because their body passed its cap: declared (the declared length, none of it read) or read (the bytes read).",
)
.capped(&BODY_REFUSED_OVERFLOW, &["peer"]);

/// Observations of [`BODY_REFUSED`] past its cap.
pub const BODY_REFUSED_OVERFLOW: Family = Family::counter(
    "http_request_body_refused_overflow_total",
    Labels::None,
    "observations of http_request_body_refused_total past its cap, each counted on its series whose peer reads _overflow",
);

/// The `to` label values of [`BREAKER_TRANSITIONS`].
const TO: &[&str] = &["open", "closed"];

/// Breaker transitions, by the state entered.
pub const BREAKER_TRANSITIONS: Family = Family::counter(
    "http_request_breaker_transitions_total",
    Labels::Product(&[Dim::new("peer", &[]), Dim::new("to", TO)]),
    "Backend circuit breaker transitions, by the state entered.",
)
.capped(&BREAKER_TRANSITIONS_OVERFLOW, &["peer"]);

/// Observations of [`BREAKER_TRANSITIONS`] past its cap.
pub const BREAKER_TRANSITIONS_OVERFLOW: Family = Family::counter(
    "http_request_breaker_transitions_overflow_total",
    Labels::None,
    "observations of http_request_breaker_transitions_total past its cap, each counted on its series whose peer reads _overflow",
);

/// The families this module renders, in exposition order.
pub const FAMILIES: &[Family] = &[
    SLOT_WAITS,
    SLOT_WAITS_OVERFLOW,
    SLOT_WAIT_MS,
    SLOT_WAIT_MS_OVERFLOW,
    SLOT_EXPIRED,
    SLOT_EXPIRED_OVERFLOW,
    RETRIES,
    RETRIES_OVERFLOW,
    BREAKER_TRANSITIONS,
    BREAKER_TRANSITIONS_OVERFLOW,
    ERROR_ANSWERS,
    ERROR_ANSWERS_OVERFLOW,
    CONNECTION_CLOSES,
    CONNECTION_CLOSES_OVERFLOW,
    CLOSES_BUDGET_EXHAUSTED,
    CLOSES_BUDGET_EXHAUSTED_OVERFLOW,
    BODY_REFUSED,
    BODY_REFUSED_OVERFLOW,
];

struct Rows {
    slot_waits: OpenRows,
    slot_wait_ms: OpenRows,
    slot_expired: OpenRows,
    retries: OpenRows,
    transitions: OpenRows,
    error_answers: OpenRows,
    closes: OpenRows,
    budget_exhausted: OpenRows,
    body_refused: OpenRows,
}

static ROWS: OnceLock<Rows> = OnceLock::new();

fn rows() -> &'static Rows {
    ROWS.get_or_init(|| Rows {
        slot_waits: OpenRows::new(&SLOT_WAITS),
        slot_wait_ms: OpenRows::new(&SLOT_WAIT_MS),
        slot_expired: OpenRows::new(&SLOT_EXPIRED),
        retries: OpenRows::new(&RETRIES),
        transitions: OpenRows::new(&BREAKER_TRANSITIONS),
        error_answers: OpenRows::new(&ERROR_ANSWERS),
        closes: OpenRows::new(&CONNECTION_CLOSES),
        budget_exhausted: OpenRows::new(&CLOSES_BUDGET_EXHAUSTED),
        body_refused: OpenRows::new(&BODY_REFUSED),
    })
}

/// A client closed a connection to `peer` for `cause`.
pub(crate) fn connection_closed(peer: &str, cause: CloseCause) {
    rows().closes.add(&[peer, cause.label()], 1);
}

/// A close of a connection to `peer` found the client's budget spent.
pub(crate) fn closes_budget_exhausted(peer: &str) {
    rows().budget_exhausted.add(&[peer], 1);
}

/// An answer body of `peer` was refused past the client's cap.
pub(crate) fn body_refused(peer: &str, refusal: BodyRefusal) {
    rows().body_refused.add(&[peer, refusal.label()], 1);
}

/// Connections to `peer` a client closed for `cause`.
pub fn connection_closes(peer: &str, cause: CloseCause) -> u64 {
    rows().closes.get(&[peer, cause.label()])
}

/// Closes of connections to `peer` the client's budget refused.
pub fn closes_budget_exhausted_count(peer: &str) -> u64 {
    rows().budget_exhausted.get(&[peer])
}

/// Answer bodies of `peer` refused as `refusal` states.
pub fn bodies_refused(peer: &str, refusal: BodyRefusal) -> u64 {
    rows().body_refused.get(&[peer, refusal.label()])
}

/// A request to `peer` found every slot taken.
pub(crate) fn slot_waited(peer: &str) {
    rows().slot_waits.add(&[peer], 1);
}

/// A request to `peer` waited `waited` for a slot, got one or not.
pub(crate) fn slot_wait_ended(peer: &str, waited: Duration, got_one: bool) {
    rows().slot_wait_ms.add(&[peer], u64::try_from(waited.as_millis()).unwrap_or(u64::MAX));
    if !got_one {
        rows().slot_expired.add(&[peer], 1);
    }
}

/// A request to `peer` was sent a second time.
pub(crate) fn retried(peer: &str) {
    rows().retries.add(&[peer], 1);
}

/// `peer`'s breaker opened (`true`) or closed.
pub(crate) fn breaker_moved(peer: &str, open: bool) {
    rows().transitions.add(&[peer, if open { "open" } else { "closed" }], 1);
}

/// A 5xx answer of `peer` came; its connection `closed` or was pooled.
pub(crate) fn error_answered(peer: &str, closed: bool) {
    rows().error_answers.add(&[peer, if closed { "closed" } else { "pooled" }], 1);
}

/// 5xx answers of `peer` whose connection was closed (`true`) or pooled.
pub fn error_answers(peer: &str, closed: bool) -> u64 {
    rows().error_answers.get(&[peer, if closed { "closed" } else { "pooled" }])
}

/// Requests to `peer` that waited for a slot.
pub fn slot_waits(peer: &str) -> u64 {
    rows().slot_waits.get(&[peer])
}

/// Milliseconds requests to `peer` waited for a slot.
pub fn slot_wait_ms(peer: &str) -> u64 {
    rows().slot_wait_ms.get(&[peer])
}

/// Requests to `peer` whose budget ran out waiting for a slot.
pub fn slot_expired(peer: &str) -> u64 {
    rows().slot_expired.get(&[peer])
}

/// Requests to `peer` sent a second time.
pub fn retries(peer: &str) -> u64 {
    rows().retries.get(&[peer])
}

/// Transitions of `peer`'s breaker into open (`true`) or closed.
pub fn breaker_transitions(peer: &str, open: bool) -> u64 {
    rows().transitions.get(&[peer, if open { "open" } else { "closed" }])
}

/// Prometheus exposition, appended by each runner's `/metrics` handler:
/// every family's header even before its first observation, each followed
/// by its overflow counter.
pub fn prometheus_text() -> String {
    let rows = rows();
    let mut s = String::new();
    rows.slot_waits.render(&mut s);
    rows.slot_wait_ms.render(&mut s);
    rows.slot_expired.render(&mut s);
    rows.retries.render(&mut s);
    rows.transitions.render(&mut s);
    rows.error_answers.render(&mut s);
    rows.closes.render(&mut s);
    rows.budget_exhausted.render(&mut s);
    rows.body_refused.render(&mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_renders_and_checks() {
        slot_waited("10.9.0.1:80");
        slot_wait_ended("10.9.0.1:80", Duration::from_millis(40), false);
        retried("10.9.0.1:80");
        breaker_moved("10.9.0.1:80", true);
        error_answered("10.9.0.1:80", true);
        connection_closed("10.9.0.1:80", CloseCause::Shed);
        closes_budget_exhausted("10.9.0.1:80");
        body_refused("10.9.0.1:80", BodyRefusal::Read);
        let text = prometheus_text();
        for family in FAMILIES {
            assert_eq!(family.check(&text), Ok(()), "{}", family.name);
        }
        assert_eq!(
            (slot_waits("10.9.0.1:80"), slot_wait_ms("10.9.0.1:80"), slot_expired("10.9.0.1:80")),
            (1, 40, 1)
        );
        assert!(
            text.contains(
                "http_request_breaker_transitions_total{peer=\"10.9.0.1:80\",to=\"open\"} 1"
            ),
            "{text}"
        );
        for line in [
            "http_request_connection_closes_total{peer=\"10.9.0.1:80\",cause=\"shed\"} 1",
            "http_request_connection_closes_budget_exhausted_total{peer=\"10.9.0.1:80\"} 1",
            "http_request_body_refused_total{peer=\"10.9.0.1:80\",cause=\"read\"} 1",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    }
}
