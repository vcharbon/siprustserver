//! What ingress vouches for of a message's body: that a reader can frame it.
//! A body states its Content-Type (RFC 3261 §20.15), and a `multipart/…` one
//! names one boundary of RFC 2046 §5.1.1's form, which no reader could split
//! by otherwise, and holds no line starting with its delimiter that is none.

use crate::error::SipParseError;
use crate::header::{HeaderId, HeaderValue as _, MediaType, ParamValue};
use crate::header_table::HeaderTable;
use crate::multipart::{frames_cleanly, is_boundary};
use crate::parser::{Exemption, SipParserLimits};

/// Why a message whose head is `headers` cannot carry `body`, `None` when it
/// can. `options_200` says the message is a 200 to OPTIONS, which
/// `options-body-no-type` lets carry an untyped body.
pub(super) fn body_type_refusal(
    headers: &HeaderTable,
    body: &[u8],
    options_200: bool,
    limits: &SipParserLimits,
) -> Option<SipParseError> {
    if body.is_empty() {
        return None;
    }
    let Some(line) = headers.of_id(HeaderId::ContentType).next() else {
        if options_200 && limits.exemptions.holds(&Exemption::OptionsBodyNoType) {
            return None;
        }
        return Some(SipParseError::new("A message body with no Content-Type (RFC 3261 §20.15)"));
    };
    // An unreadable Content-Type line is refused where the line is read.
    let media = MediaType::parse(&line.value).ok()?;
    if !media.is_multipart() {
        return None;
    }
    match media.param("boundary").and_then(ParamValue::as_str) {
        Some(boundary) if is_boundary(boundary) && frames_cleanly(body, boundary) => None,
        Some(boundary) if is_boundary(boundary) => Some(SipParseError::new(format!(
            "Content-Type {} body holds a line starting with --{boundary} that is no \
             delimiter (RFC 2046 §5.1.1)",
            media.token()
        ))),
        Some(boundary) => Some(SipParseError::new(format!(
            "Content-Type {} boundary {boundary:?} is not 1 to 70 bchars ending in no space \
             (RFC 2046 §5.1.1)",
            media.token()
        ))),
        None => Some(SipParseError::new(format!(
            "Content-Type {} states no boundary (RFC 2046 §5.1.1)",
            media.token()
        ))),
    }
}
