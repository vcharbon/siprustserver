//! **Lane scoping** as the check sites apply it (`PCAP2TEST_PIVOT_V3.md` §9.1).
//!
//! [`Scope::disposition`] holds the whole decision — the lane's configuration
//! bound against the document's `origin_lane` — so a check site asks one
//! question instead of carrying two arguments that can drift apart, and pairs
//! what a check found with the class that decides what it costs. [`RunConfig`]
//! carries the DATA (the lane name, the per-class overrides); what they mean is
//! decided here and nowhere else.
//!
//! A downgrade is not a skip: every site evaluates a classified check exactly as
//! it evaluates a gating one, and only where the finding LANDS differs.

use pivot_schema::bundle::{CheckDisposition, Failure, RunConfig};
use pivot_schema::known_bug::KnownBug;
use pivot_schema::scoping::CheckClass;

/// One run's lane scoping.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    config: &'a RunConfig,
    origin_lane: Option<&'a str>,
}

impl<'a> Scope<'a> {
    /// Bind `config` against the lane the document's content came from.
    pub fn new(config: &'a RunConfig, origin_lane: Option<&'a str>) -> Self {
        Scope { config, origin_lane }
    }

    /// What a check of `class` costs on this run.
    ///
    /// The ONE built-in rule: a classified check does not gate when this run's
    /// lane is not the lane the asserted content came from. An unclassified
    /// check always gates, a document with no origin lane downgrades nothing,
    /// and a `check_scoping` override decides its class outright.
    pub fn disposition(&self, class: Option<CheckClass>) -> CheckDisposition {
        let Some(class) = class else { return CheckDisposition::Gating };
        if let Some(stated) = self.config.check_scoping.get(&class) {
            return *stated;
        }
        match self.origin_lane {
            Some(origin) if origin != self.config.lane => CheckDisposition::Informative,
            _ => CheckDisposition::Gating,
        }
    }

    /// Whether this lane declared `bug` — a defect its SUT is KNOWN to produce,
    /// whose gate stands down so the run reaches the steps behind the symptom.
    ///
    /// Not an acceptance and not a class: a class says what vocabulary a
    /// document's assertion reads, this says what the lane's system does wrong.
    /// What the waived check found is recorded (`RunVerdict.waived`).
    pub fn waives(&self, bug: KnownBug) -> bool {
        self.config.waives(bug)
    }

    /// Whether a check of `class` decides the run's status here.
    ///
    /// A frozen header that does not gate is not part of the MATCH either:
    /// matching on it rejects the message outright, which is a harder verdict
    /// than recording that the header was not there.
    pub fn gates(&self, class: Option<CheckClass>) -> bool {
        self.disposition(class) == CheckDisposition::Gating
    }
}

/// A check that did not hold, with the class that decides what it costs.
///
/// A site reports what it found; the run routes it (`Instance::record`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The vocabulary the check reads, where it states one.
    pub class: Option<CheckClass>,
    /// What did not hold.
    pub failure: Failure,
}

impl Finding {
    pub fn new(class: Option<CheckClass>, failure: Failure) -> Self {
        Finding { class, failure }
    }

    /// A finding from an unclassified check: the protocol's own, gating on every
    /// lane.
    pub fn gating(failure: Failure) -> Self {
        Finding { class: None, failure }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pivot_schema::bundle::ClockMode;

    fn failure() -> Failure {
        Failure::CdrMismatch { expected: "events matches Bye".into(), observed: "bye".into() }
    }

    #[test]
    fn a_scope_answers_per_class_and_an_unclassified_check_always_gates() {
        let config = RunConfig::new("upstream-demo", ClockMode::Virtual, "h:1");
        let foreign = Scope::new(&config, Some("origin-platform"));
        assert!(!foreign.gates(Some(CheckClass::CdrVocabulary)));
        assert!(!foreign.gates(Some(CheckClass::OriginPlatformHeader)));
        assert!(foreign.gates(None));

        let home = Scope::new(&config, Some("upstream-demo"));
        assert!(home.gates(Some(CheckClass::CdrVocabulary)));
        // A document that names no origin lane has nothing to compare against.
        assert!(Scope::new(&config, None).gates(Some(CheckClass::CdrVocabulary)));
    }

    #[test]
    fn a_lane_override_decides_a_class_in_either_direction() {
        // A lane that shares the vocabulary without sharing the name.
        let config = RunConfig::new("upstream-fake", ClockMode::Virtual, "h:1")
            .with_check_scoping(CheckClass::CdrVocabulary, CheckDisposition::Gating);
        assert!(Scope::new(&config, Some("origin-platform")).gates(Some(CheckClass::CdrVocabulary)));

        // A lane replaying its OWN document under a foreign header profile.
        let config = RunConfig::new("origin-platform", ClockMode::Virtual, "h:1")
            .with_check_scoping(CheckClass::OriginPlatformHeader, CheckDisposition::Informative);
        let home = Scope::new(&config, Some("origin-platform"));
        assert!(!home.gates(Some(CheckClass::OriginPlatformHeader)));
        // The override is per class: the other one still follows the lane rule.
        assert!(home.gates(Some(CheckClass::CdrVocabulary)));
    }

    #[test]
    fn a_finding_carries_the_class_that_decides_what_it_costs() {
        assert_eq!(Finding::gating(failure()).class, None);
        assert_eq!(
            Finding::new(Some(CheckClass::CdrVocabulary), failure()).class,
            Some(CheckClass::CdrVocabulary)
        );
    }
}
