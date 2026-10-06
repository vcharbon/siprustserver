//! The recording handle takes the DATAGRAM — bytes off the socket — and keeps
//! every one of them: a body that is not UTF-8 is recorded as it crossed the
//! wire, and a repeat of it is recognised by those bytes.

use pivot_interpreter::Recording;
use pivot_schema::bundle::Dir;

/// An INFO whose body holds bytes no UTF-8 decoder accepts.
fn binary_info() -> Vec<u8> {
    let body: &[u8] = &[0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x00, 0x7f];
    let mut out = format!(
        "INFO sip:b@h SIP/2.0\r\nCSeq: 2 INFO\r\nContent-Type: application/vnd.example.blob\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

#[test]
fn a_pushed_datagram_is_recorded_byte_for_byte() {
    let wire = binary_info();
    assert!(std::str::from_utf8(&wire).is_err(), "the datagram is not UTF-8 by construction");
    let rec = Recording::new();
    rec.push("B", Dir::In, 1_000, wire.as_slice(), Some("s11"), None);
    let recorded = &rec.legs()["B"][0];
    assert_eq!(recorded.wire(), wire, "every byte survives: {:02x?}", recorded.wire());
    // And on the line it writes, no byte was replaced: the datagram rides its
    // head as text and its body as base64.
    let line = rec.to_jsonl()["B"].clone();
    assert!(!line.contains('\u{FFFD}'), "{line}");
    let json: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
    assert!(json.get("raw").is_none(), "{line}");
    assert!(json["head"].as_str().is_some_and(|h| h.starts_with("INFO ")), "{line}");
    assert!(json["body_b64"].as_str().is_some(), "{line}");
}

#[test]
fn a_repeat_of_a_binary_datagram_resolves_to_the_first_copy() {
    let wire = binary_info();
    let rec = Recording::new();
    rec.push("A", Dir::Out, 0, wire.as_slice(), Some("s10"), None);
    rec.push_repeat("A", Dir::Out, 500_000, wire.as_slice(), Some("s10"), Some("retransmission"));
    let mut other = wire.clone();
    *other.last_mut().unwrap() ^= 0x01;
    rec.push_repeat("A", Dir::Out, 1_000_000, other.as_slice(), None, Some("another"));
    let messages = rec.legs()["A"].clone();
    assert_eq!(messages[0].repeat_of, None);
    assert_eq!(messages[1].repeat_of, Some(1), "the same bytes repeat the first copy");
    assert_eq!(messages[2].repeat_of, None, "one byte off is another datagram");
    assert_eq!(rec.first_seq_of("A", Dir::Out, &wire), Some(1));
}
