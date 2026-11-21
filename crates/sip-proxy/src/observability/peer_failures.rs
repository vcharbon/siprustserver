//! [`PeerFailures`] — per-peer failure/timeout counts, rendered as
//! `sip_proxy_peer_failures_total{peer,scope,kind}` (observability only; no
//! behaviour rides it).
//!
//! `kind` is a closed set ([`PeerFailureKind`]) and `scope` splits internal
//! from external peers ([`PeerScope`]). `peer` is open: internal peers (the
//! destinations that resolve to a known worker) are a population the
//! cluster bounds and always keep their own series; external peers share
//! the family's cap of label sets (`metric_catalogue::Cap`), past which
//! their counts land on the `_overflow` series and on
//! `sip_proxy_peer_failures_overflow_total`. No count is lost.

use std::net::SocketAddr;

use metric_values::OpenRows;

use super::catalogue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerScope {
    Internal,
    External,
}

impl PeerScope {
    pub const ALL: [PeerScope; 2] = [PeerScope::Internal, PeerScope::External];

    pub const fn label(self) -> &'static str {
        match self {
            PeerScope::Internal => "internal",
            PeerScope::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerFailureKind {
    /// A health probe's OPTIONS drew no final response within its reply
    /// window.
    ResponseTimeout,
    TransactionTimeout,
    /// Outbound send to the peer failed (ENOBUFS/EPERM/…).
    SendFailure,
    /// Outbound send refused the datagram as too long (`EMSGSIZE`): the message
    /// exceeds what this path carries even fragmented (ADR-0027). Split out
    /// from [`Self::SendFailure`] because it accuses the message, not the peer.
    MessageTooLong,
}

impl PeerFailureKind {
    pub const ALL: [PeerFailureKind; 4] = [
        PeerFailureKind::ResponseTimeout,
        PeerFailureKind::TransactionTimeout,
        PeerFailureKind::SendFailure,
        PeerFailureKind::MessageTooLong,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            PeerFailureKind::ResponseTimeout => "response_timeout",
            PeerFailureKind::TransactionTimeout => "transaction_timeout",
            PeerFailureKind::SendFailure => "send_failure",
            PeerFailureKind::MessageTooLong => "message_too_long",
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
    pub fn new() -> Self {
        Self { rows: OpenRows::new(&catalogue::PEER_FAILURES) }
    }

    pub fn record(&self, peer: &SocketAddr, scope: PeerScope, kind: PeerFailureKind) {
        let peer = peer.to_string();
        let row = [peer.as_str(), scope.label(), kind.label()];
        match scope {
            PeerScope::Internal => self.rows.add_pinned(&row, 1),
            PeerScope::External => self.rows.add(&row, 1),
        }
    }

    pub fn get(&self, peer: &str, scope: PeerScope, kind: PeerFailureKind) -> u64 {
        self.rows.get(&[peer, scope.label(), kind.label()])
    }

    /// Append `sip_proxy_peer_failures_total` and its overflow counter.
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
        assert!(text.contains("\nsip_proxy_peer_failures_overflow_total 10\n"), "{text}");
        assert_eq!(catalogue::PEER_FAILURES.check(&text), Ok(()));
    }

    #[test]
    fn internal_peers_keep_their_series_past_the_cap() {
        let pf = PeerFailures::new();
        for i in 0..DEFAULT_CAP as u32 + 1 {
            pf.record(&addr(i), PeerScope::External, PeerFailureKind::SendFailure);
        }
        let pinned = SocketAddr::from(([192, 0, 2, 1], 5060));
        pf.record(&pinned, PeerScope::Internal, PeerFailureKind::ResponseTimeout);
        assert_eq!(
            pf.get(&pinned.to_string(), PeerScope::Internal, PeerFailureKind::ResponseTimeout),
            1
        );
    }

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
            "sip_proxy_peer_failures_total{peer=\"10.0.0.1:5060\",scope=\"internal\",kind=\"response_timeout\"} 1\n"
        ));
        assert!(text.contains(
            "sip_proxy_peer_failures_total{peer=\"10.0.0.2:5060\",scope=\"external\",kind=\"send_failure\"} 1\n"
        ));
    }
}
