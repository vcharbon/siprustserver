//! [`PeerFailures`] — a cardinality-bounded per-peer failure/timeout counter
//! family (observability only; no behaviour rides it).
//!
//! Records, per SIP peer (the destination / remote `SocketAddr`), a fixed set of
//! distinct failure [`PeerFailureKind`]s, split by an internal-vs-external
//! [`PeerScope`]. Rendered as `b2bua_peer_failures_total{peer,scope,kind}`.
//!
//! ## Why this exists / cardinality discipline
//! A naive `{peer}`-labelled counter is unbounded: an open SIP service talks to
//! arbitrarily many remote addresses, so the label set would grow without limit
//! and blow up Prometheus. We bound it the same way the rest of the proxy bounds
//! wire-controlled label inputs:
//!   - **`kind` is a fixed enum** ([`PeerFailureKind`]) — a small closed set.
//!   - **`scope` is a 2-value enum** ([`PeerScope`]).
//!   - **internal peers** (the cluster: the configured outbound proxy / known
//!     replication peers) are a naturally-tiny set and are **pinned** — always
//!     tracked, never evicted.
//!   - **external peers** are **LRU-bounded** to a cap (default 100, env
//!     `PEER_METRICS_EXTERNAL_CAP`). On overflow the least-recently-recorded
//!     external peer is evicted and its remaining per-kind counts are **folded**
//!     into a single aggregate bucket rendered as `peer="__external_overflow__"`
//!     — so totals are conserved (no record is ever lost, only its address
//!     resolution is coarsened).
//!
//! Memory is bounded to `internal.len()` (≈ cluster size) + `external.len()`
//! (≤ cap) + 1 overflow row, each row a fixed `[u64; N_KINDS]`. No per-call
//! growth.
//!
//! This mirrors the existing locked-map metric shape (`metrics::Inner`'s
//! `Mutex<BTreeMap<String, u64>>` keyed counters) and the LRU recency pattern in
//! `sip-proxy`'s `CancelBranchLru`, kept std-only (no `unsafe`, no new metrics
//! dependency).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

/// Internal (cluster) vs external (off-cluster) peer. Internal peers are pinned;
/// external peers are LRU-bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerScope {
    Internal,
    External,
}

impl PeerScope {
    const fn label(self) -> &'static str {
        match self {
            PeerScope::Internal => "internal",
            PeerScope::External => "external",
        }
    }
}

/// The closed set of failure kinds. Each is a distinct bucket. Keep this small
/// and stable — it is half the cardinality bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerFailureKind {
    /// Sent a request, NO final SIP response arrived: client Timer B (INVITE) /
    /// Timer F (non-INVITE) fired.
    ResponseTimeout,
    /// The longer-horizon give-up: the INVITE_INITIAL_TIMEOUT out-of-dialog
    /// backstop fired (distinct from [`Self::ResponseTimeout`]).
    TransactionTimeout,
    /// In-dialog keepalive OPTIONS got no 200 within its deadline (b2bua only;
    /// peer = that leg's next hop).
    KeepaliveTimeout,
    /// Outbound send to the peer failed (ENOBUFS/EPERM/…).
    SendFailure,
}

impl PeerFailureKind {
    const fn index(self) -> usize {
        match self {
            PeerFailureKind::ResponseTimeout => 0,
            PeerFailureKind::TransactionTimeout => 1,
            PeerFailureKind::KeepaliveTimeout => 2,
            PeerFailureKind::SendFailure => 3,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            PeerFailureKind::ResponseTimeout => "response_timeout",
            PeerFailureKind::TransactionTimeout => "transaction_timeout",
            PeerFailureKind::KeepaliveTimeout => "keepalive_timeout",
            PeerFailureKind::SendFailure => "send_failure",
        }
    }

    const ALL: [PeerFailureKind; N_KINDS] = [
        PeerFailureKind::ResponseTimeout,
        PeerFailureKind::TransactionTimeout,
        PeerFailureKind::KeepaliveTimeout,
        PeerFailureKind::SendFailure,
    ];
}

const N_KINDS: usize = 4;

/// The literal peer label for folded-in evicted external peers.
const OVERFLOW_PEER: &str = "__external_overflow__";

/// Default external-peer cap (overridable via `PEER_METRICS_EXTERNAL_CAP`).
const DEFAULT_EXTERNAL_CAP: usize = 100;

/// Per-peer counts: one `u64` per [`PeerFailureKind`].
#[derive(Default, Clone)]
struct PeerCounts {
    kinds: [u64; N_KINDS],
}

impl PeerCounts {
    fn fold_in(&mut self, other: &PeerCounts) {
        for i in 0..N_KINDS {
            self.kinds[i] = self.kinds[i].saturating_add(other.kinds[i]);
        }
    }
}

struct ExternalEntry {
    counts: PeerCounts,
    /// Monotonic recency stamp; bumped on every `record()` for this peer. The
    /// least value is the LRU victim. (A counter, not wall-clock — we only need
    /// a total order over the live external set.)
    last_seen: u64,
}

struct Inner {
    /// Pinned cluster peers — never evicted. Naturally small (cluster size).
    internal: HashMap<SocketAddr, PeerCounts>,
    /// LRU-bounded external peers (≤ `cap`).
    external: HashMap<SocketAddr, ExternalEntry>,
    /// Folded-in counts from evicted external peers (the overflow bucket).
    overflow: PeerCounts,
    /// Monotonic recency clock for the external LRU.
    tick: u64,
}

/// Cardinality-bounded per-peer failure counters. Cheap to share behind an
/// `Arc`; all mutation is behind a single `Mutex` (the record path is cold —
/// only failures hit it).
pub struct PeerFailures {
    inner: Mutex<Inner>,
    cap: usize,
}

impl PeerFailures {
    /// Construct, reading the external cap from `PEER_METRICS_EXTERNAL_CAP` once
    /// (default [`DEFAULT_EXTERNAL_CAP`] = 100). A `0`/unparseable value falls
    /// back to the default.
    pub fn new() -> Self {
        let cap = std::env::var("PEER_METRICS_EXTERNAL_CAP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&c| c > 0)
            .unwrap_or(DEFAULT_EXTERNAL_CAP);
        Self::with_cap(cap)
    }

    /// Explicit cap (tests).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                internal: HashMap::new(),
                external: HashMap::new(),
                overflow: PeerCounts::default(),
                tick: 0,
            }),
            cap: cap.max(1),
        }
    }

    /// Record one failure of `kind` against `peer` in `scope`. Internal peers are
    /// pinned; external peers are LRU-bounded with overflow folded into the
    /// aggregate bucket.
    pub fn record(&self, peer: &SocketAddr, scope: PeerScope, kind: PeerFailureKind) {
        let mut inner = self.inner.lock().unwrap();
        match scope {
            PeerScope::Internal => {
                inner.internal.entry(*peer).or_default().kinds[kind.index()] += 1;
            }
            PeerScope::External => {
                inner.tick += 1;
                let now = inner.tick;
                if let Some(e) = inner.external.get_mut(peer) {
                    e.counts.kinds[kind.index()] += 1;
                    e.last_seen = now;
                    return;
                }
                // New external peer. Evict the LRU victim first if at cap, so the
                // map never exceeds `cap` live external rows.
                if inner.external.len() >= self.cap {
                    if let Some(victim) = inner
                        .external
                        .iter()
                        .min_by_key(|(_, e)| e.last_seen)
                        .map(|(k, _)| *k)
                    {
                        if let Some(ev) = inner.external.remove(&victim) {
                            inner.overflow.fold_in(&ev.counts);
                        }
                    }
                }
                let mut counts = PeerCounts::default();
                counts.kinds[kind.index()] += 1;
                inner.external.insert(*peer, ExternalEntry { counts, last_seen: now });
            }
        }
    }

    /// Render the Prometheus exposition for this family under `metric_name`
    /// (e.g. `"b2bua_peer_failures_total"`). One line per `{peer,scope,kind}`
    /// with a non-zero count.
    pub fn prometheus_text(&self, metric_name: &str) -> String {
        let inner = self.inner.lock().unwrap();
        let mut s = String::new();
        s.push_str(&format!(
            "# HELP {metric_name} Per-peer SIP failures/timeouts by kind, split internal/external. \
External peers are LRU-bounded (PEER_METRICS_EXTERNAL_CAP, default 100); overflow folds into peer=\"{OVERFLOW_PEER}\".\n"
        ));
        s.push_str(&format!("# TYPE {metric_name} counter\n"));

        let emit = |s: &mut String, peer: &str, scope: PeerScope, counts: &PeerCounts| {
            for kind in PeerFailureKind::ALL {
                let v = counts.kinds[kind.index()];
                if v > 0 {
                    s.push_str(&format!(
                        "{metric_name}{{peer=\"{peer}\",scope=\"{}\",kind=\"{}\"}} {v}\n",
                        scope.label(),
                        kind.label(),
                    ));
                }
            }
        };

        for (addr, counts) in &inner.internal {
            emit(&mut s, &addr.to_string(), PeerScope::Internal, counts);
        }
        for (addr, e) in &inner.external {
            emit(&mut s, &addr.to_string(), PeerScope::External, &e.counts);
        }
        emit(&mut s, OVERFLOW_PEER, PeerScope::External, &inner.overflow);
        s
    }
}

impl Default for PeerFailures {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PeerFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap();
        f.debug_struct("PeerFailures")
            .field("cap", &self.cap)
            .field("internal_peers", &inner.internal.len())
            .field("external_peers", &inner.external.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u16) -> SocketAddr {
        format!("10.0.0.{}:{}", n % 250 + 1, 5060 + n).parse().unwrap()
    }

    /// Sum of every recorded count across all rows (internal + external +
    /// overflow). Must equal the number of `record()` calls.
    fn grand_total(pf: &PeerFailures) -> u64 {
        let inner = pf.inner.lock().unwrap();
        let mut t = 0;
        for c in inner.internal.values() {
            t += c.kinds.iter().sum::<u64>();
        }
        for e in inner.external.values() {
            t += e.counts.kinds.iter().sum::<u64>();
        }
        t += inner.overflow.kinds.iter().sum::<u64>();
        t
    }

    #[test]
    fn external_lru_bounds_map_and_conserves_totals() {
        let pf = PeerFailures::with_cap(4);
        // Record against 10 distinct external peers (> cap of 4).
        for i in 0..10u16 {
            pf.record(&addr(i), PeerScope::External, PeerFailureKind::ResponseTimeout);
        }
        {
            let inner = pf.inner.lock().unwrap();
            assert!(inner.external.len() <= 4, "live external rows must stay <= cap");
            // 6 of the 10 were evicted and folded into overflow.
            assert_eq!(inner.overflow.kinds[PeerFailureKind::ResponseTimeout.index()], 6);
        }
        // No count was lost.
        assert_eq!(grand_total(&pf), 10);
    }

    #[test]
    fn internal_peers_are_never_evicted() {
        let pf = PeerFailures::with_cap(2);
        let pinned = addr(200);
        pf.record(&pinned, PeerScope::Internal, PeerFailureKind::KeepaliveTimeout);
        // Churn many external peers past the cap.
        for i in 0..50u16 {
            pf.record(&addr(i), PeerScope::External, PeerFailureKind::SendFailure);
        }
        let inner = pf.inner.lock().unwrap();
        assert!(inner.internal.contains_key(&pinned), "internal peer must survive external churn");
        assert_eq!(inner.internal[&pinned].kinds[PeerFailureKind::KeepaliveTimeout.index()], 1);
        assert!(inner.external.len() <= 2);
    }

    #[test]
    fn recency_is_tracked_so_lru_evicts_the_least_recent() {
        let pf = PeerFailures::with_cap(2);
        let a = addr(1);
        let b = addr(2);
        pf.record(&a, PeerScope::External, PeerFailureKind::ResponseTimeout);
        pf.record(&b, PeerScope::External, PeerFailureKind::ResponseTimeout);
        // Touch `a` again so `b` becomes the LRU victim.
        pf.record(&a, PeerScope::External, PeerFailureKind::ResponseTimeout);
        // Insert a third → evicts `b` (least recently recorded), keeps `a`.
        let c = addr(3);
        pf.record(&c, PeerScope::External, PeerFailureKind::ResponseTimeout);
        let inner = pf.inner.lock().unwrap();
        assert!(inner.external.contains_key(&a), "recently-touched peer must survive");
        assert!(inner.external.contains_key(&c));
        assert!(!inner.external.contains_key(&b), "least-recent peer must be evicted");
    }

    #[test]
    fn prometheus_text_renders_labels_and_conserves_total() {
        let pf = PeerFailures::with_cap(100);
        pf.record(&addr(1), PeerScope::Internal, PeerFailureKind::ResponseTimeout);
        pf.record(&addr(1), PeerScope::Internal, PeerFailureKind::TransactionTimeout);
        pf.record(&addr(2), PeerScope::External, PeerFailureKind::SendFailure);
        let text = pf.prometheus_text("b2bua_peer_failures_total");
        assert!(text.contains("scope=\"internal\""));
        assert!(text.contains("scope=\"external\""));
        assert!(text.contains("kind=\"response_timeout\""));
        assert!(text.contains("kind=\"transaction_timeout\""));
        assert!(text.contains("kind=\"send_failure\""));
        // Sum of the rendered counter values == number of record() calls (3).
        let sum: u64 = text
            .lines()
            .filter(|l| l.starts_with("b2bua_peer_failures_total{"))
            .filter_map(|l| l.rsplit(' ').next())
            .filter_map(|v| v.parse::<u64>().ok())
            .sum();
        assert_eq!(sum, 3);
    }
}
