//! The lines of a message a later request reads when it carries that
//! message's body on another dialog: what describes the body, and what states
//! the sender's advertisement.

use super::relay::describes_body;
use crate::header::HeaderName;
use crate::types::SipHeader;

/// What a message states of its sender's advertisement: `Allow`, `Supported`,
/// `Accept` (RFC 3261 §20.5, §20.37, §20.1).
const ADVERTISEMENT: &[HeaderName] =
    &[HeaderName::Allow, HeaderName::Supported, HeaderName::Accept];

/// The lines of `headers` that describe the body of the message they head
/// ([`describes_body`]), state its media type, or state its sender's
/// advertisement, in wire order, repeats kept.
pub fn body_and_advertisement_lines(headers: &[SipHeader]) -> Vec<SipHeader> {
    let read = |h: &&SipHeader| {
        describes_body(&h.name)
            || HeaderName::ContentType.matches(&h.name)
            || ADVERTISEMENT.iter().any(|name| name.matches(&h.name))
    };
    headers.iter().filter(read).cloned().collect()
}
