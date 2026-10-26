//! The serializer writes exactly one Content-Length, stating the body it
//! writes (RFC 3261 §20.14): a header list carrying several rows of it, in
//! either name form, still leaves with one.

use sip_message::serializer::render;
use sip_message::SipHeader;

fn content_lengths(wire: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(wire);
    let head = text.split("\r\n\r\n").next().unwrap_or_default().to_string();
    head.split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| {
            name.trim().eq_ignore_ascii_case("content-length")
                || name.trim().eq_ignore_ascii_case("l")
        })
        .map(|(_, value)| value.trim().to_string())
        .collect()
}

fn headers(rows: &[(&str, &str)]) -> Vec<SipHeader> {
    rows.iter().map(|(name, value)| SipHeader::new(*name, *value)).collect()
}

#[test]
fn several_content_length_rows_leave_as_one() {
    for rows in [
        [("Content-Length", "13"), ("Content-Length", "5")],
        [("Content-Length", "5"), ("Content-Length", "5")],
        [("Content-Length", "5"), ("l", "99")],
        [("l", "99"), ("Content-Length", "5")],
    ] {
        let wire =
            render(&headers(&rows), b"hello", |out| out.extend_from_slice(b"SIP/2.0 200 OK"));
        assert_eq!(content_lengths(&wire), ["5"], "{rows:?}: {:?}", String::from_utf8_lossy(&wire));
        assert!(wire.ends_with(b"\r\n\r\nhello"));
    }
}

#[test]
fn a_compact_content_length_is_restated_to_the_body() {
    let wire =
        render(&headers(&[("l", "99")]), b"hello", |out| out.extend_from_slice(b"SIP/2.0 200 OK"));
    assert_eq!(content_lengths(&wire), ["5"], "{:?}", String::from_utf8_lossy(&wire));
}
