//! The inputs the no-panic lane found panicking, one test per site: each
//! names the entry point and the outcome it returns instead.

use sip_message::generators::{
    generate_ack_for_2xx, generate_ack_for_2xx_from_invite, generate_ack_for_non_2xx_in_dialog,
    generate_bye_for_2xx_from_invite, generate_in_dialog_request, generate_response,
    GenerateAckFor2xxOpts, GenerateError, GenerateInDialogRequestOpts, GenerateResponseOpts,
    InDialogMethod, StackDialog,
};
use sip_message::header::{HeaderName, Via};
use sip_message::sdp_session::StatedSession;
use sip_message::{
    in_author_order, restate_session, CustomParser, SdpOrigin, SipMessage, SipParser,
    SipParserLimits, SipRequest, SipResponse, SipStr,
};

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
    for line in ["a€", "é"] {
        let origin = SdpOrigin {
            username: "-".into(),
            session_id: "1".into(),
            session_version: 1,
            nettype: "IN".into(),
            addrtype: "IP4".into(),
            unicast_address: "192.0.2.1".into(),
            raw_origin_line: line.into(),
            body_excluding_origin: String::new(),
        };
        assert_eq!(origin.value(), line);
    }
}

/// The wire reader refuses a From or To whose `<` no `>` closes: a tag a
/// response adds after it would read as part of the URI, so no response to
/// the request could carry its To-tag.
#[test]
fn an_unterminated_name_addr_is_refused_on_the_wire() {
    for (from, to) in [
        ("<sip:a@192.0.2.1>;tag=a1", "\"日本語\" <sip:jp@example.org"),
        ("<sip:a@192.0.2.1;tag=a1", "<sip:b@192.0.2.2>"),
    ] {
        let raw = format!(
            "MESSAGE sip:b@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-1\r\n\
             From: {from}\r\nTo: {to}\r\nCall-ID: c1\r\nCSeq: 1 MESSAGE\r\n\
             Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        );
        let read = CustomParser::new().parse(raw.as_bytes());
        assert!(read.is_err(), "From {from:?} / To {to:?} read as {read:?}");
    }
}

/// A leniently read request whose `To` line no response can echo under a
/// stack tag draws a refusal from the response generator, the same for a
/// request whose `To` reads with no address at all.
#[test]
fn a_response_to_an_unechoable_to_is_refused() {
    let lenient = SipParserLimits { wire_grammar: false, ..SipParserLimits::default() };
    let parser = CustomParser::with_limits(lenient);
    for to in ["sipuser@example.com;tag=bd76ya", "sip:user@example.com;t<ag=bd76ya"] {
        let raw = format!(
            "BYE sip:bob@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@192.0.2.1>;tag=a1\r\nTo: {to}\r\nCall-ID: c1\r\nCSeq: 2 BYE\r\n\
             Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        );
        let Ok(SipMessage::Request(req)) = parser.parse(raw.as_bytes()) else {
            panic!("the lenient reader takes {to:?}");
        };
        let opts = GenerateResponseOpts { to_tag: Some("b2".into()), ..Default::default() };
        let answered = generate_response(&req, 180, "Ringing", &opts);
        assert!(answered.is_err(), "{to:?} drew {answered:?}");
    }
}

fn read(raw: &str) -> SipMessage {
    CustomParser::new().parse(raw.as_bytes()).expect("the message reads")
}

/// The Request-URI of a built request, which must freeze.
fn request_uri(label: &str, built: Result<SipRequest, GenerateError>) -> String {
    match built {
        Ok(req) => req.request_uri().to_string(),
        Err(e) => panic!("{label}: {e}"),
    }
}

/// A remote target whose Contact carries escaped headers (RFC 3261 §19.1.1
/// allows them in a Contact) becomes every in-dialog Request-URI, and the
/// Route of a strict route set, without them: they are never a target's.
#[test]
fn a_contact_with_escaped_headers_targets_requests_without_them() {
    let target = "sip:b@192.0.2.2:5070;transport=udp";
    let headed = format!("{target}?Subject=x");
    let invite = |to_tag: &str, route: &str| {
        let SipMessage::Request(r) = read(&format!(
            "INVITE sip:b@192.0.2.2:5070 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-1\r\n\
             {route}From: <sip:a@192.0.2.1>;tag=a1\r\nTo: <sip:b@192.0.2.2>{to_tag}\r\n\
             Call-ID: c1\r\nCSeq: 1 INVITE\r\nContact: <sip:a@192.0.2.1:5060>\r\n\
             Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        )) else {
            panic!("an INVITE");
        };
        r
    };
    let SipMessage::Response(ok) = read(&format!(
        "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-1\r\n\
         From: <sip:a@192.0.2.1>;tag=a1\r\nTo: <sip:b@192.0.2.2>;tag=b1\r\nCall-ID: c1\r\n\
         CSeq: 1 INVITE\r\nContact: <{headed}>\r\nContent-Length: 0\r\n\r\n"
    )) else {
        panic!("a 200");
    };
    let ok: SipResponse = ok;
    let initial = invite("", "");
    let reinvite = invite(";tag=b1", "");
    for (label, built) in [
        ("ack-2xx", generate_ack_for_2xx_from_invite(&initial, &ok, "z9hG4bK-a", None)),
        ("ack-2xx-reinvite", generate_ack_for_2xx_from_invite(&reinvite, &ok, "z9hG4bK-a", None)),
        ("bye-2xx", generate_bye_for_2xx_from_invite(&initial, &ok, 2, "z9hG4bK-b")),
    ] {
        assert_eq!(request_uri(label, built), target, "{label}");
    }

    let via = || Some(Via::udp("192.0.2.1", 5060).with_branch(SipStr::from_static("z9hG4bK-c")));
    let dialog = |route_set: Vec<String>| StackDialog {
        call_id: "c1".into(),
        local_tag: "a1".into(),
        remote_tag: "b1".into(),
        local_uri: "sip:a@192.0.2.1".into(),
        remote_uri: "sip:b@192.0.2.2".into(),
        remote_target: headed.clone(),
        local_cseq: 1,
        route_set,
    };
    let loose = dialog(Vec::new());
    let ack_opts = GenerateAckFor2xxOpts { via: via(), cseq: Some(1), ..Default::default() };
    let bye_opts = GenerateInDialogRequestOpts { via: via(), ..Default::default() };
    let bye = generate_in_dialog_request(InDialogMethod::Bye, &loose, &bye_opts);
    for (label, built) in [
        ("ack-non-2xx-in-dialog", generate_ack_for_non_2xx_in_dialog(&loose, &ok)),
        ("ack-in-dialog", generate_ack_for_2xx(None, &loose, &ack_opts)),
        ("bye-in-dialog", bye.map(|r| r.request)),
    ] {
        assert_eq!(request_uri(label, built), target, "{label}");
    }

    let strict = dialog(vec!["<sip:p@192.0.2.9>".into()]);
    let bye = generate_in_dialog_request(InDialogMethod::Bye, &strict, &bye_opts)
        .unwrap_or_else(|e| panic!("bye-strict: {e}"))
        .request;
    let routes: Vec<String> = bye.raw_text(HeaderName::Route).map(|r| r.to_string()).collect();
    assert_eq!(routes, [format!("<{target}>")]);
}
