//! Minimal MSCML (Media Server Control Markup Language, RFC 5022 family) for
//! the announcement service: build a `<play>` request, and tell a `<response>`
//! the media server sends by the code [`sip_message::mscml`] reads off it. The
//! body is opaque to the B2BUA and rides `SendRequestToLeg`.

use sip_message::header::HeaderName;
use sip_message::{mscml, SipRequest};

/// The MSCML content type carried on the INFO control channel.
pub const CONTENT_TYPE: &str = mscml::CONTENT_TYPE;

/// Build an MSCML `<play>` request for `clip_id` (the audio prompt to play).
pub fn build_play(clip_id: &str) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<MediaServerControl version=\"1.0\">\
<request>\
<play>\
<prompt><audio href=\"{clip_id}\"/></prompt>\
</play>\
</request>\
</MediaServerControl>"
    )
    .into_bytes()
}

/// Build an MSCML `<response>` with the given status code (for the MRF side of a
/// test). A 2xx code is a success.
pub fn build_response(code: u16) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<MediaServerControl version=\"1.0\">\
<response request=\"play\" code=\"{code}\"/>\
</MediaServerControl>"
    )
    .into_bytes()
}

/// The code of the MSCML `<response>` `req` carries: its body under the
/// MSCML media type, or the first MSCML part of a multipart body. `None` where
/// it carries no response stating a three-digit code.
pub fn response_code(req: &SipRequest) -> Option<u16> {
    let content_type = req.raw(HeaderName::ContentType).next();
    mscml::responses_in(content_type, req.body()).into_iter().next()?.code
}

/// Whether `req` carries an MSCML `<response>` reporting success (a 2xx code).
pub fn is_success_response(req: &SipRequest) -> bool {
    matches!(response_code(req), Some(c) if (200..300).contains(&c))
}

/// Whether `req` carries an MSCML `<response>` reporting a failed playback (a
/// code that is present but not 2xx — e.g. a max-duration/no-answer abort).
/// Distinct from "not an MSCML response" (no code), so the failure rule fires
/// only on a genuine negative `<response>`.
pub fn is_failure_response(req: &SipRequest) -> bool {
    matches!(response_code(req), Some(c) if !(200..300).contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_carries_the_clip_id() {
        let body = build_play("intro-001");
        let s = String::from_utf8(body).unwrap();
        assert!(s.contains("<play>"));
        assert!(s.contains("href=\"intro-001\""));
    }

    /// An in-dialog INFO carrying `body` under `content_type`.
    fn info(content_type: &str, body: &[u8]) -> SipRequest {
        use sip_message::SipParser;
        let head = format!(
            "INFO sip:as@192.0.2.1:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 192.0.2.2:5070;branch=z9hG4bK-info-1\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:mrf@192.0.2.2:5070>;tag=mrf-1\r\n\
             To: <sip:as@192.0.2.1:5060>;tag=as-1\r\n\
             Call-ID: info-1@192.0.2.2\r\n\
             CSeq: 2 INFO\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        );
        let raw = [head.as_bytes(), body].concat();
        match sip_message::CustomParser::new().parse(&raw) {
            Ok(sip_message::SipMessage::Request(r)) => r,
            other => panic!("the INFO fixture parses as a request: {other:?}"),
        }
    }

    fn mscml_info(body: &[u8]) -> SipRequest {
        info(CONTENT_TYPE, body)
    }

    #[test]
    fn response_round_trips_the_code() {
        assert_eq!(response_code(&mscml_info(&build_response(200))), Some(200));
        assert_eq!(response_code(&mscml_info(&build_response(420))), Some(420));
    }

    #[test]
    fn success_is_2xx_only() {
        assert!(is_success_response(&mscml_info(&build_response(200))));
        assert!(is_success_response(&mscml_info(&build_response(206))));
        assert!(!is_success_response(&mscml_info(&build_response(420))));
        assert!(!is_success_response(&mscml_info(b"not xml")));
        let no_code = b"<MediaServerControl version=\"1.0\"><response request=\"play\"/></MediaServerControl>";
        assert!(!is_success_response(&mscml_info(no_code)));
    }

    #[test]
    fn failure_is_present_non_2xx() {
        assert!(is_failure_response(&mscml_info(&build_response(480))));
        assert!(is_failure_response(&mscml_info(&build_response(420))));
        assert!(!is_failure_response(&mscml_info(&build_response(200))));
        assert!(!is_failure_response(&mscml_info(b"not xml"))); // no code ⇒ not a failure report
    }

    /// The code is the response element's own attribute: XML spacing reads, a
    /// look-alike attribute or a `code=` inside another value does not.
    #[test]
    fn the_code_is_the_response_element_s_own() {
        let body = b"<MediaServerControl version=\"1.0\"><response request = \"play\" \
                     text=\"a>b code=599\" xcode=\"599\" code = \"200\"/></MediaServerControl>";
        assert_eq!(response_code(&mscml_info(body)), Some(200));
    }

    /// A body of another media type is no MSCML response, whatever it holds.
    #[test]
    fn only_an_mscml_body_reports() {
        assert_eq!(response_code(&info("application/xml", &build_response(480))), None);
        assert!(!is_failure_response(&info("text/plain", &build_response(480))));
    }
}
