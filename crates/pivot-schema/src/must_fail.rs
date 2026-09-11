//! `must_fail` (`PCAP2TEST_PIVOT_V3.md` §11.2): the failure this document's run
//! MUST produce, stated as the document's own expected outcome.
//!
//! A negative case replays a source whose non-compliance this platform does not
//! share. The run cannot pass by behaving well, and it must not be excused
//! either: it fails, and the document says in advance exactly HOW. A run that
//! fails as declared is the case succeeding; a run that fails any other way, or
//! that does not fail at all, is the case failing.
//!
//! `failure` is a CLOSED enum for the same reason [`crate::violation::RfcRule`]
//! is: a failure nothing can PREDICT off the capture is a claim nothing can
//! hold a run to, so the vocabulary grows one prediction at a time.
//! `derived_from` names the rule whose violation in the SOURCE predicts it, so
//! a declaration is traceable to the evidence rather than hand-guessed.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::violation::RfcRule;

/// One failure this run must produce, at or immediately after one step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MustFail {
    /// What the run must produce.
    pub failure: DeclaredFailure,
    /// The flow step the failure happens at or immediately after — the anchor,
    /// so a reader lands on the datagram the divergence turns on.
    pub step: String,
    /// The rule the SOURCE broke, whose violation predicts this failure. A
    /// declaration is DERIVED from a detector's hit, never hand-guessed.
    pub derived_from: RfcRule,
}

/// The failures this format can declare. Closed: each member names a failure
/// the generator can predict from a detector's hit plus this lane's behaviour.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum DeclaredFailure {
    /// This platform ACKs a dialog-creating 2xx locally (RFC 3261 §13.2.2.4)
    /// where the source platform relayed an ACK that never came. The capture
    /// holds no such datagram, so the document holds no step for it, and the
    /// ACK arrives where nothing expects it.
    #[serde(rename = "unexpected-ack")]
    UnexpectedAck,
    /// This platform PRACKs a reliable provisional it receives (RFC 3262 §4)
    /// where the source platform never did. The capture holds no PRACK, so the
    /// document holds no step for it, and the PRACK arrives where nothing
    /// expects it.
    #[serde(rename = "unexpected-prack")]
    UnexpectedPrack,
    /// This platform CANCELs an INVITE client transaction while it is still in
    /// flight (RFC 3261 §9.1) where the source platform sent its CANCEL only
    /// after that transaction had taken — and ACKed — a final response. The
    /// capture places the CANCEL behind the final, so the document's own CANCEL
    /// step sits behind it too, and this platform's CANCEL arrives ahead of
    /// where anything expects it.
    #[serde(rename = "unexpected-cancel")]
    UnexpectedCancel,
}

impl fmt::Display for DeclaredFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeclaredFailure::UnexpectedAck => f.write_str("unexpected-ack"),
            DeclaredFailure::UnexpectedPrack => f.write_str("unexpected-prack"),
            DeclaredFailure::UnexpectedCancel => f.write_str("unexpected-cancel"),
        }
    }
}

impl FromStr for DeclaredFailure {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unexpected-ack" => Ok(DeclaredFailure::UnexpectedAck),
            "unexpected-prack" => Ok(DeclaredFailure::UnexpectedPrack),
            "unexpected-cancel" => Ok(DeclaredFailure::UnexpectedCancel),
            other => Err(format!("declared failure {other:?} is not in the closed vocabulary")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declaration_states_its_failure_its_anchor_and_the_rule_it_came_from() {
        let declared: MustFail = serde_json::from_str(
            r#"{"failure":"unexpected-ack","step":"s14","derived_from":"no-ack-to-dialog-creating-2xx"}"#,
        )
        .unwrap();
        assert_eq!(declared.failure, DeclaredFailure::UnexpectedAck);
        assert_eq!(declared.step, "s14");
        assert_eq!(declared.derived_from, RfcRule::NoAckToDialogCreating2xx);
        assert_eq!(declared.failure.to_string(), "unexpected-ack");
    }

    #[test]
    fn a_declaration_round_trips_through_the_canonical_bytes() {
        // Canonical order is the formatter's (§2.1): `derived_from`, `failure`,
        // `step`. Parsing those bytes and re-emitting them reproduces them.
        let text = concat!(
            "{\n",
            "  \"derived_from\": \"no-ack-to-dialog-creating-2xx\",\n",
            "  \"failure\": \"unexpected-ack\",\n",
            "  \"step\": \"s14\"\n",
            "}\n"
        );
        let declared: MustFail = serde_json::from_str(text).unwrap();
        assert_eq!(crate::canonical::format(&declared).unwrap(), text);
    }

    #[test]
    fn a_failure_outside_the_closed_vocabulary_is_refused() {
        // An open token here would let a document declare a failure nothing can
        // predict, and a negative case that cannot be checked passes by
        // accident.
        assert!(
            serde_json::from_str::<MustFail>(
                r#"{"failure":"unexpected-bye","step":"s14","derived_from":"no-ack-to-dialog-creating-2xx"}"#
            )
            .is_err()
        );
        assert!("unexpected-bye".parse::<DeclaredFailure>().is_err());
        assert_eq!(
            "unexpected-ack".parse::<DeclaredFailure>().unwrap(),
            DeclaredFailure::UnexpectedAck
        );
    }

    #[test]
    fn a_declaration_missing_a_field_or_carrying_a_spare_one_is_refused() {
        assert!(serde_json::from_str::<MustFail>(r#"{"failure":"unexpected-ack","step":"s14"}"#)
            .is_err());
        assert!(
            serde_json::from_str::<MustFail>(
                r#"{"failure":"unexpected-ack","step":"s14","derived_from":"no-ack-to-dialog-creating-2xx","leg":"B"}"#
            )
            .is_err()
        );
    }

    /// The field sorts where §2.1 puts it and disappears when empty (§2.2), so
    /// a document that declares nothing is byte-identical to one written before
    /// the field existed.
    #[test]
    fn the_field_sorts_canonically_and_is_omitted_when_empty() {
        let with = document(concat!(
            "  \"must_fail\": [\n",
            "    {\n",
            "      \"derived_from\": \"no-ack-to-dialog-creating-2xx\",\n",
            "      \"failure\": \"unexpected-ack\",\n",
            "      \"step\": \"s1\"\n",
            "    }\n",
            "  ],\n"
        ));
        let pivot = crate::PivotV3::from_json(&with).unwrap();
        assert_eq!(pivot.must_fail.len(), 1);
        assert_eq!(pivot.to_canonical_json(), with);

        let without = document("");
        let plain = crate::PivotV3::from_json(&without).unwrap();
        assert!(plain.must_fail.is_empty());
        assert_eq!(plain.to_canonical_json(), without);
    }

    /// A canonical document with `block` spliced in where the sorted key order
    /// puts it: after `legs`, before `pivot_version`.
    fn document(block: &str) -> String {
        format!(
            concat!(
                "{{\n",
                "  \"actors\": [\n    {{\n      \"endpoint\": \"ep0\",\n      \"id\": \"uas1\",\n      \"kind\": \"uas\"\n    }}\n  ],\n",
                "  \"calls\": [\n    {{\n      \"attempts\": [],\n      \"caller_leg\": \"B\",\n      \"id\": \"c1\"\n    }}\n  ],\n",
                "  \"case\": {{\n    \"family\": \"transparent\",\n    \"id\": \"negative\",\n    \"lanes\": {{\n      \"upstream-fake\": \"ok\"\n    }},\n    \"origin\": \"authored\",\n    \"title\": \"one 2xx nobody acks\",\n    \"variant\": \"repro\"\n  }},\n",
                "  \"endpoints\": [\n    {{\n      \"binding\": \"dedicated\",\n      \"id\": \"ep0\",\n      \"observed\": \"127.0.0.1:5060\",\n      \"side\": \"peer\"\n    }}\n  ],\n",
                "  \"flow\": [\n    {{\n      \"delay\": {{\n        \"compressible\": true,\n        \"from\": \"trigger\",\n        \"ms\": 0,\n        \"timer_linked\": false\n      }},\n      \"id\": \"s1\",\n      \"leg\": \"B\",\n      \"msg\": {{\n        \"cseq-method\": \"INVITE\",\n        \"status\": 200\n      }},\n      \"op\": \"send\"\n    }}\n  ],\n",
                "  \"identities\": [],\n",
                "  \"legs\": [\n    {{\n      \"actor\": \"uas1\",\n      \"dir\": \"in\",\n      \"id\": \"B\"\n    }}\n  ],\n",
                "{block}",
                "  \"pivot_version\": 3,\n",
                "  \"timing\": {{\n    \"expect_budget_ms\": 32000,\n    \"settle_budget_ms\": 32000\n  }}\n",
                "}}\n"
            ),
            block = block
        )
    }

    #[test]
    fn the_rule_a_declaration_cites_is_the_violation_vocabulary_and_nothing_else() {
        // `derived_from` reuses §11.1's closed enum on purpose: the evidence a
        // prediction rests on is a rule some detector decided off the wire.
        assert!(serde_json::from_str::<MustFail>(
            r#"{"failure":"unexpected-ack","step":"s14","derived_from":"peer-was-rude"}"#
        )
        .is_err());
    }
}
