//! `b2bua_operator_released_total`: the calls an operator's release-all
//! reached, by what it did to them (ADR-0050 §10).

use std::sync::atomic::{AtomicU64, Ordering};

use super::catalogue::worker::{OPERATOR_RELEASED, RELEASE_KIND};
use super::B2buaMetrics;
use crate::release_all::ReleasedAll;

/// The `kind` label values: answered calls released, calls in setup aborted.
pub const OPERATOR_RELEASE_KINDS: [&str; 2] = ["released", "aborted"];

/// The counts, indexed like [`OPERATOR_RELEASE_KINDS`].
#[derive(Debug, Default)]
pub(super) struct OperatorReleases([AtomicU64; 2]);

impl B2buaMetrics {
    /// Count what one release-all reached.
    pub fn count_operator_release(&self, reached: ReleasedAll) {
        let counts = &self.inner.operator_releases.0;
        counts[0].fetch_add(reached.released as u64, Ordering::Relaxed);
        counts[1].fetch_add(reached.aborted as u64, Ordering::Relaxed);
    }

    /// Every release-all's counts, summed.
    pub fn operator_released_total(&self) -> ReleasedAll {
        let counts = &self.inner.operator_releases.0;
        ReleasedAll {
            released: counts[0].load(Ordering::Relaxed) as usize,
            aborted: counts[1].load(Ordering::Relaxed) as usize,
        }
    }
}

/// Writes the family, one series per kind.
pub(super) fn render(metrics: &B2buaMetrics, s: &mut String) {
    let counts = &metrics.inner.operator_releases.0;
    OPERATOR_RELEASED
        .render(s, |series| counts[series.index(&RELEASE_KIND)].load(Ordering::Relaxed));
}
