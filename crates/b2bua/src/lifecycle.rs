//! The b2bua's aggregated lifecycle-log vocabulary (ADR-0026 §1).
//!
//! Every per-call event class the worker can emit in a burst — a takeover
//! storm, a keepalive-timeout wave, a backend outage — is recorded into a
//! [`WaveSet`] built here, so the log carries a rising edge, a ~5 s summary and
//! a falling-edge total per episode instead of one line per call. The
//! constructors below own the field shape of each class; the aggregation rules
//! themselves live in `observe`.
//!
//! State transitions and rare events do NOT belong here — they are individual
//! `info!` lines at their own site.

use std::sync::Arc;

use observe::{WaveReport, WaveSet};

/// Acting-backup takeover, keyed by the dead peer whose partition we are
/// serving: `hydrated` (calls loaded off the replica), `resolved` (in-dialog
/// requests re-keyed through the replica index), `self_released` (takeover
/// copies shed once their transactions cleared) and `refused_terminated` (a
/// released copy's `Terminated` replica refused, the datagram falling to the
/// orphan path) fold into ONE episode per peer.
pub fn takeover_waves() -> Arc<WaveSet> {
    WaveSet::new(|peer: &str, r: &WaveReport| {
        tracing::info!(
            node = observe::node(),
            peer,
            edge = %r.edge,
            elapsed_ms = r.elapsed_ms,
            totals = %r.tally,
            "acting-backup takeover"
        );
    })
}

/// Keepalive timeouts, keyed by the failed leg's egress hop. A wave here is a
/// peer or a network path going away, not one call dying.
pub fn keepalive_timeout_waves() -> Arc<WaveSet> {
    WaveSet::new(|hop: &str, r: &WaveReport| {
        tracing::info!(
            node = observe::node(),
            peer = hop,
            edge = %r.edge,
            elapsed_ms = r.elapsed_ms,
            totals = %r.tally,
            "keepalive-timeout wave"
        );
    })
}

/// A backend degradation episode — decision-engine deadline breaches, limiter
/// fail-open — keyed by the target the calls were headed for. `backend` names
/// which dependency; the episode ends once successes have run for the idle
/// window, so a backend answering intermittently stays one episode.
pub fn backend_waves(backend: &'static str) -> Arc<WaveSet> {
    WaveSet::new(move |target: &str, r: &WaveReport| {
        tracing::info!(
            node = observe::node(),
            backend,
            target,
            edge = %r.edge,
            elapsed_ms = r.elapsed_ms,
            totals = %r.tally,
            "backend degraded"
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The traffic-independence guarantee at the site that would break it
    /// first: a 5000-call failover owes a handful of lines, and the last one
    /// carries the episode totals — including the self-releases that ended it.
    #[tokio::test(start_paused = true)]
    async fn a_five_thousand_call_takeover_prints_a_handful_of_lines() {
        let (_guard, log) = observe::test_buffer();
        let waves = takeover_waves();

        for _ in 0..5_000 {
            waves.record("w-3", "hydrated", 1);
        }
        for _ in 0..5_000 {
            waves.record("w-3", "resolved", 1);
        }
        for _ in 0..5_000 {
            waves.record("w-3", "self_released", 1);
        }
        // Let the episode's driver task arm its timer before moving the clock,
        // then advance exactly to the idle window that closes the episode.
        tokio::task::yield_now().await;
        tokio::time::advance(observe::DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;

        let lines = log.matching("acting-backup takeover");
        assert!(lines.len() <= 4, "15000 events must not print 15000 lines: {lines:?}");
        assert!(lines[0].contains("edge=rising"), "{:?}", lines[0].line());
        let last = lines.last().expect("the episode closes");
        assert!(last.contains("edge=falling"), "{}", last.line());
        assert!(
            last.contains("totals=hydrated=5000 resolved=5000 self_released=5000"),
            "the falling edge carries the episode totals: {}",
            last.line(),
        );
        assert!(last.contains("peer=w-3"), "{}", last.line());
    }
}
