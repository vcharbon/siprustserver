//! The detector ROSTER: a captured document's account of what its extractor
//! looked for.
//!
//! §2.2 reads absence as "none" and nothing else, so a document that states no
//! `relay18x` is indistinguishable between "transparent, decided" and "the
//! detector never ran". The roster closes that by naming every detector and
//! its outcome. These rules check the account is INTERNALLY complete — which
//! detectors a deployment runs is its own to say, so the document names them
//! and lint holds it to the list it named.

use std::collections::{BTreeMap, BTreeSet};

use crate::case::Origin;
use crate::lint::{Index, Report};

/// The flag naming the roster the document accounts for, comma-separated.
const ROSTER: &str = "detector-roster";

/// The three outcomes a rostered detector may state, `<prefix>:<detector>`.
const OUTCOMES: [&str; 3] = ["detected", "detected-none", "detection-unavailable"];

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    if index.pivot.case.origin != Origin::Capture {
        return;
    }
    let Some(annotations) = index.pivot.case.annotations.as_ref() else {
        return;
    };
    let Some(roster) = annotations.flags.iter().find(|f| f.kind == ROSTER) else {
        return;
    };
    let named: BTreeSet<&str> =
        roster.detail.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();

    let mut stated: BTreeMap<&str, usize> = BTreeMap::new();
    for flag in &annotations.flags {
        let Some((prefix, detector)) = flag.kind.split_once(':') else {
            continue;
        };
        if !OUTCOMES.contains(&prefix) {
            continue;
        }
        *stated.entry(detector).or_default() += 1;
        if !named.contains(detector) {
            report.error(
                "annotations/detector-outcome-unrostered",
                "case.annotations.flags",
                &format!("detector {detector:?} states an outcome and the roster does not name it"),
                "add the detector to the `detector-roster` flag, or drop its outcome flag",
            );
        }
    }

    for detector in named {
        match stated.get(detector).copied().unwrap_or(0) {
            1 => {}
            0 => report.error(
                "annotations/detector-roster-incomplete",
                "case.annotations.flags",
                &format!("the roster names detector {detector:?} and no flag states its outcome"),
                "emit exactly one of detected:/detected-none:/detection-unavailable: for it",
            ),
            n => report.error(
                "annotations/detector-roster-incomplete",
                "case.annotations.flags",
                &format!("detector {detector:?} states {n} outcomes; a detector concludes once"),
                "emit exactly one of detected:/detected-none:/detection-unavailable: for it",
            ),
        }
    }
}
