//! The family's OWN semantics under a CLOSED observation: which INVITEs are
//! re-offers, what one omission costs, and which answer discharges a §8.2
//! rejection.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{
    AllowSupportedOnInvite, NoRequireOnCancelOrAck, OptionsResponseEchoes, Unsupported415Accepts,
    UnsupportedExtension420, UnsupportedExtension421, UnsupportedMethod405Allow,
};

mod relayed;

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
/// A second socket of bob's, the one it answers from.
const BOB_SENDS: &str = "127.0.0.1:41000";

/// An INVITE alice sent bob, with a caller-chosen capability block.
fn invite(at_us: u64, branch: &str, cseq: u32, to_tag: Option<&str>, extra: &str) -> Msg {
    let to = match to_tag {
        Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
        None => "<sip:bob@127.0.0.1>".to_string(),
    };
    let head = format!(
        "INVITE sip:bob@127.0.0.1 SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:fixture@192.0.2.99>\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} INVITE\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    );
    Msg {
        refused: false,
        at_us,
        src: ALICE.to_string(),
        dst: BOB.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: "INVITE".to_string() },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: "INVITE".to_string(),
        from_tag: Some("at".to_string()),
        to_tag: to_tag.map(str::to_string),
        via_branch: Some(branch.to_string()),
        head: Some(head.into_bytes()),
        body: None,
    }
}

/// A 2xx bob sent alice, with a caller-chosen capability block.
fn ok(at_us: u64, cseq: u32, extra: &str) -> Msg {
    let head = format!(
        "SIP/2.0 200 OK\r\nContact: <sip:fixture@192.0.2.98>\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} INVITE\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    );
    Msg {
        refused: false,
        at_us,
        src: BOB.to_string(),
        dst: ALICE.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status: 200 },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: "INVITE".to_string(),
        from_tag: Some("at".to_string()),
        to_tag: Some("bt".to_string()),
        via_branch: Some("z9hG4bK-i".to_string()),
        head: Some(head.into_bytes()),
        body: None,
    }
}

const BOTH: &str = "Allow: INVITE, ACK, BYE\r\nSupported: 100rel\r\n";

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

fn eval(msgs: &[Msg]) -> Vec<Finding> {
    AllowSupportedOnInvite.eval(&WireView { msgs, obs: &obs(msgs) })
}

fn hits(msgs: &[Msg]) -> Vec<Finding> {
    eval(msgs).into_iter().filter(Finding::violated).collect()
}

/// A re-INVITE that advertises both is the obligation met.
#[test]
fn a_re_invite_advertising_both_is_compliant() {
    let msgs =
        [invite(1_000, "z9hG4bK-i", 1, None, ""), invite(2_000, "z9hG4bK-r", 2, Some("bt"), BOTH)];
    let all = eval(&msgs);
    assert_eq!(all.len(), 1, "the initial INVITE is exempt: {all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
}

/// Both headers missing is ONE occasion naming both — the collapse.
#[test]
fn a_re_invite_advertising_neither_is_one_finding() {
    let msgs =
        [invite(1_000, "z9hG4bK-i", 1, None, ""), invite(2_000, "z9hG4bK-r", 2, Some("bt"), "")];
    let f = hits(&msgs);
    assert_eq!(f.len(), 1, "one occasion, both headers on it: {f:?}");
    assert_eq!(f[0].emitter, ALICE, "the endpoint that sent it is charged");
    assert_eq!(f[0].taker, BOB, "read at the endpoint that needed it");
    let Decision::Violated(Evidence::CapabilitiesNotAdvertised { missing, status, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(missing.as_slice(), ["Allow".to_string(), "Supported".to_string()]);
    assert_eq!(*status, 0, "0 marks the re-INVITE");
}

/// One header missing names only that one.
#[test]
fn one_missing_header_is_named_alone() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", 1, None, ""),
        invite(2_000, "z9hG4bK-r", 2, Some("bt"), "Allow: INVITE, ACK\r\n"),
    ];
    let f = hits(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::CapabilitiesNotAdvertised { missing, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(missing.as_slice(), ["Supported".to_string()]);
}

/// A 2xx to an INVITE is judged the same way, and its status is on the
/// finding.
#[test]
fn an_invite_2xx_advertising_neither_is_violated() {
    let msgs = [invite(1_000, "z9hG4bK-i", 1, None, ""), ok(2_000, 1, "")];
    let f = hits(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, BOB);
    let Decision::Violated(Evidence::CapabilitiesNotAdvertised { status, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*status, 200);
}

/// A UAS that answers from another socket than the one it took the initial
/// INVITE at takes the re-offer at that second socket: a re-offer still.
#[test]
fn a_re_invite_to_a_two_socket_uas_s_other_socket_is_a_re_offer() {
    let mut answer = ok(2_000, 1, BOTH);
    answer.src = BOB_SENDS.to_string();
    let mut re_offer = invite(3_000, "z9hG4bK-r", 2, Some("bt"), "");
    re_offer.dst = BOB_SENDS.to_string();
    let msgs = [invite(1_000, "z9hG4bK-i", 1, None, BOTH), answer, re_offer];
    let f = hits(&msgs);
    assert_eq!(f.len(), 1, "{:?}", eval(&msgs));
    assert_eq!((f[0].emitter.as_str(), f[0].taker.as_str()), (ALICE, BOB_SENDS));
}

/// A §17.2.3 retransmission of the initial INVITE repeats its branch: it is
/// that INVITE again, not a re-offer.
#[test]
fn a_retransmitted_initial_invite_is_exempt() {
    let msgs = [invite(1_000, "z9hG4bK-i", 1, None, ""), invite(2_000, "z9hG4bK-i", 1, None, "")];
    assert!(eval(&msgs).is_empty(), "{:?}", eval(&msgs));
}

/// A §22.2 auth retry sends a SECOND To-tag-less INVITE on the same call:
/// another attempt at establishing the dialog, never a re-offer.
#[test]
fn an_auth_retry_invite_is_exempt() {
    let msgs = [invite(1_000, "z9hG4bK-i1", 1, None, ""), invite(2_000, "z9hG4bK-i2", 2, None, "")];
    assert!(eval(&msgs).is_empty(), "{:?}", eval(&msgs));
}

/// The compact `k` spelling IS the Supported header (§7.3.3).
#[test]
fn the_compact_supported_spelling_counts() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", 1, None, ""),
        invite(2_000, "z9hG4bK-r", 2, Some("bt"), "Allow: INVITE\r\nk: 100rel\r\n"),
    ];
    assert!(hits(&msgs).is_empty(), "{:?}", hits(&msgs));
}

// ---- the §8.2 / §11.2 rejection and capability answers ----------------

/// A request alice sent bob — the side bob's rejection obligations run on.
fn taken(at_us: u64, method: &str, branch: &str, cseq: u32, extra: &str) -> Msg {
    let head = format!(
        "{method} sip:bob@127.0.0.1 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    );
    Msg {
        refused: false,
        at_us,
        src: ALICE.to_string(),
        dst: BOB.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: method.to_string() },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: method.to_string(),
        from_tag: Some("at".to_string()),
        to_tag: None,
        via_branch: Some(branch.to_string()),
        head: Some(head.into_bytes()),
        body: None,
    }
}

/// The response bob sent back on that transaction.
fn answered(at_us: u64, status: u16, method: &str, branch: &str, cseq: u32, extra: &str) -> Msg {
    let head = format!(
        "SIP/2.0 {status} Response\r\nContact: <sip:fixture@192.0.2.98>\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    );
    Msg {
        refused: false,
        at_us,
        src: BOB.to_string(),
        dst: ALICE.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: method.to_string(),
        from_tag: Some("at".to_string()),
        to_tag: Some("bt".to_string()),
        via_branch: Some(branch.to_string()),
        head: Some(head.into_bytes()),
        body: None,
    }
}

fn run(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    crate::registry::judged_alone(rule, &WireView { msgs, obs: &obs(msgs) })
}

fn violations(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    run(rule, msgs).into_iter().filter(Finding::violated).collect()
}

/// A 405 listing what it does accept meets §8.2.1.
#[test]
fn a_405_with_allow_discharges_the_unrecognised_method() {
    let msgs = [
        taken(1_000, "FROBNICATE", "z9hG4bK-x", 1, ""),
        answered(2_000, 405, "FROBNICATE", "z9hG4bK-x", 1, "Allow: INVITE, BYE\r\n"),
    ];
    let all = run(&UnsupportedMethod405Allow, &msgs);
    assert_eq!(all.len(), 1, "one occasion: {all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
    assert_eq!(all[0].emitter, BOB, "the endpoint that took the verb owes the 405");
    assert_eq!(all[0].taker, ALICE);
}

/// Serving the unknown verb instead is the miss, and the finding names what
/// was answered.
#[test]
fn serving_an_unrecognised_method_is_violated() {
    let msgs = [
        taken(1_000, "FROBNICATE", "z9hG4bK-x", 1, ""),
        answered(2_000, 200, "FROBNICATE", "z9hG4bK-x", 1, ""),
    ];
    let f = violations(&UnsupportedMethod405Allow, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::RejectionNotIssued {
        method,
        answered_status,
        listed_rows,
        unsupported_tags,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(method, "FROBNICATE");
    assert_eq!((*answered_status, *listed_rows), (200, 0));
    assert!(unsupported_tags.is_empty(), "the METHOD is unrecognised, not an extension");
}

/// A UAS that answers from another socket than the one it took the request
/// at still answered its own transaction.
#[test]
fn a_405_from_a_second_uas_socket_discharges_the_unrecognised_method() {
    let mut reply = answered(2_000, 405, "FROBNICATE", "z9hG4bK-x", 1, "Allow: INVITE, BYE\r\n");
    reply.src = BOB_SENDS.to_string();
    let msgs = [taken(1_000, "FROBNICATE", "z9hG4bK-x", 1, ""), reply];
    let all = run(&UnsupportedMethod405Allow, &msgs);
    assert_eq!(all.len(), 1, "one occasion: {all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
    assert_eq!(all[0].emitter, BOB, "the socket that took the verb owes the 405");
}

/// A 405 carrying no Allow states no alternative — still the miss.
#[test]
fn a_405_without_allow_is_violated() {
    let msgs = [
        taken(1_000, "FROBNICATE", "z9hG4bK-x", 1, ""),
        answered(2_000, 405, "FROBNICATE", "z9hG4bK-x", 1, ""),
    ];
    assert_eq!(violations(&UnsupportedMethod405Allow, &msgs).len(), 1);
}

/// A 100 Trying answers nothing about the rejection: the FIRST FINAL is the
/// answer, so a UAS that tries and then rejects correctly is clean.
#[test]
fn a_provisional_is_not_the_answer() {
    let msgs = [
        taken(1_000, "FROBNICATE", "z9hG4bK-x", 1, ""),
        answered(2_000, 100, "FROBNICATE", "z9hG4bK-x", 1, ""),
        answered(3_000, 405, "FROBNICATE", "z9hG4bK-x", 1, "Allow: INVITE\r\n"),
    ];
    assert!(violations(&UnsupportedMethod405Allow, &msgs).is_empty());
}

/// A §17.2.3 retransmission of the request is the same occasion, and the
/// answer already recorded stands.
#[test]
fn a_retransmitted_request_is_one_occasion() {
    let msgs = [
        taken(1_000, "FROBNICATE", "z9hG4bK-x", 1, ""),
        taken(2_000, "FROBNICATE", "z9hG4bK-x", 1, ""),
        answered(3_000, 200, "FROBNICATE", "z9hG4bK-x", 1, ""),
    ];
    assert_eq!(run(&UnsupportedMethod405Allow, &msgs).len(), 1);
}

/// A recognised method opens no occasion at all.
#[test]
fn a_recognised_method_is_not_an_occasion() {
    let msgs = [
        taken(1_000, "INVITE", "z9hG4bK-i", 1, ""),
        answered(2_000, 200, "INVITE", "z9hG4bK-i", 1, ""),
    ];
    assert!(run(&UnsupportedMethod405Allow, &msgs).is_empty());
}

/// An unanswered request inside an OPEN observation's window is truncation,
/// not silence.
#[test]
fn an_open_observation_inside_the_window_settles_nothing() {
    let msgs = [taken(1_000, "FROBNICATE", "z9hG4bK-x", 1, "")];
    let open = Observation { last_us: 1_500, closed: false, ..Observation::default() };
    let all = UnsupportedMethod405Allow.eval(&WireView { msgs: &msgs, obs: &open });
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(!all[0].decided(), "{:?}", all[0].decision);
    // The same stream CLOSED decides immediately: nothing was in flight.
    assert_eq!(violations(&UnsupportedMethod405Allow, &msgs).len(), 1);
}

/// A 420 naming the tags it refused meets §8.2.2.
#[test]
fn a_420_with_unsupported_discharges_the_require() {
    let msgs = [
        taken(1_000, "INVITE", "z9hG4bK-i", 1, "Require: frobnicate\r\n"),
        answered(2_000, 420, "INVITE", "z9hG4bK-i", 1, "Unsupported: frobnicate\r\n"),
    ];
    let all = run(&UnsupportedExtension420, &msgs);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
}

/// Accepting the request instead is the miss, and the finding names the tag.
#[test]
fn accepting_an_unsupported_require_is_violated() {
    let msgs = [
        taken(1_000, "INVITE", "z9hG4bK-i", 1, "Require: frobnicate\r\n"),
        answered(2_000, 200, "INVITE", "z9hG4bK-i", 1, ""),
    ];
    let f = violations(&UnsupportedExtension420, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::RejectionNotIssued { unsupported_tags, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(unsupported_tags.as_slice(), ["frobnicate".to_string()]);
}

/// A `Require` naming only recognised tags is no occasion.
#[test]
fn a_recognised_require_is_not_an_occasion() {
    let msgs = [
        taken(1_000, "INVITE", "z9hG4bK-i", 1, "Require: 100rel, timer\r\n"),
        answered(2_000, 200, "INVITE", "z9hG4bK-i", 1, ""),
    ];
    assert!(run(&UnsupportedExtension420, &msgs).is_empty());
}

/// The `Require` tags are what make the occasion exist, so a request the
/// vantage carried no header block for is no occasion.
#[test]
fn a_request_with_no_head_opens_no_require_occasion() {
    let mut msgs = [
        taken(1_000, "INVITE", "z9hG4bK-i", 1, "Require: frobnicate\r\n"),
        answered(2_000, 200, "INVITE", "z9hG4bK-i", 1, ""),
    ];
    msgs[0].head = None;
    assert!(run(&UnsupportedExtension420, &msgs).is_empty());
}

/// [`taken`] on the dialog `at`/`bt`.
fn taken_in_dialog(at_us: u64, method: &str, branch: &str, cseq: u32, extra: &str) -> Msg {
    Msg { to_tag: Some("bt".to_string()), ..taken(at_us, method, branch, cseq, extra) }
}

/// Alice's BYE of the dialog `at`/`bt`, answered 200 at `at_us`.
fn bye_ended(at_us: u64) -> [Msg; 2] {
    [
        taken_in_dialog(at_us - 500, "BYE", "z9hG4bK-b", 2, ""),
        answered(at_us, 200, "BYE", "z9hG4bK-b", 2, ""),
    ]
}

/// RFC 3261 §12.2.2: a UAS matches a To-tagged request to its dialogs before
/// it applies the §8.2 checks; one naming a dialog a BYE ended is refused
/// 481, which discharges the `Require` occasion.
#[test]
fn a_481_to_a_request_on_a_dialog_a_bye_ended_discharges_the_require() {
    let [bye, ok] = bye_ended(2_000);
    let msgs = [
        bye,
        ok,
        taken_in_dialog(3_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n"),
        answered(4_000, 481, "INFO", "z9hG4bK-i", 3, ""),
    ];
    let all = run(&UnsupportedExtension420, &msgs);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
}

/// So for an unrecognised method: the 481 is the answer its ended dialog
/// owes, before §8.2.1 is reached.
#[test]
fn a_481_to_an_unknown_method_on_a_dialog_a_bye_ended_discharges_the_405() {
    let [bye, ok] = bye_ended(2_000);
    let msgs = [
        bye,
        ok,
        taken_in_dialog(3_000, "FROBNICATE", "z9hG4bK-x", 3, ""),
        answered(4_000, 481, "FROBNICATE", "z9hG4bK-x", 3, ""),
    ];
    assert!(violations(&UnsupportedMethod405Allow, &msgs).is_empty());
}

/// Bob's BYE of the dialog `at`/`bt` at `at_us`, and alice's 200 to it at
/// `ok_us`.
fn bye_from_bob(at_us: u64, ok_us: u64) -> [Msg; 2] {
    let bye = Msg {
        src: BOB.to_string(),
        dst: ALICE.to_string(),
        from_tag: Some("bt".to_string()),
        to_tag: Some("at".to_string()),
        ..taken(at_us, "BYE", "z9hG4bK-bb", 7, "")
    };
    let ok = Msg {
        src: ALICE.to_string(),
        dst: BOB.to_string(),
        from_tag: Some("bt".to_string()),
        to_tag: Some("at".to_string()),
        ..answered(ok_us, 200, "BYE", "z9hG4bK-bb", 7, "")
    };
    [bye, ok]
}

/// RFC 3261 §15.1.1 and RFC 5407 §3.3: a UA's dialog ends for new requests
/// once it sends or takes a BYE, so a request crossing bob's BYE draws the
/// 481 of a dialog bob no longer holds. Bob's vantage shows the crossing: his
/// BYE before the request. Alice's shows her request before bob's BYE, which
/// bob may have taken on the live dialog as well: that settles nothing.
#[test]
fn a_481_to_a_request_crossing_the_taker_s_bye_discharges_the_require() {
    let info = || taken_in_dialog(1_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n");
    let refused = |at_us| answered(at_us, 481, "INFO", "z9hG4bK-i", 3, "");
    let [bye, ok] = bye_from_bob(1_200, 1_600);
    let at_bob = [bye.clone(), Msg { at_us: 1_300, ..info() }, refused(1_400), ok.clone()];
    let all = run(&UnsupportedExtension420, &at_bob);
    assert_eq!(all.len(), 1, "bob's: {all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "bob's: {:?}", all[0].decision);

    let at_alice = [info(), bye, ok, refused(2_000)];
    let all = run(&UnsupportedExtension420, &at_alice);
    assert_eq!(all.len(), 1, "alice's: {all:?}");
    assert!(matches!(all[0].decision, Decision::Undecidable(_)), "alice's: {:?}", all[0].decision);
}

/// §8.2.2.3 and §15.1.2: a request its UAS took on a live dialog owes its
/// 420 whatever BYE follows it. Alice's own BYE behind her request reaches bob
/// after it, so his 481 is the miss; bob's BYE behind the request, alice's 200
/// to it, and then his 481, reads as the crossing it may be: undecided.
#[test]
fn a_481_to_a_request_taken_before_a_later_bye_still_owes_the_420() {
    let info = taken_in_dialog(1_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n");
    let refused = |at_us| answered(at_us, 481, "INFO", "z9hG4bK-i", 3, "");
    let alice_s_bye = [
        info.clone(),
        taken_in_dialog(1_100, "BYE", "z9hG4bK-b", 4, ""),
        refused(1_300),
        answered(1_400, 200, "BYE", "z9hG4bK-b", 4, ""),
    ];
    let f = violations(&UnsupportedExtension420, &alice_s_bye);
    assert_eq!(f.len(), 1, "alice's BYE: {f:?}");
    let Decision::Violated(Evidence::RejectionNotIssued { answered_status, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*answered_status, 481);

    let [bye, ok] = bye_from_bob(3_000, 3_500);
    let bob_s_bye = [info, bye, ok, refused(4_000)];
    let all = run(&UnsupportedExtension420, &bob_s_bye);
    assert_eq!(all.len(), 1, "bob's BYE: {all:?}");
    assert!(
        matches!(all[0].decision, Decision::Undecidable(_)),
        "bob's BYE: {:?}",
        all[0].decision
    );
}

/// So does a request bob takes after alice's BYE reached him, before he
/// answered that BYE.
#[test]
fn a_481_to_a_request_after_the_bye_reached_its_taker_discharges_the_require() {
    let [bye, _] = bye_ended(2_000);
    let msgs = [
        bye,
        taken_in_dialog(3_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n"),
        answered(3_500, 200, "BYE", "z9hG4bK-b", 2, ""),
        answered(4_000, 481, "INFO", "z9hG4bK-i", 3, ""),
    ];
    assert!(violations(&UnsupportedExtension420, &msgs).is_empty());
}

/// A vantage that carried the BYE's 2xx and not the BYE still shows the
/// dialog ended there; a BYE the strict reading refuses ends nothing.
#[test]
fn a_bye_s_2xx_ends_the_dialog_and_a_refused_bye_does_not() {
    let info = taken_in_dialog(3_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n");
    let refused = answered(4_000, 481, "INFO", "z9hG4bK-i", 3, "");
    let [bye, ok] = bye_ended(2_000);
    let only_the_2xx = [ok, info.clone(), refused.clone()];
    assert!(violations(&UnsupportedExtension420, &only_the_2xx).is_empty(), "the 2xx alone");
    let strictly_refused = Msg { refused: true, ..bye };
    let refused_bye = [strictly_refused, info, refused];
    assert_eq!(violations(&UnsupportedExtension420, &refused_bye).len(), 1, "a refused BYE");
}

/// A 481 on a dialog no BYE ended, to a request on no dialog (no To-tag),
/// or sent before any BYE of the dialog, still owes the 420.
#[test]
fn a_481_anywhere_else_is_violated() {
    let info = |at_us, to_tag: Option<&str>| Msg {
        to_tag: to_tag.map(str::to_string),
        ..taken(at_us, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n")
    };
    let refused = answered(4_000, 481, "INFO", "z9hG4bK-i", 3, "");
    let live = [info(3_000, Some("bt")), refused.clone()];
    assert_eq!(violations(&UnsupportedExtension420, &live).len(), 1, "a live dialog");
    let [bye, ok] = bye_ended(2_000);
    let out_of_dialog = [bye, ok, info(3_000, None), refused.clone()];
    assert_eq!(violations(&UnsupportedExtension420, &out_of_dialog).len(), 1, "no To-tag");
    let [late_bye, late_ok] = bye_from_bob(4_500, 5_000);
    let before_the_bye = [info(3_000, Some("bt")), refused, late_bye, late_ok];
    assert_eq!(violations(&UnsupportedExtension420, &before_the_bye).len(), 1, "before the BYE");
}

/// A request the strict reading refuses owes its 400, whatever method it
/// names: an unknown one opens no §8.2.1 occasion.
#[test]
fn a_refused_request_owes_no_405() {
    let mut refused = taken(1_000, "FROBNICATE", "z9hG4bK-x", 1, "");
    refused.refused = true;
    let msgs = [refused, answered(2_000, 400, "FROBNICATE", "z9hG4bK-x", 1, "")];
    assert!(run(&UnsupportedMethod405Allow, &msgs).is_empty());
}

/// A request the strict reading refuses owes its 400, whatever it requires:
/// an unsupported tag opens no §8.2.2 occasion.
#[test]
fn a_refused_request_owes_no_420() {
    let mut refused = taken(1_000, "INVITE", "z9hG4bK-i", 1, "Require: frobnicate\r\n");
    refused.refused = true;
    let msgs = [refused, answered(2_000, 400, "INVITE", "z9hG4bK-i", 1, "")];
    assert!(run(&UnsupportedExtension420, &msgs).is_empty());
}

/// §8.2.3: a 415 naming any one Accept family header is compliant.
#[test]
fn a_415_naming_what_it_accepts_is_compliant() {
    let msgs = [answered(1_000, 415, "INVITE", "z9hG4bK-i", 1, "Accept: application/sdp\r\n")];
    let all = run(&Unsupported415Accepts, &msgs);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
}

/// A bare 415 names all three headers on ONE finding, and charges its
/// sender.
#[test]
fn a_bare_415_is_one_finding_naming_all_three() {
    let msgs = [answered(1_000, 415, "INVITE", "z9hG4bK-i", 1, "")];
    let f = violations(&Unsupported415Accepts, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, BOB, "the sender of the 415 is charged");
    let Decision::Violated(Evidence::ResponseHeadersMissing { missing, status, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(missing.as_slice(), ["Accept", "Accept-Encoding", "Accept-Language"]);
    assert_eq!(*status, 415);
}

/// A §17.2.1 retransmission of that 415 is the same answer again.
#[test]
fn a_retransmitted_415_is_one_occasion() {
    let msgs = [
        answered(1_000, 415, "INVITE", "z9hG4bK-i", 1, ""),
        answered(2_000, 415, "INVITE", "z9hG4bK-i", 1, ""),
    ];
    assert_eq!(run(&Unsupported415Accepts, &msgs).len(), 1);
}

/// §21.4.15: a 421 listing what it demands is compliant; an EMPTY Require
/// states no demand and does not discharge it.
#[test]
fn a_421_states_what_it_requires() {
    let listed = [answered(1_000, 421, "INVITE", "z9hG4bK-i", 1, "Require: 100rel\r\n")];
    assert!(violations(&UnsupportedExtension421, &listed).is_empty());
    let bare = [answered(1_000, 421, "INVITE", "z9hG4bK-i", 1, "")];
    assert_eq!(violations(&UnsupportedExtension421, &bare).len(), 1);
    let empty = [answered(1_000, 421, "INVITE", "z9hG4bK-i", 1, "Require: \r\n")];
    assert_eq!(
        violations(&UnsupportedExtension421, &empty).len(),
        1,
        "a Require naming nothing demands nothing"
    );
}

/// §11.2: a 2xx to OPTIONS naming any one capability header is compliant; a
/// bare one is the miss.
#[test]
fn an_options_2xx_describes_what_it_supports() {
    let clean = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        answered(2_000, 200, "OPTIONS", "z9hG4bK-o", 1, "Allow: INVITE\r\n"),
    ];
    assert!(violations(&OptionsResponseEchoes, &clean).is_empty());
    let bare = [
        taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""),
        answered(2_000, 200, "OPTIONS", "z9hG4bK-o", 1, ""),
    ];
    let f = violations(&OptionsResponseEchoes, &bare);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, BOB);
    assert!(!f[0].relayed, "bob sent no OPTIONS onward: the bare 2xx is his own");
    let Decision::Violated(Evidence::ResponseHeadersMissing { missing, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(missing.as_slice(), ["Allow", "Supported", "Accept"]);
}

/// A bare 2xx to OPTIONS answered from a second UAS socket is the same miss.
#[test]
fn a_bare_options_2xx_from_a_second_uas_socket_is_violated() {
    let mut reply = answered(2_000, 200, "OPTIONS", "z9hG4bK-o", 1, "");
    reply.src = BOB_SENDS.to_string();
    let msgs = [taken(1_000, "OPTIONS", "z9hG4bK-o", 1, ""), reply];
    let f = violations(&OptionsResponseEchoes, &msgs);
    assert_eq!(f.len(), 1, "{:?}", run(&OptionsResponseEchoes, &msgs));
    assert_eq!(f[0].emitter, BOB_SENDS, "the socket that sent the 2xx is charged");
}

/// A 2xx whose OPTIONS this vantage never carried settles nothing about
/// what it was answering.
#[test]
fn an_options_2xx_without_its_request_is_not_an_occasion() {
    let msgs = [answered(1_000, 200, "OPTIONS", "z9hG4bK-o", 1, "")];
    assert!(run(&OptionsResponseEchoes, &msgs).is_empty());
}

// ---- no-require-on-cancel-or-ack ------------------------------------

#[test]
fn a_cancel_demanding_an_extension_is_violated_and_a_bare_one_is_compliant() {
    let f = violations(
        &NoRequireOnCancelOrAck,
        &[taken(1_000, "CANCEL", "z9hG4bK-i", 1, "Require: 100rel\r\n")],
    );
    let Decision::Violated(Evidence::ForbiddenHeaderPresent { on, header, value, .. }) =
        &f[0].decision
    else {
        panic!("require evidence: {:?}", f[0].decision)
    };
    assert_eq!((on.as_str(), header.as_str(), value.as_str()), ("CANCEL", "Require", "100rel"));

    let clean = run(&NoRequireOnCancelOrAck, &[taken(1_000, "CANCEL", "z9hG4bK-i", 1, "")]);
    assert_eq!(clean.len(), 1, "{clean:?}");
    assert!(matches!(clean[0].decision, Decision::Compliant), "{:?}", clean[0].decision);
}

/// The ACK of a non-2xx rides the INVITE's branch, so the walk pairs it
/// with the reject its sender took there.
#[test]
fn the_ack_of_a_non_2xx_demanding_an_extension_is_violated() {
    let msgs = [
        taken(1_000, "INVITE", "z9hG4bK-i", 1, ""),
        answered(2_000, 486, "INVITE", "z9hG4bK-i", 1, ""),
        taken(3_000, "ACK", "z9hG4bK-i", 1, "Proxy-Require: foo\r\n"),
    ];
    let f = violations(&NoRequireOnCancelOrAck, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::ForbiddenHeaderPresent { on, header, .. }) = &f[0].decision
    else {
        panic!("require evidence: {:?}", f[0].decision)
    };
    assert_eq!((on.as_str(), header.as_str()), ("ACK", "Proxy-Require"));
}

/// The 200 to a CANCEL answers the CANCEL's own transaction (RFC 3261 §9.2),
/// never the INVITE's: the cancelled INVITE's branch ends in its 487, so the
/// ACK riding it acknowledges a non-2xx and owes §8.2.2.3.
#[test]
fn the_ack_of_a_cancelled_invite_s_487_demanding_an_extension_is_violated() {
    let msgs = [
        taken(1_000, "INVITE", "z9hG4bK-i", 1, ""),
        taken(2_000, "CANCEL", "z9hG4bK-i", 1, ""),
        answered(3_000, 200, "CANCEL", "z9hG4bK-i", 1, ""),
        answered(4_000, 487, "INVITE", "z9hG4bK-i", 1, ""),
        taken(5_000, "ACK", "z9hG4bK-i", 1, "Require: 100rel\r\n"),
    ];
    let f = violations(&NoRequireOnCancelOrAck, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].anchor, 4, "the ACK is charged, not the CANCEL");
}

/// The §13.2.2.4 2xx ACK is a fresh transaction on its own branch: §8.2.2.3
/// does not reach it, so it opens no occasion at all.
#[test]
fn the_ack_of_a_2xx_is_no_occasion() {
    let msgs = [
        taken(1_000, "INVITE", "z9hG4bK-i", 1, ""),
        answered(2_000, 200, "INVITE", "z9hG4bK-i", 1, ""),
        taken(3_000, "ACK", "z9hG4bK-k", 1, "Require: 100rel\r\n"),
    ];
    let f: Vec<Finding> =
        run(&NoRequireOnCancelOrAck, &msgs).into_iter().filter(|x| x.anchor == 2).collect();
    assert!(f.is_empty(), "{f:?}");
}

/// A vantage with no header block cannot say what the request demanded.
#[test]
fn a_byte_less_cancel_is_undecidable() {
    let mut m = taken(1_000, "CANCEL", "z9hG4bK-i", 1, "");
    m.head = None;
    let f = run(&NoRequireOnCancelOrAck, &[m]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(!f[0].decided(), "{:?}", f[0].decision);
}
