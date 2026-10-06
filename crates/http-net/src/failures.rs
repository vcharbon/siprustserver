//! Cause-labelled client failure counters — `http_request_failures_total{peer,
//! cause}`.
//!
//! Counters only: a failing HTTP call is a per-call event, so it never logs
//! here (ADR-0026 forbids traffic-proportional output). The caller's own
//! aggregation — the b2bua's limiter fail-open episode — owns the narrative;
//! this is the label-split an operator needs to tell "the peer refused the
//! connection" from "DNS is down" from "we ran out of time".
//!
//! Peers are cluster services, but the label space is capped anyway: past
//! the family's cap of label sets (`metric_catalogue::Cap`) a failure lands
//! on the `_overflow` series and on `http_request_failures_overflow_total`,
//! so a misconfigured target cannot blow up the exposition. Recording a
//! failure for a label set already seen allocates nothing.

use std::sync::OnceLock;

use metric_catalogue::{label_values, Dim, Family, Labels, OpenRows};

/// The classified cause of a failed request. Everything the reqwest/hyper error
/// chain can tell us apart, and nothing invented.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailureCause {
    /// The connection attempt itself timed out.
    ConnectTimeout,
    /// The request was sent but no (complete) response arrived in time.
    RequestTimeout,
    /// TLS handshake / certificate failure.
    Tls,
    /// The peer reset an established connection.
    ConnReset,
    /// The peer refused the connection.
    Refused,
    /// The name did not resolve.
    Dns,
    /// Anything the chain does not identify.
    Other,
}

impl FailureCause {
    /// The `cause` label value.
    pub const fn label(self) -> &'static str {
        match self {
            FailureCause::ConnectTimeout => "connect_timeout",
            FailureCause::RequestTimeout => "request_timeout",
            FailureCause::Tls => "tls",
            FailureCause::ConnReset => "conn_reset",
            FailureCause::Refused => "refused",
            FailureCause::Dns => "dns",
            FailureCause::Other => "other",
        }
    }

    /// Every cause, in declaration order.
    pub const ALL: [FailureCause; 7] = [
        FailureCause::ConnectTimeout,
        FailureCause::RequestTimeout,
        FailureCause::Tls,
        FailureCause::ConnReset,
        FailureCause::Refused,
        FailureCause::Dns,
        FailureCause::Other,
    ];
}

const CAUSE_VALUES: [&str; 7] = label_values!(FailureCause::ALL, FailureCause::label);

/// Client HTTP failures by peer and cause; every peer observed gets its own
/// series, under the cap.
pub const FAILURES: Family = Family::counter(
    "http_request_failures_total",
    Labels::Product(&[Dim::new("peer", &[]), Dim::new("cause", &CAUSE_VALUES)]),
    "Client HTTP requests that failed, by peer and classified cause.",
)
.capped(&FAILURES_OVERFLOW, &["peer"]);

/// Failures past the cap of `http_request_failures_total`.
pub const FAILURES_OVERFLOW: Family = Family::counter(
    "http_request_failures_overflow_total",
    Labels::None,
    "observations of http_request_failures_total past its cap, each counted on its series whose peer reads _overflow",
);

/// The families this module renders, in exposition order.
pub const FAMILIES: &[Family] = &[FAILURES, FAILURES_OVERFLOW];

metric_catalogue::assert_exposition_order!(
    FailureCause: ConnectTimeout,
    RequestTimeout,
    Tls,
    ConnReset,
    Refused,
    Dns,
    Other,
);

static ROWS: OnceLock<OpenRows> = OnceLock::new();

fn rows() -> &'static OpenRows {
    ROWS.get_or_init(|| OpenRows::new(&FAILURES))
}

/// Count one failed request to `peer` with `cause`.
pub fn record(peer: &str, cause: FailureCause) {
    rows().add(&[peer, cause.label()], 1);
}

/// The count recorded for `(peer, cause)`.
pub fn get(peer: &str, cause: FailureCause) -> u64 {
    rows().get(&[peer, cause.label()])
}

/// Prometheus exposition, appended by each runner's `/metrics` handler: the
/// family's header even with no failures, so a dashboard's query never has
/// to tell "no data" from "nothing broke", then its overflow counter.
pub fn prometheus_text() -> String {
    let mut s = String::new();
    rows().render(&mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_split_by_peer_and_cause_and_expose_as_prometheus() {
        record("10.0.0.1:8080", FailureCause::Refused);
        record("10.0.0.1:8080", FailureCause::Refused);
        record("10.0.0.1:8080", FailureCause::Dns);
        assert_eq!(get("10.0.0.1:8080", FailureCause::Refused), 2);
        assert_eq!(get("10.0.0.1:8080", FailureCause::Dns), 1);
        assert_eq!(get("10.0.0.1:8080", FailureCause::Tls), 0);

        let text = prometheus_text();
        assert!(text.contains("# TYPE http_request_failures_total counter"));
        assert_eq!(FAILURES.check(&text), Ok(()));
        assert_eq!(FAILURES_OVERFLOW.check(&text), Ok(()));
        assert!(
            text.contains(
                "http_request_failures_total{peer=\"10.0.0.1:8080\",cause=\"refused\"} 2"
            ),
            "{text}"
        );
    }

    #[test]
    fn past_the_cap_a_failure_lands_on_the_overflow_series() {
        let rows = OpenRows::new(&FAILURES);
        for i in 0..metric_catalogue::DEFAULT_CAP {
            rows.add(&[&format!("10.1.{}.{}:80", i / 250, i % 250), "refused"], 1);
        }
        rows.add(&["10.1.0.0:80", "refused"], 1);
        rows.add(&["10.9.9.1:80", "tls"], 1);
        rows.add(&["10.9.9.2:80", "tls"], 1);
        assert_eq!(rows.get(&["10.1.0.0:80", "refused"]), 2, "a known peer keeps its series");
        assert_eq!(rows.get(&["10.9.9.1:80", "tls"]), 0);
        let overflow = metric_catalogue::OVERFLOW;
        assert_eq!(rows.get(&[overflow, "tls"]), 2, "the cause is kept");
        assert_eq!(rows.overflowed(), 2);
    }
}
