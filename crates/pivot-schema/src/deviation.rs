//! `deviations` (`PCAP2TEST_PIVOT_V3.md` §11): named, reviewed, grep-able
//! non-compliance the replay must REPRODUCE — captured from a peer, or
//! deliberate in an authored test.
//!
//! Every violation lives here and nowhere else. There are no inline tier-1
//! overrides on a step: the flow reads as intent, and what breaks the rules is
//! greppable in one block. `kind` stays an open token — the set grows with the
//! corpus, and a document naming a kind a given interpreter does not implement
//! must still parse so lint can say so.
//!
//! A held automatic is NOT a deviation: §6.1 states the hold as the auto step's
//! `delay` and the repeat count as `retransmits` on the final that was repeated.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::accessor::Computed;

/// One reproduced non-compliance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Deviation {
    /// Unique within the document; what a review comment cites.
    pub id: String,
    /// Open token naming the non-compliance. The kinds with a stated payload:
    /// `cseq-override` (`value`), `suppress-auto` (`step`, an auto step),
    /// `raw-order` and `verbatim-emission` (`preserve`).
    pub kind: String,
    /// The leg the deviation applies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg: Option<String>,
    /// The step id the deviation applies to. For `verbatim-emission` this is
    /// the SCRIPTED send that triggers it: an auto step is the stack's own
    /// composition, so asking this document to preserve its header order means
    /// nothing. For `suppress-auto` it is precisely the auto step withheld —
    /// which is why a withheld ACK stays a step with a deviation on it, rather
    /// than a hole in the flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// The header the deviation targets, canonically named. A deliberate
    /// content-level malformation breaks ONE header's grammar in an otherwise
    /// compliant message; without this the entry points at the step and an
    /// interpreter cannot tell which of its own renderers to stand down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// What must survive emission byte-for-byte (e.g. `header-order`,
    /// `casing`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserve: Vec<String>,
    /// Retransmissions the capture shows, where the deviation IS the repeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retransmits: Option<u32>,
    /// The other step this one races with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub races: Option<String>,
    /// The CSeq a `cseq-override` emits instead of the stack's own: an absolute
    /// number, or an offset from a CSeq the run already saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<CseqValue>,
}

/// A `cseq-override`'s value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CseqValue {
    /// The number to emit, stated outright.
    Absolute(u32),
    /// Relative to a CSeq the run observed: `{ from: "${step:s7.cseq}",
    /// delta: 1 }`. A capture cannot state this — the number it saw is
    /// absolute — so it is an authored form.
    Relative(Computed),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Deviation {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn a_cseq_override_states_an_absolute_or_a_relative_value() {
        let absolute = parse(r#"{"id":"d1","kind":"cseq-override","leg":"A","value":42}"#);
        assert_eq!(absolute.value, Some(CseqValue::Absolute(42)));
        let relative = parse(
            r#"{"id":"d2","kind":"cseq-override","leg":"A","value":{"from":"${step:s7.cseq}","delta":1}}"#,
        );
        assert!(matches!(relative.value, Some(CseqValue::Relative(_))));
    }

    #[test]
    fn a_withheld_automatic_points_at_the_step_it_withholds() {
        let deviation = parse(r#"{"id":"d3","kind":"suppress-auto","leg":"B","step":"s12"}"#);
        assert_eq!(deviation.step.as_deref(), Some("s12"));
    }

    #[test]
    fn a_content_level_malformation_names_the_header_it_breaks() {
        let deviation = parse(
            r#"{"id":"d1","kind":"malformed-header","leg":"B","step":"s8","header":"Refer-To","preserve":["header-value"]}"#,
        );
        assert_eq!(deviation.header.as_deref(), Some("Refer-To"));
        // Absent everywhere else, and omitted rather than written null.
        let plain = parse(r#"{"id":"d2","kind":"raw-order","leg":"A"}"#);
        assert_eq!(plain.header, None);
        assert!(!serde_json::to_string(&plain).unwrap().contains("header"));
    }

    #[test]
    fn an_unknown_deviation_field_is_refused_rather_than_ignored() {
        assert!(serde_json::from_str::<Deviation>(r#"{"id":"d1","kind":"raw-order","at-step":3}"#).is_err());
    }
}
