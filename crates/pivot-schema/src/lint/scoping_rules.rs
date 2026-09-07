//! Lane scoping: a check class only means something beside an origin lane.
//!
//! Classifying an assertion states which platform's vocabulary it reads; the
//! downgrade compares that platform's lane with the lane the run is on. Without
//! `case.origin_lane` there is nothing to compare, so the class gates
//! everywhere and the document reads as if it did not.

use crate::lint::{Index, Report};

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    if index.pivot.case.origin_lane.is_some() {
        return;
    }
    let classified_checks = index
        .all_steps()
        .flat_map(|(_, step)| step.checks.iter())
        .chain(index.pivot.postconditions.iter().flat_map(|p| p.checks.iter()))
        .chain(
            index
                .pivot
                .postconditions
                .iter()
                .filter_map(|p| p.cdr.as_ref())
                .flat_map(|cdr| cdr.checks()),
        )
        .any(|check| check.class.is_some());
    let classified_headers = index
        .all_steps()
        .flat_map(|(_, step)| step.msg.headers.iter())
        .any(|header| header.class.is_some());
    if classified_checks || classified_headers {
        report.warn(
            "lane/class-without-origin",
            "case.origin_lane",
            "the document classifies assertions and states no origin lane",
            "state the lane whose system produced the asserted content, or drop the classes",
        );
    }
}
