//! RFC-violation rules over an observed SIP wire — the ONE home for this
//! knowledge. A rule body here is written once against the wire model in
//! [`wire`] and consumed by two adapters: the capture-side census
//! (`sip_pcap::rfc`, reading pcap flows documents) and the live recorded-trace
//! audit (`sip_net::rfc_audit`, reading the harness event channel). What
//! differs between those consumers is OBSERVATION POLICY — how the stream was
//! seen, whether the recording is complete, what to do with an undecidable or
//! relay-attributed finding — and all of it is data on the model
//! ([`wire::Observation`], [`verdict::Decision`], `Finding::relayed`), never a
//! property of which module a rule happens to live in.
//!
//! **A rule decides an OCCASION three ways** ([`verdict::Decision`]):
//! violated with evidence, compliant, or undecidable — and the population
//! contract `hits ⊆ decided ⊆ occasions` makes the cost of conservatism
//! visible on every report. Consumers choose their policy over `Undecidable`
//! (the census under-reports; the live gate may still fail hard) — the rule
//! itself only ever states what the wire proves.

pub mod registry;
pub mod rules;
pub mod verdict;
pub mod wire;

pub use registry::RuleId;
pub use verdict::{Decision, Evidence, Finding, Population};
pub use wire::{Endpoint, Kind, Msg, Observation, WireView};

/// Every declared rule, in [`RuleId::ALL`] order.
pub fn all_rules() -> impl Iterator<Item = &'static registry::Rule> {
    RuleId::ALL.iter().map(|id| id.rule())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every family row is one rule of [`RuleId::ALL`], and no id is declared
    /// twice.
    #[test]
    fn every_rule_with_a_body_is_registered_once() {
        let mut registered: Vec<RuleId> =
            rules::FAMILIES.iter().flat_map(|f| f.iter()).map(|r| r.id).collect();
        registered.sort();
        let before = registered.len();
        registered.dedup();
        assert_eq!(before, registered.len(), "a rule is registered twice");
        let mut declared = RuleId::ALL.to_vec();
        declared.sort();
        assert_eq!(registered, declared);
    }

    /// Every token round-trips, and no two rules share one.
    #[test]
    fn tokens_are_unique_and_parse_back() {
        let mut tokens: Vec<&str> = RuleId::ALL.iter().map(|r| r.token()).collect();
        tokens.sort_unstable();
        let before = tokens.len();
        tokens.dedup();
        assert_eq!(before, tokens.len(), "two rules share a token");
        for rule in RuleId::ALL {
            assert_eq!(rule.token().parse::<RuleId>().unwrap(), *rule);
        }
    }
}
