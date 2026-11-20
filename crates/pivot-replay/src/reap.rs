//! The worker's post-run reap check as a verdict failure: what the worker
//! still held once the case settled (a call, a lock, a stamp, a mark, a
//! dropped INVITE final), charged to the cell's verdict at [`SITE`].

use pivot_schema::bundle::{Failure, RunVerdict};

/// The verdict site a reap breach is charged to.
pub const SITE: &str = "post-run cleanup";

/// What a clean worker holds once the case settled, as the failure states it.
pub const EXPECTED: &str = "every call removed, no lock, stamp, mark or dropped INVITE final left";

/// Fails `verdict` with every distinct breach in `stages`, the reap check's
/// results in the order the lane ran them, as one failure at [`SITE`]. A
/// breach two stages both saw is stated once.
pub fn record(stages: &[Result<(), String>], verdict: &mut RunVerdict) {
    let mut breaches: Vec<&str> = Vec::new();
    for breach in stages.iter().filter_map(|stage| stage.as_ref().err()) {
        if !breaches.contains(&breach.as_str()) {
            breaches.push(breach);
        }
    }
    if breaches.is_empty() {
        return;
    }
    verdict.fail(Failure::CheckFailed {
        site: SITE.into(),
        field: "sut.reaped".into(),
        op: "reaped".into(),
        expected: EXPECTED.into(),
        observed: breaches.join("; "),
    });
}

/// Whether `failure` is a reap breach, charged at [`SITE`].
pub fn charges(failure: &Failure) -> bool {
    matches!(failure, Failure::CheckFailed { site, .. } if site == SITE)
}

#[cfg(test)]
#[path = "reap_tests.rs"]
mod tests;
