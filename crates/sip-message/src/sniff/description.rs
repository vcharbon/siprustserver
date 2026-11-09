//! The session description a raw datagram carries, read as a parsed
//! message's `sdp()` reads it.

use super::{body, content_length, header_values};
use crate::header::{HeaderValue, MediaType};

/// The session description `raw` carries, as a parsed message's `sdp()` reads
/// it ([`sdp_range`](crate::sdp_range)) under its first Content-Type and
/// Content-Disposition rows, its body bounded by a stated Content-Length;
/// `None` where it carries none.
pub fn session_description(raw: &[u8]) -> Option<&[u8]> {
    let carried = body(raw)?;
    let declared = content_length(raw).and_then(|len| usize::try_from(len).ok());
    let body = declared.map_or(Some(carried), |len| carried.get(..len))?;
    let content_type = header_values(raw, "Content-Type").into_iter().next()?;
    let content_type = MediaType::parse(&crate::sip_str::SipStr::owned(&content_type)).ok()?;
    let disposition = header_values(raw, "Content-Disposition").into_iter().next();
    crate::sdp_range(&content_type, disposition.as_deref(), body).and_then(|r| body.get(r))
}
