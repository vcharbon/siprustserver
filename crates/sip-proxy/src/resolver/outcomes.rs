//! The closed label sets of the named-target send and refresh metrics.

/// Outcome labels for `sip_proxy_named_sends_total{outcome}`.
pub(crate) mod outcome {
    pub const CACHED: &str = "cached";
    pub const RESOLVED: &str = "resolved";
    pub const RESOLVE_FAILED: &str = "resolve_failed";
    pub const DROPPED_NEGATIVE: &str = "dropped_negative";
    /// No resolve could hold the packet: its name's wait is full, the
    /// in-flight cap is reached, or its name's lookup is past its bound.
    pub const DROPPED_IN_FLIGHT: &str = "dropped_in_flight";
    pub const LOOKUP_TIMED_OUT: &str = "lookup_timed_out";
    /// Every outcome.
    pub const ALL: [&str; 6] =
        [CACHED, RESOLVED, RESOLVE_FAILED, DROPPED_NEGATIVE, DROPPED_IN_FLIGHT, LOOKUP_TIMED_OUT];
}

/// Outcome labels for `sip_proxy_resolver_refresh_total{outcome}` (proactive
/// refresh + startup prewarm — closed set).
pub(crate) mod refresh_outcome {
    pub const REFRESHED: &str = "refreshed";
    pub const FAILED: &str = "failed";
    pub const IDLE_STOPPED: &str = "idle_stopped";
    pub const PREWARMED: &str = "prewarmed";
    pub const PREWARM_FAILED: &str = "prewarm_failed";
    /// Every outcome.
    pub const ALL: [&str; 5] = [REFRESHED, FAILED, IDLE_STOPPED, PREWARMED, PREWARM_FAILED];
}
