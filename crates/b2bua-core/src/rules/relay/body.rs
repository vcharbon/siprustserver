//! The media type describing a body the B2BUA emits. The B2BUA sends its own
//! bodies, so the header describing one is the stack's to state (§16.6); these
//! are the readers every emission site shares.

use sip_message::header::{HeaderName, HeaderValue, MediaType};
use sip_message::{SipHeader, SipRequest, SipStr};

use b2bua_sdk::model::Body;

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

/// True iff `req` carries a session description: a body typed
/// `application/sdp`, or a `multipart/…` body framing one (RFC 5621 §3.1).
/// What the offer/answer paths read to tell an offer from a delayed-offer
/// INVITE.
pub fn carries_sdp(req: &SipRequest) -> bool {
    req.sdp().is_some()
}

/// The media type of an override's bytes: the one it states, else the
/// originator's — an override without one re-sends the originator's body.
pub fn source_content_type(a_invite: &SipRequest, body: &Body) -> Option<String> {
    body.content_type
        .clone()
        .or_else(|| a_invite.raw(HeaderName::ContentType).next().map(str::to_string))
}

/// `headers` with the lines describing a sent body ([`Body::descriptors`])
/// beside them, each whose name `headers` does not state already: a body sent
/// whole reads on the new message as it read on the one it was taken from
/// (RFC 3261 §20.11). A message sending no body states none.
pub fn describe_body(headers: &mut Vec<SipHeader>, sent: &[u8], descriptors: &[SipHeader]) {
    if sent.is_empty() {
        return;
    }
    let stated: Vec<HeaderName> =
        headers.iter().map(|h| HeaderName::from(h.name.as_str())).collect();
    headers.extend(
        descriptors.iter().filter(|d| !stated.iter().any(|name| name.matches(&d.name))).cloned(),
    );
}

/// Whether the INVITE minted from `a_invite` under `body_override` makes an
/// offer: the originator's body when there is no override, an override's
/// bytes when it has any, and the session description an override with
/// attached parts carries (RFC 5621 §3.1).
pub fn mints_offer(a_invite: &SipRequest, body_override: Option<&Body>) -> bool {
    match body_override {
        None => carries_sdp(a_invite),
        Some(body) if body.parts.is_none() => !body.bytes.is_empty(),
        Some(body) => source_content_type(a_invite, body)
            .and_then(|ct| MediaType::parse(&SipStr::owned(&ct)).ok())
            .is_some_and(|ct| sip_message::sdp_range(&ct, &body.bytes).is_some()),
    }
}

#[cfg(test)]
mod tests {
    use super::carries_sdp;
    use sip_message::{compose_multipart, MultipartPart, SipMessage, SipParser, SipRequest};

    fn invite(content_type: &str, body: &[u8]) -> SipRequest {
        let head = format!(
            "INVITE sip:bob@10.0.0.2 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1;branch=z9hG4bK1\r\n\
             From: <sip:alice@10.0.0.1>;tag=a\r\nTo: <sip:bob@10.0.0.2>\r\n\
             Call-ID: c\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\n\
             Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let raw = [head.as_bytes(), body].concat();
        match sip_message::parser::custom::CustomParser::new().parse(&raw).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// RFC 5621 §3.1: an INVITE whose multipart body frames a description
    /// makes an offer like one typed `application/sdp`; one framing none, or
    /// carrying no body, is a delayed offer.
    #[test]
    fn a_description_framed_in_a_multipart_body_is_an_offer() {
        let sdp = b"v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                    m=audio 4000 RTP/AVP 8\r\n";
        assert!(carries_sdp(&invite("application/sdp", sdp)));
        let framed = compose_multipart(
            "multipart/mixed",
            &[
                MultipartPart::new("application/sdp", sdp.to_vec()),
                MultipartPart::new("application/vnd.example.indata", vec![0x77, 0x15]),
            ],
        )
        .unwrap();
        assert!(carries_sdp(&invite(&framed.content_type, &framed.body)));
        let unframed = compose_multipart(
            "multipart/mixed",
            &[MultipartPart::new("application/vnd.example.indata", vec![0x77, 0x15])],
        )
        .unwrap();
        assert!(!carries_sdp(&invite(&unframed.content_type, &unframed.body)));
        assert!(!carries_sdp(&invite("application/sdp", b"")));
    }
}
