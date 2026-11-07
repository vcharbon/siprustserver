//! Where a body holds its session description (RFC 3261 §13.2.1, RFC 5621):
//! the one reading every offer/answer decision rests on.
//!
//! - A description is a non-empty body or part typed `application/sdp` whose
//!   disposition is absent or `session` (RFC 3261 §20.11, RFC 5621 §8.2); an
//!   `early-session` or `render` one is no offer or answer.
//! - A `multipart/…` body is searched part by part, nested ones included, down
//!   to [`MAX_DEPTH`] levels: the first description of a `mixed` (or any other)
//!   body, the last of an `alternative` one (RFC 2046 §5.1.4, RFC 5621 §6.1).
//!   The disposition of a multipart body or part never hides what it frames.

use std::ops::Range;

use crate::header::{ContentDisposition, HeaderValue, MediaType, ParamValue};
use crate::sip_str::SipStr;

use super::entity::PartHead;
use super::framing::framed_parts;

/// How many multipart levels are searched, the message's own included. The
/// input states the depth, so the search stops: past this level a body holds
/// no description.
pub(super) const MAX_DEPTH: usize = 4;

/// A session description found in a body: its range, and the head of the part
/// framing it, `None` when the body is the description.
pub(super) struct Found {
    pub(super) range: Range<usize>,
    pub(super) part: Option<PartHead>,
}

/// Whether a body of disposition `disposition` (a `Content-Disposition`
/// value, `None` when none is stated) is a session's: absent, or one that
/// reads as `session`.
fn is_session(disposition: Option<&str>) -> bool {
    disposition.is_none_or(|text| {
        ContentDisposition::parse(&SipStr::owned(text)).is_ok_and(|d| d.is("session"))
    })
}

/// The description `body`, typed `content_type` and disposed `disposition`,
/// holds at multipart level `depth`.
pub(super) fn find(
    content_type: &MediaType,
    disposition: Option<&str>,
    body: &[u8],
    depth: usize,
) -> Option<Found> {
    if body.is_empty() {
        return None;
    }
    if content_type.is_sdp() {
        return is_session(disposition).then(|| Found { range: 0..body.len(), part: None });
    }
    if !content_type.is_multipart() || depth > MAX_DEPTH {
        return None;
    }
    let boundary = content_type.param("boundary").and_then(ParamValue::as_str)?;
    let mut found = framed_parts(body, boundary).into_iter().filter_map(|part| {
        let part_type = MediaType::parse(&SipStr::owned(part.head.get("content-type")?)).ok()?;
        let content = body.get(part.offset..part.offset + part.len)?;
        let disposition = part.head.get("content-disposition");
        let inner = find(&part_type, disposition, content, depth + 1)?;
        let range = part.offset + inner.range.start..part.offset + inner.range.end;
        Some(Found { range, part: inner.part.or(Some(part.head)) })
    });
    if content_type.subtype().eq_ignore_ascii_case("alternative") {
        found.last()
    } else {
        found.next()
    }
}
