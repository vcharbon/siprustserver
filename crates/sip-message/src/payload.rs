//! The lossless JSON form of one datagram: one encoding for every document
//! that carries a wire (a capture's flows document, a run bundle's recording).
//!
//! A datagram is bytes. [`Payload`] states them in EXACTLY ONE of three arms
//! chosen purely from the bytes, so identical bytes always take the same arm
//! and a document re-emitted after a transformation keeps its encoding.
//! [`BodyLayout`] is the enrichment that locates the body and its MIME parts
//! inside those bytes by offset, so no reader downstream owns MIME.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::header::{MediaType, ParamValue};
use crate::SipMessage;

/// Exact wire bytes, in EXACTLY ONE of three forms chosen purely from the
/// bytes so re-emitting a transformed model is deterministic. Reassembly:
/// `raw` as UTF-8 | `head` as UTF-8 ++ decode(`body_b64`) | decode(`raw_b64`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Payload {
    /// Whole payload is valid UTF-8 — the common, diff-readable case.
    Text { raw: String },
    /// Start line + headers + blank line as UTF-8, then a binary body as
    /// standard base64. A binary body or MIME part is what produces this form.
    HeadBody { head: String, body_b64: String },
    /// Even the head is not UTF-8 — opaque, standard base64.
    Opaque { raw_b64: String },
}

impl Payload {
    /// The form these bytes take, chosen purely from the bytes: whole-UTF-8 ⇒
    /// [`Payload::Text`]; else a UTF-8 head through the blank line that ends
    /// it ([`crate::sniff::body`]) ⇒ [`Payload::HeadBody`] with everything
    /// after that line as the body, a declared `Content-Length` notwithstanding;
    /// else (the head is not UTF-8, or is unterminated) ⇒ [`Payload::Opaque`].
    /// Deterministic, so a capture and a recording of the same bytes take the
    /// same arm and a document re-emitted after a transformation keeps its
    /// encoding.
    pub fn of_datagram(raw: &[u8]) -> Self {
        if let Ok(s) = std::str::from_utf8(raw) {
            return Payload::Text { raw: s.to_string() };
        }
        if let Some(body) = crate::sniff::body(raw) {
            let head_len = raw.len() - body.len();
            if let Ok(head) = std::str::from_utf8(&raw[..head_len]) {
                return Payload::HeadBody { head: head.to_string(), body_b64: base64(body) };
            }
        }
        Payload::Opaque { raw_b64: base64(raw) }
    }

    /// The exact wire bytes, whichever form carries them.
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        match self {
            Payload::Text { raw } => Ok(raw.clone().into_bytes()),
            Payload::HeadBody { head, body_b64 } => {
                let mut out = head.clone().into_bytes();
                out.extend(unbase64(body_b64)?);
                Ok(out)
            }
            Payload::Opaque { raw_b64 } => unbase64(raw_b64),
        }
    }

    /// The BODY bytes alone, or `None` where this form does not carry them.
    /// A whole-UTF-8 payload states them after the blank line that ends the
    /// head, a split payload states them base64, and an opaque one states
    /// nothing readable. A terminated head with nothing after it carries an
    /// EMPTY body — a fact, not an absence.
    pub fn body(&self) -> Option<Vec<u8>> {
        match self {
            Payload::Text { raw } => crate::sniff::body(raw.as_bytes()).map(<[u8]>::to_vec),
            Payload::HeadBody { body_b64, .. } => unbase64(body_b64).ok(),
            Payload::Opaque { .. } => None,
        }
    }
}

/// The message body's layout. A multipart body arrives ALREADY SPLIT: no
/// consumer downstream owns MIME.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BodyLayout {
    /// Media type, parameters dropped. Empty when the message declares none.
    pub content_type: String,
    /// Body length in bytes.
    pub len: usize,
    /// MIME parts in body order; empty for a single-part body.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PartLayout>,
}

impl BodyLayout {
    /// The layout of a parsed message's body; `None` when it carries none.
    /// Parts are located by [`crate::decompose_multipart`] under the
    /// `boundary` the `Content-Type` states.
    pub fn of(msg: &SipMessage) -> Option<Self> {
        let body = msg.body();
        if body.is_empty() {
            return None;
        }
        let content_type = msg.header::<MediaType>().and_then(Result::ok);
        let media_type = content_type.as_ref().map(|ct| ct.token().to_string()).unwrap_or_default();
        let boundary = content_type
            .as_ref()
            .and_then(|ct| ct.param("boundary"))
            .and_then(ParamValue::as_str)
            .map(str::to_string);
        Some(BodyLayout {
            content_type: media_type,
            len: body.len(),
            parts: boundary
                .map(|b| {
                    crate::decompose_multipart(body, &b).into_iter().map(PartLayout::from).collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// One MIME part, located rather than copied: `offset`/`len` index the body
/// bytes the message already carries, so the document holds each byte once and
/// a transformed body cannot disagree with its parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PartLayout {
    /// The part's own `Content-Type`, parameters included; `text/plain` when
    /// the part declares none (RFC 2045 §5.2).
    pub content_type: String,
    /// The part's `Content-ID`, angle brackets as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// The part's remaining entity headers in wire order, name and value as
    /// written (RFC 2045 §3) — `Content-Transfer-Encoding`,
    /// `Content-Disposition`, and any other the part states. `Content-Type` and
    /// `Content-ID` have their own fields and are not repeated here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<PartHeader>,
    /// Offset of the part's CONTENT (after its blank line) into the body.
    pub offset: usize,
    pub len: usize,
}

impl From<crate::LocatedPart> for PartLayout {
    fn from(part: crate::LocatedPart) -> Self {
        PartLayout {
            content_type: part.content_type,
            content_id: part.content_id,
            headers: part
                .headers
                .into_iter()
                .map(|(name, value)| PartHeader { name, value })
                .collect(),
            offset: part.offset,
            len: part.len,
        }
    }
}

/// One entity header of a MIME part, as the part wrote it. Unlike a message
/// header it is not canonicalized: a part replays under the spelling it came in
/// with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PartHeader {
    pub name: String,
    pub value: String,
}

/// Standard base64 (RFC 4648 §4, padded) of `bytes`.
pub fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The bytes a standard base64 text encodes.
pub fn unbase64(text: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "INFO sip:b@h SIP/2.0\r\nContent-Type: application/octet-stream\r\nContent-Length: 3\r\n\r\n";

    #[test]
    fn the_arm_is_chosen_by_the_bytes_alone() {
        let text = b"INFO sip:b@h SIP/2.0\r\nContent-Length: 0\r\n\r\n";
        assert!(matches!(Payload::of_datagram(text), Payload::Text { .. }));

        let mut head_body = HEAD.as_bytes().to_vec();
        head_body.extend([0xff, 0x00, 0x80]);
        let p = Payload::of_datagram(&head_body);
        assert_eq!(p, Payload::HeadBody { head: HEAD.into(), body_b64: "/wCA".into() });
        assert_eq!(p.bytes().unwrap(), head_body);
        assert_eq!(p.body().unwrap(), vec![0xff, 0x00, 0x80]);

        let opaque = vec![0xff, b'I', b'N', b'F', b'O'];
        let p = Payload::of_datagram(&opaque);
        assert!(matches!(p, Payload::Opaque { .. }));
        assert_eq!(p.bytes().unwrap(), opaque);
        assert_eq!(p.body(), None);
    }

    #[test]
    fn a_tail_longer_than_the_declared_length_still_splits() {
        let mut raw = HEAD.as_bytes().to_vec();
        raw.extend([0xff, 0x00, 0x80, 0x0d, 0x0a]);
        let p = Payload::of_datagram(&raw);
        assert_eq!(p, Payload::HeadBody { head: HEAD.into(), body_b64: "/wCADQo=".into() });
        assert_eq!(p.bytes().unwrap(), raw);
    }

    #[test]
    fn an_unterminated_head_that_is_not_utf8_is_opaque() {
        let raw = b"INFO sip:b@h SIP/2.0\r\nX: \xff\r\n".to_vec();
        assert!(matches!(Payload::of_datagram(&raw), Payload::Opaque { .. }));
    }
}
