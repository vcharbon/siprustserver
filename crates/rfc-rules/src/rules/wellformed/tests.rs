//! The family's OWN semantics: which message is an occasion, what the bytes
//! settle, and what a vantage carrying less than the datagram can say.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::*;

const UA: &str = "127.0.0.1:5060";
const PEER: &str = "127.0.0.1:5070";

/// A request the UA SENT, with `extra` header rows and `body` bytes; the
/// Content-Length row is written by the caller through `extra`.
fn request(method: &str, extra: &str, body: &[u8]) -> Msg {
    let mut text = format!(
        "{method} sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@h>;tag=at\r\n\
             To: <sip:bob@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 {method}\r\n"
    );
    if !extra.is_empty() {
        text.push_str(extra);
        text.push_str("\r\n");
    }
    text.push_str("\r\n");
    let mut head = text.into_bytes();
    head.extend_from_slice(body);
    Msg {
        at_us: 1_000,
        src: UA.to_string(),
        dst: PEER.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: method.to_string() },
        call_id: "c1".to_string(),
        cseq: 1,
        cseq_method: method.to_string(),
        via_branch: Some("z9hG4bK-1".to_string()),
        from_tag: Some("at".to_string()),
        to_tag: None,
        head: Some(head),
        body: Some(body.to_vec()),
    }
}

/// A response the UA SENT, `to_tag` omitted where `None`.
fn response(status: u16, to_tag: Option<&str>) -> Msg {
    let to = match to_tag {
        Some(t) => format!("<sip:bob@h>;tag={t}"),
        None => "<sip:bob@h>".to_string(),
    };
    let head = format!(
        "SIP/2.0 {status} Ringing\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@h>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
    );
    let mut m = request("INVITE", "Content-Length: 0", b"");
    m.kind = Kind::Response { status };
    m.to_tag = to_tag.map(str::to_string);
    m.head = Some(head.into_bytes());
    m.body = Some(Vec::new());
    m
}

fn obs(msgs: &[Msg]) -> Observation {
    let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
    let mut last_us = 0;
    for m in msgs {
        last_us = last_us.max(m.at_us);
        for ep in [&m.src, &m.dst] {
            let at = endpoint_last_us.entry(ep.clone()).or_default();
            *at = (*at).max(m.at_us);
        }
    }
    Observation { last_us, endpoint_last_us, closed: true }
}

fn run(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    rule.eval(&WireView { msgs, obs: &obs(msgs) })
}

/// The only occasion of the whole family is a message the endpoint EMITTED
/// fresh: a retransmission is the same act again.
#[test]
fn a_retransmission_opens_no_occasion() {
    let mut again = request("INVITE", "Content-Length: 0", b"");
    again.repeat = true;
    assert!(run(&BranchPrefix, &[again]).is_empty());
}

// ── branch-prefix ───────────────────────────────────────────────────────

#[test]
fn a_cookie_prefixed_branch_is_compliant() {
    let f = run(&BranchPrefix, &[request("INVITE", "Content-Length: 0", b"")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UA, "the sender is charged");
}

#[test]
fn a_legacy_branch_is_violated() {
    let mut m = request("INVITE", "Content-Length: 0", b"");
    m.head = Some(
        String::from_utf8(m.head.take().unwrap())
            .unwrap()
            .replace("z9hG4bK-1", "legacy-2543")
            .into_bytes(),
    );
    let f = run(&BranchPrefix, &[m]);
    let Decision::Violated(Evidence::HeaderValueRejected { value, expected, .. }) = &f[0].decision
    else {
        panic!("branch evidence: {:?}", f[0].decision)
    };
    assert_eq!(value, "legacy-2543");
    assert!(expected.contains("z9hG4bK"), "{expected}");
}

#[test]
fn a_branchless_request_is_violated_as_an_absence() {
    let mut m = request("INVITE", "Content-Length: 0", b"");
    m.head = Some(
        String::from_utf8(m.head.take().unwrap())
            .unwrap()
            .replace(";branch=z9hG4bK-1", "")
            .into_bytes(),
    );
    let f = run(&BranchPrefix, &[m]);
    assert!(
        matches!(
            &f[0].decision,
            Decision::Violated(Evidence::RequiredHeaderAbsent { header, .. })
                if header == "Via;branch"
        ),
        "{:?}",
        f[0].decision
    );
}

/// A vantage with no bytes cannot say what the request stated: the occasion
/// stands, UNDECIDED.
#[test]
fn a_request_without_header_bytes_is_undecidable() {
    let mut m = request("INVITE", "Content-Length: 0", b"");
    m.head = None;
    let f = run(&BranchPrefix, &[m]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(!f[0].decided(), "{:?}", f[0].decision);
}

// ── max-forwards ────────────────────────────────────────────────────────

#[test]
fn max_forwards_at_seventy_is_compliant() {
    let f = run(&MaxForwards, &[request("INVITE", "Max-Forwards: 70\r\nContent-Length: 0", b"")]);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

#[test]
fn a_request_without_max_forwards_is_violated() {
    let f = run(&MaxForwards, &[request("INVITE", "Content-Length: 0", b"")]);
    assert!(
        matches!(
            &f[0].decision,
            Decision::Violated(Evidence::RequiredHeaderAbsent { header, .. })
                if header == "Max-Forwards"
        ),
        "{:?}",
        f[0].decision
    );
}

/// Above 70 the count was minted, not decremented.
#[test]
fn max_forwards_above_seventy_is_violated() {
    let f = run(&MaxForwards, &[request("INVITE", "Max-Forwards: 200\r\nContent-Length: 0", b"")]);
    let Decision::Violated(Evidence::HeaderValueRejected { value, expected, .. }) = &f[0].decision
    else {
        panic!("max-forwards evidence: {:?}", f[0].decision)
    };
    assert_eq!((value.as_str(), expected.as_str()), ("200", "at most 70"));
}

#[test]
fn an_unreadable_max_forwards_is_violated() {
    let f = run(&MaxForwards, &[request("INVITE", "Max-Forwards: many\r\nContent-Length: 0", b"")]);
    let Decision::Violated(Evidence::HeaderValueRejected { value, expected, .. }) = &f[0].decision
    else {
        panic!("max-forwards evidence: {:?}", f[0].decision)
    };
    assert_eq!(value, "many");
    assert!(expected.contains("0..=255"), "{expected}");
}

// ── content-length ──────────────────────────────────────────────────────

#[test]
fn a_matching_content_length_is_compliant() {
    let f = run(&ContentLength, &[request("INVITE", "Content-Length: 5", b"v=0\r\n")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

#[test]
fn a_length_above_the_bytes_is_violated() {
    let f = run(&ContentLength, &[request("INVITE", "Content-Length: 99", b"v=0\r\n")]);
    let Decision::Violated(Evidence::HeaderValueRejected { value, expected, .. }) = &f[0].decision
    else {
        panic!("content-length evidence: {:?}", f[0].decision)
    };
    assert_eq!((value.as_str(), expected.as_str()), ("99", "5"));
}

/// The count is RAW BYTES: a non-UTF-8 body measured as decoded text would
/// inflate every byte into a replacement character and flag a faithful
/// message.
#[test]
fn a_binary_body_is_counted_in_bytes() {
    let body: [u8; 8] = [0xFF, 0xFE, 0x00, 0x80, 0xC0, 0x01, 0x02, 0x03];
    let f = run(&ContentLength, &[request("INFO", "Content-Length: 8", &body)]);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

    let f = run(&ContentLength, &[request("INFO", "Content-Length: 5", &body)]);
    let Decision::Violated(Evidence::HeaderValueRejected { expected, .. }) = &f[0].decision else {
        panic!("content-length evidence: {:?}", f[0].decision)
    };
    assert_eq!(expected, "8", "the raw byte count, not the decoded one");
}

/// A message declaring no length claims nothing to check.
#[test]
fn a_message_declaring_no_length_is_no_occasion() {
    assert!(run(&ContentLength, &[request("INVITE", "Subject: x", b"")]).is_empty());
}

/// A vantage that kept the head and the body apart already cut the body to
/// the declared length, so it can only agree with itself.
#[test]
fn a_split_head_and_body_vantage_is_undecidable() {
    let mut m = request("INVITE", "Content-Length: 5", b"");
    m.body = Some(b"v=0\r\n".to_vec());
    let f = run(&ContentLength, &[m]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(!f[0].decided(), "{:?}", f[0].decision);
}

// ── content-type ────────────────────────────────────────────────────────

#[test]
fn a_body_with_content_type_is_compliant() {
    let f = run(
        &ContentType,
        &[request("INVITE", "Content-Type: application/sdp\r\nContent-Length: 5", b"v=0\r\n")],
    );
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

#[test]
fn a_body_without_content_type_is_violated() {
    let f = run(&ContentType, &[request("INVITE", "Content-Length: 5", b"v=0\r\n")]);
    assert!(
        matches!(
            &f[0].decision,
            Decision::Violated(Evidence::RequiredHeaderAbsent { header, .. })
                if header == "Content-Type"
        ),
        "{:?}",
        f[0].decision
    );
}

/// A message carrying no body owes no Content-Type — no obligation arises.
#[test]
fn a_bodiless_message_is_no_occasion() {
    assert!(run(&ContentType, &[request("INVITE", "Content-Length: 0", b"")]).is_empty());
}

// ── contact-presence / no-contact-on-bye ────────────────────────────────

#[test]
fn an_invite_with_contact_is_compliant_and_one_without_is_violated() {
    let with = request("INVITE", "Contact: <sip:a@h>\r\nContent-Length: 0", b"");
    assert!(matches!(run(&ContactPresence, &[with])[0].decision, Decision::Compliant));
    let f = run(&ContactPresence, &[request("INVITE", "Content-Length: 0", b"")]);
    assert!(
        matches!(
            &f[0].decision,
            Decision::Violated(Evidence::RequiredHeaderAbsent { header, .. })
                if header == "Contact"
        ),
        "{:?}",
        f[0].decision
    );
}

/// Only a dialog-ESTABLISHING method owes a Contact.
#[test]
fn a_bye_is_no_contact_presence_occasion() {
    assert!(run(&ContactPresence, &[request("BYE", "Content-Length: 0", b"")]).is_empty());
}

#[test]
fn a_bye_carrying_contact_is_violated() {
    let f = run(&NoContactOnBye, &[request("BYE", "Contact: <sip:a@h>\r\nContent-Length: 0", b"")]);
    let Decision::Violated(Evidence::ForbiddenHeaderPresent { header, value, .. }) = &f[0].decision
    else {
        panic!("bye-contact evidence: {:?}", f[0].decision)
    };
    assert_eq!((header.as_str(), value.as_str()), ("Contact", "<sip:a@h>"));
    let clean = run(&NoContactOnBye, &[request("BYE", "Content-Length: 0", b"")]);
    assert!(matches!(clean[0].decision, Decision::Compliant), "{:?}", clean[0].decision);
}

// ── to-tag-presence ─────────────────────────────────────────────────────

#[test]
fn a_tagged_response_above_100_is_compliant() {
    let f = run(&ToTagPresence, &[response(180, Some("bt"))]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

#[test]
fn an_untagged_response_above_100_is_violated() {
    let f = run(&ToTagPresence, &[response(180, None)]);
    assert!(
        matches!(
            &f[0].decision,
            Decision::Violated(Evidence::RequiredHeaderAbsent { header, on, .. })
                if header == "To;tag" && on == "180"
        ),
        "{:?}",
        f[0].decision
    );
}

/// `100 Trying` names no dialog, so it owes no tag — and a request is no
/// occasion at all.
#[test]
fn a_100_and_a_request_are_no_occasion() {
    assert!(run(&ToTagPresence, &[response(100, None)]).is_empty());
    assert!(run(&ToTagPresence, &[request("INVITE", "Content-Length: 0", b"")]).is_empty());
}

// ── from-tag-presence ───────────────────────────────────────────────────

/// The request with `from` as its From row.
fn request_from(from: &str) -> Msg {
    let head = format!(
        "INFO sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: {from}\r\n\
             To: <sip:bob@h>;tag=bt\r\n\
             Call-ID: c1\r\n\
             CSeq: 2 INFO\r\n\
             Content-Length: 0\r\n\r\n"
    );
    let mut m = request("INFO", "Content-Length: 0", b"");
    m.head = Some(head.into_bytes());
    m
}

#[test]
fn a_request_with_a_from_tag_is_compliant() {
    let f = run(&FromTagPresence, &[request_from("<sip:alice@h>;tag=at")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// No tag and an empty tag both leave the request naming no dialog.
#[test]
fn a_request_without_a_from_tag_or_with_an_empty_one_is_violated() {
    for from in ["<sip:alice@h>", "<sip:alice@h>;tag="] {
        let f = run(&FromTagPresence, &[request_from(from)]);
        assert!(
            matches!(
                &f[0].decision,
                Decision::Violated(Evidence::RequiredHeaderAbsent { header, on, .. })
                    if header == "From;tag" && on == "INFO"
            ),
            "{from}: {:?}",
            f[0].decision
        );
    }
}

#[test]
fn a_response_is_no_from_tag_occasion() {
    assert!(run(&FromTagPresence, &[response(180, Some("bt"))]).is_empty());
}

// ── no-record-route-from-ua ─────────────────────────────────────────────

#[test]
fn a_marked_record_route_is_violated() {
    let f = run(
        &NoRecordRouteFromUa,
        &[request("INVITE", "Record-Route: <sip:b2b@h;callRef=7>\r\nContent-Length: 0", b"")],
    );
    let Decision::Violated(Evidence::ForbiddenHeaderPresent { header, value, .. }) = &f[0].decision
    else {
        panic!("record-route evidence: {:?}", f[0].decision)
    };
    assert_eq!(header, "Record-Route");
    assert!(value.contains("callRef=7"), "{value}");
}

/// A proxy's own Record-Route carries no B2BUA leg marker and is its right.
#[test]
fn an_unmarked_record_route_is_compliant() {
    let f = run(
        &NoRecordRouteFromUa,
        &[request("INVITE", "Record-Route: <sip:p@h;lr>\r\nContent-Length: 0", b"")],
    );
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

// ── cancel-cseq-method ──────────────────────────────────────────────────

/// A CANCEL whose CSeq token names the cancelled method, built past field
/// extraction — the only way the state is reachable at all.
#[test]
fn a_cancel_whose_cseq_names_another_method_is_violated() {
    let mut m = request("CANCEL", "Content-Length: 0", b"");
    m.cseq_method = "INVITE".to_string();
    let f = run(&CancelCseqMethod, &[m]);
    let Decision::Violated(Evidence::HeaderValueRejected { header, value, expected, .. }) =
        &f[0].decision
    else {
        panic!("cancel-cseq evidence: {:?}", f[0].decision)
    };
    assert_eq!(
        (header.as_str(), value.as_str(), expected.as_str()),
        ("CSeq;method", "INVITE", "CANCEL")
    );
}

/// Both tokens are wire-model facts, so a byte-less vantage still decides.
#[test]
fn a_well_formed_cancel_is_compliant_even_without_bytes() {
    let mut m = request("CANCEL", "Content-Length: 0", b"");
    m.head = None;
    let f = run(&CancelCseqMethod, &[m]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert!(run(&CancelCseqMethod, &[request("BYE", "Content-Length: 0", b"")]).is_empty());
}

// ── no-100rel-require-on-non-invite ─────────────────────────────────────

#[test]
fn a_non_invite_requiring_100rel_is_violated_and_an_invite_is_no_occasion() {
    let f = run(
        &No100relRequireOnNonInvite,
        &[request("BYE", "Require: 100rel\r\nContent-Length: 0", b"")],
    );
    let Decision::Violated(Evidence::ForbiddenHeaderPresent { on, header, value, .. }) =
        &f[0].decision
    else {
        panic!("100rel evidence: {:?}", f[0].decision)
    };
    assert_eq!((on.as_str(), header.as_str(), value.as_str()), ("BYE", "Require", "100rel"));

    let invite = request("INVITE", "Require: 100rel\r\nContent-Length: 0", b"");
    assert!(run(&No100relRequireOnNonInvite, &[invite]).is_empty());
    let plain = run(&No100relRequireOnNonInvite, &[request("BYE", "Content-Length: 0", b"")]);
    assert!(matches!(plain[0].decision, Decision::Compliant), "{:?}", plain[0].decision);
}

/// `Proxy-Require` states a hop-by-hop demand §4 does not name.
#[test]
fn a_proxy_require_100rel_is_not_this_rules_offence() {
    let f = run(
        &No100relRequireOnNonInvite,
        &[request("BYE", "Proxy-Require: 100rel\r\nContent-Length: 0", b"")],
    );
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

// ── reliable-1xx-headers ────────────────────────────────────────────────

/// A 1xx the UAS sent, with `extra` rows spelled by the caller.
fn provisional(status: u16, extra: &str) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@h>;tag=at\r\n\
             To: <sip:bob@h>;tag=bt\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n"
    );
    let mut m = response(status, Some("bt"));
    m.head = Some(head.into_bytes());
    m
}

#[test]
fn a_plain_100_and_a_reliable_183_with_a_good_rseq_are_compliant() {
    for m in [provisional(100, ""), provisional(183, "Require: 100rel\r\nRSeq: 1\r\n")] {
        let f = run(&Reliable1xxHeaders, &[m]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }
}

/// A 100 made reliable by BOTH markers is ONE finding naming both.
#[test]
fn a_reliable_100_is_one_finding_naming_every_marker() {
    let f = run(&Reliable1xxHeaders, &[provisional(100, "RSeq: 1\r\nRequire: 100rel\r\n")]);
    assert_eq!(f.len(), 1, "one act, one finding: {f:?}");
    let Decision::Violated(Evidence::Reliable1xx { status, carried, .. }) = &f[0].decision else {
        panic!("reliable-1xx evidence: {:?}", f[0].decision)
    };
    assert_eq!(*status, 100);
    assert_eq!(carried.as_slice(), ["RSeq: 1", "Require: 100rel"]);
}

#[test]
fn a_reliable_provisional_without_an_rseq_is_violated() {
    let f = run(&Reliable1xxHeaders, &[provisional(183, "Require: 100rel\r\n")]);
    let Decision::Violated(Evidence::Reliable1xx { carried, admits, .. }) = &f[0].decision else {
        panic!("reliable-1xx evidence: {:?}", f[0].decision)
    };
    assert_eq!(carried.as_slice(), ["Require: 100rel"]);
    assert!(admits.contains("RSeq"), "{admits}");
}

/// Zero, and a row no reader accepts, are both out of the `1..=2^31-1`
/// space — presence and readability asked separately.
#[test]
fn an_out_of_range_or_unreadable_rseq_is_violated() {
    for extra in ["Require: 100rel\r\nRSeq: 0\r\n", "Require: 100rel\r\nRSeq: many\r\n"] {
        let f = run(&Reliable1xxHeaders, &[provisional(183, extra)]);
        let Decision::Violated(Evidence::Reliable1xx { admits, .. }) = &f[0].decision else {
            panic!("reliable-1xx evidence: {:?}", f[0].decision)
        };
        assert!(admits.contains("2^31-1"), "{admits}");
    }
}

/// An UNreliable 18x owes no RSeq, and a final is no occasion at all.
#[test]
fn a_plain_180_is_compliant_and_a_final_is_no_occasion() {
    let f = run(&Reliable1xxHeaders, &[provisional(180, "")]);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert!(run(&Reliable1xxHeaders, &[response(200, Some("bt"))]).is_empty());
}
