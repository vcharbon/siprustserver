//! Process-wide observability counters, scraped as Prometheus text.
//!
//! Every path that silently discards something — a log line the writer could
//! not keep up with, a trace the admission chain refused — bumps a counter
//! here. Denials are counted, never logged: a rejected sample must not become
//! the very traffic-proportional output sampling exists to avoid.

use std::sync::atomic::{AtomicU64, Ordering};

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

/// Bump a counter by one.
pub fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Read a counter.
pub fn get(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}

/// Prometheus exposition for every counter above, appended by each runner's
/// `/metrics` handler.
pub fn prometheus_text() -> String {
    let mut s = String::new();
    for (name, help, value) in [
        (
            "log_lines_dropped_total",
            "Lifecycle log lines dropped because the non-blocking writer queue was full.",
            get(&LOG_LINES_DROPPED),
        ),
        (
            "trace_dropped_no_exporter_total",
            "Trace activation attempts refused because no OTLP endpoint is configured.",
            get(&TRACE_DROPPED_NO_EXPORTER),
        ),
        (
            "trace_denied_rate_total",
            "Trace activations refused by the activation token bucket.",
            get(&TRACE_DENIED_RATE),
        ),
        (
            "trace_denied_active_cap_total",
            "Trace activations refused by the concurrent active-trace cap.",
            get(&TRACE_DENIED_ACTIVE_CAP),
        ),
        (
            "trace_header_malformed_total",
            "X-Trace-Sample header values ignored because they did not read as a float in 0..=1.",
            get(&TRACE_HEADER_MALFORMED),
        ),
        (
            "trace_admitted_total",
            "Calls admitted for tracing (a root span was opened).",
            get(&TRACE_ADMITTED),
        ),
    ] {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"));
    }
    s
}
