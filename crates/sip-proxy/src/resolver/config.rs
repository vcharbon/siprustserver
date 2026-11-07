//! The named-target resolver's cache and concurrency policy.

/// Cache + concurrency policy for [`NamedForwarder`](super::NamedForwarder).
#[derive(Debug, Clone, Copy)]
pub struct ResolverConfig {
    /// How long a successful resolution is served from cache.
    pub positive_ttl_ms: u64,
    /// How long a FAILED resolution short-circuits further lookups.
    pub negative_ttl_ms: u64,
    /// Cache size cap (names are wire-influenceable; see [`crate::resolver`]).
    pub max_entries: usize,
    /// Max concurrent in-flight resolve tasks (each can park for the full
    /// resolver timeout, so this also bounds spawned-task buildup).
    pub max_in_flight: usize,
    /// The bound on one name lookup (see [`crate::resolver`]).
    pub lookup_timeout_ms: u64,
    /// The most packets held on one name's in-flight resolve, the one that
    /// started it included.
    pub max_waiting_per_name: usize,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            positive_ttl_ms: 60_000,
            negative_ttl_ms: 5_000,
            max_entries: 4096,
            max_in_flight: 64,
            lookup_timeout_ms: 2_000,
            max_waiting_per_name: 32,
        }
    }
}

impl ResolverConfig {
    /// When the proactive refresh fires, relative to a positive store:
    /// `positive_ttl − margin`, margin = min(ttl/4, 5 s). Both the margin and
    /// the delay are clamped ≥ 1 ms so degenerate tiny TTLs still schedule
    /// (a refresh landing at/after expiry idle-stops harmlessly).
    pub(super) fn refresh_after_ms(&self) -> u64 {
        let margin = (self.positive_ttl_ms / 4).min(5_000).max(1);
        self.positive_ttl_ms.saturating_sub(margin).max(1)
    }
}
