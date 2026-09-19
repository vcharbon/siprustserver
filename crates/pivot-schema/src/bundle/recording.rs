//! One line of the **verbatim per-leg recording** (`PCAP2TEST_PIVOT_V3.md` §14
//! item 10): the record kind `recording/<leg>.jsonl` holds, one JSON object per
//! line, in wire order.
//!
//! A recorded datagram is BYTES (ADR-0035). In memory it is the datagram that
//! crossed the socket; on disk it is written in exactly one of the extractor's
//! three arms ([`Payload`]: `raw` | `head` + `body_b64` | `raw_b64`), chosen by
//! the bytes alone, beside the body's layout so a reader locates a MIME part
//! by offset and never splits on a boundary.
//!
//! Attribution is best-effort by design — a datagram no step claimed carries no
//! `step`, and is recorded rather than dropped — but the datagram itself never
//! is. The handle that MAKES a recording is the interpreter's; this is the shape
//! it writes and every reader decodes.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use sip_message::payload::{BodyLayout, Payload};
use sip_message::{CustomParser, SipParser as _};

/// Which way a datagram crossed the leg's vantage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// The leg's actor emitted it.
    Out,
    /// It arrived at the leg's actor.
    In,
}

/// One recorded datagram. Equality is byte equality of the datagram plus its
/// attribution: two lines carrying the same bytes in two arms are the same
/// recorded message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RecordedLine", into = "RecordedLine")]
pub struct RecordedMessage {
    /// Order within the leg, 1-based.
    pub seq: u64,
    pub dir: Dir,
    /// Microseconds from the start of the run.
    pub at_us: u64,
    /// The flow step the interpreter attributed it to, where one claimed it.
    pub step: Option<String>,
    /// The datagram, byte for byte.
    wire: Vec<u8>,
    /// The body's layout, present iff the datagram carries a body: media type,
    /// length and the MIME parts located by offset into the body bytes.
    pub body: Option<BodyLayout>,
    /// The `seq` of the earliest datagram on this leg, in this direction, that
    /// this one repeats byte for byte (friction H8). Set by the caller that
    /// KNOWS it is a repeat — the §17.2 seam for an arrival, the emitting step
    /// for a ladder it sent — never derived here.
    pub repeat_of: Option<u64>,
    /// Why the message is here when no step claimed it: a background policy's
    /// answer, an absorbed repeat, an unclaimed arrival.
    pub note: Option<String>,
}

impl RecordedMessage {
    /// A recorded line for `wire`, its body layout derived from the bytes.
    pub fn new(seq: u64, dir: Dir, at_us: u64, step: Option<String>, wire: Vec<u8>) -> Self {
        let body = Self::layout_of(&wire);
        RecordedMessage { seq, dir, at_us, step, wire, body, repeat_of: None, note: None }
    }

    /// The datagram that crossed the socket, whichever arm the line wrote.
    pub fn wire(&self) -> &[u8] {
        &self.wire
    }

    /// The datagram's on-disk arm, chosen by its bytes alone.
    pub fn payload(&self) -> Payload {
        Payload::of_datagram(&self.wire)
    }

    /// The layout of the body `wire` carries: the datagram is parsed once and
    /// [`BodyLayout::of`] read off it; a datagram that does not parse, or
    /// carries no body, has none.
    pub fn layout_of(wire: &[u8]) -> Option<BodyLayout> {
        CustomParser::new().parse(wire).ok().and_then(|parsed| BodyLayout::of(&parsed))
    }
}

/// The line as written: the datagram in exactly one of three arms beside its
/// attribution. A line with two arms would be two datagrams, one with none no
/// datagram at all; both are refused, as is any field not listed here.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(description = "One recorded datagram, in wire order within its leg. The datagram is \
                          written in exactly one of three arms chosen by its bytes alone: `raw` \
                          when the whole datagram is UTF-8, `head` + `body_b64` when only the \
                          body is not, `raw_b64` when not even the head is.")]
struct RecordedLine {
    /// Order within the leg, 1-based.
    seq: u64,
    dir: Dir,
    /// Microseconds from the start of the run.
    at_us: u64,
    /// The flow step the interpreter attributed it to, where one claimed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step: Option<String>,
    /// The whole datagram, when it is valid UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    raw: Option<String>,
    /// Start line, headers and the blank line as UTF-8, when the body is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    /// The body in standard base64, beside `head`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_b64: Option<String>,
    /// The whole datagram in standard base64, when not even the head is UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    raw_b64: Option<String>,
    /// The body's layout, present iff the datagram carries a body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<BodyLayout>,
    /// The `seq` of the earliest datagram on this leg, in this direction, that
    /// this one repeats byte for byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repeat_of: Option<u64>,
    /// Why the message is here when no step claimed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

impl TryFrom<RecordedLine> for RecordedMessage {
    type Error = String;

    fn try_from(line: RecordedLine) -> Result<Self, String> {
        let RecordedLine {
            seq,
            dir,
            at_us,
            step,
            raw,
            head,
            body_b64,
            raw_b64,
            body,
            repeat_of,
            note,
        } = line;
        let payload = match (raw, head, body_b64, raw_b64) {
            (Some(raw), None, None, None) => Payload::Text { raw },
            (None, Some(head), Some(body_b64), None) => Payload::HeadBody { head, body_b64 },
            (None, None, None, Some(raw_b64)) => Payload::Opaque { raw_b64 },
            (None, None, None, None) => {
                return Err(format!("recorded line {seq} carries no datagram"))
            }
            _ => {
                return Err(format!(
                    "recorded line {seq} states its datagram in more than one of raw, head + body_b64, raw_b64"
                ))
            }
        };
        let wire = payload.bytes().map_err(|e| format!("recorded line {seq}: {e}"))?;
        // The layout is a pure function of the bytes: a line written without
        // one gets it derived, a line written with one keeps the writer's.
        let body = body.or_else(|| RecordedMessage::layout_of(&wire));
        Ok(RecordedMessage { seq, dir, at_us, step, wire, body, repeat_of, note })
    }
}

impl From<RecordedMessage> for RecordedLine {
    fn from(message: RecordedMessage) -> Self {
        let (raw, head, body_b64, raw_b64) = match message.payload() {
            Payload::Text { raw } => (Some(raw), None, None, None),
            Payload::HeadBody { head, body_b64 } => (None, Some(head), Some(body_b64), None),
            Payload::Opaque { raw_b64 } => (None, None, None, Some(raw_b64)),
        };
        RecordedLine {
            seq: message.seq,
            dir: message.dir,
            at_us: message.at_us,
            step: message.step,
            raw,
            head,
            body_b64,
            raw_b64,
            body: message.body,
            repeat_of: message.repeat_of,
            note: message.note,
        }
    }
}

/// The exported schema is the line's: the arms as optional sibling keys, no
/// other key admitted.
impl JsonSchema for RecordedMessage {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("RecordedMessage")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        RecordedLine::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_message_round_trips_through_its_one_line_form() {
        let mut claimed = RecordedMessage::new(
            1,
            Dir::Out,
            0,
            Some("s1".into()),
            b"INVITE sip:x SIP/2.0\r\n\r\n".to_vec(),
        );
        let text = serde_json::to_string(&claimed).unwrap();
        assert!(!text.contains('\n'), "one datagram is one JSON Lines line: {text}");
        assert_eq!(serde_json::from_str::<RecordedMessage>(&text).unwrap(), claimed);
        // A datagram no step claimed carries neither field rather than a null.
        claimed.step = None;
        let text = serde_json::to_string(&claimed).unwrap();
        assert!(!text.contains("step") && !text.contains("note"), "{text}");
    }

    #[test]
    fn an_unknown_recording_field_is_refused_rather_than_ignored() {
        let text = r#"{"seq":1,"dir":"out","at_us":0,"raw":"X","leg":"A"}"#;
        assert!(serde_json::from_str::<RecordedMessage>(text).is_err(), "the leg is the FILE");
    }

    #[test]
    fn the_exported_schema_admits_the_arms_and_no_other_key() {
        let schema = serde_json::to_value(schemars::schema_for!(RecordedMessage)).unwrap();
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        for key in ["raw", "head", "body_b64", "raw_b64", "body"] {
            assert!(schema["properties"].get(key).is_some(), "{key}: {schema}");
        }
    }
}
