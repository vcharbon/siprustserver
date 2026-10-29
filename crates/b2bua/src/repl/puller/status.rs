//! [`PullerStatus`] — what a puller publishes of its flow.

use repl_net::frame::Watermark;

/// Shared, observable per-`(peer, flow)` puller status. The supervisor reads
/// `current` for readiness (scoped to the **Reclaim** flow) and the retained
/// `watermark`.
#[derive(Clone, Copy, Debug)]
pub struct PullerStatus {
    /// Sticky current flag — set on the first `Noop`, never cleared.
    pub current: bool,
    /// The retained watermark (advances on Noop / post-bootstrap tail). The
    /// supervisor owns the authoritative copy keyed by `(ordinal, flow)`; the
    /// puller publishes its progress here so a Park/re-add resumes from it.
    pub watermark: Watermark,
    /// Sticky **bootstrap-complete** flag (ADR-0011 X5). Set when the first
    /// catch-up `Noop` arrives OR the bootstrap hard timer fires (best-effort) OR
    /// the puller resumes warm (`W > (0,0)` — no bootstrap needed). Cleared only
    /// by a `ResetToBootstrap`. Readiness consumes this.
    pub bootstrap_complete: bool,
    /// Monotonic reset generation: bumped each time the server pushes
    /// `ResetToBootstrap` (watermark forced back to `(0,0)`). Lets the supervisor
    /// distinguish "never advanced past `(0,0)`" from "was just reset" and pull
    /// the retained watermark down accordingly (survives the `watch` channel's
    /// last-value-wins coalescing of a reset-then-advance).
    pub reset_gen: u64,
    /// Set the first time `connect` succeeds for this peer. Readiness uses it so
    /// an unreachable peer (hard-timer bootstrap-complete, never connected) does
    /// not pin the node NotReady.
    pub ever_connected: bool,
    /// Whether a stream to the peer is open now: set once a connect succeeds,
    /// cleared when that stream ends, by a cut, a failed send or the idle bound.
    pub streaming: bool,
}
