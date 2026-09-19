//! A recorded line holds BYTES: whichever arm the line wrote, `wire()` is the
//! datagram that crossed the socket, and the arm is chosen by those bytes
//! alone. A line whose datagram carries a body also states the body's LAYOUT —
//! the extractor's `body` enrichment, media type, length and the MIME parts
//! located by offset — so a reader finds a part without splitting on a
//! boundary (`PCAP2TEST_PIVOT_V3.md` §14 item 10).

use base64::Engine as _;
use pivot_schema::bundle::RecordedMessage;
use sip_message::SipParser as _;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A recorded line for `wire`, in the arm the bytes call for: written the way
/// the interpreter writes it, then read back through the one decoder.
fn line_for(seq: u64, wire: &[u8]) -> RecordedMessage {
    let head_len =
        sip_message::sniff::body(wire).map_or(wire.len(), |body| wire.len() - body.len());
    let arm = match std::str::from_utf8(wire) {
        Ok(text) => format!(r#""raw":{}"#, serde_json::to_string(text).unwrap()),
        Err(_) => match std::str::from_utf8(&wire[..head_len]) {
            Ok(head) => format!(
                r#""head":{},"body_b64":"{}""#,
                serde_json::to_string(head).unwrap(),
                b64(&wire[head_len..])
            ),
            Err(_) => format!(r#""raw_b64":"{}""#, b64(wire)),
        },
    };
    let line = format!(r#"{{"seq":{seq},"dir":"in","at_us":0,{arm}}}"#);
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line}: {e}"))
}

/// A parseable in-dialog INFO carrying `body`: the layout is read off the
/// parsed message, so the datagram states what RFC 3261 §8.1.1 requires.
fn datagram(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "INFO sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP a.invalid;branch=z9hG4bK1\r\nFrom: <sip:a@h>;tag=a1\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c1@a.invalid\r\nCSeq: 2 INFO\r\nMax-Forwards: 70\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

const BLOB: &[u8] = &[0x00, 0x01, 0x02, 0xff, 0xfe, 0x80, 0x00];

#[test]
fn the_wire_is_the_datagram_whichever_arm_the_line_wrote() {
    let binary = datagram("application/vnd.example.blob", BLOB);
    assert!(std::str::from_utf8(&binary).is_err());
    let message = line_for(1, &binary);
    assert_eq!(message.wire(), binary, "head + body_b64 reassembles to the bytes");
    let json: serde_json::Value = serde_json::to_value(&message).unwrap();
    assert!(json.get("head").is_some() && json.get("body_b64").is_some(), "{json}");
    assert!(json.get("raw").is_none(), "{json}");

    let text = datagram("text/plain", b"hello\r\n");
    let message = line_for(2, &text);
    assert_eq!(message.wire(), text);
    let json: serde_json::Value = serde_json::to_value(&message).unwrap();
    assert!(json.get("raw").is_some() && json.get("head").is_none(), "{json}");

    let mut opaque = vec![0xff, 0xfe];
    opaque.extend_from_slice(&text);
    let message = line_for(3, &opaque);
    assert_eq!(message.wire(), opaque);
    let json: serde_json::Value = serde_json::to_value(&message).unwrap();
    assert!(json.get("raw_b64").is_some() && json.get("head").is_none(), "{json}");
}

/// The arm is a function of the bytes, not of the line that carried them: a
/// datagram read off a base64 arm whose bytes are UTF-8 is written back as
/// text, and one whose head alone is UTF-8 is written back split.
#[test]
fn the_arm_is_chosen_by_the_bytes_alone() {
    let text = datagram("text/plain", b"hello\r\n");
    let line = format!(r#"{{"seq":1,"dir":"in","at_us":0,"raw_b64":"{}"}}"#, b64(&text));
    let message: RecordedMessage = serde_json::from_str(&line).unwrap();
    assert_eq!(message.wire(), text);
    let json: serde_json::Value = serde_json::to_value(&message).unwrap();
    assert_eq!(json["raw"], String::from_utf8(text).unwrap(), "{json}");

    let binary = datagram("application/vnd.example.blob", BLOB);
    let line = format!(r#"{{"seq":2,"dir":"in","at_us":0,"raw_b64":"{}"}}"#, b64(&binary));
    let message: RecordedMessage = serde_json::from_str(&line).unwrap();
    assert_eq!(message.wire(), binary);
    let json: serde_json::Value = serde_json::to_value(&message).unwrap();
    assert_eq!(json["body_b64"], b64(BLOB), "{json}");
}

/// Equality is byte equality: two lines carrying the same datagram in two arms
/// are the same recorded message, which is what a repeat lookup compares.
#[test]
fn two_lines_with_the_same_bytes_compare_equal() {
    let text = datagram("text/plain", b"hello\r\n");
    let as_text = line_for(1, &text);
    let line = format!(r#"{{"seq":1,"dir":"in","at_us":0,"raw_b64":"{}"}}"#, b64(&text));
    let as_bytes: RecordedMessage = serde_json::from_str(&line).unwrap();
    assert_eq!(as_text.wire(), as_bytes.wire());
    assert_eq!(as_text, as_bytes);
}

#[test]
fn the_body_layout_is_present_exactly_where_a_body_is_carried() {
    let bodiless = b"BYE sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP a.invalid;branch=z9hG4bK2\r\nFrom: <sip:a@h>;tag=a1\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c1@a.invalid\r\nCSeq: 3 BYE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let message = line_for(1, bodiless);
    assert!(message.body.is_none(), "{:?}", message.body);
    let json: serde_json::Value = serde_json::to_value(&message).unwrap();
    assert!(json.get("body").is_none(), "no layout key on a bodiless line: {json}");

    let single = datagram("application/vnd.example.blob;v=1", BLOB);
    let message = line_for(2, &single);
    let layout = message.body.as_ref().expect("a body is carried");
    assert_eq!(layout.content_type, "application/vnd.example.blob", "parameters dropped");
    assert_eq!(layout.len, BLOB.len());
    assert!(layout.parts.is_empty(), "a single body has no parts");
    let json: serde_json::Value = serde_json::to_value(&message).unwrap();
    assert_eq!(json["body"]["len"], BLOB.len(), "{json}");
    assert!(json["body"].get("parts").is_none(), "an empty part list is not written: {json}");
}

#[test]
fn a_multipart_body_s_parts_are_located_in_the_recorded_bytes() {
    let mut body = Vec::new();
    body.extend_from_slice(b"--b1\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n");
    body.extend_from_slice(
        b"\r\n--b1\r\nContent-Type: application/vnd.example.blob\r\nContent-ID: <blob@example.invalid>\r\nContent-Transfer-Encoding: binary\r\n\r\n",
    );
    body.extend_from_slice(BLOB);
    body.extend_from_slice(b"\r\n--b1--\r\n");
    let wire = datagram("multipart/mixed;boundary=b1", &body);
    assert!(std::str::from_utf8(&wire).is_err());

    let message = line_for(1, &wire);
    assert_eq!(message.wire(), wire);
    let layout = message.body.as_ref().expect("a body is carried");
    assert_eq!(layout.content_type, "multipart/mixed");
    assert_eq!(layout.len, body.len());
    assert_eq!(layout.parts.len(), 2, "{:#?}", layout.parts);
    let recorded = message.wire();
    let head_len = recorded.len() - layout.len;
    let part = |n: usize| {
        let at = head_len + layout.parts[n].offset;
        &recorded[at..at + layout.parts[n].len]
    };
    assert_eq!(layout.parts[0].content_type, "application/sdp");
    assert_eq!(part(0), b"v=0\r\n");
    assert_eq!(layout.parts[1].content_type, "application/vnd.example.blob");
    assert_eq!(layout.parts[1].content_id.as_deref(), Some("<blob@example.invalid>"));
    assert_eq!(
        layout.parts[1].headers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
        ["Content-Transfer-Encoding"]
    );
    assert_eq!(part(1), BLOB, "the binary part is located byte for byte");

    // On disk, the same layout rides the line beside the arm.
    let json: serde_json::Value = serde_json::to_value(&message).unwrap();
    assert_eq!(json["body"]["parts"].as_array().map(Vec::len), Some(2), "{json}");
    assert_eq!(json["body"]["parts"][1]["offset"], layout.parts[1].offset, "{json}");
}

/// The recording's layout is the extractor's own type: one enrichment, one
/// encoding, for every document that carries a wire.
#[test]
fn the_layout_is_the_shared_wire_type() {
    let wire = datagram("application/vnd.example.blob", BLOB);
    let message = line_for(1, &wire);
    let parsed =
        sip_message::parser::custom::CustomParser::new().parse(&wire).expect("the datagram parses");
    let expected = sip_message::payload::BodyLayout::of(&parsed).expect("a body is carried");
    assert_eq!(message.body, Some(expected));
    assert_eq!(
        serde_json::to_value(sip_message::payload::Payload::of_datagram(&wire)).unwrap(),
        serde_json::to_value(&message)
            .unwrap()
            .as_object()
            .map(|line| {
                serde_json::Value::Object(
                    line.iter()
                        .filter(|(key, _)| {
                            matches!(key.as_str(), "raw" | "head" | "body_b64" | "raw_b64")
                        })
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                )
            })
            .unwrap(),
        "the line's arm is the payload's own serialization"
    );
}
