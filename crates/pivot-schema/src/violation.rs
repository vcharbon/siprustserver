//! `rfc_violations` (`PCAP2TEST_PIVOT_V3.md` §11.1): a rule a message the flow
//! ALREADY carries breaks, stated as a fact about the run.
//!
//! Distinct from `deviations` (§11), which changes what an emission looks like.
//! A violation here is behavioural: the message is byte-compliant and its
//! TIMING or its context is what breaks the rule, so an interpreter reproduces
//! it by replaying the flow unchanged.
//!
//! `rule` is any rule of the validator (`rfc_rules::RuleId`), closed over the
//! rules that have a body, unlike a deviation `kind`: each one is decided the
//! same way off a capture (the census) and off a run (the live audit). A
//! scripted peer's entry is the statement of what that party broke in the
//! source — it gates nothing itself, and it is what cancels the replay's
//! finding of the same violation on the same transaction; the system under
//! test's gates.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

/// The token naming the system under test as an emitter, where no actor of the
/// document emitted the violating message.
pub const SUT_EMITTER: &str = "sut";

/// One RFC rule a message of this flow breaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RfcViolation {
    /// Which rule is broken.
    #[schemars(with = "RuleToken")]
    pub rule: RfcRule,
    /// The flow step whose message breaks it — the anchor, so a reader lands on
    /// the datagram rather than on a paragraph.
    pub step: String,
    /// Who emitted it: an `actors` id, or `sut`. A scripted peer's entry is
    /// listed and gates nothing; it cancels the run's finding of the same rule
    /// against that actor on the transaction `step` names. The system under
    /// test's gates.
    pub emitter: String,
}

impl RfcViolation {
    /// Whether the system under test is the emitter.
    pub fn sut_emitted(&self) -> bool {
        self.emitter == SUT_EMITTER
    }
}

/// The rules this format can state: every rule of the validator, by the token
/// its census hits and audit findings carry.
pub use rfc_rules::RuleId as RfcRule;

/// The JSON Schema of an [`RfcRule`] field: one of the validator's rule tokens.
pub struct RuleToken;

impl JsonSchema for RuleToken {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RfcRule")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        let tokens: Vec<&str> = RfcRule::ALL.iter().map(|r| r.token()).collect();
        schemars::json_schema!({
            "type": "string",
            "description": "A rule of the RFC validator, by its wire token.",
            "enum": tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_violation_states_its_rule_its_anchor_and_its_emitter() {
        let violation: RfcViolation =
            serde_json::from_str(r#"{"rule":"no-200-after-cancel","step":"s11","emitter":"uas1"}"#)
                .unwrap();
        assert_eq!(violation.rule, RfcRule::No200AfterCancel);
        assert_eq!(violation.step, "s11");
        assert_eq!(violation.emitter, "uas1");
        assert!(!violation.sut_emitted());
        assert_eq!(violation.rule.to_string(), "no-200-after-cancel");
    }

    #[test]
    fn the_system_under_test_is_an_emitter_of_its_own() {
        let violation: RfcViolation =
            serde_json::from_str(r#"{"rule":"no-200-after-cancel","step":"s4","emitter":"sut"}"#)
                .unwrap();
        assert!(violation.sut_emitted());
    }

    /// Every rule of the validator can be stated, by the token it is decided
    /// under: the census charges a captured party with any of them, and a run
    /// cancels a scripted party's finding of any of them.
    #[test]
    fn every_validator_rule_can_be_stated() {
        for rule in RfcRule::ALL {
            let text = format!(r#"{{"rule":"{}","step":"s1","emitter":"uac1"}}"#, rule.token());
            let violation: RfcViolation = serde_json::from_str(&text).unwrap();
            assert_eq!(violation.rule, *rule);
        }
        assert_eq!(RfcRule::WIRE, RfcRule::ALL, "the wire contract is every rule");
    }

    #[test]
    fn a_rule_the_validator_does_not_hold_is_refused() {
        // A token no rule body decides is a claim nothing can hold the run to.
        assert!(serde_json::from_str::<RfcViolation>(
            r#"{"rule":"answer-after-cancel","step":"s11","emitter":"uas1"}"#
        )
        .is_err());
        assert!("answer-after-cancel".parse::<RfcRule>().is_err());
    }

    #[test]
    fn an_entry_missing_a_field_or_carrying_a_spare_one_is_refused() {
        assert!(serde_json::from_str::<RfcViolation>(
            r#"{"rule":"no-200-after-cancel","step":"s11"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<RfcViolation>(
            r#"{"rule":"no-200-after-cancel","step":"s11","emitter":"uas1","allowed":true}"#
        )
        .is_err());
    }

    /// The validator's tokens, one per line, pinned in a fixture the
    /// TypeScript mirror reads (`RFC_RULES`), so the two never drift.
    #[test]
    fn the_rule_tokens_fixture_is_the_validators_vocabulary() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rfc-rule-tokens.txt");
        let expected: String = RfcRule::ALL.iter().map(|r| format!("{}\n", r.token())).collect();
        if std::env::var_os("UPDATE_GOLDENS").is_some() {
            std::fs::write(&path, &expected).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected, "UPDATE_GOLDENS=1 to accept");
    }
}
