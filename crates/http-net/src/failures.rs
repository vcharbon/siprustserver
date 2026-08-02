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
//! [`MAX_PEERS`] every peer folds into `other`, so a misconfigured target
//! cannot blow up the exposition.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

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
    pub fn label(self) -> &'static str {
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

    /// Every variant, for a zero-valued exposition of an idle process.
    const ALL: [FailureCause; 7] = [
        FailureCause::ConnectTimeout,
        FailureCause::RequestTimeout,
        FailureCause::Tls,
        FailureCause::ConnReset,
        FailureCause::Refused,
        FailureCause::Dns,
        FailureCause::Other,
    ];
}

/// Distinct peer label values tracked before folding into `other`.
pub const MAX_PEERS: usize = 32;

/// Peer label used once [`MAX_PEERS`] is reached.
const OVERFLOW_PEER: &str = "other";

static COUNTS: OnceLock<Mutex<HashMap<(String, &'static str), u64>>> = OnceLock::new();

fn counts() -> &'static Mutex<HashMap<(String, &'static str), u64>> {
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Count one failed request to `peer` with `cause`.
pub fn record(peer: &str, cause: FailureCause) {
    let mut counts = counts().lock().unwrap();
    let distinct = counts.keys().map(|(p, _)| p.as_str()).collect::<std::collections::HashSet<_>>();
    let key = if distinct.contains(peer) || distinct.len() < MAX_PEERS {
        peer.to_string()
    } else {
        OVERFLOW_PEER.to_string()
    };
    *counts.entry((key, cause.label())).or_insert(0) += 1;
}

/// The count recorded for `(peer, cause)`.
pub fn get(peer: &str, cause: FailureCause) -> u64 {
    counts().lock().unwrap().get(&(peer.to_string(), cause.label())).copied().unwrap_or(0)
}

/// Prometheus exposition, appended by each runner's `/metrics` handler. Emits
/// the metric header even with no failures, so a dashboard's query never has to
/// distinguish "no data" from "nothing broke".
pub fn prometheus_text() -> String {
    let name = "http_request_failures_total";
    let mut s = format!(
        "# HELP {name} Client HTTP requests that failed, by peer and classified cause.\n\
         # TYPE {name} counter\n"
    );
    let counts = counts().lock().unwrap();
    let mut peers: Vec<&str> = counts.keys().map(|(p, _)| p.as_str()).collect();
    peers.sort_unstable();
    peers.dedup();
    for peer in peers {
        for cause in FailureCause::ALL {
            let v = counts.get(&(peer.to_string(), cause.label())).copied().unwrap_or(0);
            if v > 0 {
                s.push_str(&format!(
                    "{name}{{peer=\"{peer}\",cause=\"{}\"}} {v}\n",
                    cause.label()
                ));
            }
        }
    }
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
        assert!(
            text.contains("http_request_failures_total{peer=\"10.0.0.1:8080\",cause=\"refused\"} 2"),
            "{text}"
        );
    }
}
