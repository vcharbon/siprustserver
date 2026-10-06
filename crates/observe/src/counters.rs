//! Process-wide observability counters, scraped as Prometheus text.
//!
//! Every path that silently discards something — a log line the writer could
//! not keep up with, a trace the admission chain refused — bumps a counter
//! here. Denials are counted, never logged: a rejected sample must not become
//! the very traffic-proportional output sampling exists to avoid.

use std::sync::atomic::{AtomicU64, Ordering};

use metric_catalogue::{Family, Labels};

/// Lifecycle log lines dropped because the writer's bounded queue was full.
pub static LOG_LINES_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Trace activations refused because no OTLP endpoint is configured — the
/// sampling machinery is inert in this process.
pub static TRACE_DROPPED_NO_EXPORTER: AtomicU64 = AtomicU64::new(0);

/// Trace activations refused by the token bucket (activation rate ceiling).
pub static TRACE_DENIED_RATE: AtomicU64 = AtomicU64::new(0);

/// Trace activations refused by the concurrent active-trace cap.
pub static TRACE_DENIED_ACTIVE_CAP: AtomicU64 = AtomicU64::new(0);

/// `X-Trace-Sample` header values that did not read as a float in `0..=1` and
/// were ignored in favour of the configured rate.
pub static TRACE_HEADER_MALFORMED: AtomicU64 = AtomicU64::new(0);

/// Traces admitted (a root span was opened for the call).
pub static TRACE_ADMITTED: AtomicU64 = AtomicU64::new(0);

/// Hydrated traced calls this node opened no span for because the admission
/// chain refused the adoption. The call stays sampled — sampling is monotonic —
/// so its story simply has a gap at this node; a mass takeover turns that into a
/// population, which is what makes the count worth having.
pub static TRACE_ADOPTION_REFUSED: AtomicU64 = AtomicU64::new(0);

/// Bump a counter by one.
pub fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Read a counter.
pub fn get(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}

/// Lifecycle log lines dropped.
pub const LOG_LINES_DROPPED_TOTAL: Family = Family::counter(
    "log_lines_dropped_total",
    Labels::None,
    "Lifecycle log lines dropped because the non-blocking writer queue was full.",
);

/// Trace activations refused without an exporter.
pub const TRACE_DROPPED_NO_EXPORTER_TOTAL: Family = Family::counter(
    "trace_dropped_no_exporter_total",
    Labels::None,
    "Trace activation attempts refused because no OTLP endpoint is configured.",
);

/// Trace activations refused by the token bucket.
pub const TRACE_DENIED_RATE_TOTAL: Family = Family::counter(
    "trace_denied_rate_total",
    Labels::None,
    "Trace activations refused by the activation token bucket.",
);

/// Trace activations refused by the active-trace cap.
pub const TRACE_DENIED_ACTIVE_CAP_TOTAL: Family = Family::counter(
    "trace_denied_active_cap_total",
    Labels::None,
    "Trace activations refused by the concurrent active-trace cap.",
);

/// Malformed `X-Trace-Sample` values ignored.
pub const TRACE_HEADER_MALFORMED_TOTAL: Family = Family::counter(
    "trace_header_malformed_total",
    Labels::None,
    "X-Trace-Sample header values ignored because they did not read as a float in 0..=1.",
);

/// Calls admitted for tracing.
pub const TRACE_ADMITTED_TOTAL: Family = Family::counter(
    "trace_admitted_total",
    Labels::None,
    "Calls admitted for tracing (a root span was opened).",
);

/// Hydrated traced calls refused a root span.
pub const TRACE_ADOPTION_REFUSED_TOTAL: Family = Family::counter(
    "trace_adoption_refused_total",
    Labels::None,
    "Hydrated traced calls this node opened no root span for because the admission chain refused.",
);

/// Every family [`prometheus_text`] renders, in exposition order.
pub const FAMILIES: &[Family] = &[
    LOG_LINES_DROPPED_TOTAL,
    TRACE_DROPPED_NO_EXPORTER_TOTAL,
    TRACE_DENIED_RATE_TOTAL,
    TRACE_DENIED_ACTIVE_CAP_TOTAL,
    TRACE_HEADER_MALFORMED_TOTAL,
    TRACE_ADMITTED_TOTAL,
    TRACE_ADOPTION_REFUSED_TOTAL,
];

/// Prometheus exposition for every counter above, appended by each runner's
/// `/metrics` handler.
pub fn prometheus_text() -> String {
    let mut s = String::new();
    for (family, counter) in FAMILIES.iter().zip([
        &LOG_LINES_DROPPED,
        &TRACE_DROPPED_NO_EXPORTER,
        &TRACE_DENIED_RATE,
        &TRACE_DENIED_ACTIVE_CAP,
        &TRACE_HEADER_MALFORMED,
        &TRACE_ADMITTED,
        &TRACE_ADOPTION_REFUSED,
    ]) {
        family.render_value(&mut s, get(counter));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each counter is rendered under its own family, as declared.
    #[test]
    fn each_counter_renders_under_its_own_family() {
        let before: Vec<u64> = [&TRACE_DENIED_RATE, &TRACE_ADMITTED].map(get).to_vec();
        bump(&TRACE_DENIED_RATE);
        bump(&TRACE_DENIED_RATE);
        bump(&TRACE_ADMITTED);
        let text = prometheus_text();
        for family in FAMILIES {
            assert_eq!(family.check(&text), Ok(()));
        }
        let value = |name: &str| {
            let line = text.lines().find(|l| l.starts_with(&format!("{name} "))).unwrap();
            line.rsplit(' ').next().unwrap().parse::<u64>().unwrap()
        };
        assert!(value("trace_denied_rate_total") >= before[0] + 2);
        assert!(value("trace_admitted_total") > before[1]);
    }
}
