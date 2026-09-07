//! One line of the **verbatim per-leg recording** (`PCAP2TEST_PIVOT_V3.md` §14
//! item 10): the record kind `recording/<leg>.jsonl` holds, one JSON object per
//! line, in wire order.
//!
//! Attribution is best-effort by design — a datagram no step claimed carries no
//! `step`, and is recorded rather than dropped — but the datagram itself never
//! is. The handle that MAKES a recording is the interpreter's; this is the shape
//! it writes and every reader decodes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which way a datagram crossed the leg's vantage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// The leg's actor emitted it.
    Out,
    /// It arrived at the leg's actor.
    In,
}

/// One recorded datagram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordedMessage {
    /// Order within the leg, 1-based.
    pub seq: u64,
    pub dir: Dir,
    /// Microseconds from the start of the run.
    pub at_us: u64,
    /// The flow step the interpreter attributed it to, where one claimed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// The datagram, verbatim.
    pub raw: String,
    /// The `seq` of the earliest datagram on this leg, in this direction, that
    /// this one repeats byte for byte (friction H8). Set by the caller that
    /// KNOWS it is a repeat — the §17.2 seam for an arrival, the emitting step
    /// for a ladder it sent — never derived here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_of: Option<u64>,
    /// Why the message is here when no step claimed it: a background policy's
    /// answer, an absorbed repeat, an unclaimed arrival.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_message_round_trips_through_its_one_line_form() {
        let claimed = RecordedMessage {
            seq: 1,
            dir: Dir::Out,
            at_us: 0,
            step: Some("s1".into()),
            raw: "INVITE sip:x SIP/2.0\r\n\r\n".into(),
            repeat_of: None,
            note: None,
        };
        let text = serde_json::to_string(&claimed).unwrap();
        assert!(!text.contains('\n'), "one datagram is one JSON Lines line: {text}");
        assert_eq!(serde_json::from_str::<RecordedMessage>(&text).unwrap(), claimed);
        // A datagram no step claimed carries neither field rather than a null.
        let unclaimed = RecordedMessage { step: None, ..claimed };
        let text = serde_json::to_string(&unclaimed).unwrap();
        assert!(!text.contains("step") && !text.contains("note"), "{text}");
    }

    #[test]
    fn an_unknown_recording_field_is_refused_rather_than_ignored() {
        let text = r#"{"seq":1,"dir":"out","at_us":0,"raw":"X","leg":"A"}"#;
        assert!(serde_json::from_str::<RecordedMessage>(text).is_err(), "the leg is the FILE");
    }
}
