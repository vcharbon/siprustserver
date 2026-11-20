//! The `--verify` verdict: every check over one anonymisation, counted —
//! numbers of the raw document left in place ([`super::survivors`]), display
//! names left unmasked ([`super::unmasked_displays`]), harvested names found
//! again ([`super::names`]), tokens no fake explains ([`super::leaks`]), and
//! dialog identifiers merged ([`super::dialogs`], the Call-IDs the rewrite
//! merged included). Any count refuses the output.

use serde::Serialize;

use crate::anonymize::Report;
use crate::fake::address::Scope;
use crate::model::Flows;
use crate::plan::Plan;
use crate::reading::Reading;

use super::leaks::Leaks;

/// The counts of every check.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub survivors: usize,
    pub unmasked_displays: usize,
    pub name_survivors: usize,
    pub leaks: Leaks,
    pub merged_identifiers: usize,
}

impl Verdict {
    pub fn total(&self) -> usize {
        self.survivors
            + self.unmasked_displays
            + self.name_survivors
            + self.leaks.total()
            + self.merged_identifiers
    }

    /// The refusal, as a line: counts only.
    pub fn refusal(&self) -> String {
        let l = &self.leaks;
        format!(
            "{} numbers of the raw document survive anonymisation, {} display names stay \
             unmasked, {} names survive, {} dialog identifiers merge, and {} domains, {} \
             addresses, {} words, {} non-ASCII words and {} digit runs no fake explains",
            self.survivors,
            self.unmasked_displays,
            self.name_survivors,
            self.merged_identifiers,
            l.domains,
            l.addresses,
            l.words,
            l.non_ascii,
            l.digits
        )
    }
}

/// Every check of `anonymised` against `raw`, both read under `reading`, the
/// run's `report` naming its fakes and the Call-IDs it merged.
pub fn judge(
    raw: &Flows,
    anonymised: &Flows,
    plan: &Plan,
    scope: &Scope,
    reading: &Reading,
    report: &Report,
) -> Verdict {
    Verdict {
        survivors: super::survivors(raw, anonymised, plan, scope),
        unmasked_displays: super::unmasked_displays(anonymised, scope),
        name_survivors: super::names::name_survivors(raw, anonymised, scope),
        leaks: super::leaks::leaks(anonymised, scope, &report.fakes, reading),
        merged_identifiers: super::dialogs::merged(raw, anonymised, reading)
            + report.merged_call_ids,
    }
}
