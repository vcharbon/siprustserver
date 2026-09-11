//! Shared construction plumbing every recipe funnels through: the freeze
//! boundary, body framing, and the raw seam the stringly options ride.

use bytes::Bytes;

use crate::draft::{Draft, RequestDraft, ResponseDraft, StartKind};
use crate::header::{ContentLength, HeaderName, MaxForwards, MediaType, Uri};
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest, SipResponse};

/// The hop budget a stack-originated request starts with (RFC 3261 §8.1.1.6),
/// as a number — the recipes that state it as a header push
/// [`MaxForwards::DEFAULT`].
pub(super) const DEFAULT_MAX_FORWARDS: u32 = MaxForwards::DEFAULT.value();

/// The media type a body carries when the caller names none.
const DEFAULT_CONTENT_TYPE: &str = "application/sdp";

/// A recipe supplies every mandatory header, so a freeze failure is a stack
/// bug — surfaced as a panic at construction time rather than threading a
/// `Result` through every generator.
pub(super) fn request(draft: RequestDraft) -> SipRequest {
    draft.freeze().unwrap_or_else(|e| panic!("generators built a malformed request: {e}"))
}

pub(super) fn response(draft: ResponseDraft) -> SipResponse {
    draft.freeze().unwrap_or_else(|e| panic!("generators built a malformed response: {e}"))
}

/// The Request-URI a caller named as text. A value that does not read as a URI
/// is carried whole, so the freeze reports it rather than this line.
///
/// Reached only for a dialog's stored `remote_target` — text this stack wrote
/// from an address it already read, re-read here because the dialog layer keeps
/// it as a string (ADR-0008). A caller that has the address as a value passes it
/// through `opts.request_uri`, which is typed. Nothing routes a *decision* here:
/// there an unreadable address is an `Err`.
pub(super) fn uri(text: &str) -> Uri {
    Uri::parse_or_verbatim(&SipStr::owned(text))
}

/// A `[display-name] <uri>[;tag=…]` header value assembled from text: the URI
/// gains angle brackets unless the caller already supplied a name-addr, and an
/// empty tag is no tag (`;tag=` with no value is malformed).
pub(super) fn name_addr_text(uri_or_name_addr: &str, tag: Option<&str>) -> SipStr {
    let bracketed = uri_or_name_addr.contains('<');
    match tag.filter(|t| !t.is_empty()) {
        Some(tag) if bracketed => SipStr::owned(&format!("{uri_or_name_addr};tag={tag}")),
        Some(tag) => SipStr::owned(&format!("<{uri_or_name_addr}>;tag={tag}")),
        None if bracketed => SipStr::owned(uri_or_name_addr),
        None => SipStr::owned(&format!("<{uri_or_name_addr}>")),
    }
}

/// Append the caller's own header lines — name and value exactly as given, so
/// a captured message replays with the spelling it was captured in. The lines
/// still answer to the header they name, so a stack default never duplicates
/// one of them.
pub(super) fn extra_headers<S: StartKind>(mut draft: Draft<S>, extra: &[SipHeader]) -> Draft<S> {
    for header in extra {
        draft = draft.push_raw(HeaderName::Other(header.name.clone()), header.value.clone());
    }
    draft
}

/// Whether the caller already carries `name` — the probe that keeps a stack
/// default from duplicating a header the caller supplied. Compact-form aware
/// (RFC 3261 §7.3.3).
pub(super) fn carries(extra: &[SipHeader], name: &HeaderName) -> bool {
    extra.iter().any(|header| name.matches(&header.name))
}

/// Frame the body: the media type when there is a body and the caller has not
/// already stated one, then the length the message actually carries
/// (RFC 3261 §7.4.1).
pub(super) fn framed<S: StartKind>(
    draft: Draft<S>,
    body: Vec<u8>,
    content_type: Option<MediaType>,
) -> Draft<S> {
    let body = Bytes::from(body);
    let mut draft = draft;
    if !body.is_empty() && !draft.has(&HeaderName::ContentType) {
        let media = content_type
            .unwrap_or_else(|| MediaType::new(SipStr::from_static(DEFAULT_CONTENT_TYPE)));
        draft = draft.push(media);
    }
    draft.push(ContentLength::new(body.len() as u32)).with_body(body)
}
