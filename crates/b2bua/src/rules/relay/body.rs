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

/// True iff `req` carries a session description: a body typed
/// `application/sdp`, or a `multipart/…` body framing one (RFC 5621 §3.1).
/// What the offer/answer paths read to tell an offer from a delayed-offer
/// INVITE.
pub fn carries_sdp(req: &SipRequest) -> bool {
    req.sdp().is_some()
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
