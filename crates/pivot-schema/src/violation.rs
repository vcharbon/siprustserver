//! `rfc_violations` (`PCAP2TEST_PIVOT_V3.md` §11.1): a rule a message the flow
//! ALREADY carries breaks, stated as a fact about the run.
//!
//! Distinct from `deviations` (§11), which changes what an emission looks like.
//! A violation here is behavioural: the message is byte-compliant and its
//! TIMING or its context is what breaks the rule, so an interpreter reproduces
//! it by replaying the flow unchanged.
//!
//! `rule` is a CLOSED enum, unlike a deviation `kind`: a rule nothing detects is
//! a rule nothing can be held to, so the vocabulary grows one detector at a
//! time. `emitter` decides gating — a scripted peer's violation is listed and
//! never gates, the system under test's gates.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The token naming the system under test as an emitter, where no actor of the
/// document emitted the violating message.
pub const SUT_EMITTER: &str = "sut";

/// One RFC rule a message of this flow breaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RfcViolation {
    /// Which rule is broken.
    pub rule: RfcRule,
    /// The flow step whose message breaks it — the anchor, so a reader lands on
    /// the datagram rather than on a paragraph.
    pub step: String,
    /// Who emitted it: an `actors` id, or `sut`. A scripted peer's violation is
    /// listed loudly and gates nothing, because reproducing a peer's
    /// non-compliance is what the case is for; the system under test's gates.
    pub emitter: String,
}

impl RfcViolation {
    /// Whether the system under test is the emitter.
    pub fn sut_emitted(&self) -> bool {
        self.emitter == SUT_EMITTER
    }
}

/// The rules this format can state. Closed: each member names a rule a detector
/// can decide off the wire.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum RfcRule {
    /// RFC 3261 §9.2: a UAS that has taken a CANCEL for an INVITE answers 487,
    /// never a 2xx.
    #[serde(rename = "no-200-after-cancel")]
    No200AfterCancel,
    /// RFC 3262 §4: a UAC that took a reliable provisional (RFC 3262 §3:
    /// `Require: 100rel` + `RSeq`, on an INVITE that offered `100rel`) answers
    /// it with a PRACK whose RAck names it.
    #[serde(rename = "unacked-reliable-provisional")]
    UnackedReliableProvisional,
    /// RFC 3261 §13.2.2.4: a UAC that took a dialog-creating 2xx to its own
    /// INVITE answers it with an ACK naming that dialog. One ACK is owed per
    /// 2xx RECEIVED, so a retransmission ladder is one obligation.
    #[serde(rename = "no-ack-to-dialog-creating-2xx")]
    NoAckToDialogCreating2xx,
    /// RFC 3261 §9.1: a UAC CANCELs a client transaction still in flight. Once
    /// a final has landed the transaction is completed (§17.1.1.2) and the
    /// CANCEL names none the server holds, so it draws a 481 (§9.2).
    #[serde(rename = "no-cancel-after-final")]
    NoCancelAfterFinal,
    /// RFC 3261 §13.2.1: one dialog carries one answer — a later BINDING
    /// description on it re-states that answer's transport plan, never another
    /// one, since the peer takes the first and ignores the rest.
    #[serde(rename = "second-answer-repeats-the-first")]
    SecondAnswerRepeatsTheFirst,
}

impl RfcRule {
    /// The merged vocabulary's id (`rfc_rules::RuleId`) this wire token names.
    /// This enum IS `RuleId::WIRE` (issue 29 R2: a member arrives with its
    /// detector, its conservatism and its corpus numbers); the conformance
    /// test below pins the two position for position, so growing either side
    /// alone fails the build.
    pub fn rule_id(self) -> rfc_rules::RuleId {
        match self {
            RfcRule::No200AfterCancel => rfc_rules::RuleId::No200AfterCancel,
            RfcRule::UnackedReliableProvisional => rfc_rules::RuleId::UnackedReliableProvisional,
            RfcRule::NoAckToDialogCreating2xx => rfc_rules::RuleId::NoAckToDialogCreating2xx,
            RfcRule::NoCancelAfterFinal => rfc_rules::RuleId::NoCancelAfterFinal,
            RfcRule::SecondAnswerRepeatsTheFirst => rfc_rules::RuleId::SecondAnswerRepeatsTheFirst,
        }
    }
}

impl fmt::Display for RfcRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RfcRule::No200AfterCancel => f.write_str("no-200-after-cancel"),
            RfcRule::UnackedReliableProvisional => f.write_str("unacked-reliable-provisional"),
            RfcRule::NoAckToDialogCreating2xx => f.write_str("no-ack-to-dialog-creating-2xx"),
            RfcRule::NoCancelAfterFinal => f.write_str("no-cancel-after-final"),
            RfcRule::SecondAnswerRepeatsTheFirst => f.write_str("second-answer-repeats-the-first"),
        }
    }
}

impl FromStr for RfcRule {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "no-200-after-cancel" => Ok(RfcRule::No200AfterCancel),
            "unacked-reliable-provisional" => Ok(RfcRule::UnackedReliableProvisional),
            "no-ack-to-dialog-creating-2xx" => Ok(RfcRule::NoAckToDialogCreating2xx),
            "no-cancel-after-final" => Ok(RfcRule::NoCancelAfterFinal),
            "second-answer-repeats-the-first" => Ok(RfcRule::SecondAnswerRepeatsTheFirst),
            other => Err(format!("rfc violation rule {other:?} is not in the closed vocabulary")),
        }
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
    fn the_vocabulary_holds_every_rule_a_detector_decides() {
        // `sip-pcap`'s census is the detector side of this vocabulary: a token
        // it can charge an endpoint with must be a token a document can state.
        let violation: RfcViolation = serde_json::from_str(
            r#"{"rule":"unacked-reliable-provisional","step":"s5","emitter":"uac1"}"#,
        )
        .unwrap();
        assert_eq!(violation.rule, RfcRule::UnackedReliableProvisional);
        assert_eq!(violation.rule.to_string(), "unacked-reliable-provisional");
        assert!(!violation.sut_emitted());

        let violation: RfcViolation = serde_json::from_str(
            r#"{"rule":"no-ack-to-dialog-creating-2xx","step":"s7","emitter":"sut"}"#,
        )
        .unwrap();
        assert_eq!(violation.rule, RfcRule::NoAckToDialogCreating2xx);
        assert_eq!(violation.rule.to_string(), "no-ack-to-dialog-creating-2xx");
        assert!(violation.sut_emitted());
    }

    #[test]
    fn the_system_under_test_is_an_emitter_of_its_own() {
        let violation: RfcViolation =
            serde_json::from_str(r#"{"rule":"no-200-after-cancel","step":"s4","emitter":"sut"}"#)
                .unwrap();
        assert!(violation.sut_emitted());
    }

    #[test]
    fn the_wire_vocabulary_is_the_rfc_rules_wire_subset_position_for_position() {
        let mine = [
            RfcRule::No200AfterCancel,
            RfcRule::UnackedReliableProvisional,
            RfcRule::NoAckToDialogCreating2xx,
            RfcRule::NoCancelAfterFinal,
            RfcRule::SecondAnswerRepeatsTheFirst,
        ];
        assert_eq!(mine.len(), rfc_rules::RuleId::WIRE.len());
        for (rule, id) in mine.into_iter().zip(rfc_rules::RuleId::WIRE) {
            assert_eq!(rule.rule_id(), *id);
            assert_eq!(rule.to_string(), id.token(), "one spelling on the wire");
        }
    }

    #[test]
    fn a_rule_outside_the_closed_vocabulary_is_refused() {
        // An open token here would let a document name a rule no detector can
        // decide, which is a claim nothing can hold the run to.
        assert!(serde_json::from_str::<RfcViolation>(
            r#"{"rule":"answer-after-cancel","step":"s11","emitter":"uas1"}"#
        )
        .is_err());
        assert!("answer-after-cancel".parse::<RfcRule>().is_err());
        // A merged-vocabulary rule (`rfc_rules::RuleId`) that is NOT on the
        // wire contract is refused the same way: membership in rfc-rules
        // alone does not put a rule on §11.1 — graduation takes a census run.
        assert!(serde_json::from_str::<RfcViolation>(
            r#"{"rule":"unacked-2xx-not-cleared","step":"s2","emitter":"sut"}"#
        )
        .is_err());
        assert_eq!("no-200-after-cancel".parse::<RfcRule>().unwrap(), RfcRule::No200AfterCancel);
        assert_eq!(
            "unacked-reliable-provisional".parse::<RfcRule>().unwrap(),
            RfcRule::UnackedReliableProvisional
        );
        assert_eq!(
            "no-ack-to-dialog-creating-2xx".parse::<RfcRule>().unwrap(),
            RfcRule::NoAckToDialogCreating2xx
        );
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
}
