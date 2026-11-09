//! What ingress vouches for beyond the grammar of the line it splits: the
//! headers whose every line reads by its one reader, and those whose grammar
//! needs a value.

use crate::error::SipParseError;
use crate::header::{
    ContentDisposition, Event, HeaderName, HeaderValue as _, MediaType, ProxyRequire, Require,
    Supported,
};
use crate::sip_str::SipStr;

/// The reading of a line of `known` that ingress vouches for whole, held to
/// on wire bytes, `None` when ingress reads no such header: the body's
/// description and disposition (RFC 3261 §20.15, §20.11), the option tags a peer requires or supports
/// (§20.29, §20.32, §20.37) and the event package (RFC 6665 §8.2.1).
pub(super) fn vouched_reading(
    known: &HeaderName,
    value: &SipStr,
) -> Option<Result<(), SipParseError>> {
    Some(match known {
        HeaderName::ContentType => MediaType::parse(value).map(drop),
        HeaderName::ContentDisposition => ContentDisposition::parse(value).map(drop),
        HeaderName::Require => Require::parse(value).map(drop),
        HeaderName::ProxyRequire => ProxyRequire::parse(value).map(drop),
        HeaderName::Supported => Supported::parse(value).map(drop),
        HeaderName::Event => Event::parse(value).map(drop),
        _ => return None,
    })
}

/// The vouched headers whose grammar needs a value: an empty line of one is
/// refused here, where the readers of a comma list would read it as none.
pub(super) fn is_vouched_value(name: &HeaderName) -> bool {
    matches!(
        name,
        HeaderName::Via
            | HeaderName::Contact
            | HeaderName::Route
            | HeaderName::RecordRoute
            | HeaderName::ContentType
            | HeaderName::ContentDisposition
    )
}
