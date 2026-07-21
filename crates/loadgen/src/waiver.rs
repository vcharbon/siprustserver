//! Per-CAMPAIGN structural-waiver aggregation (ADR-0024 §6). A load campaign
//! samples a subset of its calls; the unused-waiver gate must aggregate a
//! waiver's "did it ever filter a finding" ACROSS those sampled calls, never
//! per call — a single call legitimately exercises only its own branch, so a
//! per-call gate would fire on every waiver that call did not happen to need.
//!
//! A `.conditional()` scope (every capture-derived waiver, and a case's lowered
//! `allowViolations`) is NEVER reported: a divergent branch may legitimately
//! never trip the waived rule. Only a hand-written non-conditional waiver that
//! filtered nothing all campaign long surfaces — the meaningful "this waiver is
//! stale" signal.

use scenario_harness::WaiverScope;

/// One scenario's waivers plus their campaign-wide "was used" tally.
pub struct CampaignWaivers {
    scopes: Vec<WaiverScope>,
    /// OR-aggregated across every sampled call's per-waiver used mask.
    used: Vec<bool>,
}

impl CampaignWaivers {
    /// Start a campaign tally over a scenario's (stable) merged waiver list.
    pub fn new(scopes: Vec<WaiverScope>) -> Self {
        let n = scopes.len();
        CampaignWaivers { scopes, used: vec![false; n] }
    }

    /// OR-fold one sampled call's per-waiver used mask (aligned to `scopes`). A
    /// wrong-length mask is ignored defensively — a campaign's waiver list is
    /// stable, so a mismatch is a programming error, not silent data.
    pub fn record(&mut self, mask: &[bool]) {
        if mask.len() != self.used.len() {
            debug_assert_eq!(mask.len(), self.used.len(), "waiver mask length drifted");
            return;
        }
        for (u, &m) in self.used.iter_mut().zip(mask) {
            *u = *u || m;
        }
    }

    /// The non-conditional waivers that filtered NOTHING across the whole
    /// campaign — the surfaced "stale waiver" set.
    pub fn unused(&self) -> Vec<&WaiverScope> {
        self.scopes
            .iter()
            .zip(&self.used)
            .filter(|(s, &u)| !u && !s.conditional)
            .map(|(s, _)| s)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0024 §6: per-campaign aggregation — an unused NON-conditional waiver
    /// surfaces; a `.conditional()` one stays silent; a waiver used by SOME
    /// sampled call (not all) does not surface.
    #[test]
    fn per_campaign_unused_aggregation() {
        let scopes = vec![
            WaiverScope::rule("rfc3261.a", "stale hand-written").on_party("alice"),
            WaiverScope::rule("rfc3261.b", "conditional capture waiver").conditional(),
            WaiverScope::rule("rfc3261.c", "used somewhere").on_party("bob"),
        ];
        let mut c = CampaignWaivers::new(scopes);
        // Three sampled calls. None uses `a`; none uses `b`; call 2 uses `c`.
        c.record(&[false, false, false]);
        c.record(&[false, false, true]);
        c.record(&[false, false, false]);

        let unused: Vec<&str> = c.unused().iter().map(|w| w.rule.as_str()).collect();
        assert_eq!(unused, vec!["rfc3261.a"], "only the stale non-conditional waiver surfaces");
    }

    #[test]
    fn empty_campaign_reports_nothing() {
        let c = CampaignWaivers::new(vec![]);
        assert!(c.unused().is_empty());
    }
}
