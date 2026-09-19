//! The recording line's on-disk ENCODING, read and refused through serde alone:
//! a datagram is written in exactly one of the extractor's three arms — `raw`
//! when the whole datagram is UTF-8, `head` + `body_b64` when only the body is
//! not, `raw_b64` when not even the head is — and every reader decodes the same
//! three (`PCAP2TEST_PIVOT_V3.md` §14 item 10).

use pivot_schema::bundle::RecordedMessage;

const HEAD: &str = "INFO sip:b@h SIP/2.0\r\nCSeq: 2 INFO\r\nContent-Type: application/vnd.example.blob\r\nContent-Length: 6\r\n\r\n";

/// Six bytes, three of them never valid UTF-8 (`ff fe 80` after `00 01 02`).
const BODY_B64: &str = "AAEC//6A";

fn decode(line: &str) -> serde_json::Result<RecordedMessage> {
    serde_json::from_str(line)
}

#[test]
fn a_head_body_line_is_a_recorded_message() {
    let line = format!(
        r#"{{"seq":1,"dir":"in","at_us":10,"step":"s11","head":{},"body_b64":"{BODY_B64}"}}"#,
        serde_json::to_string(HEAD).unwrap()
    );
    let message = decode(&line).unwrap_or_else(|e| panic!("a head+body_b64 line decodes: {e}"));
    assert_eq!(message.seq, 1);
    assert_eq!(message.step.as_deref(), Some("s11"));
    // And it re-serializes in the same arm, on one line.
    let text = serde_json::to_string(&message).unwrap();
    assert!(!text.contains('\n'), "one line: {text}");
    let back: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(back["head"], HEAD, "{text}");
    assert_eq!(back["body_b64"], BODY_B64, "{text}");
    assert!(back.get("raw").is_none(), "one arm only: {text}");
}

#[test]
fn an_opaque_line_is_a_recorded_message() {
    // `ff fe` then text: not a UTF-8 head, so the whole datagram is base64.
    let line =
        r#"{"seq":2,"dir":"in","at_us":11,"raw_b64":"//5JTkZPIHNpcDpiQGggU0lQLzIuMA0KDQo="}"#;
    let message = decode(line).unwrap_or_else(|e| panic!("a raw_b64 line decodes: {e}"));
    let back: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
    assert_eq!(back["raw_b64"], "//5JTkZPIHNpcDpiQGggU0lQLzIuMA0KDQo=");
    assert!(back.get("raw").is_none() && back.get("head").is_none(), "one arm only: {back}");
}

#[test]
fn a_text_line_stays_a_text_line() {
    let line = r#"{"seq":1,"dir":"out","at_us":0,"raw":"INVITE sip:x SIP/2.0\r\n\r\n"}"#;
    let message = decode(line).unwrap();
    assert_eq!(serde_json::to_string(&message).unwrap(), line);
}

/// A line states one arm. Two arms would be two datagrams on one line, and
/// none is a line with no datagram at all.
#[test]
fn a_line_with_two_arms_or_none_is_refused() {
    let head = serde_json::to_string(HEAD).unwrap();
    for line in [
        format!(
            r#"{{"seq":1,"dir":"in","at_us":0,"raw":"X","head":{head},"body_b64":"{BODY_B64}"}}"#
        ),
        r#"{"seq":1,"dir":"in","at_us":0,"raw":"X","raw_b64":"WA=="}"#.to_string(),
        format!(r#"{{"seq":1,"dir":"in","at_us":0,"head":{head}}}"#),
        format!(r#"{{"seq":1,"dir":"in","at_us":0,"body_b64":"{BODY_B64}"}}"#),
        r#"{"seq":1,"dir":"in","at_us":0}"#.to_string(),
    ] {
        assert!(decode(&line).is_err(), "refused: {line}");
    }
}

/// The unknown-field refusal survives the arms: the leg is the FILE, and a
/// typo is still a typo whichever arm the line carries.
#[test]
fn an_unknown_field_is_still_refused_on_every_arm() {
    let head = serde_json::to_string(HEAD).unwrap();
    for line in [
        r#"{"seq":1,"dir":"out","at_us":0,"raw":"X","leg":"A"}"#.to_string(),
        format!(
            r#"{{"seq":1,"dir":"in","at_us":0,"head":{head},"body_b64":"{BODY_B64}","leg":"A"}}"#
        ),
        r#"{"seq":1,"dir":"in","at_us":0,"raw_b64":"WA==","leg":"A"}"#.to_string(),
    ] {
        assert!(decode(&line).is_err(), "refused: {line}");
    }
}
