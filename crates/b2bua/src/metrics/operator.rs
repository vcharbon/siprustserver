//! `b2bua_operator_released_total`: the calls an operator's release-all
//! reached, by what it did to them (ADR-0050 §10).

use std::sync::atomic::{AtomicU64, Ordering};

use super::catalogue::worker::{OPERATOR_RELEASED, RELEASE_KIND};
use super::B2buaMetrics;
use crate::release_all::ReleasedAll;

/// The `kind` label values: answered calls released, calls in setup aborted.
pub const OPERATOR_RELEASE_KINDS: [&str; 2] = ["released", "aborted"];

/// The counts, one per [`OPERATOR_RELEASE_KINDS`] value.
#[derive(Debug, Default)]
pub(super) struct OperatorReleases {
    released: AtomicU64,
    aborted: AtomicU64,
}

impl OperatorReleases {
    /// The count of the kind `at` of [`OPERATOR_RELEASE_KINDS`].
    fn of(&self, at: usize) -> Option<&AtomicU64> {
        match at {
            0 => Some(&self.released),
            1 => Some(&self.aborted),
            _ => None,
        }
    }
}

/// `n` as a count.
fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A count as a `usize`, saturating.
fn size(a: &AtomicU64) -> usize {
    usize::try_from(a.load(Ordering::Relaxed)).unwrap_or(usize::MAX)
}

impl B2buaMetrics {
    /// Count what one release-all reached.
    pub fn count_operator_release(&self, reached: ReleasedAll) {
        let counts = &self.inner.operator_releases;
        counts.released.fetch_add(count(reached.released), Ordering::Relaxed);
        counts.aborted.fetch_add(count(reached.aborted), Ordering::Relaxed);
    }

    /// Every release-all's counts, summed.
    pub fn operator_released_total(&self) -> ReleasedAll {
        let counts = &self.inner.operator_releases;
        ReleasedAll { released: size(&counts.released), aborted: size(&counts.aborted) }
    }
}

/// Writes the family, one series per kind.
pub(super) fn render(metrics: &B2buaMetrics, s: &mut String) {
    let counts = &metrics.inner.operator_releases;
    OPERATOR_RELEASED.render(s, |series| {
        counts.of(series.index(&RELEASE_KIND)).map_or(0, |a| a.load(Ordering::Relaxed))
    });
}
