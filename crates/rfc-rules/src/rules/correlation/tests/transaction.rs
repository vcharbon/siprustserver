//! A response against the transaction it answers.

use super::*;

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
