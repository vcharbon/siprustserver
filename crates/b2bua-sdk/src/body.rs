//! The body a rule action puts on a message ([`Body`]), with the party that
//! wrote it ([`BodyAuthor`]). The author decides whether a session
//! description continues the session the dialog carries or is restated under
//! it (RFC 3264 §8), so every body names it.

use sip_message::MultipartPart;

/// Who wrote a body a rule action sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyAuthor {
    /// This stack: a description or payload of its own.
    Stack,
    /// The peer of the named leg: its description forwarded.
    Leg(String),
}

/// A body a rule action sends: its bytes, the media type stated for them, and
/// who wrote them. Empty bytes send no body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub bytes: Vec<u8>,
    /// The media type; `None` defaults to `application/sdp` where a body is
    /// present.
    pub content_type: Option<String>,
    pub author: BodyAuthor,
    /// Entity parts sent beside the session description `bytes` carry, in
    /// place of every other part they frame (RFC 5621 §3,
    /// [`sip_message::attach_parts`]). `None` sends `bytes` as they are.
    pub parts: Option<Vec<MultipartPart>>,
}

impl Body {
    /// `bytes` typed `content_type`, written by `author`.
    pub fn new(bytes: Vec<u8>, content_type: Option<String>, author: BodyAuthor) -> Self {
        Self { bytes, content_type, author, parts: None }
    }

    /// The same body with `parts` sent beside its session description.
    pub fn with_parts(mut self, parts: Vec<MultipartPart>) -> Self {
        self.parts = Some(parts);
        self
    }

    /// A session description written by the peer of `leg`.
    pub fn from_leg(bytes: Vec<u8>, leg: impl Into<String>) -> Self {
        Self::new(bytes, None, BodyAuthor::Leg(leg.into()))
    }

    /// A body of this stack's own, typed `content_type` (`None`: SDP).
    pub fn own(bytes: Vec<u8>, content_type: Option<String>) -> Self {
        Self::new(bytes, content_type, BodyAuthor::Stack)
    }
}
