//! The reporter's inspection accessors: bucket counts and stored samples, for
//! tests and summaries.

use super::Reporter;
use crate::class::ResultClass;
use crate::scenarios::ScenarioId;

impl Reporter {
    /// Total count for a `(scenario, class)` bucket, summed across all case
    /// sub-buckets (for tests / summaries).
    pub fn count(&self, scenario: ScenarioId, class: &ResultClass) -> u64 {
        let label = class.label();
        self.inner
            .lock()
            .unwrap()
            .counts
            .iter()
            .filter(|((s, c, _), _)| *s == scenario && *c == label)
            .map(|(_, n)| *n)
            .sum()
    }

    /// Number of stored samples for a `(scenario, class)` bucket, across all
    /// case sub-buckets.
    pub fn sample_count(&self, scenario: ScenarioId, class: &ResultClass) -> u32 {
        let label = class.label();
        self.inner
            .lock()
            .unwrap()
            .sample_taken
            .iter()
            .filter(|((s, c, _), _)| *s == scenario && *c == label)
            .map(|(_, t)| t.taken)
            .sum()
    }

    /// Number of stored samples for a `(scenario, class)` bucket that carry no
    /// rendered callflow, across all case sub-buckets.
    pub fn stub_sample_count(&self, scenario: ScenarioId, class: &ResultClass) -> usize {
        let label = class.label();
        self.inner
            .lock()
            .unwrap()
            .samples
            .iter()
            .filter(|((s, c, _), _)| *s == scenario && *c == label)
            .map(|(_, stored)| stored.iter().filter(|s| s.html.is_none()).count())
            .sum()
    }

    /// Total completed calls across all buckets.
    pub fn total_calls(&self) -> u64 {
        self.inner.lock().unwrap().counts.values().sum()
    }

    /// Recorded calls of `scenario` the RFC audit judged.
    pub fn audited_count(&self, scenario: ScenarioId) -> u64 {
        self.inner.lock().unwrap().audited.get(scenario).copied().unwrap_or(0)
    }
}
