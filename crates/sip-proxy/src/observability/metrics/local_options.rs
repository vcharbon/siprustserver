//! The supervision OPTIONS the proxy answers itself (ADR-0069), by how.

use super::ProxyMetrics;

/// How a supervision OPTIONS ended, for `sip_proxy_local_options_total{answer}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalOptions {
    /// Answered `200`: a worker reads `Alive` or `Slow`.
    Ok,
    /// Dropped unanswered: no worker serves, or the first view is not in.
    Dropped,
}

impl LocalOptions {
    pub const ALL: [LocalOptions; 2] = [LocalOptions::Ok, LocalOptions::Dropped];

    pub const fn as_str(self) -> &'static str {
        match self {
            LocalOptions::Ok => "ok",
            LocalOptions::Dropped => "dropped",
        }
    }
}

impl ProxyMetrics {
    /// Count one supervision OPTIONS under how it ended; each retransmission
    /// counts.
    pub fn record_local_options(&self, answer: LocalOptions) {
        self.local_options.add(&[answer.as_str()], 1);
    }

    pub fn local_options(&self, answer: LocalOptions) -> u64 {
        self.local_options.get(&[answer.as_str()])
    }
}
