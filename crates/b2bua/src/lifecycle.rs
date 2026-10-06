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

/// Messages and events that resolved to no call (`router::unroutable`), keyed
/// by class (`wire:ACK`, `wire:BYE:481`, `internal:timeout:OPTIONS`, …). Each
/// line names the class, why it resolved to nothing, and the most recent
/// example of its own episode (`sample`: its Call-ID or transaction branch),
/// so an episode of thousands prints a handful of lines that still point at
/// one real message.
pub struct UnroutableWaves {
    /// Each episode's payload is the (reason, sample) of its latest event.
    waves: Arc<WaveSet<(&'static str, String)>>,
}

impl Default for UnroutableWaves {
    fn default() -> Self {
        Self::new()
    }
}

impl UnroutableWaves {
    pub fn new() -> Self {
        let waves = WaveSet::with_payload(
            |class: &str, r: &WaveReport, (reason, sample): &(&'static str, String)| {
                tracing::info!(
                    node = observe::node(),
                    class,
                    reason,
                    sample = %sample,
                    edge = %r.edge,
                    elapsed_ms = r.elapsed_ms,
                    totals = %r.tally,
                    "unroutable"
                );
            },
        );
        Self { waves }
    }

    /// Record one event of `class`; `reason` says why it resolved to no call
    /// and `sample` identifies it. The sample lives with its episode, at most
    /// [`observe::MAX_KEYS`] of them.
    pub fn record(&self, class: &str, reason: &'static str, sample: String) {
        self.waves.record_with(class, "events", 1, (reason, sample));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classes a peer can mint (one per method token) open at most
    /// [`observe::MAX_KEYS`] episodes, each line naming its own class's sample.
    #[tokio::test(start_paused = true)]
    async fn the_samples_stay_bounded_whatever_the_classes() {
        let (_guard, log) = observe::test_buffer();
        let waves = UnroutableWaves::new();
        for i in 0..(observe::MAX_KEYS * 4) {
            waves.record(&format!("wire:X{i}:405"), "r", format!("call_id=c{i}"));
        }
        let lines = log.matching("unroutable");
        assert_eq!(lines.len(), observe::MAX_KEYS);
        for (i, line) in lines.iter().enumerate() {
            assert!(line.contains(&format!("class=wire:X{i}:405")), "{}", line.line());
            assert!(line.contains(&format!("sample=call_id=c{i} ")), "{}", line.line());
        }
    }

    /// A storm of one class prints a handful of lines, each naming the class,
    /// its reason and a real example.
    #[tokio::test(start_paused = true)]
    async fn an_unroutable_storm_prints_a_handful_of_lines_with_a_sample() {
        let (_guard, log) = observe::test_buffer();
        let waves = UnroutableWaves::new();
        for i in 0..5_000 {
            waves.record("wire:BYE:481", "no-ruri-callref-no-index", format!("call_id=c{i}"));
        }
        tokio::task::yield_now().await;
        tokio::time::advance(observe::DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;

        let lines = log.matching("unroutable");
        assert!(lines.len() <= 4, "5000 events must not print 5000 lines: {lines:?}");
        assert!(lines[0].contains("sample=call_id=c0"), "{}", lines[0].line());
        assert!(lines[0].contains("reason=no-ruri-callref-no-index"), "{}", lines[0].line());
        let last = lines.last().expect("the episode closes");
        assert!(last.contains("edge=falling") && last.contains("events=5000"), "{}", last.line());
        assert!(last.contains("sample=call_id=c4999"), "{}", last.line());
    }

    /// Two successive episodes of one class: each episode's lines name its
    /// own reason and sample.
    #[tokio::test(start_paused = true)]
    async fn each_successive_episode_names_its_own_sample() {
        let (_guard, log) = observe::test_buffer();
        let waves = UnroutableWaves::new();
        waves.record("wire:ACK", "no-dialog", "call_id=old".into());
        tokio::task::yield_now().await;
        tokio::time::advance(observe::DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;
        waves.record("wire:ACK", "no-transaction", "call_id=new".into());
        tokio::task::yield_now().await;
        tokio::time::advance(observe::DEFAULT_IDLE_CLOSE_AFTER).await;
        tokio::task::yield_now().await;

        let lines = log.matching("unroutable");
        let shape: Vec<_> = lines
            .iter()
            .map(|l| {
                let edge =
                    ["rising", "falling"].into_iter().find(|e| l.contains(&format!("edge={e}")));
                let sample =
                    ["old", "new"].into_iter().find(|s| l.contains(&format!("sample=call_id={s}")));
                (edge, sample)
            })
            .collect();
        assert_eq!(
            shape,
            [
                (Some("rising"), Some("old")),
                (Some("falling"), Some("old")),
                (Some("rising"), Some("new")),
                (Some("falling"), Some("new")),
            ],
            "{lines:?}",
        );
        assert!(lines[3].contains("reason=no-transaction"), "{}", lines[3].line());
    }

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
