//! Backend client counters, by backend (`peer`, the target as configured):
//! the in-flight cap's saturation and wait, retries, and breaker
//! transitions. Counters only, never a per-request log line (ADR-0026). A
//! runner appends [`prometheus_text`] to its `/metrics` body. Capped like
//! [`crate::failures`]: each family has its overflow counter.

use std::sync::OnceLock;
use std::time::Duration;

use metric_catalogue::{Dim, Family, Labels};
use metric_values::OpenRows;

/// Requests that found every in-flight slot taken and waited for one.
pub const SLOT_WAITS: Family = Family::counter(
    "http_client_slot_waits_total",
    Labels::Product(&[Dim::new("peer", &[])]),
    "Backend requests that found every in-flight slot taken and waited for one.",
)
.capped(&SLOT_WAITS_OVERFLOW, &["peer"]);

/// Observations of [`SLOT_WAITS`] past its cap.
pub const SLOT_WAITS_OVERFLOW: Family = Family::counter(
    "http_client_slot_waits_overflow_total",
    Labels::None,
    "observations of http_client_slot_waits_total past its cap, each counted on its series whose peer reads _overflow",
);

/// Time spent waiting for an in-flight slot.
pub const SLOT_WAIT_MS: Family = Family::counter(
    "http_client_slot_wait_ms_total",
    Labels::Product(&[Dim::new("peer", &[])]),
    "Milliseconds backend requests spent waiting for an in-flight slot.",
)
.capped(&SLOT_WAIT_MS_OVERFLOW, &["peer"]);

/// Observations of [`SLOT_WAIT_MS`] past its cap.
pub const SLOT_WAIT_MS_OVERFLOW: Family = Family::counter(
    "http_client_slot_wait_ms_overflow_total",
    Labels::None,
    "observations of http_client_slot_wait_ms_total past its cap, each counted on its series whose peer reads _overflow",
);

/// Requests whose budget ran out waiting for a slot: never sent.
pub const SLOT_EXPIRED: Family = Family::counter(
    "http_client_slot_expired_total",
    Labels::Product(&[Dim::new("peer", &[])]),
    "Backend requests whose budget ran out waiting for an in-flight slot, never sent.",
)
.capped(&SLOT_EXPIRED_OVERFLOW, &["peer"]);

/// Observations of [`SLOT_EXPIRED`] past its cap.
pub const SLOT_EXPIRED_OVERFLOW: Family = Family::counter(
    "http_client_slot_expired_overflow_total",
    Labels::None,
    "observations of http_client_slot_expired_total past its cap, each counted on its series whose peer reads _overflow",
);

/// Second attempts sent after a connection-level failure or a timeout.
pub const RETRIES: Family = Family::counter(
    "http_client_retries_total",
    Labels::Product(&[Dim::new("peer", &[])]),
    "Backend requests sent a second time after a connection-level failure or a timeout.",
)
.capped(&RETRIES_OVERFLOW, &["peer"]);

/// Observations of [`RETRIES`] past its cap.
pub const RETRIES_OVERFLOW: Family = Family::counter(
    "http_client_retries_overflow_total",
    Labels::None,
    "observations of http_client_retries_total past its cap, each counted on its series whose peer reads _overflow",
);

/// The `to` label values of [`BREAKER_TRANSITIONS`].
const TO: &[&str] = &["open", "closed"];

/// Breaker transitions, by the state entered.
pub const BREAKER_TRANSITIONS: Family = Family::counter(
    "http_client_breaker_transitions_total",
    Labels::Product(&[Dim::new("peer", &[]), Dim::new("to", TO)]),
    "Backend circuit breaker transitions, by the state entered.",
)
.capped(&BREAKER_TRANSITIONS_OVERFLOW, &["peer"]);

/// Observations of [`BREAKER_TRANSITIONS`] past its cap.
pub const BREAKER_TRANSITIONS_OVERFLOW: Family = Family::counter(
    "http_client_breaker_transitions_overflow_total",
    Labels::None,
    "observations of http_client_breaker_transitions_total past its cap, each counted on its series whose peer reads _overflow",
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
];

struct Rows {
    slot_waits: OpenRows,
    slot_wait_ms: OpenRows,
    slot_expired: OpenRows,
    retries: OpenRows,
    transitions: OpenRows,
}

static ROWS: OnceLock<Rows> = OnceLock::new();

fn rows() -> &'static Rows {
    ROWS.get_or_init(|| Rows {
        slot_waits: OpenRows::new(&SLOT_WAITS),
        slot_wait_ms: OpenRows::new(&SLOT_WAIT_MS),
        slot_expired: OpenRows::new(&SLOT_EXPIRED),
        retries: OpenRows::new(&RETRIES),
        transitions: OpenRows::new(&BREAKER_TRANSITIONS),
    })
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
                "http_client_breaker_transitions_total{peer=\"10.9.0.1:80\",to=\"open\"} 1"
            ),
            "{text}"
        );
    }
}
