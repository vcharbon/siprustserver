//! The media type describing a body the B2BUA emits. The B2BUA sends its own
//! bodies, so the header describing one is the stack's to state (§16.6); these
//! are the readers every emission site shares.

use sip_message::header::{HeaderValue, MediaType};
use sip_message::{SipRequest, SipStr};

/// The media type a policy- or peer-supplied value names. The B2BUA emits its
/// own body, so the header describing it is the stack's to state (§16.6). Text
/// the reader rejects is carried as the peer wrote it rather than replaced by a
/// media type this stack invented — the body is still the peer's.
pub fn media_type(text: &str) -> Option<MediaType> {
    Some(
        MediaType::parse(&SipStr::owned(text))
            .unwrap_or_else(|_| MediaType::new(SipStr::owned(text))),
    )
}

/// `application/sdp` — the media type the B2BUA's own offers and answers carry.
pub fn sdp() -> MediaType {
    MediaType::new(SipStr::from_static("application/sdp"))
}

/// True iff `req` carries an SDP body: a non-empty body whose `Content-Type`
/// names `application/sdp`. What the offer/answer paths read to tell an offer
/// from a delayed-offer INVITE.
pub fn carries_sdp(req: &SipRequest) -> bool {
    !req.body().is_empty()
        && req.header::<MediaType>().and_then(Result::ok).is_some_and(|ct| ct.is("application/sdp"))
}
