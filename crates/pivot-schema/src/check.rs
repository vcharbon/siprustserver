//! The one check vocabulary (`PCAP2TEST_PIVOT_V3.md` §9), borrowed unchanged
//! from the upstream `e2e-model` so a reviewer reads one assertion grammar
//! across both worlds.
//!
//! A check is `{ field, op, value }`. It appears inline on an `expect`, where
//! it asserts over the matched message, and in `postconditions`, where it
//! asserts over what the run left behind. The field grammar and the observable
//! names are DEPLOYMENT vocabulary: this crate fixes the shape and the four
//! operators, nothing else.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::scoping::CheckClass;

/// One field assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Field selector. Open token: `from.userInfo`, `header(P-Asserted-Identity)`,
    /// `body`, or a deployment observable's name in a postcondition.
    pub field: String,
    /// How the field is compared.
    pub op: CheckOp,
    /// Expected value: a literal, a regex (op `Regex`), or a string carrying
    /// `${…}` accessors. Absent exactly for `Exists` and `Absent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Which vocabulary the check reads (§9.1). Stated where the assertion is
    /// the ORIGIN platform's rather than SIP's, so a run on another lane
    /// evaluates and records it without gating on it. An unclassified check
    /// gates on every lane.
    #[serde(rename = "class", default, skip_serializing_if = "Option::is_none")]
    pub class: Option<CheckClass>,
}

impl Check {
    /// Whether `value` is stated exactly where the operator takes one.
    pub fn value_is_declarable(&self) -> bool {
        self.op.takes_value() == self.value.is_some()
    }
}

/// The assertion operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CheckOp {
    /// `value` (after `${…}` substitution) must equal the extracted field.
    Eq,
    /// `value` is a regex the extracted field must match.
    Regex,
    /// The field must be present. Takes no `value`.
    Exists,
    /// The field must be absent. Takes no `value`.
    Absent,
}

impl CheckOp {
    /// Whether the operator compares against a stated value.
    pub fn takes_value(self) -> bool {
        matches!(self, CheckOp::Eq | CheckOp::Regex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Check {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn the_operators_carry_their_wire_spelling() {
        for (text, op) in [
            (r#"{"field":"body","op":"eq","value":"x"}"#, CheckOp::Eq),
            (r#"{"field":"body","op":"regex","value":"^x"}"#, CheckOp::Regex),
            (r#"{"field":"header(X-A)","op":"exists"}"#, CheckOp::Exists),
            (r#"{"field":"header(X-A)","op":"absent"}"#, CheckOp::Absent),
        ] {
            assert_eq!(check(text).op, op);
            assert_eq!(serde_json::to_string(&check(text)).unwrap(), text);
        }
    }

    #[test]
    fn a_check_may_name_the_vocabulary_it_reads() {
        let classified =
            check(r#"{"field":"events","op":"regex","value":"InviteReceived","class":"cdr-vocabulary"}"#);
        assert_eq!(classified.class, Some(CheckClass::CdrVocabulary));
        // Unclassified is the default, and it is omitted rather than written null.
        let plain = check(r#"{"field":"to.tag","op":"exists"}"#);
        assert_eq!(plain.class, None);
        assert!(!serde_json::to_string(&plain).unwrap().contains("class"));
        // A class outside the closed vocabulary is refused, not carried.
        assert!(
            serde_json::from_str::<Check>(
                r#"{"field":"events","op":"exists","class":"platform-quirk"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_value_belongs_to_the_two_operators_that_compare() {
        assert!(check(r#"{"field":"body","op":"eq","value":"x"}"#).value_is_declarable());
        assert!(check(r#"{"field":"body","op":"exists"}"#).value_is_declarable());
        assert!(!check(r#"{"field":"body","op":"eq"}"#).value_is_declarable());
        assert!(!check(r#"{"field":"body","op":"absent","value":"x"}"#).value_is_declarable());
    }
}
