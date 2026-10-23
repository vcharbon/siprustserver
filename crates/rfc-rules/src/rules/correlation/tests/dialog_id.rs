//! An in-dialog message against its taker's dialog identity.

use super::*;

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
