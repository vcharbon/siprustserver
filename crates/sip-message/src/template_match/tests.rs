use super::*;
use crate::parser::custom::CustomParser;
use crate::parser::SipParser;
use crate::template::{MessageTemplate, TemplateHeader};

fn parse(raw: &[u8]) -> SipMessage {
    CustomParser::new().parse(raw).expect("well-formed")
}

fn captured_invite(subject: &str, body: &str) -> SipMessage {
    let raw = format!(
        "INVITE sip:b@ex SIP/2.0\r\n\
Via: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-cap\r\n\
Max-Forwards: 70\r\n\
From: <sip:a@ex>;tag=at\r\n\
To: <sip:b@ex>\r\n\
Call-ID: cid@1.1.1.1\r\n\
CSeq: 3 INVITE\r\n\
Contact: <sip:a@1.1.1.1:5060>\r\n\
Subject: {}\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {}\r\n\r\n{}",
        subject,
        body.len(),
        body,
    );
    parse(raw.as_bytes())
}

#[test]
fn full_match_passes_despite_regenerated_tier1() {
    let tmpl = MessageTemplate::from_message(&captured_invite("hi", "sdp-body"));
    let inbound = parse(
        b"INVITE sip:b@other SIP/2.0\r\n\
Via: SIP/2.0/UDP 9.9.9.9:5061;branch=z9hG4bK-fresh\r\n\
Max-Forwards: 70\r\n\
From: <sip:x@ex>;tag=freshtag\r\n\
To: <sip:b@ex>\r\n\
Call-ID: fresh@9.9.9.9\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:a@9.9.9.9:5061>\r\n\
Allow: INVITE, ACK, BYE\r\n\
Subject: hi\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 8\r\n\r\nsdp-body",
    );
    assert_eq!(tmpl.match_inbound(&inbound, &MatchOpts::default()), Ok(()));
}

#[test]
fn frozen_value_drift_is_caught_precisely() {
    let tmpl = MessageTemplate::from_message(&captured_invite("expected-subject", "b"));
    let inbound = captured_invite("changed-subject", "b");
    match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
        Err(Mismatch::Value { name, row, expected, got }) => {
            assert_eq!(name, "subject");
            assert_eq!(row, 0);
            assert_eq!(expected, "expected-subject");
            assert_eq!(got, "changed-subject");
        }
        other => panic!("expected a Value mismatch, got {other:?}"),
    }
}

#[test]
fn compact_and_full_names_match_both_directions() {
    let tmpl = MessageTemplate::response(
        200,
        "OK",
        vec![TemplateHeader::frozen("k", "replaces")],
        Vec::new(),
    );
    let inbound = parse(
        b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nSupported: replaces\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(tmpl.match_inbound(&inbound, &MatchOpts::default()), Ok(()));

    let tmpl2 = MessageTemplate::response(
        200,
        "OK",
        vec![TemplateHeader::frozen("Supported", "replaces")],
        Vec::new(),
    );
    let inbound2 = parse(
        b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nk: replaces\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(tmpl2.match_inbound(&inbound2, &MatchOpts::default()), Ok(()));
}

#[test]
fn duplicate_row_count_mismatch_is_caught() {
    let tmpl = MessageTemplate::response(
        200,
        "OK",
        vec![TemplateHeader::frozen("X-Trace", "a"), TemplateHeader::frozen("X-Trace", "b")],
        Vec::new(),
    );
    let inbound = parse(
        b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nX-Trace: a\r\nContent-Length: 0\r\n\r\n",
    );
    match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
        Err(Mismatch::RowCount { name, expected, got }) => {
            assert_eq!(name, "x-trace");
            assert_eq!(expected, 2);
            assert_eq!(got, 1);
        }
        other => panic!("expected RowCount, got {other:?}"),
    }
}

#[test]
fn body_drift_reports_first_offset() {
    let tmpl = MessageTemplate::response(
        200,
        "OK",
        vec![TemplateHeader::frozen("Content-Type", "text/plain")],
        b"hello world".to_vec(),
    );
    let inbound = parse(
        b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nContent-Type: text/plain\r\nContent-Length: 11\r\n\r\nhello WORLD",
    );
    match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
        Err(Mismatch::Body { expected_len, got_len, first_diff }) => {
            assert_eq!(expected_len, 11);
            assert_eq!(got_len, 11);
            assert_eq!(first_diff, Some(6));
        }
        other => panic!("expected Body, got {other:?}"),
    }
}

#[test]
fn bodiless_template_rejects_inbound_with_body() {
    let tmpl = MessageTemplate::response(200, "OK", vec![], Vec::new());
    let inbound = parse(
        b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\r\nxyz",
    );
    match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
        Err(Mismatch::Body { expected_len, got_len, .. }) => {
            assert_eq!(expected_len, 0);
            assert_eq!(got_len, 3);
        }
        other => panic!("expected Body, got {other:?}"),
    }
}

#[test]
fn start_line_status_and_reason_mismatch() {
    // Status mismatch (the pilot 400-vs-200 shape).
    let tmpl = MessageTemplate::response(400, "Bad Request", vec![], Vec::new());
    let got_200 = parse(
        b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 2 UPDATE\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(
        tmpl.match_inbound(&got_200, &MatchOpts::default()),
        Err(Mismatch::StartLine { expected: "400".into(), got: "200".into() }),
    );

    // Reason-phrase mismatch — the distinctive defect signature.
    let tmpl2 =
        MessageTemplate::response(400, "Bad Request - Invalid Dialog State", vec![], Vec::new());
    let got_plain_400 = parse(
        b"SIP/2.0 400 Bad Request\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 2 UPDATE\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(
        tmpl2.match_inbound(&got_plain_400, &MatchOpts::default()),
        Err(Mismatch::ReasonPhrase {
            expected: "Bad Request - Invalid Dialog State".into(),
            got: "Bad Request".into(),
        }),
    );
}

#[test]
fn missing_content_length_is_tolerated() {
    // A template with Content-Length matched against an inbound WITHOUT one
    // (RFC 3261 §20.14 legal) passes — Content-Length presence is not required.
    let tmpl = MessageTemplate::response(
        200,
        "OK",
        vec![TemplateHeader::frozen("X-Note", "n")],
        Vec::new(),
    );
    let inbound = parse(
        b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nX-Note: n\r\n\r\n",
    );
    assert_eq!(tmpl.match_inbound(&inbound, &MatchOpts::default()), Ok(()));
}

// --- remote-target: user part + params (host:port ignored) -----------------

fn response_with_contact(contact: &str) -> SipMessage {
    let raw = format!(
        "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nContact: {contact}\r\nContent-Length: 0\r\n\r\n"
    );
    parse(raw.as_bytes())
}

#[test]
fn remote_target_hostport_ignored_but_user_compared() {
    // Same user + params, DIFFERENT host:port → match (host:port ignored).
    let tmpl =
        MessageTemplate::from_message(&response_with_contact("<sip:bob@1.1.1.1:5060;user=phone>"));
    let diff_hostport = response_with_contact("<sip:bob@9.9.9.9:5070;user=phone>");
    assert_eq!(tmpl.match_inbound(&diff_hostport, &MatchOpts::default()), Ok(()));

    // Different USER part → caught precisely.
    let diff_user = response_with_contact("<sip:totally-different@9.9.9.9:5070;user=phone>");
    match tmpl.match_inbound(&diff_user, &MatchOpts::default()) {
        Err(Mismatch::UserPart { name, expected, got, .. }) => {
            assert_eq!(name, "contact");
            assert_eq!(expected, Some("bob".to_string()));
            assert_eq!(got, Some("totally-different".to_string()));
        }
        other => panic!("expected a UserPart mismatch, got {other:?}"),
    }
}

#[test]
fn remote_target_added_param_tolerated_only_when_ignored() {
    let tmpl =
        MessageTemplate::from_message(&response_with_contact("<sip:b@1.1.1.1:5060;user=phone>"));
    let inbound = response_with_contact("<sip:b@1.1.1.1:5060;user=phone;primary=lb1>");
    match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
        Err(Mismatch::Param { name, kind, param, expected, got, .. }) => {
            assert_eq!(name, "contact");
            assert_eq!(kind, "uri");
            assert_eq!(param, "primary");
            assert_eq!(expected, None);
            assert_eq!(got, Some("lb1".to_string()));
        }
        other => panic!("expected a Param mismatch, got {other:?}"),
    }
    assert_eq!(tmpl.match_inbound(&inbound, &MatchOpts::ignoring(["primary", "backup"])), Ok(()),);
}

#[test]
fn remote_target_non_ignored_param_drift_fails_with_detail() {
    let tmpl =
        MessageTemplate::from_message(&response_with_contact("<sip:b@1.1.1.1:5060;user=phone>"));
    let inbound = response_with_contact("<sip:b@1.1.1.1:5060;user=isdn>");
    match tmpl.match_inbound(&inbound, &MatchOpts::ignoring(["primary", "backup"])) {
        Err(Mismatch::Param { name, param, expected, got, .. }) => {
            assert_eq!(name, "contact");
            assert_eq!(param, "user");
            assert_eq!(expected, Some("phone".to_string()));
            assert_eq!(got, Some("isdn".to_string()));
        }
        other => panic!("expected a Param mismatch, got {other:?}"),
    }
}

#[test]
fn remote_target_param_placement_move_fails() {
    // `expires` is a URI param on the template but a header param on the
    // inbound — the two sets compare separately, so this fails.
    let tmpl = MessageTemplate::from_message(&response_with_contact("<sip:b@h:5060;expires=60>"));
    let moved = response_with_contact("<sip:b@h:5060>;expires=60");
    match tmpl.match_inbound(&moved, &MatchOpts::default()) {
        Err(Mismatch::Param { kind, param, .. }) => {
            assert_eq!(kind, "uri");
            assert_eq!(param, "expires");
        }
        other => panic!("expected a Param mismatch (placement move), got {other:?}"),
    }
}

#[test]
fn frozen_contact_override_is_byte_compared() {
    // An explicitly FROZEN Contact is byte-compared whole (host:port too).
    let tmpl = MessageTemplate::response(
        200,
        "OK",
        vec![TemplateHeader::frozen("Contact", "<sip:b@1.1.1.1:5060>")],
        Vec::new(),
    );
    let same = response_with_contact("<sip:b@1.1.1.1:5060>");
    assert_eq!(tmpl.match_inbound(&same, &MatchOpts::default()), Ok(()));
    let diff = response_with_contact("<sip:b@9.9.9.9:5070>");
    assert!(matches!(
        tmpl.match_inbound(&diff, &MatchOpts::default()),
        Err(Mismatch::Value { .. })
    ));
}
