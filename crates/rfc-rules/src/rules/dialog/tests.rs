//! The rules' OWN semantics under a CLOSED observation: which messages fix
//! a dialog, what an in-dialog request then owes it, and where the wire
//! settles nothing.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{
    MidDialogRoute, MidDialogUri, MidDialogWireDestination, NoByeOutsideOrEarlyDialog,
    RecordRoutePlacement, UnknownDialog481,
};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const A_URI: &str = "sip:alice@127.0.0.1";
const B_URI: &str = "sip:bob@127.0.0.1";

/// A request on the wire between two endpoints, with a caller-controlled
/// header block — the §12 rules read From/To/Route off the bytes.
#[allow(clippy::too_many_arguments)]
fn req(
    at_us: u64,
    src: &str,
    dst: &str,
    method: &str,
    r_uri: &str,
    branch: &str,
    cseq: u32,
    from: &str,
    to: &str,
    to_tag: Option<&str>,
    extra: &str,
) -> Msg {
    let to_hdr = match to_tag {
        Some(t) => format!("<{to}>;tag={t}"),
        None => format!("<{to}>"),
    };
    let head = format!(
        "{method} {r_uri} SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{from}>;tag=at\r\n\
             To: {to_hdr}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    );
    Msg {
        refused: false,
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: method.to_string() },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: method.to_string(),
        from_tag: Some("at".to_string()),
        to_tag: to_tag.map(str::to_string),
        via_branch: Some(branch.to_string()),
        head: Some(head.into_bytes()),
        body: None,
    }
}

/// Alice's dialog-creating INVITE toward bob.
fn invite(at_us: u64, branch: &str, extra: &str) -> Msg {
    req(at_us, ALICE, BOB, "INVITE", B_URI, branch, 1, A_URI, B_URI, None, extra)
}

/// A response bob sent alice.
fn rsp(at_us: u64, status: u16, cseq: u32, method: &str, branch: &str, extra: &str) -> Msg {
    rsp_tagged(at_us, status, cseq, method, branch, Some("bt"), extra)
}

fn rsp_tagged(
    at_us: u64,
    status: u16,
    cseq: u32,
    method: &str,
    branch: &str,
    to_tag: Option<&str>,
    extra: &str,
) -> Msg {
    let to_hdr = match to_tag {
        Some(t) => format!("<{B_URI}>;tag={t}"),
        None => format!("<{B_URI}>"),
    };
    let head = format!(
        "SIP/2.0 {status} X\r\nContact: <sip:fixture@192.0.2.98>\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A_URI}>;tag=at\r\n\
             To: {to_hdr}\r\n\
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
        to_tag: to_tag.map(str::to_string),
        via_branch: Some(branch.to_string()),
        head: Some(head.into_bytes()),
        body: None,
    }
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

fn eval(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    rule.eval(&WireView { msgs, obs: &obs(msgs) })
}

fn hits(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    eval(rule, msgs).into_iter().filter(Finding::violated).collect()
}

// ── MidDialogUri (RFC 3261 §12.2.1.1) ───────────────────────────────────

/// A BYE that keeps the dialog's URIs is the obligation met, and it is one
/// occasion — the rule charges the endpoint that sent it.
#[test]
fn an_in_dialog_request_repeating_the_dialog_uris_is_clean() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        req(3_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), ""),
    ];
    let all = eval(&MidDialogUri, &msgs);
    assert_eq!(all.len(), 1, "one occasion, the BYE: {all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
    assert_eq!(all[0].emitter, ALICE, "the endpoint that sent it is charged");
    assert_eq!(all[0].taker, BOB);
}

/// A BYE that rewrites the From URI mid-dialog: ONE finding naming both
/// URIs, since rewriting the dialog's identity is one act.
#[test]
fn a_rewritten_from_uri_is_violated() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        req(
            3_000,
            ALICE,
            BOB,
            "BYE",
            B_URI,
            "z9hG4bK-b",
            2,
            "sip:eve@127.0.0.1",
            B_URI,
            Some("bt"),
            "",
        ),
    ];
    let f = hits(&MidDialogUri, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::MidDialogUriChanged {
        method,
        sent_from_uri,
        dialog_local_uri,
        sent_to_uri,
        dialog_remote_uri,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(method.as_str(), "BYE");
    assert_eq!(sent_from_uri.as_str(), "sip:eve@127.0.0.1");
    assert_eq!(dialog_local_uri.as_str(), A_URI);
    assert_eq!(sent_to_uri, dialog_remote_uri, "the To URI held");
}

/// Both URIs rewritten is still ONE occasion — the collapse the report then
/// spells out.
#[test]
fn both_uris_rewritten_are_one_finding() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        req(
            3_000,
            ALICE,
            BOB,
            "BYE",
            B_URI,
            "z9hG4bK-b",
            2,
            "sip:eve@127.0.0.1",
            "sip:mallory@127.0.0.1",
            Some("bt"),
            "",
        ),
    ];
    let f = hits(&MidDialogUri, &msgs);
    assert_eq!(f.len(), 1, "one occasion, both URIs on it: {f:?}");
    let Decision::Violated(Evidence::MidDialogUriChanged {
        sent_from_uri,
        dialog_local_uri,
        sent_to_uri,
        dialog_remote_uri,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_ne!(sent_from_uri, dialog_local_uri);
    assert_ne!(sent_to_uri, dialog_remote_uri);
}

/// A dialog whose creation this vantage never carried owes URIs nobody can
/// name — undecidable, never a guess.
#[test]
fn an_in_dialog_request_on_an_unseen_dialog_is_undecidable() {
    let msgs = [req(1_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), "")];
    let f = eval(&MidDialogUri, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(f[0].decision, Decision::Undecidable("no dialog-creating INVITE at this vantage")),
        "{:?}",
        f[0].decision
    );
}

/// The dialog-creating INVITE and a CANCEL are not in-dialog requests: the
/// first precedes the dialog, the second is hop-by-hop (§9.1).
#[test]
fn the_establishing_invite_and_a_cancel_open_no_occasion() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        req(2_000, ALICE, BOB, "CANCEL", B_URI, "z9hG4bK-i", 1, A_URI, B_URI, Some("bt"), ""),
    ];
    assert!(eval(&MidDialogUri, &msgs).is_empty(), "{:?}", eval(&MidDialogUri, &msgs));
}

/// Forking (§12.1.2): one INVITE, two callee tags. The establishing INVITE
/// is replicated into BOTH dialogs, so a request on either fork is judged
/// against the URIs that one INVITE fixed.
#[test]
fn a_forked_dialog_inherits_the_establishing_invites_uris() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp_tagged(2_000, 180, 1, "INVITE", "z9hG4bK-i", Some("b1"), ""),
        rsp_tagged(3_000, 200, 1, "INVITE", "z9hG4bK-i", Some("b2"), ""),
        req(
            4_000,
            ALICE,
            BOB,
            "BYE",
            B_URI,
            "z9hG4bK-b2",
            2,
            "sip:eve@127.0.0.1",
            B_URI,
            Some("b2"),
            "",
        ),
    ];
    let f = hits(&MidDialogUri, &msgs);
    assert_eq!(f.len(), 1, "the second fork knows the dialog's URIs too: {f:?}");
}

// ── MidDialogWireDestination (RFC 3261 §8.1.2 + RFC 3263 §4) ────────────

/// With no Route rows the Request-URI names the destination; the bytes went
/// there.
#[test]
fn bytes_sent_to_the_request_uri_are_clean() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        req(
            3_000,
            ALICE,
            BOB,
            "BYE",
            "sip:bob@127.0.0.1:5070",
            "z9hG4bK-b",
            2,
            A_URI,
            B_URI,
            Some("bt"),
            "",
        ),
    ];
    let all = eval(&MidDialogWireDestination, &msgs);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
}

/// The bytes left for a destination the request's own routing never named.
#[test]
fn bytes_sent_past_the_derived_destination_are_violated() {
    let mut bye = req(
        3_000,
        ALICE,
        BOB,
        "BYE",
        "sip:bob@127.0.0.1:5070",
        "z9hG4bK-b",
        2,
        A_URI,
        B_URI,
        Some("bt"),
        "",
    );
    bye.dst = "127.0.0.1:9999".to_string();
    let msgs = [invite(1_000, "z9hG4bK-i", ""), rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""), bye];
    let f = hits(&MidDialogWireDestination, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::MidDialogWireTargetDiverged {
        sent_to,
        target_host,
        target_port,
        from_route,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(sent_to.as_str(), "127.0.0.1:9999");
    assert_eq!((target_host.as_str(), *target_port), ("127.0.0.1", 5070));
    assert!(!from_route, "with no Route rows the Request-URI named it");
}

/// A non-empty Route set moves the derivation to the TOPMOST Route.
#[test]
fn the_topmost_route_names_the_destination() {
    let mut bye = req(
        3_000,
        ALICE,
        BOB,
        "BYE",
        "sip:bob@127.0.0.1:5070",
        "z9hG4bK-b",
        2,
        A_URI,
        B_URI,
        Some("bt"),
        "Route: <sip:p1@127.0.0.1:5090;lr>\r\n",
    );
    bye.dst = "127.0.0.1:5090".to_string();
    let msgs = [invite(1_000, "z9hG4bK-i", ""), rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""), bye];
    assert!(hits(&MidDialogWireDestination, &msgs).is_empty(), "the Route hop is the target");
}

/// Behind a STRICT first route the Request-URI names the next hop
/// (§12.2.1.1 put the route there), not the target riding the last Route.
#[test]
fn a_strict_first_route_names_the_destination_in_the_request_uri() {
    let dialog = |sent_to: &str| {
        let mut bye = req(
            3_000,
            ALICE,
            BOB,
            "BYE",
            "sip:127.0.0.1:5090",
            "z9hG4bK-b",
            2,
            A_URI,
            B_URI,
            Some("bt"),
            "Route: <sip:bob@127.0.0.1:5070>\r\n",
        );
        bye.dst = sent_to.to_string();
        [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:127.0.0.1:5090>\r\n"),
            bye,
        ]
    };
    assert!(
        hits(&MidDialogWireDestination, &dialog("127.0.0.1:5090")).is_empty(),
        "the strict route is the next hop",
    );
    let f = hits(&MidDialogWireDestination, &dialog("127.0.0.1:5070"));
    assert_eq!(f.len(), 1, "sent past the strict route to the target: {f:?}");
    let Decision::Violated(Evidence::MidDialogWireTargetDiverged { from_route, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert!(from_route, "the strict first route named the destination");
}

/// A strict dialog's request built loose-style but sent to the strict
/// route — the §8.1.2 next hop — went to the right place: the mis-built
/// header is `MidDialogRoute`'s finding alone.
#[test]
fn a_strict_dialog_request_sent_to_its_first_route_is_charged_once() {
    let mut bye = req(
        3_000,
        ALICE,
        BOB,
        "BYE",
        "sip:bob@127.0.0.1:5070",
        "z9hG4bK-b",
        2,
        A_URI,
        B_URI,
        Some("bt"),
        "Route: <sip:127.0.0.1:5090>\r\n",
    );
    bye.dst = "127.0.0.1:5090".to_string();
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:127.0.0.1:5090>\r\n"),
        bye,
    ];
    let wire = hits(&MidDialogWireDestination, &msgs);
    assert!(wire.is_empty(), "the bytes went to the strict route: {wire:?}");
    assert_eq!(hits(&MidDialogRoute, &msgs).len(), 1, "the header is charged once");
}

// ── RecordRoutePlacement (RFC 3261 §12.1.1 / §12.2.2) ───────────────────

/// Record-Route on the dialog-CREATING response is where it belongs.
#[test]
fn record_route_on_the_dialog_creating_response_is_clean() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
    ];
    assert!(hits(&RecordRoutePlacement, &msgs).is_empty());
}

/// A 100 creates no dialog, so a Record-Route on it is vestigial — and the
/// finding charges the endpoint that SENT the 100, reported at its taker.
#[test]
fn record_route_on_a_100_is_violated() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp_tagged(
            2_000,
            100,
            1,
            "INVITE",
            "z9hG4bK-i",
            None,
            "Record-Route: <sip:p1@127.0.0.1;lr>\r\n",
        ),
    ];
    let f = hits(&RecordRoutePlacement, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, BOB, "the endpoint that sent the 100 is charged");
    assert_eq!(f[0].taker, ALICE, "read at the endpoint that took it");
    let Decision::Violated(Evidence::RecordRouteMisplaced {
        status,
        record_route,
        request_method,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*status, 100);
    assert_eq!(record_route.as_str(), "<sip:p1@127.0.0.1;lr>");
    assert!(request_method.is_empty(), "a 100 is vestigial whatever it answers");
}

/// The route set is fixed at establishment: a 200 to an in-dialog re-INVITE
/// cannot carry Record-Route.
#[test]
fn record_route_on_a_response_to_an_in_dialog_request_is_violated() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        req(3_000, ALICE, BOB, "INVITE", B_URI, "z9hG4bK-r", 2, A_URI, B_URI, Some("bt"), ""),
        rsp(4_000, 200, 2, "INVITE", "z9hG4bK-r", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
    ];
    let f = hits(&RecordRoutePlacement, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::RecordRouteMisplaced { request_method, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(request_method.as_str(), "INVITE");
}

/// A UAC that sends its re-INVITE from another socket than its sent-by still
/// opened the transaction its response answers.
#[test]
fn record_route_answering_a_two_socket_uac_s_in_dialog_request_is_violated() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        req(
            3_000,
            "127.0.0.1:40000",
            BOB,
            "INVITE",
            B_URI,
            "z9hG4bK-r",
            2,
            A_URI,
            B_URI,
            Some("bt"),
            "",
        ),
        rsp(4_000, 200, 2, "INVITE", "z9hG4bK-r", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
    ];
    let f = hits(&RecordRoutePlacement, &msgs);
    assert_eq!(f.len(), 1, "{:?}", eval(&RecordRoutePlacement, &msgs));
}

/// A UAC that sends from another socket than its sent-by learned the route
/// set from the 2xx its sent-by took: its in-dialog request owes that set.
#[test]
fn a_two_socket_uac_owes_the_route_set_its_sent_by_learned() {
    const ALICE_SENDS: &str = "127.0.0.1:40000";
    let msgs = [
        req(1_000, ALICE_SENDS, BOB, "INVITE", B_URI, "z9hG4bK-i", 1, A_URI, B_URI, None, ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1:5090;lr>\r\n"),
        req(3_000, ALICE_SENDS, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), ""),
    ];
    let f = hits(&MidDialogRoute, &msgs);
    assert_eq!(f.len(), 1, "{:?}", eval(&MidDialogRoute, &msgs));
    assert_eq!(f[0].emitter, ALICE_SENDS, "the socket that sent the BYE is charged");
}

/// §17.1.1.3: the ACK for a non-2xx final reuses the INVITE's branch and
/// carries the final's To tag. A §17.2.1 retransmit landing after it must
/// still read as answering the INVITE — an ACK elicits no response, so it is
/// never a correlation target.
#[test]
fn a_retransmitted_final_crossing_its_ack_still_answers_the_invite() {
    let final_480 = |at_us| {
        rsp(at_us, 480, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n")
    };
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        final_480(2_000),
        req(3_000, ALICE, BOB, "ACK", B_URI, "z9hG4bK-i", 1, A_URI, B_URI, Some("bt"), ""),
        final_480(4_000),
    ];
    assert!(
        hits(&RecordRoutePlacement, &msgs).is_empty(),
        "{:?}",
        hits(&RecordRoutePlacement, &msgs)
    );
}

/// A response on a transaction this vantage never carried the request for
/// settles nothing: whether it was in-dialog is exactly the open question.
#[test]
fn a_record_route_on_an_unplaceable_response_is_undecidable() {
    let msgs = [rsp(1_000, 200, 9, "OPTIONS", "z9hG4bK-x", "Record-Route: <sip:p1@h;lr>\r\n")];
    let f = eval(&RecordRoutePlacement, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(
            f[0].decision,
            Decision::Undecidable("no request on this transaction at this vantage")
        ),
        "{:?}",
        f[0].decision
    );
}

/// An in-dialog request alice sent bob, with a caller-chosen tag pair.
fn in_dialog(at_us: u64, method: &str, branch: &str, cseq: u32, ft: &str, tt: &str) -> Msg {
    let head = format!(
        "{method} {B_URI} SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A_URI}>;tag={ft}\r\n\
             To: <{B_URI}>;tag={tt}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
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
        from_tag: Some(ft.to_string()),
        to_tag: Some(tt.to_string()),
        via_branch: Some(branch.to_string()),
        head: Some(head.into_bytes()),
        body: None,
    }
}

// ── NoByeOutsideOrEarlyDialog (RFC 3261 §15) ────────────────────────────

/// A request bob sent alice, with caller-controlled tags — the
/// callee-initiated orientation, where From/To are reversed relative to the
/// establishing INVITE.
fn from_bob(at_us: u64, method: &str, branch: &str, cseq: u32, ft: &str, tt: &str) -> Msg {
    let head = format!(
        "{method} {A_URI} SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5070;branch={branch}\r\n\
             From: <{B_URI}>;tag={ft}\r\n\
             To: <{A_URI}>;tag={tt}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
    );
    Msg {
        refused: false,
        at_us,
        src: BOB.to_string(),
        dst: ALICE.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: method.to_string() },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: method.to_string(),
        from_tag: Some(ft.to_string()),
        to_tag: (!tt.is_empty()).then(|| tt.to_string()),
        via_branch: Some(branch.to_string()),
        head: Some(head.into_bytes()),
        body: None,
    }
}

/// The caller BYE-ing a dialog its own 2xx confirmed: the ordinary
/// teardown, and one occasion.
#[test]
fn a_caller_bye_on_a_confirmed_dialog_is_compliant() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
    ];
    let f = eval(&NoByeOutsideOrEarlyDialog, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].rule, RuleId::NoByeOutsideOrEarlyDialog);
    assert_eq!(f[0].emitter, ALICE, "the BYE's sender is charged");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A BYE carrying no peer tag names no dialog at all.
#[test]
fn a_bye_with_no_peer_tag_is_violated() {
    let mut bye = in_dialog(1_000, "BYE", "z9hG4bK-b", 2, "at", "bt");
    bye.to_tag = None;
    let f = hits(&NoByeOutsideOrEarlyDialog, &[bye]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::ByeOffDialog { early_dialog, to_tag, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert!(!early_dialog, "the no-dialog shape, not the early one");
    assert!(to_tag.is_empty());
}

/// The callee BYE-ing a dialog it answered 180 and never accepted: §15 has
/// it reject the pending INVITE instead, so the caller's transaction still
/// gets a final.
#[test]
fn a_callee_bye_on_an_early_dialog_is_violated() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", ""),
        from_bob(3_000, "BYE", "z9hG4bK-b", 2, "bt", "at"),
    ];
    let f = hits(&NoByeOutsideOrEarlyDialog, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, BOB, "the callee is charged");
    let Decision::Violated(Evidence::ByeOffDialog { early_dialog, .. }) = &f[0].decision else {
        panic!("{:?}", f[0].decision)
    };
    assert!(early_dialog);
}

/// Once the callee has accepted, its own BYE is the ordinary teardown —
/// tags reversed and all, because the dialog is keyed by the PEER's tag.
#[test]
fn a_callee_bye_after_its_own_2xx_is_compliant() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", ""),
        rsp(3_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        from_bob(4_000, "BYE", "z9hG4bK-b", 2, "bt", "at"),
    ];
    assert!(
        hits(&NoByeOutsideOrEarlyDialog, &msgs).is_empty(),
        "{:?}",
        eval(&NoByeOutsideOrEarlyDialog, &msgs)
    );
}

/// A UAC answering its PEER's later re-INVITE has answered an INVITE, but
/// not THE INVITE: it is nobody's callee, so its own BYE is clean. Reading
/// "is the callee" off the establishing INVITE's branch is what keeps it so.
#[test]
fn a_caller_that_rejected_a_reverse_re_invite_may_still_bye() {
    // Alice establishes, bob re-INVITEs from his side, alice rejects it 500,
    // then alice tears the (long-confirmed) dialog down.
    let mut reverse_500 = rsp_tagged(4_000, 500, 1, "INVITE", "z9hG4bK-r", Some("at"), "");
    reverse_500.src = ALICE.to_string();
    reverse_500.dst = BOB.to_string();
    reverse_500.from_tag = Some("bt".to_string());
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        from_bob(3_000, "INVITE", "z9hG4bK-r", 1, "bt", "at"),
        reverse_500,
        in_dialog(5_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
    ];
    assert!(
        hits(&NoByeOutsideOrEarlyDialog, &msgs).is_empty(),
        "{:?}",
        eval(&NoByeOutsideOrEarlyDialog, &msgs)
    );
}

/// A retransmitted BYE is the same act again: one occasion, not two.
#[test]
fn a_retransmitted_bye_is_one_occasion() {
    let mut again = in_dialog(4_000, "BYE", "z9hG4bK-b", 2, "at", "bt");
    again.repeat = true;
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
        again,
    ];
    assert_eq!(eval(&NoByeOutsideOrEarlyDialog, &msgs).len(), 1);
}

mod bye_after_final;
mod route;
mod unknown_dialog;
mod wire_destination;
