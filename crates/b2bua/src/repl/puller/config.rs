//! [`PullerConfig`] — a puller's backoff, bootstrap and link bounds.

/// Default backoff floor / ceiling (ms). The ceiling bounds how long a heal
/// waits for the next reconnect: one attempt costs at most one connect bound,
/// so a low ceiling is cheap (ADR-0045).
const DEFAULT_BACKOFF_INIT_MS: u64 = 100;
const DEFAULT_BACKOFF_MAX_MS: u64 = 2_000;
/// Default bootstrap hard-timeout (ms): the upper bound on how long the puller
/// waits — for a reachable connection AND for the first catch-up `Noop` — before
/// declaring bootstrap-complete best-effort (ADR-0011 X5 — liveness over
/// completeness). The connection is NOT dropped on expiry; it keeps streaming.
const DEFAULT_BOOTSTRAP_HARD_TIMEOUT_MS: u64 = 10_000;
/// Default bound on one connect (ms).
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 2_000;
/// Default bound on a stream delivering nothing (ms): four of the server's
/// default `Noop` intervals (500 ms), so a peer that went silent is read down
/// about 2 s after its last frame.
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 2_000;
/// Default bound on one blocked send (ms).
const DEFAULT_SEND_STALL_MS: u64 = 15_000;

/// A puller's backoff, bootstrap and link bounds (tests inject short values).
#[derive(Clone, Copy, Debug)]
pub struct PullerConfig {
    /// Backoff floor in ms (`init` in `min(init·2^attempt, max)`).
    pub backoff_init_ms: u64,
    /// Backoff ceiling in ms.
    pub backoff_max_ms: u64,
    /// Bootstrap **liveness** deadline in ms (X5): the budget for reaching a peer
    /// AND for the first catch-up `Noop`. On expiry the puller marks **bootstrap-
    /// complete (best-effort)** so the node boots and serves regardless of an
    /// unreachable or pathologically-slow peer — WITHOUT dropping the connection,
    /// which keeps streaming until the real `Noop` arrives. Kept short — it gates
    /// readiness, never correctness (the `(p,b)` merge admits late frames).
    pub bootstrap_hard_timeout_ms: u64,
    /// The longest one connect is waited for, in every phase.
    pub connect_timeout_ms: u64,
    /// A stream that delivers nothing for this long is left and pulled again
    /// from the retained watermark, in every phase.
    pub idle_timeout_ms: u64,
    /// A send to the peer blocked this long leaves the stream.
    pub send_stall_ms: u64,
}

impl Default for PullerConfig {
    fn default() -> Self {
        Self {
            backoff_init_ms: DEFAULT_BACKOFF_INIT_MS,
            backoff_max_ms: DEFAULT_BACKOFF_MAX_MS,
            bootstrap_hard_timeout_ms: DEFAULT_BOOTSTRAP_HARD_TIMEOUT_MS,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            idle_timeout_ms: DEFAULT_IDLE_TIMEOUT_MS,
            send_stall_ms: DEFAULT_SEND_STALL_MS,
        }
    }
}

impl PullerConfig {
    /// Test-tier config: short backoff + a 2 s bootstrap hard timeout, so a few
    /// paused-clock advances trip the relevant deadline deterministically, and
    /// the default link bounds. ONE definition — these values are
    /// timing-load-bearing under the paused clock (tests advance exactly to a
    /// deadline).
    pub fn fast_test() -> Self {
        Self {
            backoff_init_ms: 100,
            backoff_max_ms: 1_000,
            bootstrap_hard_timeout_ms: 2_000,
            ..Self::default()
        }
    }
}
