//! The inputs the no-panic lane found panicking, one test per site: each
//! names the entry point and the outcome it returns instead.

use sip_message::sdp_session::StatedSession;
use sip_message::{in_author_order, restate_session, SdpOrigin};

/// A description reordered for its author whose streams are all taken out
/// ends as it was written — without a final line ending — keeping the last
/// character of its session-level text, multi-byte or not.
#[test]
fn a_reordered_description_keeps_its_last_character() {
    let multibyte = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=€\nm=audio 0 RTP/AVP 0";
    let out = in_author_order(multibyte.as_bytes(), &[]).expect("a reordered description");
    assert_eq!(String::from_utf8(out).unwrap(), "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=€");

    let ascii = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=x\nm=audio 0 RTP/AVP 0";
    let out = in_author_order(ascii.as_bytes(), &[]).expect("a reordered description");
    assert_eq!(String::from_utf8(out).unwrap(), "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=x");
}

/// A restated description whose last slot carries a stream written with a
/// bare LF ends without that line ending and keeps the character before it.
#[test]
fn a_restated_description_keeps_its_last_character() {
    let stated = StatedSession::of(
        b"v=0\r\no=b 7 7 IN IP4 192.0.2.2\r\ns=-\r\nt=0 0\r\nm=video 5000 RTP/AVP 31\r\nm=audio 5002 RTP/AVP 0\r\n",
    )
    .expect("a stated session");
    let authored = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=label:€\nm=video 0 RTP/AVP 31";
    let restated = restate_session(authored.as_bytes(), &stated).expect("a restatement");
    assert_eq!(restated.slots, vec![Some(1), Some(0)]);
    let text = String::from_utf8(restated.sdp).unwrap();
    assert!(text.ends_with("m=audio 4000 RTP/AVP 0\r\na=label:€"), "{text:?}");
}

/// An origin built by hand without its `o=` prefix answers its whole line as
/// its value.
#[test]
fn an_origin_without_its_prefix_is_its_own_value() {
    let origin = SdpOrigin {
        username: "-".into(),
        session_id: "1".into(),
        session_version: 1,
        nettype: "IN".into(),
        addrtype: "IP4".into(),
        unicast_address: "192.0.2.1".into(),
        raw_origin_line: "é".into(),
        body_excluding_origin: String::new(),
    };
    assert_eq!(origin.value(), "é");
}
