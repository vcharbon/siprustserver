//! What a completed call hands the reporter beside its verdict.

use std::sync::Arc;

/// One captured call: its rendered callflow HTML (`None` only for a call that
/// never ran, which has no flow) plus a one-line detail and its end-to-end
/// time. Stored bounded per `(scenario, class)`; the page is shared, so a
/// report snapshot takes it without copying.
pub struct RenderedSample {
    pub html: Option<Arc<str>>,
    pub detail: Option<String>,
    pub e2e_ms: f64,
}

/// What one completed call leaves beyond its verdict.
#[derive(Default)]
pub struct CallEvidence {
    /// The call ran recorded.
    pub recorded: bool,
    /// The call ended on an accepted branch before the RFC audit, so the
    /// audit judged it: its UAs' dialog verdicts always, its recording when
    /// recorded.
    pub accepted: bool,
    /// Its callflow sample, when its bucket wants one.
    pub sample: Option<RenderedSample>,
}
