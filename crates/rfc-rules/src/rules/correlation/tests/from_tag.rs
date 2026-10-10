//! The From-tag of a request an endpoint takes.

use super::*;

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
