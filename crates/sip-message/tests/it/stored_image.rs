//! What the stack keeps as bytes reads back whatever its setting: no limit, no
//! exemption and no ingress gate applies, and what is no message is refused,
//! never a panic.

use sip_message::{read_stored, CustomParser, SipMessage, SipParser};

const VIA: &str = "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-stored";

/// A BYE as the stack would store it, `line` among its headers.
fn bye_with(line: &str) -> Vec<u8> {
    format!(
        "BYE sip:bob@192.0.2.20 SIP/2.0\r\nVia: {VIA}\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=a1\r\nTo: <sip:bob@example.com>;tag=b1\r\n\
         Call-ID: stored@192.0.2.10\r\nCSeq: 2 BYE\r\n{line}\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// `image` reads back and the strict parser refuses it.
#[track_caller]
fn only_the_stored_read_takes(image: &[u8]) {
    assert!(CustomParser::new().parse(image).is_err(), "the strict parser refuses it");
    let read = read_stored(image);
    assert!(read.is_ok(), "{read:?}");
}

#[test]
fn a_line_past_every_limit_reads() {
    only_the_stored_read_takes(&bye_with(&format!("X-Long: {}", "a".repeat(70_000))));
}

#[test]
fn a_request_uri_past_its_limit_reads() {
    let image = String::from_utf8(bye_with("X-Fine: 1"))
        .expect("text")
        .replace("sip:bob@192.0.2.20 ", &format!("sip:{}@192.0.2.20 ", "b".repeat(70_000)));
    only_the_stored_read_takes(image.as_bytes());
}

#[test]
fn a_block_past_the_line_limit_reads() {
    only_the_stored_read_takes(&bye_with(&vec!["X-Pad: 1"; 200].join("\r\n")));
}

#[test]
fn a_value_ingress_would_refuse_reads() {
    for line in [
        "Expires: soon",
        "Session-Expires: soon",
        "Max-Forwards: 70",
        "Subject: he said \"hi",
        "Authorization: Digest realm",
    ] {
        only_the_stored_read_takes(&bye_with(line));
    }
}

#[test]
fn a_repeated_tag_reads() {
    let image = String::from_utf8(bye_with("X-Fine: 1"))
        .expect("text")
        .replace(";tag=a1", ";tag=a1;tag=a1");
    only_the_stored_read_takes(image.as_bytes());
}

#[test]
fn a_final_with_no_to_tag_reads() {
    let image = format!(
        "SIP/2.0 486 Busy Here\r\nVia: {VIA}\r\nFrom: <sip:alice@example.com>;tag=a1\r\n\
         To: <sip:bob@example.com>\r\nCall-ID: stored@192.0.2.10\r\nCSeq: 1 INVITE\r\n\
         Content-Length: 0\r\n\r\n"
    );
    only_the_stored_read_takes(image.as_bytes());
    let Ok(SipMessage::Response(read)) = read_stored(image.as_bytes()) else {
        panic!("a response");
    };
    assert_eq!(read.status(), 486);
}

/// A freeze refuses what a stored read would: a status past 699 is no message,
/// so no retained final can hold one.
#[test]
fn a_status_no_reader_takes_is_never_frozen() {
    use sip_message::generators::{generate_response, GenerateResponseOpts};
    let SipMessage::Request(req) =
        CustomParser::new().parse(&bye_with("X-Fine: 1")).expect("reads")
    else {
        panic!("a request");
    };
    let opts = GenerateResponseOpts { to_tag: Some("t".into()), ..Default::default() };
    for status in [0, 99, 700, 999] {
        assert!(generate_response(&req, status, "Past", &opts).is_err(), "{status}");
    }
    let frozen = generate_response(&req, 699, "Last", &opts).expect("the last class");
    assert!(read_stored(frozen.image()).is_ok());
}

#[test]
fn what_is_no_message_is_refused() {
    let unframed = String::from_utf8(bye_with("X-Fine: 1"))
        .expect("text")
        .replace("Content-Length: 0", "Content-Length: many");
    for image in
        [&b""[..], b"\xff\xfe\x00", b"INVITE", b"SIP/2.0 200 OK\r\n\r\n", unframed.as_bytes()]
    {
        assert!(read_stored(image).is_err(), "{:?}", String::from_utf8_lossy(image));
    }
}
