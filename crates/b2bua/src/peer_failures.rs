//! [`PeerFailures`] — per-peer failure/timeout counts, rendered as
//! `b2bua_peer_failures_total{peer,scope,kind}` (observability only; no
//! behaviour rides it).
//!
//! `kind` is a closed set ([`PeerFailureKind`]) and `scope` splits internal
//! from external peers ([`PeerScope`]). `peer` is open: internal peers (the
//! outbound proxy, the replication peers) are a population the cluster
//! bounds and always keep their own series; external peers share the
//! family's cap of label sets (`metric_catalogue::Cap`), past which their
//! counts land on the `_overflow` series and on
//! `b2bua_peer_failures_overflow_total`. No count is lost.

use std::net::SocketAddr;

use metric_catalogue::OpenRows;

use crate::metrics::catalogue;

/// Internal (cluster) vs external (off-cluster) peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerScope {
    Internal,
    External,
}

impl PeerScope {
    /// Every scope, in declaration order.
    pub const ALL: [PeerScope; 2] = [PeerScope::Internal, PeerScope::External];

    /// The `scope` label.
    pub const fn label(self) -> &'static str {
        match self {
            PeerScope::Internal => "internal",
            PeerScope::External => "external",
        }
    }
}

/// The closed set of failure kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerFailureKind {
    /// Sent a request, NO final SIP response arrived: client Timer B (INVITE) /
    /// Timer F (non-INVITE) fired.
    ResponseTimeout,
    /// The longer-horizon give-up: the configured out-of-dialog INVITE bound
    /// (default 158 s) fired (distinct from [`Self::ResponseTimeout`]).
    TransactionTimeout,
    /// In-dialog keepalive OPTIONS got no 200 within its deadline (peer = that
    /// leg's next hop).
    KeepaliveTimeout,
    /// Outbound send to the peer failed (ENOBUFS/EPERM/…).
    SendFailure,
}

impl PeerFailureKind {
    /// Every kind, in declaration order.
    pub const ALL: [PeerFailureKind; 4] = [
        PeerFailureKind::ResponseTimeout,
        PeerFailureKind::TransactionTimeout,
        PeerFailureKind::KeepaliveTimeout,
        PeerFailureKind::SendFailure,
    ];

    /// The `kind` label.
    pub const fn label(self) -> &'static str {
        match self {
            PeerFailureKind::ResponseTimeout => "response_timeout",
            PeerFailureKind::TransactionTimeout => "transaction_timeout",
            PeerFailureKind::KeepaliveTimeout => "keepalive_timeout",
            PeerFailureKind::SendFailure => "send_failure",
        }
    }
}

/// Per-peer failure counts. Share behind an `Arc`; the record path is cold
/// (only failures reach it).
#[derive(Debug)]
pub struct PeerFailures {
    rows: OpenRows,
}

impl PeerFailures {
    /// No failure counted.
    pub fn new() -> Self {
        Self { rows: OpenRows::new(&catalogue::worker::PEER_FAILURES) }
    }

    /// Count one failure of `kind` against `peer` in `scope`.
    pub fn record(&self, peer: &SocketAddr, scope: PeerScope, kind: PeerFailureKind) {
        let peer = peer.to_string();
        let row = [peer.as_str(), scope.label(), kind.label()];
        match scope {
            PeerScope::Internal => self.rows.add_pinned(&row, 1),
            PeerScope::External => self.rows.add(&row, 1),
        }
    }

    /// The count of one `{peer,scope,kind}` series.
    pub fn get(&self, peer: &str, scope: PeerScope, kind: PeerFailureKind) -> u64 {
        self.rows.get(&[peer, scope.label(), kind.label()])
    }

    /// Append `b2bua_peer_failures_total` and its overflow counter.
    pub fn render(&self, out: &mut String) {
        self.rows.render(out);
    }
}

impl Default for PeerFailures {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use metric_catalogue::{DEFAULT_CAP, OVERFLOW};

    use super::*;

    fn addr(n: u32) -> SocketAddr {
        SocketAddr::from(([10, (n >> 16) as u8, (n >> 8) as u8, n as u8], 5060))
    }

    /// Past the cap an external peer's count lands on the overflow series and
    /// is counted there; every count is kept.
    #[test]
    fn external_peers_past_the_cap_land_on_the_overflow_series() {
        let pf = PeerFailures::new();
        let n = DEFAULT_CAP as u32 + 10;
        for i in 0..n {
            pf.record(&addr(i), PeerScope::External, PeerFailureKind::ResponseTimeout);
        }
        assert_eq!(
            pf.get(&addr(0).to_string(), PeerScope::External, PeerFailureKind::ResponseTimeout),
            1
        );
        assert_eq!(
            pf.get(&addr(n - 1).to_string(), PeerScope::External, PeerFailureKind::ResponseTimeout),
            0
        );
        assert_eq!(pf.rows.get(&[OVERFLOW, "external", "response_timeout"]), 10);
        let mut text = String::new();
        pf.render(&mut text);
        assert!(text.contains("\nb2bua_peer_failures_overflow_total 10\n"), "{text}");
        assert_eq!(catalogue::worker::PEER_FAILURES.check(&text), Ok(()));
    }

    /// An internal peer keeps its own series however many external peers
    /// filled the cap.
    #[test]
    fn internal_peers_keep_their_series_past_the_cap() {
        let pf = PeerFailures::new();
        for i in 0..DEFAULT_CAP as u32 + 1 {
            pf.record(&addr(i), PeerScope::External, PeerFailureKind::SendFailure);
        }
        let pinned = SocketAddr::from(([192, 0, 2, 1], 5060));
        pf.record(&pinned, PeerScope::Internal, PeerFailureKind::KeepaliveTimeout);
        assert_eq!(
            pf.get(&pinned.to_string(), PeerScope::Internal, PeerFailureKind::KeepaliveTimeout),
            1
        );
    }

    /// Internal peers spend none of the cap: past any number of them, the
    /// external peers still get the whole budget under their own labels.
    #[test]
    fn internal_peers_leave_the_external_budget_whole() {
        let pf = PeerFailures::new();
        for i in 0..DEFAULT_CAP as u32 {
            for kind in PeerFailureKind::ALL {
                pf.record(&addr(100_000 + i), PeerScope::Internal, kind);
            }
        }
        for i in 0..DEFAULT_CAP as u32 {
            pf.record(&addr(i), PeerScope::External, PeerFailureKind::ResponseTimeout);
        }
        let last = addr(DEFAULT_CAP as u32 - 1).to_string();
        assert_eq!(pf.get(&last, PeerScope::External, PeerFailureKind::ResponseTimeout), 1);
        assert_eq!(pf.rows.overflowed(), 0);
    }

    #[test]
    fn a_failure_renders_under_its_peer_scope_and_kind() {
        let pf = PeerFailures::new();
        pf.record(&addr(1), PeerScope::Internal, PeerFailureKind::ResponseTimeout);
        pf.record(&addr(2), PeerScope::External, PeerFailureKind::SendFailure);
        let mut text = String::new();
        pf.render(&mut text);
        assert!(text.contains(
            "b2bua_peer_failures_total{peer=\"10.0.0.1:5060\",scope=\"internal\",kind=\"response_timeout\"} 1\n"
        ));
        assert!(text.contains(
            "b2bua_peer_failures_total{peer=\"10.0.0.2:5060\",scope=\"external\",kind=\"send_failure\"} 1\n"
        ));
    }
}
