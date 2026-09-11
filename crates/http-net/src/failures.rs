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
//!
//! Counters are held one ROW per peer — a fixed slot per [`FailureCause`] — so
//! recording a failure for a peer already seen is a borrowed lookup and an
//! increment, and the cardinality cap is the row count compared against
//! [`MAX_PEERS`]. A total backend outage therefore costs no allocation per
//! failed request.

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

    /// Every variant, for a zero-valued exposition of an idle process. Its order
    /// IS the row layout: `ALL[c.slot()] == c`.
    const ALL: [FailureCause; 7] = [
        FailureCause::ConnectTimeout,
        FailureCause::RequestTimeout,
        FailureCause::Tls,
        FailureCause::ConnReset,
        FailureCause::Refused,
        FailureCause::Dns,
        FailureCause::Other,
    ];

    /// This cause's slot in a peer's counter [`Row`].
    const fn slot(self) -> usize {
        match self {
            FailureCause::ConnectTimeout => 0,
            FailureCause::RequestTimeout => 1,
            FailureCause::Tls => 2,
            FailureCause::ConnReset => 3,
            FailureCause::Refused => 4,
            FailureCause::Dns => 5,
            FailureCause::Other => 6,
        }
    }
}

/// One peer's counters, one slot per [`FailureCause`].
type Row = [u64; FailureCause::ALL.len()];

/// Distinct peer label values tracked before folding into `other`.
pub const MAX_PEERS: usize = 32;

/// Peer label used once [`MAX_PEERS`] is reached.
const OVERFLOW_PEER: &str = "other";

static COUNTS: OnceLock<Mutex<HashMap<String, Row>>> = OnceLock::new();

fn counts() -> &'static Mutex<HashMap<String, Row>> {
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Count one failed request to `peer` with `cause`.
///
/// A peer already carrying a row is found by borrowed lookup and incremented in
/// place; only a label seen for the FIRST time allocates, and only while the map
/// still has room for it.
pub fn record(peer: &str, cause: FailureCause) {
    record_into(&mut counts().lock().unwrap(), peer, cause);
}

/// [`record`] against a given map — the seam the cardinality-cap test drives
/// without touching the process-wide counters other tests read.
fn record_into(counts: &mut HashMap<String, Row>, peer: &str, cause: FailureCause) {
    if let Some(row) = counts.get_mut(peer) {
        row[cause.slot()] += 1;
        return;
    }
    let key = if counts.len() < MAX_PEERS { peer } else { OVERFLOW_PEER };
    counts.entry(key.to_string()).or_insert([0; FailureCause::ALL.len()])[cause.slot()] += 1;
}

/// The count recorded for `(peer, cause)`.
pub fn get(peer: &str, cause: FailureCause) -> u64 {
    counts().lock().unwrap().get(peer).map(|row| row[cause.slot()]).unwrap_or(0)
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
    let mut peers: Vec<&str> = counts.keys().map(String::as_str).collect();
    peers.sort_unstable();
    for peer in peers {
        let row = &counts[peer];
        for cause in FailureCause::ALL {
            let v = row[cause.slot()];
            if v > 0 {
                s.push_str(&format!("{name}{{peer=\"{peer}\",cause=\"{}\"}} {v}\n", cause.label()));
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
            text.contains(
                "http_request_failures_total{peer=\"10.0.0.1:8080\",cause=\"refused\"} 2"
            ),
            "{text}"
        );
    }

    #[test]
    fn every_cause_owns_its_own_row_slot() {
        for cause in FailureCause::ALL {
            assert_eq!(FailureCause::ALL[cause.slot()], cause, "{cause:?} names its own slot");
        }
    }

    #[test]
    fn a_known_peer_is_counted_in_place_and_the_cap_folds_the_rest() {
        let mut counts: HashMap<String, Row> = HashMap::new();
        for i in 0..MAX_PEERS {
            record_into(&mut counts, &format!("10.0.0.{i}:80"), FailureCause::Refused);
        }
        assert_eq!(counts.len(), MAX_PEERS);

        // A peer already known keeps its own label however full the map is, and
        // its row is incremented in place.
        record_into(&mut counts, "10.0.0.0:80", FailureCause::Dns);
        assert_eq!(counts.len(), MAX_PEERS, "a known peer adds no row");
        assert_eq!(counts["10.0.0.0:80"][FailureCause::Refused.slot()], 1);
        assert_eq!(counts["10.0.0.0:80"][FailureCause::Dns.slot()], 1);

        // Past the cap every new label folds into one overflow row.
        record_into(&mut counts, "10.9.9.1:80", FailureCause::Tls);
        record_into(&mut counts, "10.9.9.2:80", FailureCause::Tls);
        assert!(!counts.contains_key("10.9.9.1:80"), "a peer past the cap gets no row of its own");
        assert_eq!(counts[OVERFLOW_PEER][FailureCause::Tls.slot()], 2);
        assert_eq!(counts.len(), MAX_PEERS + 1, "the overflow row is the only one added");
    }
}
