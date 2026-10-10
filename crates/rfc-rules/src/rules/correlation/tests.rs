//! The family's OWN semantics: which message is an occasion, what the
//! taker's own stream settles, and what a relaying vantage may not judge.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::*;

const A: &str = "10.0.0.1:5060";
const B: &str = "10.0.0.2:5070";

#[allow(clippy::too_many_arguments)]
fn msg(
    at_us: u64,
    src: &str,
    dst: &str,
    kind: Kind,
    call_id: &str,
    cseq: u32,
    cseq_method: &str,
    branch: Option<&str>,
    from_tag: Option<&str>,
    to_tag: Option<&str>,
    head: String,
) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind,
        call_id: call_id.to_string(),
        cseq,
        cseq_method: cseq_method.to_string(),
        via_branch: branch.map(str::to_string),
        from_tag: from_tag.map(str::to_string),
        to_tag: to_tag.map(str::to_string),
        head: Some(head.into_bytes()),
        body: Some(Vec::new()),
    }
}

/// A request `src` sent, with `vias` Via rows.
#[allow(clippy::too_many_arguments)]
fn req(
    at_us: u64,
    src: &str,
    dst: &str,
    method: &str,
    uri: &str,
    cseq: u32,
    branch: &str,
    from_uri: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    vias: usize,
) -> Msg {
    let to = match to_tag {
        Some(t) => format!("<sip:bob@h>;tag={t}"),
        None => "<sip:bob@h>".to_string(),
    };
    let mut head = format!("{method} {uri} SIP/2.0\r\n");
    for i in 0..vias.max(1) {
        let b = if i == 0 { branch.to_string() } else { format!("{branch}-hop{i}") };
        head.push_str(&format!("Via: SIP/2.0/UDP 10.0.0.{}:5060;branch={b}\r\n", i + 1));
    }
    head.push_str(&format!(
        "From: <{from_uri}>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\r\n"
    ));
    msg(
        at_us,
        src,
        dst,
        Kind::Request { method: method.to_string() },
        "c1",
        cseq,
        method,
        Some(branch),
        Some(from_tag),
        to_tag,
        head,
    )
}

/// A response `src` sent, with `vias` Via rows.
#[allow(clippy::too_many_arguments)]
fn resp(
    at_us: u64,
    src: &str,
    dst: &str,
    status: u16,
    cseq: u32,
    method: &str,
    branch: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    vias: usize,
) -> Msg {
    let to = match to_tag {
        Some(t) => format!("<sip:bob@h>;tag={t}"),
        None => "<sip:bob@h>".to_string(),
    };
    let mut head = format!("SIP/2.0 {status} X\r\n");
    for i in 0..vias {
        let b = if i == 0 { branch.to_string() } else { format!("{branch}-hop{i}") };
        head.push_str(&format!("Via: SIP/2.0/UDP 10.0.0.{}:5060;branch={b}\r\n", i + 1));
    }
    head.push_str(&format!(
        "From: <sip:alice@h>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\r\n"
    ));
    msg(
        at_us,
        src,
        dst,
        Kind::Response { status },
        "c1",
        cseq,
        method,
        Some(branch),
        Some(from_tag),
        to_tag,
        head,
    )
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

/// Only the findings charged to `emitter` — the live consumer's vantage
/// filter, applied here so a test states one party's verdict.
fn charged(f: Vec<Finding>, emitter: &str) -> Vec<Finding> {
    f.into_iter().filter(|x| x.emitter == emitter).collect()
}

/// A dialog A opens to B: INVITE, 180, 200, and the tags they mint.
fn opened() -> Vec<Msg> {
    vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
    ]
}

// ── response-echoes-request-via ─────────────────────────────────────────

#[test]
fn a_response_echoing_the_stack_is_compliant() {
    let f = charged(run(&ResponseEchoesRequestVia, &opened()), B);
    assert_eq!(f.len(), 2, "one occasion per response A took: {f:?}");
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
}

#[test]
fn a_response_on_a_foreign_branch_is_violated() {
    let mut msgs = opened();
    msgs.truncate(1);
    msgs.push(resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-other", "at", Some("bt"), 1));
    let f = charged(run(&ResponseEchoesRequestVia, &msgs), B);
    let Decision::Violated(Evidence::ResponseViaDiverged {
        response_branch, request_branch, ..
    }) = &f[0].decision
    else {
        panic!("via evidence: {:?}", f[0].decision)
    };
    assert_eq!((response_branch.as_str(), request_branch.as_str()), ("z9hG4bK-other", "z9hG4bK-i"));
}

/// A grown Via stack was rewritten in transit, and the count is judged
/// against the request the BRANCH matched, not the most recent one.
#[test]
fn a_grown_via_stack_is_violated() {
    let mut msgs = opened();
    msgs.truncate(1);
    msgs.push(resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 3));
    let f = charged(run(&ResponseEchoesRequestVia, &msgs), B);
    let Decision::Violated(Evidence::ResponseViaDiverged { response_vias, request_vias, .. }) =
        &f[0].decision
    else {
        panic!("via evidence: {:?}", f[0].decision)
    };
    assert_eq!((*response_vias, *request_vias), (3, 1));
}

/// Correlation is branch-first: two same-key client transactions in flight
/// at once (crossing BYEs on one CSeq) are each answered on their own
/// branch, and picking "the most recent" would flag a correct wire.
#[test]
fn same_key_transactions_correlate_by_branch_not_recency() {
    let msgs = vec![
        req(1_000, A, B, "BYE", "sip:bob@h", 5, "z9hG4bK-x", "sip:alice@h", "at", Some("bt"), 1),
        req(1_500, A, B, "BYE", "sip:bob@h", 5, "z9hG4bK-y", "sip:alice@h", "at", Some("bt"), 1),
        resp(2_000, B, A, 200, 5, "BYE", "z9hG4bK-x", "at", Some("bt"), 1),
    ];
    let f = charged(run(&ResponseEchoesRequestVia, &msgs), B);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A response answering nothing the endpoint sent has no stack to echo.
#[test]
fn an_uncorrelated_response_is_no_occasion() {
    let msgs = vec![resp(1_000, B, A, 200, 9, "INVITE", "z9hG4bK-z", "at", Some("bt"), 1)];
    assert!(run(&ResponseEchoesRequestVia, &msgs).is_empty());
}

// ── response-correlation ────────────────────────────────────────────────

#[test]
fn a_response_echoing_a_sent_cseq_is_compliant() {
    let f = charged(run(&ResponseCorrelation, &opened()), B);
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
}

#[test]
fn a_phantom_response_cseq_is_violated() {
    let mut msgs = opened();
    msgs.truncate(1);
    msgs.push(resp(2_000, B, A, 200, 9, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1));
    let f = charged(run(&ResponseCorrelation, &msgs), B);
    let Decision::Violated(Evidence::ResponseCseqPhantom { response_cseq, sent_cseqs, .. }) =
        &f[0].decision
    else {
        panic!("correlation evidence: {:?}", f[0].decision)
    };
    assert_eq!((*response_cseq, sent_cseqs.as_slice()), (9, [1].as_slice()));
}

/// With no request of that method sent, the mismatch is the method's own
/// concern and this rule states nothing.
#[test]
fn a_response_to_an_unsent_method_is_no_occasion() {
    let msgs = vec![resp(1_000, B, A, 200, 1, "OPTIONS", "z9hG4bK-o", "at", Some("bt"), 1)];
    assert!(run(&ResponseCorrelation, &msgs).is_empty());
}

// ── mid-dialog-tags ─────────────────────────────────────────────────────

#[test]
fn an_in_dialog_request_naming_a_local_tag_is_compliant() {
    let mut msgs = opened();
    msgs.push(req(
        4_000,
        B,
        A,
        "BYE",
        "sip:alice@h",
        1,
        "z9hG4bK-b",
        "sip:bob@h",
        "bt",
        Some("at"),
        1,
    ));
    let f = charged(run(&MidDialogTags, &msgs), B);
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
    assert!(f.iter().any(|x| x.anchor == 3), "the BYE is an occasion: {f:?}");
}

#[test]
fn an_in_dialog_request_naming_a_foreign_tag_is_violated() {
    let mut msgs = opened();
    msgs.push(req(
        4_000,
        B,
        A,
        "BYE",
        "sip:alice@h",
        1,
        "z9hG4bK-b",
        "sip:bob@h",
        "bt",
        Some("nope"),
        1,
    ));
    let f = charged(run(&MidDialogTags, &msgs), B);
    let violated = f.iter().find(|x| x.violated()).expect("a violated occasion");
    let Decision::Violated(Evidence::DialogTagForeign { tag_header, tag, local_tags, .. }) =
        &violated.decision
    else {
        panic!("tags evidence: {:?}", violated.decision)
    };
    assert_eq!((tag_header.as_str(), tag.as_str()), ("To", "nope"));
    assert_eq!(local_tags.as_slice(), ["at"]);
}

/// A vantage forwarding BOTH directions of one dialog conflates its two
/// peers, so its tag state proves nothing.
#[test]
fn a_relaying_vantage_decides_nothing() {
    // B takes A's INVITE and forwards one of its own on the same Call-ID.
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(
            1_500,
            B,
            "10.0.0.3:5080",
            "INVITE",
            "sip:bob@h",
            1,
            "z9hG4bK-j",
            "sip:alice@h",
            "at",
            None,
            1,
        ),
        resp(2_000, "10.0.0.3:5080", B, 200, 1, "INVITE", "z9hG4bK-j", "at", Some("ct"), 1),
        req(3_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", Some("nope"), 1),
    ];
    let f: Vec<Finding> = run(&MidDialogTags, &msgs).into_iter().filter(|x| x.taker == B).collect();
    assert!(!f.is_empty(), "the occasions still stand: {f:?}");
    assert!(f.iter().all(|x| !x.violated()), "a relay charges nothing: {f:?}");
}

// ── peer-uri-stable ─────────────────────────────────────────────────────

#[test]
fn a_stable_in_dialog_from_uri_is_compliant() {
    let mut msgs = opened();
    msgs.push(req(
        4_000,
        A,
        B,
        "BYE",
        "sip:bob@h",
        2,
        "z9hG4bK-b",
        "sip:alice@h",
        "at",
        Some("bt"),
        1,
    ));
    let f = charged(run(&PeerUriStable, &msgs), A);
    assert_eq!(f.len(), 1, "the BYE alone: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

#[test]
fn a_rewritten_in_dialog_from_uri_is_violated() {
    let mut msgs = opened();
    msgs.push(req(
        4_000,
        A,
        B,
        "BYE",
        "sip:bob@h",
        2,
        "z9hG4bK-b",
        "sip:eve@h",
        "at",
        Some("bt"),
        1,
    ));
    let f = charged(run(&PeerUriStable, &msgs), A);
    let Decision::Violated(Evidence::PeerUriRewritten { sent_uri, dialog_uri, .. }) =
        &f[0].decision
    else {
        panic!("uri evidence: {:?}", f[0].decision)
    };
    assert_eq!((sent_uri.as_str(), dialog_uri.as_str()), ("sip:eve@h", "sip:alice@h"));
}

/// The establishing INVITE fixes the URI; it is not judged against itself.
#[test]
fn the_establishing_invite_is_no_occasion() {
    assert!(run(&PeerUriStable, &opened()).is_empty());
}

// ── dialog-call-id-stable ───────────────────────────────────────────────

#[test]
fn an_in_dialog_message_keeping_its_call_id_is_compliant() {
    let mut msgs = opened();
    msgs.push(req(
        4_000,
        A,
        B,
        "BYE",
        "sip:bob@h",
        2,
        "z9hG4bK-b",
        "sip:alice@h",
        "at",
        Some("bt"),
        1,
    ));
    let f = charged(run(&DialogCallIdStable, &msgs), A);
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
}

#[test]
fn a_changed_call_id_within_a_dialog_is_violated() {
    let mut msgs = opened();
    let mut bye =
        req(4_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", Some("bt"), 1);
    bye.call_id = "c2".to_string();
    msgs.push(bye);
    let f = charged(run(&DialogCallIdStable, &msgs), A);
    let violated = f.iter().find(|x| x.violated()).expect("a violated occasion");
    let Decision::Violated(Evidence::DialogCallIdChanged { call_id, dialog_call_id, .. }) =
        &violated.decision
    else {
        panic!("call-id evidence: {:?}", violated.decision)
    };
    assert_eq!((call_id.as_str(), dialog_call_id.as_str()), ("c2", "c1"));
}

/// Out-of-dialog transactions reusing a From-tag (OPTIONS probes against a
/// responder minting a constant To-tag) seed no dialog and are not judged.
#[test]
fn out_of_dialog_transactions_reusing_a_tag_are_no_occasion() {
    let mut first =
        req(1_000, A, B, "OPTIONS", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1);
    first.call_id = "p1".to_string();
    let mut second =
        req(2_000, A, B, "OPTIONS", "sip:bob@h", 1, "z9hG4bK-2", "sip:alice@h", "at", Some("k"), 1);
    second.call_id = "p2".to_string();
    assert!(run(&DialogCallIdStable, &[first, second]).is_empty());
}

// ── cancel-request-uri / cancel-via-branch ──────────────────────────────

#[test]
fn a_cancel_matching_its_invite_is_compliant() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(2_000, A, B, "CANCEL", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
    ];
    assert!(charged(run(&CancelRequestUri, &msgs), A)
        .iter()
        .all(|x| matches!(x.decision, Decision::Compliant)));
    assert!(charged(run(&CancelViaBranch, &msgs), A)
        .iter()
        .all(|x| matches!(x.decision, Decision::Compliant)));
}

#[test]
fn a_cancel_naming_another_uri_is_violated() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(2_000, A, B, "CANCEL", "sip:eve@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
    ];
    let f = charged(run(&CancelRequestUri, &msgs), A);
    let Decision::Violated(Evidence::CancelUriDiverged { cancel_uri, invite_uri, .. }) =
        &f[0].decision
    else {
        panic!("cancel-uri evidence: {:?}", f[0].decision)
    };
    assert_eq!((cancel_uri.as_str(), invite_uri.as_str()), ("sip:eve@h", "sip:bob@h"));
}

/// A CANCEL on an unmatched branch is the BRANCH rule's finding; the URI
/// rule has no INVITE to compare against and states nothing.
#[test]
fn a_cancel_on_a_foreign_branch_is_the_branch_rules_finding() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(2_000, A, B, "CANCEL", "sip:eve@h", 1, "z9hG4bK-other", "sip:alice@h", "at", None, 1),
    ];
    assert!(run(&CancelRequestUri, &msgs).is_empty());
    let f = charged(run(&CancelViaBranch, &msgs), A);
    let Decision::Violated(Evidence::CancelBranchUnmatched {
        cancel_branch, invite_branches, ..
    }) = &f[0].decision
    else {
        panic!("cancel-branch evidence: {:?}", f[0].decision)
    };
    assert_eq!(cancel_branch, "z9hG4bK-other");
    assert_eq!(invite_branches.as_slice(), ["z9hG4bK-i"]);
}

/// A re-INVITE opens a second transaction, and a CANCEL matching EITHER
/// branch is fine.
#[test]
fn a_cancel_of_a_re_invite_branch_is_compliant() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(
            2_000,
            A,
            B,
            "INVITE",
            "sip:bob@h",
            2,
            "z9hG4bK-i2",
            "sip:alice@h",
            "at",
            Some("bt"),
            1,
        ),
        req(
            3_000,
            A,
            B,
            "CANCEL",
            "sip:bob@h",
            2,
            "z9hG4bK-i2",
            "sip:alice@h",
            "at",
            Some("bt"),
            1,
        ),
    ];
    let f = charged(run(&CancelViaBranch, &msgs), A);
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
}

/// Never having seen an INVITE, nothing can say the CANCEL is orphaned.
#[test]
fn a_cancel_without_any_invite_is_no_occasion() {
    let msgs =
        vec![req(1_000, A, B, "CANCEL", "sip:bob@h", 1, "z9hG4bK-c", "sip:alice@h", "at", None, 1)];
    assert!(run(&CancelViaBranch, &msgs).is_empty());
}

// ── tag-consistency ─────────────────────────────────────────────────────

#[test]
fn a_final_keeping_the_provisional_tag_is_compliant() {
    let f = charged(run(&TagConsistency, &opened()), B);
    assert_eq!(f.len(), 1, "the final alone is the occasion: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

#[test]
fn a_final_minting_a_fresh_tag_is_violated() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt2"), 1),
    ];
    let f = charged(run(&TagConsistency, &msgs), B);
    let Decision::Violated(Evidence::UasTagFlipped { final_tag, provisional_tags, status, .. }) =
        &f[0].decision
    else {
        panic!("tag-consistency evidence: {:?}", f[0].decision)
    };
    assert_eq!((*status, final_tag.as_str()), (200, "bt2"));
    assert_eq!(provisional_tags.as_slice(), ["bt"]);
}

/// A final on a transaction whose provisionals established no tag creates
/// the dialog itself and owes nothing.
#[test]
fn a_final_without_a_prior_tagged_provisional_is_no_occasion() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
    ];
    assert!(run(&TagConsistency, &msgs).is_empty());
}

/// §17.2.3: a CANCEL rides the INVITE's branch but forms its OWN server
/// transaction, so the INVITE's provisional tag commits nothing on it — the
/// answer to a CANCEL that matches no transaction states a tag of its own.
#[test]
fn a_cancel_answered_on_the_invite_branch_is_its_own_transaction() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        req(4_000, A, B, "CANCEL", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(5_000, B, A, 481, 1, "CANCEL", "z9hG4bK-i", "at", Some("fallback"), 1),
    ];
    let f = charged(run(&TagConsistency, &msgs), B);
    assert!(f.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{f:?}");
}

// ── no-to-tag-on-initial-request ────────────────────────────────────────

#[test]
fn an_initial_request_without_a_to_tag_is_compliant() {
    let f = charged(run(&NoToTagOnInitialRequest, &opened()), A);
    assert_eq!(f.len(), 1, "the INVITE alone opens one: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

#[test]
fn an_initial_request_carrying_a_to_tag_is_violated() {
    let msgs = vec![req(
        1_000,
        A,
        B,
        "REFER",
        "sip:bob@h",
        1,
        "z9hG4bK-r",
        "sip:alice@h",
        "at",
        Some("bogus"),
        1,
    )];
    let f = charged(run(&NoToTagOnInitialRequest, &msgs), A);
    let Decision::Violated(Evidence::ForbiddenHeaderPresent { on, header, value, .. }) =
        &f[0].decision
    else {
        panic!("initial-to-tag evidence: {:?}", f[0].decision)
    };
    assert_eq!((on.as_str(), header.as_str(), value.as_str()), ("REFER", "To;tag", "bogus"));
}

/// Once ANY traffic has crossed on the Call-ID, the endpoint is inside a
/// dialog and a To-tag on what it sends is legitimate.
#[test]
fn a_request_after_earlier_traffic_on_the_call_id_is_no_occasion() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(2_000, B, A, "NOTIFY", "sip:alice@h", 1, "z9hG4bK-n", "sip:bob@h", "bt", Some("at"), 1),
    ];
    let f = charged(run(&NoToTagOnInitialRequest, &msgs), B);
    assert!(f.is_empty(), "B had already taken the INVITE: {f:?}");
}

/// An intrinsically in-dialog verb is no occasion at any point — a lone BYE
/// that opens a view is the dialog rules' business, not this one's.
#[test]
fn an_in_dialog_verb_is_no_occasion() {
    let msgs = vec![req(
        1_000,
        A,
        B,
        "BYE",
        "sip:bob@h",
        2,
        "z9hG4bK-b",
        "sip:alice@h",
        "at",
        Some("bt"),
        1,
    )];
    assert!(run(&NoToTagOnInitialRequest, &msgs).is_empty());
}

// ── in-dialog-to-tag ────────────────────────────────────────────────────

/// The caller's seat after INVITE / 200(tag=bt) / ACK.
fn confirmed() -> Vec<Msg> {
    vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        req(3_000, A, B, "ACK", "sip:bob@h", 1, "z9hG4bK-k", "sip:alice@h", "at", Some("bt"), 1),
    ]
}

#[test]
fn a_tagless_request_in_a_confirmed_dialog_is_violated() {
    let mut msgs = confirmed();
    msgs.push(req(4_000, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", None, 1));
    let f = charged(run(&InDialogToTag, &msgs), A);
    let violated = f.iter().find(|x| x.violated()).expect("a violated occasion");
    let Decision::Violated(Evidence::RequiredHeaderAbsent { on, header, .. }) = &violated.decision
    else {
        panic!("in-dialog-to-tag evidence: {:?}", violated.decision)
    };
    assert_eq!((on.as_str(), header.as_str()), ("BYE", "To;tag"));
}

#[test]
fn a_tagged_request_in_a_confirmed_dialog_is_compliant() {
    let mut msgs = confirmed();
    msgs.push(req(
        4_000,
        A,
        B,
        "BYE",
        "sip:bob@h",
        2,
        "z9hG4bK-b",
        "sip:alice@h",
        "at",
        Some("bt"),
        1,
    ));
    let f = charged(run(&InDialogToTag, &msgs), A);
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
}

/// The caller's seat after INVITE / 180(tag=bt): an early dialog.
fn early() -> Vec<Msg> {
    vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
    ]
}

/// The caller's `method` at `cseq` on a fresh branch, To-tag `to`.
fn caller_request(method: &str, cseq: u32, to: Option<&str>) -> Msg {
    req(4_000, A, B, method, "sip:bob@h", cseq, "z9hG4bK-n", "sip:alice@h", "at", to, 1)
}

#[test]
fn a_tagless_request_in_an_early_dialog_is_violated() {
    let mut msgs = early();
    msgs.push(caller_request("UPDATE", 2, None));
    let f = charged(run(&InDialogToTag, &msgs), A);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
    let mut msgs = early();
    msgs.push(caller_request("UPDATE", 2, Some("bt")));
    let f = charged(run(&InDialogToTag, &msgs), A);
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
}

/// Beside an early dialog a tag-less INVITE creates a new one, and once a
/// non-2xx final ended the INVITE no early dialog is left to name.
#[test]
fn a_new_invite_or_a_request_after_a_rejection_is_no_early_occasion() {
    let mut msgs = early();
    msgs.push(caller_request("INVITE", 2, None));
    assert!(run(&InDialogToTag, &msgs).is_empty(), "a new dialog-creating INVITE");
    let mut msgs = early();
    msgs.push(resp(3_000, B, A, 486, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1));
    msgs.push(caller_request("OPTIONS", 2, None));
    assert!(run(&InDialogToTag, &msgs).is_empty(), "the rejection ended the early dialog");
}

/// The 2xx ACK is a fresh transaction the sender builds from dialog state —
/// no response to echo, so the tag is its own to write.
#[test]
fn a_tagless_2xx_ack_is_violated() {
    let mut msgs = confirmed();
    msgs[2].to_tag = None;
    let f = charged(run(&InDialogToTag, &msgs), A);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
}

/// The ACK of a non-2xx rides the INVITE's branch and echoes that
/// response's To — the responder's business, not the ACK sender's. A
/// CANCEL likewise copies the INVITE it cancels.
#[test]
fn an_echoed_ack_or_cancel_is_no_occasion() {
    let mut msgs = confirmed();
    msgs.push(req(
        4_000,
        A,
        B,
        "INVITE",
        "sip:bob@h",
        2,
        "z9hG4bK-r",
        "sip:alice@h",
        "at",
        Some("bt"),
        1,
    ));
    msgs.push(resp(5_000, B, A, 488, 2, "INVITE", "z9hG4bK-r", "at", Some("bt"), 1));
    msgs.push(req(6_000, A, B, "ACK", "sip:bob@h", 2, "z9hG4bK-r", "sip:alice@h", "at", None, 1));
    msgs.push(req(
        7_000,
        A,
        B,
        "CANCEL",
        "sip:bob@h",
        2,
        "z9hG4bK-r",
        "sip:alice@h",
        "at",
        None,
        1,
    ));
    let violated = run(&InDialogToTag, &msgs).iter().filter(|x| x.violated()).count();
    assert_eq!(violated, 0, "the echoed requests are no occasion");
}

/// Same branch as the initial INVITE: still the dialog-establishing
/// request, which is tag-less by §8.1.1.2.
#[test]
fn an_establishing_invite_retransmit_after_the_2xx_is_no_occasion() {
    let mut msgs = confirmed();
    msgs.push(req(
        4_000,
        A,
        B,
        "INVITE",
        "sip:bob@h",
        1,
        "z9hG4bK-i",
        "sip:alice@h",
        "at",
        None,
        1,
    ));
    assert!(run(&InDialogToTag, &msgs).iter().all(|x| !x.violated()), "{msgs:?}");
}

/// Only a 2xx confirms: an unanswered INVITE creates no dialog, so the
/// tag-less request that follows is correct.
#[test]
fn an_unconfirmed_dialog_opens_no_occasion() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 401, 1, "INVITE", "z9hG4bK-1", "at", Some("bt"), 1),
        req(3_000, A, B, "OPTIONS", "sip:bob@h", 2, "z9hG4bK-o", "sip:alice@h", "at", None, 1),
    ];
    assert!(run(&InDialogToTag, &msgs).is_empty());
}

/// The caller named no From-tag, so the dialog's remote tag is null and
/// §12.2.1.1 requires the To tag parameter to be omitted: the answering
/// side's tag-less BYE is the compliant one.
#[test]
fn a_peer_that_minted_no_tag_is_owed_nothing() {
    let mut invite =
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1);
    invite.from_tag = None;
    let msgs = vec![
        invite,
        resp(2_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        req(3_000, B, A, "BYE", "sip:alice@h", 1, "z9hG4bK-b", "sip:bob@h", "bt", None, 1),
    ];
    assert!(run(&InDialogToTag, &msgs).is_empty());
}

/// A vantage that carried the establishing INVITE BOTH ways forwards the
/// originator's headers: the finding belongs to that originator.
#[test]
fn a_relaying_endpoint_judges_nothing() {
    let c = "10.0.0.3:5080";
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(1_500, B, c, "INVITE", "sip:bob@h", 1, "z9hG4bK-j", "sip:alice@h", "at", None, 1),
        resp(2_000, c, B, 200, 1, "INVITE", "z9hG4bK-j", "at", Some("ct"), 1),
        req(3_000, B, c, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", "at", None, 1),
    ];
    assert!(charged(run(&InDialogToTag, &msgs), B).is_empty());
}

// ── in-dialog-from-tag ──────────────────────────────────────────────────

/// A forking callee rings on one branch (tag `b1`) and answers on another
/// that never rang (tag `b2`); the caller ACKs the answer.
fn answered_on_an_unrung_branch() -> Vec<Msg> {
    vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("b1"), 1),
        resp(3_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("b2"), 1),
        req(4_000, A, B, "ACK", "sip:bob@h", 1, "z9hG4bK-k", "sip:alice@h", "at", Some("b2"), 1),
    ]
}

/// A request the callee side sends the caller at `at_us` under From-tag
/// `from_tag`, naming the caller's tag.
fn toward_caller(at_us: u64, method: &str, cseq: u32, from_tag: &str) -> Msg {
    let branch = format!("z9hG4bK-{method}-{at_us}");
    req(at_us, B, A, method, "sip:alice@h", cseq, &branch, "sip:bob@h", from_tag, Some("at"), 1)
}

#[test]
fn a_request_in_the_answered_dialog_is_compliant() {
    let mut msgs = answered_on_an_unrung_branch();
    msgs.push(toward_caller(300_000_000, "OPTIONS", 1, "b2"));
    let f = charged(run(&InDialogFromTag, &msgs), B);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// Long after the answer, the branch that only rang has no dialog left at
/// the caller: a request under its tag names nothing she holds.
#[test]
fn a_request_under_the_rung_branch_tag_after_the_answer_is_violated() {
    let mut msgs = answered_on_an_unrung_branch();
    msgs.push(toward_caller(300_000_000, "OPTIONS", 1, "b1"));
    let f = charged(run(&InDialogFromTag, &msgs), B);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].taker, A, "read at the caller's seat");
    let Decision::Violated(Evidence::DialogRemoteTagForeign {
        foreign_from_msg,
        method,
        from_tag,
        dialog_remote_tags,
        ..
    }) = &f[0].decision
    else {
        panic!("in-dialog-from-tag evidence: {:?}", f[0].decision)
    };
    assert_eq!(*foreign_from_msg, 4);
    assert_eq!((method.as_str(), from_tag.as_str()), ("OPTIONS", "b1"));
    assert_eq!(dialog_remote_tags, &vec!["b2".to_string()]);
}

/// A tag the caller was never shown names no dialog at all.
#[test]
fn a_request_under_an_unknown_tag_is_violated() {
    let mut msgs = answered_on_an_unrung_branch();
    msgs.push(toward_caller(5_000, "BYE", 1, "zz"));
    let f = charged(run(&InDialogFromTag, &msgs), B);
    assert!(f.iter().any(Finding::violated), "{f:?}");
}

/// Inside 64·T1 of the first 2xx the rung branch's early dialog still
/// stands at the caller (§13.2.2.4): a request on it names a dialog she
/// holds, and this rule has nothing to say.
#[test]
fn a_request_on_an_early_dialog_still_standing_is_no_occasion() {
    let mut msgs = answered_on_an_unrung_branch();
    msgs.push(toward_caller(3_000 + EARLY_DIALOG_WINDOW_US, "UPDATE", 1, "b1"));
    assert!(charged(run(&InDialogFromTag, &msgs), B).is_empty());
    let mut late = answered_on_an_unrung_branch();
    late.push(toward_caller(3_001 + EARLY_DIALOG_WINDOW_US, "UPDATE", 1, "b1"));
    assert!(run(&InDialogFromTag, &late).iter().any(Finding::violated));
}

/// The answering side never BYEs an early dialog (§15) nor re-INVITEs one
/// while its INVITE is in progress (§14.1): under the rung branch's tag,
/// inside the window too, they name no dialog the caller holds.
#[test]
fn a_bye_or_invite_under_an_early_tag_is_violated_inside_the_window() {
    for method in ["BYE", "INVITE"] {
        let mut msgs = answered_on_an_unrung_branch();
        msgs.push(toward_caller(5_000, method, 1, "b1"));
        let f = charged(run(&InDialogFromTag, &msgs), B);
        assert_eq!(f.len(), 1, "{method}: {f:?}");
        assert!(f[0].violated(), "{method}: {:?}", f[0].decision);
    }
}

/// Before any 2xx, no dialog is confirmed: early-dialog traffic is no
/// occasion.
#[test]
fn a_request_before_the_answer_is_no_occasion() {
    let mut msgs = answered_on_an_unrung_branch();
    msgs.truncate(2);
    msgs.push(toward_caller(2_500, "UPDATE", 1, "b1"));
    assert!(run(&InDialogFromTag, &msgs).is_empty());
}

/// Every 2xx a forking callee sends confirms a dialog of its own
/// (§13.2.2.4): a request in either is compliant.
#[test]
fn each_confirmed_branch_is_its_own_dialog() {
    let mut msgs = answered_on_an_unrung_branch();
    msgs.push(resp(5_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("b1"), 1));
    msgs.push(toward_caller(300_000_000, "BYE", 1, "b1"));
    msgs.push(toward_caller(300_000_001, "BYE", 1, "b2"));
    let f = charged(run(&InDialogFromTag, &msgs), B);
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
}

/// At the answering seat the remote tag is the INVITE's From-tag: the
/// caller's request under another From-tag is charged to the caller.
#[test]
fn the_answering_seat_holds_the_invite_from_tag() {
    let mut msgs = answered_on_an_unrung_branch();
    let bye = |at_us, from_tag: &str| {
        req(at_us, A, B, "BYE", "sip:bob@h", 2, "z9hG4bK-b", "sip:alice@h", from_tag, Some("b2"), 1)
    };
    msgs.push(bye(5_000, "at"));
    let f = charged(run(&InDialogFromTag, &msgs), A);
    assert_eq!(f.len(), 2, "the ACK and the BYE: {f:?}");
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");

    let mut foreign = answered_on_an_unrung_branch();
    foreign.push(bye(5_000, "other"));
    let f: Vec<Finding> =
        charged(run(&InDialogFromTag, &foreign), A).into_iter().filter(Finding::violated).collect();
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!((f[0].taker.as_str(), f[0].anchor), (B, 4));
}

/// A To-tag naming no dialog the taker confirmed is [`MidDialogTags`]'
/// finding, not this one's.
#[test]
fn a_to_tag_naming_no_confirmed_dialog_is_no_occasion() {
    let mut msgs = answered_on_an_unrung_branch();
    let mut stray = toward_caller(300_000_000, "OPTIONS", 1, "b1");
    stray.to_tag = Some("not-the-callers".to_string());
    msgs.push(stray);
    assert!(charged(run(&InDialogFromTag, &msgs), B).is_empty());
}

/// A vantage carrying the establishing INVITE both ways forwards the
/// originator's headers: it decides nothing.
#[test]
fn a_relaying_taker_decides_nothing() {
    let c = "10.0.0.3:5080";
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(1_500, B, c, "INVITE", "sip:bob@h", 1, "z9hG4bK-j", "sip:alice@h", "at", None, 1),
        resp(2_000, c, B, 200, 1, "INVITE", "z9hG4bK-j", "at", Some("ct"), 1),
        req(3_000, c, B, "BYE", "sip:alice@h", 1, "z9hG4bK-b", "sip:bob@h", "xx", Some("at"), 1),
    ];
    let f = charged(run(&InDialogFromTag, &msgs), c);
    assert!(f.iter().all(|x| !x.violated()), "{f:?}");
}
