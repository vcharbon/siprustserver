//! The To-tag of a request an endpoint sends.

use super::*;

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
