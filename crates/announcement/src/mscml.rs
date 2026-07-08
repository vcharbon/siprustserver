//! Minimal MSCML (Media Server Control Markup Language, RFC 5022 family) build
//! + parse — just enough for the announcement service: build a `<play>` request
//! and recognise a successful `<response>`. Deliberately tiny (string-level, no
//! XML dependency); the body is opaque to the B2BUA and rides `SendRequestToLeg`.

/// The MSCML content type carried on the INFO control channel.
pub const CONTENT_TYPE: &str = "application/mediaservercontrol+xml";

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

/// Parse the `code="…"` attribute of an MSCML `<response>` (the playback result).
pub fn parse_response_code(body: &[u8]) -> Option<u16> {
    let s = std::str::from_utf8(body).ok()?;
    let resp = s.find("<response")?;
    let after = &s[resp..];
    let key = after.find("code=")? + "code=".len();
    let rest = after[key..].trim_start_matches(['"', '\'']);
    let end = rest.find(['"', '\''])?;
    rest[..end].trim().parse().ok()
}

/// Whether `body` is an MSCML `<response>` reporting success (a 2xx code).
pub fn is_success_response(body: &[u8]) -> bool {
    matches!(parse_response_code(body), Some(c) if (200..300).contains(&c))
}

/// Whether `body` is an MSCML `<response>` reporting a failed playback (a code
/// that is present but not 2xx — e.g. a max-duration/no-answer abort). Distinct
/// from "not an MSCML response" (no code), so the failure rule fires only on a
/// genuine negative `<response>`.
pub fn is_failure_response(body: &[u8]) -> bool {
    matches!(parse_response_code(body), Some(c) if !(200..300).contains(&c))
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

    #[test]
    fn response_round_trips_the_code() {
        assert_eq!(parse_response_code(&build_response(200)), Some(200));
        assert_eq!(parse_response_code(&build_response(420)), Some(420));
    }

    #[test]
    fn success_is_2xx_only() {
        assert!(is_success_response(&build_response(200)));
        assert!(is_success_response(&build_response(206)));
        assert!(!is_success_response(&build_response(420)));
        assert!(!is_success_response(b"not xml"));
        assert!(!is_success_response(b"<response request=\"play\"/>")); // no code
    }

    #[test]
    fn failure_is_present_non_2xx() {
        assert!(is_failure_response(&build_response(480)));
        assert!(is_failure_response(&build_response(420)));
        assert!(!is_failure_response(&build_response(200)));
        assert!(!is_failure_response(b"not xml")); // no code ⇒ not a failure report
        assert!(!is_failure_response(b"<response request=\"play\"/>")); // no code
    }
}
