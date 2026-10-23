//! The rules' OWN semantics under a CLOSED observation — what the adapters'
//! vantage policy cannot state: which requests form one dialog, what the
//! set alone settles about them, and where the wire settles nothing.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{AckCseqMatchesInvite, CseqInDialogOrder, ResponseCseqMatchesTransaction};

const ALICE: &str = "10.0.0.1:5060";
const BOB: &str = "10.0.0.2:5070";

/// A request alice sent bob: method, CSeq, top-Via branch and dialog To tag
/// all caller-controlled, so a test can model a new transaction (fresh
/// branch), a retransmission (reused branch), a fork (distinct To tag) or a
/// dialog-creating request (`None`).
fn req(at_us: u64, method: &str, cseq: u32, branch: &str, to_tag: Option<&str>) -> Msg {
    Msg {
        at_us,
        src: ALICE.to_string(),
        dst: BOB.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: method.to_string() },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: method.to_string(),
        from_tag: Some("fa".to_string()),
        to_tag: to_tag.map(str::to_string),
        via_branch: Some(branch.to_string()),
        head: None,
        body: None,
    }
}

/// A confirmed-dialog request (the `btag` dialog every plain-call test uses).
fn in_dialog(at_us: u64, method: &str, cseq: u32, branch: &str) -> Msg {
    req(at_us, method, cseq, branch, Some("btag"))
}

/// A new-transaction OPTIONS keepalive (branch derived from the CSeq).
fn options(at_us: u64, cseq: u32) -> Msg {
    in_dialog(at_us, "OPTIONS", cseq, &format!("z9hG4bK-{cseq}"))
}

/// A response bob sent alice on `branch`, carrying a chosen `CSeq`.
fn rsp(at_us: u64, status: u16, cseq: u32, method: &str, branch: &str) -> Msg {
    Msg {
        at_us,
        src: BOB.to_string(),
        dst: ALICE.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: method.to_string(),
        from_tag: Some("fa".to_string()),
        to_tag: Some("btag".to_string()),
        via_branch: Some(branch.to_string()),
        head: None,
        body: None,
    }
}

fn on_call(mut m: Msg, call_id: &str) -> Msg {
    m.call_id = call_id.to_string();
    m
}

fn from(mut m: Msg, from_tag: &str) -> Msg {
    m.from_tag = Some(from_tag.to_string());
    m
}

/// The same message again on the wire — what the §17.2 mark states, and
/// what the rule's own fold must reach without it.
fn again(mut m: Msg) -> Msg {
    m.repeat = true;
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

fn eval(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    rule.eval(&WireView { msgs, obs: &obs(msgs) })
}

/// The violations of one rule, in observation order.
fn hits(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    eval(rule, msgs).into_iter().filter(Finding::violated).collect()
}

fn order(msgs: &[Msg]) -> Vec<Finding> {
    hits(&CseqInDialogOrder, msgs)
}

// ── CseqInDialogOrder (RFC 3261 §12.2.1.1) ──────────────────────────────

/// INVITE(1) / OPTIONS(2) / BYE(3) on distinct branches: contiguous, none
/// reused. Every accounted request is an occasion, and each is compliant.
#[test]
fn a_contiguous_dialog_run_is_clean() {
    let msgs = [
        req(1_000, "INVITE", 1, "z9hG4bK-i", None),
        options(2_000, 2),
        in_dialog(3_000, "BYE", 3, "z9hG4bK-b"),
    ];
    let all = eval(&CseqInDialogOrder, &msgs);
    assert_eq!(all.len(), 3, "one occasion per accounted request: {all:?}");
    assert!(all.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{all:?}");
    assert_eq!(all[0].emitter, ALICE, "the UAC that generated the stream is charged");
    assert_eq!(all[0].taker, BOB, "read at the endpoint that took it");
}

/// A retransmission repeats its CSeq *and* its top-Via branch: one
/// transaction, folded away, and no second occasion.
#[test]
fn a_retransmission_is_one_transaction() {
    let msgs = [options(1_000, 2), options(2_000, 2)];
    let all = eval(&CseqInDialogOrder, &msgs);
    assert_eq!(all.len(), 1, "the repeat folds into the first: {all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant));
}

/// The fold is the rule's OWN and needs no §17.2 mark: the same wire with
/// the repeat marked decides identically, so the mark can neither add nor
/// remove a verdict.
#[test]
fn the_repeat_mark_changes_no_verdict() {
    let unmarked = eval(&CseqInDialogOrder, &[options(1_000, 2), options(2_000, 2)]);
    let marked = eval(&CseqInDialogOrder, &[options(1_000, 2), again(options(2_000, 2))]);
    assert_eq!(unmarked.len(), marked.len());
    assert!(marked.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{marked:?}");
}

/// An OPTIONS keepalive at CSeq 2, then a BYE that
/// reuses CSeq 2 on a NEW branch — a new transaction whose dialog CSeq
/// never advanced, which §17.2.3 cannot fold away.
#[test]
fn a_reuse_on_a_fresh_branch_is_violated() {
    let msgs = [options(1_000, 2), in_dialog(2_000, "BYE", 2, "z9hG4bK-y")];
    let f = order(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].rule, RuleId::CseqInDialogOrder);
    assert_eq!(f[0].emitter, ALICE);
    assert_eq!(f[0].taker, BOB);
    assert_eq!(f[0].anchor, 1, "the occasion rests on the REUSING request");
    let Decision::Violated(Evidence::CseqReused {
        cseq,
        method,
        reuse_branch,
        spent_branch,
        spent_msg,
        to_tag,
        ..
    }) = &f[0].decision
    else {
        panic!("reuse evidence: {:?}", f[0].decision)
    };
    assert_eq!((*cseq, method.as_str()), (2, "BYE"));
    assert_eq!(spent_branch.as_str(), "z9hG4bK-2", "the transaction that spent it first");
    assert_eq!(reuse_branch.as_str(), "z9hG4bK-y", "a FRESH branch, so not a retransmission");
    assert_eq!(*spent_msg, 0);
    assert_eq!(to_tag.as_str(), "btag");
}

/// The stale-snapshot takeover: the primary keepalives 2 then 3, the
/// survivor re-originates OPTIONS 2 on a fresh branch. The REUSE is the
/// violation; arriving "after" CSeq 3 is not judged on its own.
#[test]
fn a_stale_takeover_reusing_a_spent_cseq_is_violated() {
    let msgs =
        [options(1_000, 2), options(2_000, 3), in_dialog(3_000, "OPTIONS", 2, "z9hG4bK-stale")];
    let f = order(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Violated(Evidence::CseqReused { .. })));
    assert_eq!(f[0].anchor, 2);
}

/// §12.2.1.1 constrains what the UAC GENERATES, not what a recording
/// observes: a NOTIFY(3) dropped and recovered after the BYE(4) leaves no
/// hole, so the set `{2,3,4}` is contiguous and the reordering is clean.
#[test]
fn a_request_recovered_out_of_order_leaves_no_hole() {
    let msgs = [
        in_dialog(1_000, "INVITE", 2, "z9hG4bK-ri"),
        in_dialog(2_000, "BYE", 4, "z9hG4bK-b"),
        in_dialog(3_000, "NOTIFY", 3, "z9hG4bK-n"),
    ];
    assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
}

/// A +2 jump between two new requests on one dialog breaks
/// increment-by-exactly-one even though it strictly increases.
#[test]
fn a_skipped_number_is_violated() {
    let msgs = [options(1_000, 2), options(2_000, 4)];
    let f = order(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::CseqNotContiguous { cseq, prior_cseq, method, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*cseq, *prior_cseq, method.as_str()), (4, 2, "OPTIONS"));
    assert_eq!(f[0].anchor, 1, "the request that skipped ahead");
}

/// The answerer's own stream opens on the early dialog, with no
/// dialog-creating request of its own to anchor it, and runs on across the
/// 2xx: a request after the answer that skips past the early ones is
/// violated.
#[test]
fn a_skip_after_the_early_dialog_requests_is_violated() {
    let clean = [
        in_dialog(1_000, "UPDATE", 2, "z9hG4bK-u1"),
        in_dialog(2_000, "UPDATE", 3, "z9hG4bK-u2"),
        in_dialog(3_000, "INVITE", 4, "z9hG4bK-ri"),
        in_dialog(4_000, "BYE", 5, "z9hG4bK-b"),
    ];
    assert!(order(&clean).is_empty(), "{:?}", order(&clean));

    let skipped = [
        in_dialog(1_000, "UPDATE", 2, "z9hG4bK-u1"),
        in_dialog(2_000, "UPDATE", 3, "z9hG4bK-u2"),
        in_dialog(3_000, "INVITE", 5, "z9hG4bK-ri"),
        in_dialog(4_000, "BYE", 6, "z9hG4bK-b"),
    ];
    let f = order(&skipped);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::CseqNotContiguous { cseq, prior_cseq, method, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*cseq, *prior_cseq, method.as_str()), (5, 3, "INVITE"));
    assert_eq!(f[0].anchor, 2, "the request that skipped ahead");
}

/// ACK and CANCEL reuse the CSeq of the request they answer: exempt, and
/// they never advance the dialog's own sequence.
#[test]
fn ack_and_cancel_reusing_a_cseq_are_exempt() {
    let acked = [
        req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
        in_dialog(2_000, "ACK", 1, "z9hG4bK-a1"),
        in_dialog(3_000, "INVITE", 2, "z9hG4bK-i2"),
        in_dialog(4_000, "ACK", 2, "z9hG4bK-a2"),
    ];
    assert!(order(&acked).is_empty(), "{:?}", order(&acked));

    let cancelled =
        [req(1_000, "INVITE", 1, "z9hG4bK-i1", None), req(2_000, "CANCEL", 1, "z9hG4bK-c1", None)];
    assert!(order(&cancelled).is_empty(), "{:?}", order(&cancelled));
}

/// A different Call-ID, and the other direction of one dialog (a different
/// From tag), each own an independent CSeq space — no cross-stream alias.
#[test]
fn separate_streams_do_not_alias() {
    let msgs = [options(1_000, 5), on_call(options(2_000, 1), "c2"), from(options(3_000, 1), "fb")];
    assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
}

/// Forking (§12.1.2): ONE INVITE creates TWO early dialogs sharing the
/// Call-ID and From tag but carrying distinct callee To tags, each with its
/// own CSeq space seeded from the INVITE — so BOTH first PRACKs at
/// `INVITE + 1` are correct, and the confirmed fork then advances by one in
/// ITS dialog.
#[test]
fn two_forks_at_one_cseq_are_distinct_dialogs() {
    let msgs = [
        req(1_000, "INVITE", 1, "z9hG4bK-i", None),
        req(2_000, "PRACK", 2, "z9hG4bK-p1", Some("fork1")),
        req(3_000, "PRACK", 2, "z9hG4bK-p2", Some("fork2")),
        req(4_000, "BYE", 3, "z9hG4bK-b", Some("fork1")),
    ];
    assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
}

/// A forked dialog's FIRST in-dialog request must be exactly the
/// dialog-creating INVITE's CSeq + 1: a PRACK at 3 off an INVITE at 1 skips
/// a number.
#[test]
fn a_fork_first_request_off_the_invite_baseline_is_violated() {
    let msgs = [
        req(1_000, "INVITE", 1, "z9hG4bK-i", None),
        req(2_000, "PRACK", 3, "z9hG4bK-p", Some("fork1")),
    ];
    let f = order(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::CseqNotContiguous { cseq, prior_cseq, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*cseq, *prior_cseq), (3, 1), "measured off the INVITE anchor");
}

/// The per-To-tag split hides a first in-dialog request that reuses the
/// INVITE's own number — the anchor test is what catches it, and a CSeq
/// BELOW the anchor (CSeq 0 always) the same way.
#[test]
fn a_first_request_at_or_below_the_anchor_never_advanced() {
    let reuse = [
        req(1_000, "INVITE", 1, "z9hG4bK-i", None),
        req(2_000, "INFO", 1, "z9hG4bK-info", Some("btag")),
    ];
    let f = order(&reuse);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::CseqNotContiguous { cseq, prior_cseq, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*cseq, *prior_cseq), (1, 1), "at the anchor: it did not advance");

    let regressed = [
        req(1_000, "INVITE", 5, "z9hG4bK-i", None),
        req(2_000, "BYE", 3, "z9hG4bK-b", Some("btag")),
    ];
    assert_eq!(order(&regressed).len(), 1, "a CSeq that went backwards");

    let zero = [
        req(1_000, "INVITE", 1, "z9hG4bK-i", None),
        req(2_000, "INFO", 0, "z9hG4bK-info", Some("btag")),
    ];
    assert_eq!(order(&zero).len(), 1, "CSeq 0 is below any ≥1 anchor");
}

/// A deferred-auth §22.2 retry leaves BOTH attempts dialog-creating (the
/// 401 established no dialog), so the empty-To-tag bucket holds `{1,2}`.
/// The confirmed dialog anchors on the attempt that ESTABLISHED it (2) —
/// folding the abandoned attempt (1) would fabricate a `[1,3]` gap — while
/// a real skip past the establishing attempt still surfaces.
#[test]
fn an_auth_retry_anchors_on_the_establishing_attempt() {
    let clean = [
        req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
        req(2_000, "ACK", 1, "z9hG4bK-a1", Some("btag")),
        req(3_000, "INVITE", 2, "z9hG4bK-i2", None),
        req(4_000, "ACK", 2, "z9hG4bK-a2", Some("btag")),
        req(5_000, "BYE", 3, "z9hG4bK-b", Some("btag")),
    ];
    assert!(order(&clean).is_empty(), "{:?}", order(&clean));

    let skipped = [
        req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
        req(2_000, "INVITE", 2, "z9hG4bK-i2", None),
        req(3_000, "BYE", 4, "z9hG4bK-b", Some("btag")),
    ];
    let f = order(&skipped);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::CseqNotContiguous { cseq, prior_cseq, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*cseq, *prior_cseq), (4, 2), "the +2 gap off the RESENT INVITE");
}

/// `m` re-addressed onto the hop `src` → `dst`.
fn hop(src: &str, dst: &str, mut m: Msg) -> Msg {
    m.src = src.to_string();
    m.dst = dst.to_string();
    m
}

/// `m` carrying the Via stack `vias`, top first (`"<sent-by>;branch=<b>"`),
/// with its top branch the first row's.
fn vias(mut m: Msg, vias: &[&str]) -> Msg {
    let rows: String = vias.iter().map(|v| format!("Via: SIP/2.0/UDP {v}\r\n")).collect();
    m.head = Some(format!("{} sip:bob@h SIP/2.0\r\n{rows}\r\n", "INVITE").into_bytes());
    m.via_branch = vias.first().and_then(|v| v.split_once(";branch=")).map(|(_, b)| b.to_string());
    m
}

const ALICE_VIA_1: &str = "10.0.0.1:5060;branch=z9hG4bK-a1";

/// alice CANCELs her INVITE and, once its 487 is ACKed, re-offers it on
/// the same Call-ID, From tag and CSeq from the same sent-by under a new
/// bottom branch: no hop pushed a Via, so this is her own new transaction
/// on a spent number (§8.1.3.5).
#[test]
fn a_re_offer_on_a_new_bottom_branch_is_violated() {
    let msgs = [
        vias(req(1_000, "INVITE", 1, "", None), &[ALICE_VIA_1]),
        req(2_000, "CANCEL", 1, "z9hG4bK-a1", None),
        rsp(3_000, 487, 1, "INVITE", "z9hG4bK-a1"),
        req(3_100, "ACK", 1, "z9hG4bK-a1", Some("btag")),
        vias(req(2_000_000, "INVITE", 1, "", None), &["10.0.0.1:5060;branch=z9hG4bK-a2"]),
    ];
    let f = order(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Violated(Evidence::CseqReused { .. })));
    assert_eq!(f[0].anchor, 4, "the re-offer that did not advance");
}

/// A spiral (§16.3): proxy P forwards alice's INVITE to X, and X routes it
/// back to P with Call-ID, From tag and CSeq unchanged. Each hop pushed its
/// Via above alice's, so P takes the copy with her bottom Via: ONE request
/// twice (§8.2.2.2), not a UAC reusing its CSeq.
#[test]
fn a_spiralled_invite_is_the_same_request_not_a_reuse() {
    const P: &str = "10.0.0.9:5060";
    const X: &str = "10.0.0.3:5060";
    let a = "10.0.0.1:5060;branch=z9hG4bK-b0";
    let p1 = "10.0.0.9:5060;branch=z9hG4bK-b1";
    let x2 = "10.0.0.3:5060;branch=z9hG4bK-b2";
    let p3 = "10.0.0.9:5060;branch=z9hG4bK-b3";
    let invite = |at_us| req(at_us, "INVITE", 1, "", None);
    let ok = |at_us, src: &str, dst: &str, branch: &str| {
        hop(src, dst, rsp(at_us, 200, 1, "INVITE", branch))
    };
    let msgs = [
        hop(ALICE, P, vias(invite(1_000), &[a])),
        hop(P, X, vias(invite(2_000), &[p1, a])),
        hop(X, P, vias(invite(3_000), &[x2, p1, a])),
        hop(P, BOB, vias(invite(4_000), &[p3, x2, p1, a])),
        ok(5_000, BOB, P, "z9hG4bK-b3"),
        ok(5_100, P, X, "z9hG4bK-b2"),
        ok(5_200, X, P, "z9hG4bK-b1"),
        ok(5_300, P, ALICE, "z9hG4bK-b0"),
        hop(ALICE, P, in_dialog(6_000, "ACK", 1, "z9hG4bK-a0")),
        hop(P, X, in_dialog(6_100, "ACK", 1, "z9hG4bK-a1")),
        hop(X, P, in_dialog(6_200, "ACK", 1, "z9hG4bK-a2")),
        hop(P, BOB, in_dialog(6_300, "ACK", 1, "z9hG4bK-a3")),
    ];
    assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
}

/// Two branches of one fork reach the same UAS (§8.2.2.2): the copies
/// differ in their top Via and share alice's bottom one, so they are one
/// request, merged.
#[test]
fn a_merged_fork_copy_is_the_same_request() {
    let msgs = [
        vias(req(1_000, "INVITE", 1, "", None), &["10.0.0.9:5060;branch=z9hG4bK-f1", ALICE_VIA_1]),
        vias(req(2_000, "INVITE", 1, "", None), &["10.0.0.9:5060;branch=z9hG4bK-f2", ALICE_VIA_1]),
    ];
    assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
}

/// A copy whose bottom Via names another sent-by was originated by another
/// element — a transparent B2BUA's outgoing leg keeping the caller's
/// Call-ID, From tag and CSeq — and is not this UAC's reuse.
#[test]
fn another_originators_request_is_not_a_reuse() {
    let msgs = [
        vias(req(1_000, "INVITE", 1, "", None), &[ALICE_VIA_1]),
        vias(req(2_000, "INVITE", 1, "", None), &["10.0.0.7:5060;branch=z9hG4bK-x1"]),
    ];
    assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
}

/// A spiralled copy reaching the taker after it answered the first one 2xx
/// is still that request by another path: the Via stack decides, not the
/// state of the first transaction.
#[test]
fn a_spiral_copy_after_the_2xx_is_not_a_reuse() {
    let msgs = [
        vias(req(1_000, "INVITE", 1, "", None), &[ALICE_VIA_1]),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-a1"),
        vias(req(3_000, "INVITE", 1, "", None), &["10.0.0.3:5060;branch=z9hG4bK-x2", ALICE_VIA_1]),
    ];
    assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
}

/// A bottom Via naming no branch cannot show two copies to be one request:
/// the reuse reading stands.
#[test]
fn a_branchless_bottom_via_proves_no_second_path() {
    let msgs = [
        vias(req(1_000, "INVITE", 1, "", None), &["10.0.0.1:5060"]),
        vias(
            req(2_000, "INVITE", 1, "", None),
            &["10.0.0.9:5060;branch=z9hG4bK-p", "10.0.0.1:5060"],
        ),
    ];
    assert_eq!(order(&msgs).len(), 1, "{:?}", order(&msgs));
}

/// A Record-Routed route set that spirals (§16.3, §12.2): alice's BYE
/// reaches P, goes on to X and comes back to P with her bottom Via kept.
/// P takes ONE in-dialog request twice, not a dialog CSeq reuse.
#[test]
fn a_spiralled_in_dialog_request_is_not_a_reuse() {
    const P: &str = "10.0.0.9:5060";
    const X: &str = "10.0.0.3:5060";
    let a = "10.0.0.1:5060;branch=z9hG4bK-y0";
    let p1 = "10.0.0.9:5060;branch=z9hG4bK-y1";
    let x2 = "10.0.0.3:5060;branch=z9hG4bK-y2";
    let bye = |at_us| in_dialog(at_us, "BYE", 2, "");
    let msgs = [
        hop(ALICE, P, in_dialog(500, "INVITE", 1, "z9hG4bK-i")),
        hop(ALICE, P, vias(bye(1_000), &[a])),
        hop(P, X, vias(bye(2_000), &[p1, a])),
        hop(X, P, vias(bye(3_000), &[x2, p1, a])),
    ];
    assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
}

/// A proxy forwards a request with its CSeq unchanged (§16.6), so a reuse
/// its originator committed reaches the next hop through it: the finding
/// there is marked relayed, the originator's is not.
#[test]
fn a_forwarded_reuse_is_marked_relayed() {
    const LB: &str = "10.0.0.9:5060";
    const W: &str = "10.0.0.4:5060";
    let lb1 = "10.0.0.9:5060;branch=z9hG4bK-l1";
    let lb2 = "10.0.0.9:5060;branch=z9hG4bK-l2";
    let a2 = "10.0.0.1:5060;branch=z9hG4bK-a2";
    let invite = |at_us| req(at_us, "INVITE", 1, "", None);
    let msgs = [
        hop(ALICE, LB, vias(invite(1_000), &[ALICE_VIA_1])),
        hop(LB, W, vias(invite(1_100), &[lb1, ALICE_VIA_1])),
        hop(ALICE, LB, vias(invite(9_000), &[a2])),
        hop(LB, W, vias(invite(9_100), &[lb2, a2])),
    ];
    let f = order(&msgs);
    assert_eq!(f.len(), 2, "the reuse at each hop: {f:?}");
    let at = |emitter: &str| f.iter().find(|x| x.emitter == emitter).expect(emitter);
    assert!(!at(ALICE).relayed, "alice originated the reuse: {f:?}");
    assert!(at(LB).relayed, "the LB forwarded it: {f:?}");
}

/// A proxy that rewrites the CSeq it forwards breaks §16.6 on its own
/// account: alice's INFO 3 leaves P as INFO 2, a number P already used in
/// that dialog. Nothing in the view shows that request arriving at P, so
/// P originated the reuse and is charged as such.
#[test]
fn a_proxy_rewriting_the_cseq_is_charged_as_originator() {
    const P: &str = "10.0.0.9:5060";
    let info = |at_us, cseq, branch| in_dialog(at_us, "INFO", cseq, branch);
    let (n2, n3) = ("10.0.0.1:5060;branch=z9hG4bK-n2", "10.0.0.1:5060;branch=z9hG4bK-n3");
    let msgs = [
        hop(ALICE, P, vias(info(1_000, 2, ""), &[n2])),
        hop(P, BOB, vias(info(1_100, 2, ""), &["10.0.0.9:5060;branch=z9hG4bK-q2", n2])),
        hop(ALICE, P, vias(info(2_000, 3, ""), &[n3])),
        hop(P, BOB, vias(info(2_100, 2, ""), &["10.0.0.9:5060;branch=z9hG4bK-q3", n3])),
    ];
    let f = order(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, P);
    assert!(!f[0].relayed, "P rewrote the CSeq: {f:?}");
}

/// A view that carries only the forwarded copies shows no request arriving
/// at the forwarder: without that evidence the reuse is charged to it.
#[test]
fn a_one_sided_view_charges_the_forwarder() {
    const LB: &str = "10.0.0.9:5060";
    const W: &str = "10.0.0.4:5060";
    let invite = |at_us| req(at_us, "INVITE", 1, "", None);
    let msgs = [
        hop(LB, W, vias(invite(1_100), &["10.0.0.9:5060;branch=z9hG4bK-l1", ALICE_VIA_1])),
        hop(
            LB,
            W,
            vias(
                invite(9_100),
                &["10.0.0.9:5060;branch=z9hG4bK-l2", "10.0.0.1:5060;branch=z9hG4bK-a2"],
            ),
        ),
    ];
    let f = order(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(!f[0].relayed, "no arrival at the LB is in view: {f:?}");
}

/// A view merged from several captures can order the forwarded copy ahead
/// of its arrival. Attribution does not follow view order: the copy nearer
/// the origin (the shorter Via stack) is the evidence for the other.
#[test]
fn relay_attribution_does_not_follow_view_order() {
    const LB: &str = "10.0.0.9:5060";
    const W: &str = "10.0.0.4:5060";
    let a2 = "10.0.0.1:5060;branch=z9hG4bK-a2";
    let invite = |at_us| req(at_us, "INVITE", 1, "", None);
    let msgs = [
        hop(ALICE, LB, vias(invite(1_000), &[ALICE_VIA_1])),
        hop(LB, W, vias(invite(1_100), &["10.0.0.9:5060;branch=z9hG4bK-l1", ALICE_VIA_1])),
        hop(LB, W, vias(invite(8_900), &["10.0.0.9:5060;branch=z9hG4bK-l2", a2])),
        hop(ALICE, LB, vias(invite(9_000), &[a2])),
    ];
    let f = order(&msgs);
    assert_eq!(f.len(), 2, "the reuse at each hop: {f:?}");
    let at = |emitter: &str| f.iter().find(|x| x.emitter == emitter).expect(emitter);
    assert!(!at(ALICE).relayed, "alice originated the reuse: {f:?}");
    assert!(at(LB).relayed, "the LB forwarded it: {f:?}");
}

/// Two captures of one request at the same Via depth from two emitters —
/// alice seen on both sides of a NAT — are neither nearer the origin than
/// the other: no copy is relayed, and the reuse is charged at each.
#[test]
fn copies_at_the_same_depth_are_not_relayed() {
    const INSIDE: &str = "192.168.1.10:5060";
    const OUTSIDE: &str = "203.0.113.5:5060";
    const T: &str = "10.0.0.4:5060";
    let a2 = "10.0.0.1:5060;branch=z9hG4bK-a2";
    let invite = |at_us| req(at_us, "INVITE", 1, "", None);
    let msgs = [
        hop(INSIDE, T, vias(invite(1_000), &[ALICE_VIA_1])),
        hop(OUTSIDE, BOB, vias(invite(1_100), &[ALICE_VIA_1])),
        hop(INSIDE, T, vias(invite(9_000), &[a2])),
        hop(OUTSIDE, BOB, vias(invite(9_100), &[a2])),
    ];
    let f = order(&msgs);
    assert_eq!(f.len(), 2, "the reuse at each capture: {f:?}");
    assert!(f.iter().all(|x| !x.relayed), "neither copy is nearer the origin: {f:?}");
}

/// The evidence must be the same request: a forwarder that changes the
/// method of what it forwards sends a request nothing in view carried
/// before, and is charged as its originator.
#[test]
fn a_forwarder_changing_the_method_is_no_relay() {
    const P: &str = "10.0.0.9:5060";
    let (n2, n3) = ("10.0.0.1:5060;branch=z9hG4bK-n2", "10.0.0.1:5060;branch=z9hG4bK-n3");
    let msgs = [
        hop(ALICE, P, vias(in_dialog(1_000, "INFO", 2, ""), &[n2])),
        hop(
            P,
            BOB,
            vias(in_dialog(1_100, "INFO", 2, ""), &["10.0.0.9:5060;branch=z9hG4bK-q2", n2]),
        ),
        hop(ALICE, P, vias(in_dialog(2_000, "MESSAGE", 2, ""), &[n3])),
        hop(
            P,
            BOB,
            vias(in_dialog(2_100, "INFO", 2, ""), &["10.0.0.9:5060;branch=z9hG4bK-q3", n3]),
        ),
    ];
    let f = order(&msgs);
    let at_bob: Vec<_> = f.iter().filter(|x| x.emitter == P).collect();
    assert_eq!(at_bob.len(), 1, "{f:?}");
    assert!(!at_bob[0].relayed, "P forwarded a MESSAGE as an INFO: {f:?}");
}

/// The same for the bottom Via's sent-by, pinned on a skipped number: a
/// reuse cannot pin it, since a copy naming another bottom host reads as
/// another originator's request and records no reuse at all.
#[test]
fn a_forwarder_changing_the_bottom_sent_by_is_no_relay() {
    const P: &str = "10.0.0.9:5060";
    let (n2, n4) = ("10.0.0.1:5060;branch=z9hG4bK-n2", "10.0.0.1:5060;branch=z9hG4bK-n4");
    let msgs = [
        hop(ALICE, P, vias(in_dialog(1_000, "INFO", 2, ""), &[n2])),
        hop(
            P,
            BOB,
            vias(in_dialog(1_100, "INFO", 2, ""), &["10.0.0.9:5060;branch=z9hG4bK-q2", n2]),
        ),
        hop(ALICE, P, vias(in_dialog(2_000, "INFO", 4, ""), &[n4])),
        hop(
            P,
            BOB,
            vias(
                in_dialog(2_100, "INFO", 4, ""),
                &["10.0.0.9:5060;branch=z9hG4bK-q4", "10.0.0.77:5060;branch=z9hG4bK-n4"],
            ),
        ),
    ];
    let f = order(&msgs);
    let at_bob: Vec<_> = f.iter().filter(|x| x.emitter == P).collect();
    assert_eq!(at_bob.len(), 1, "the skip at bob: {f:?}");
    assert!(!at_bob[0].relayed, "P rewrote the bottom sent-by: {f:?}");
}

/// A UA on TCP may send each connection from a new ephemeral port and name
/// it in its sent-by (§18.1.1): the same host under a new bottom branch is
/// still that UA's reuse.
#[test]
fn a_new_ephemeral_port_is_the_same_originator() {
    let msgs = [
        vias(req(1_000, "INVITE", 1, "", None), &["10.0.0.1:49152;branch=z9hG4bK-a1"]),
        vias(req(9_000, "INVITE", 1, "", None), &["10.0.0.1:49170;branch=z9hG4bK-a2"]),
    ];
    assert_eq!(order(&msgs).len(), 1, "{:?}", order(&msgs));
}

/// A copy whose Via stack this vantage did not carry proves neither a
/// second path nor another originator: the reuse reading of two
/// transactions on one number stands.
#[test]
fn a_copy_without_a_readable_via_stack_is_a_reuse() {
    let msgs = [
        vias(req(1_000, "INVITE", 1, "", None), &[ALICE_VIA_1]),
        req(2_000, "INVITE", 1, "z9hG4bK-f2", None),
    ];
    assert_eq!(order(&msgs).len(), 1, "{:?}", order(&msgs));
}

/// A DIFFERENT request on a spent number is a reuse even while the first
/// is pending: §8.2.2.2 names one request by its CSeq number AND method.
#[test]
fn another_method_on_a_pending_number_is_violated() {
    let msgs =
        [req(1_000, "INVITE", 1, "z9hG4bK-i1", None), req(2_000, "OPTIONS", 1, "z9hG4bK-o", None)];
    let f = order(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Violated(Evidence::CseqReused { .. })));
}

/// A request the vantage carried no branch for cannot be folded, so its
/// numbers still join the dialog's set: contiguity is judged, and only a
/// same-number pair goes unjudged because nothing tells the two
/// transactions apart.
#[test]
fn a_branchless_request_still_joins_the_set() {
    let branchless = |mut m: Msg| {
        m.via_branch = None;
        m
    };
    let gap = [branchless(options(1_000, 2)), branchless(options(2_000, 4))];
    assert_eq!(order(&gap).len(), 1, "the gap is still judged");

    let pair = [branchless(options(1_000, 2)), branchless(options(2_000, 2))];
    assert!(order(&pair).is_empty(), "nothing separates the two transactions");
}

// ── ResponseCseqMatchesTransaction (RFC 3261 §8.1.3.5) ──────────────────

/// A response copying its request's CSeq verbatim is the obligation met.
#[test]
fn a_response_copying_its_requests_cseq_is_compliant() {
    let msgs =
        [in_dialog(1_000, "PRACK", 2, "z9hG4bK-brP"), rsp(2_000, 200, 2, "PRACK", "z9hG4bK-brP")];
    let all = eval(&ResponseCseqMatchesTransaction, &msgs);
    assert_eq!(all.len(), 1, "one occasion, the response: {all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
}

/// The forking corruption: the INVITE is CSeq 1 on branch brI, and a
/// `200 (INVITE)` carrying the PRACK's number comes back on that branch —
/// no request on that transaction had it.
#[test]
fn a_response_cseq_no_request_carried_is_violated() {
    let msgs =
        [in_dialog(1_000, "INVITE", 1, "z9hG4bK-brI"), rsp(2_000, 200, 2, "INVITE", "z9hG4bK-brI")];
    let f = hits(&ResponseCseqMatchesTransaction, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, BOB, "the endpoint that sent the response is charged");
    assert_eq!(f[0].taker, ALICE);
    let Decision::Violated(Evidence::ResponseCseqUnmatched {
        status,
        response_cseq,
        response_method,
        txn_cseqs,
        branch,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*status, *response_cseq, response_method.as_str()), (200, 2, "INVITE"));
    assert_eq!(txn_cseqs.as_slice(), ["1 INVITE"], "what the transaction did carry");
    assert_eq!(branch.as_str(), "z9hG4bK-brI");
}

/// A CANCEL shares its INVITE's branch (§9.1), so one transaction key
/// carries two CSeq methods — and each side's own 200 matches one of them.
#[test]
fn a_cancel_sharing_its_invites_branch_matches_either_cseq() {
    let msgs = [
        req(1_000, "INVITE", 1, "z9hG4bK-c", None),
        req(2_000, "CANCEL", 1, "z9hG4bK-c", None),
        rsp(3_000, 200, 1, "CANCEL", "z9hG4bK-c"),
        rsp(4_000, 487, 1, "INVITE", "z9hG4bK-c"),
    ];
    assert!(
        hits(&ResponseCseqMatchesTransaction, &msgs).is_empty(),
        "{:?}",
        hits(&ResponseCseqMatchesTransaction, &msgs)
    );
}

/// A branch whose requests this vantage never carried settles nothing, and
/// neither does a response with no branch at all — undecidable, never a
/// guess, and counted so the conservatism shows.
#[test]
fn a_response_the_vantage_cannot_place_is_undecidable() {
    let unseen = [rsp(1_000, 200, 7, "OPTIONS", "z9hG4bK-unseen")];
    let f = eval(&ResponseCseqMatchesTransaction, &unseen);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(!f[0].decided(), "{:?}", f[0].decision);

    let branchless = {
        let mut m = rsp(1_000, 200, 7, "OPTIONS", "z9hG4bK-x");
        m.via_branch = None;
        [m]
    };
    let f = eval(&ResponseCseqMatchesTransaction, &branchless);
    assert!(
        matches!(f[0].decision, Decision::Undecidable("no via branch at this vantage")),
        "{:?}",
        f[0].decision
    );
}

// ── AckCseqMatchesInvite (RFC 3261 §13.2.2.4) ───────────────────────────

/// Each ACK reuses its own INVITE's CSeq — the obligation met, twice.
#[test]
fn an_ack_reusing_its_invites_cseq_is_compliant() {
    let msgs = [
        req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
        in_dialog(2_000, "ACK", 1, "z9hG4bK-a1"),
        in_dialog(3_000, "INVITE", 4, "z9hG4bK-i2"),
        in_dialog(4_000, "ACK", 4, "z9hG4bK-a2"),
    ];
    let all = eval(&AckCseqMatchesInvite, &msgs);
    assert_eq!(all.len(), 2, "one occasion per ACK: {all:?}");
    assert!(all.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{all:?}");
}

/// The bug: an early PRACK/UPDATE advanced the dialog CSeq, so the 2xx ACK
/// lands at a number no INVITE used and a real UAS cannot match it to the
/// INVITE server transaction.
#[test]
fn an_ack_on_a_number_no_invite_used_is_violated() {
    let msgs =
        [req(1_000, "INVITE", 1, "z9hG4bK-i", None), in_dialog(2_000, "ACK", 3, "z9hG4bK-a")];
    let f = hits(&AckCseqMatchesInvite, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, ALICE, "the endpoint that sent the ACK is charged");
    assert_eq!(f[0].taker, BOB);
    let Decision::Violated(Evidence::AckCseqUnmatched { ack_cseq, invite_cseqs, from_tag, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*ack_cseq, 3);
    assert_eq!(invite_cseqs.as_slice(), [1], "the INVITE it owed");
    assert_eq!(from_tag.as_str(), "fa");
}

/// An ACK is judged against the INVITEs the stream had carried BY THEN, so
/// a stream with none yet settles nothing.
#[test]
fn an_ack_before_any_invite_is_undecidable() {
    let f = eval(&AckCseqMatchesInvite, &[in_dialog(1_000, "ACK", 9, "z9hG4bK-a")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(f[0].decision, Decision::Undecidable("no INVITE on this stream at this vantage")),
        "{:?}",
        f[0].decision
    );
}

/// The ACK rule reads every copy the taker took: §12.2.1.1's fold is not
/// its fold, so a retransmitted bad ACK is charged as often as it lands.
#[test]
fn a_retransmitted_ack_is_judged_every_time() {
    let msgs = [
        req(1_000, "INVITE", 1, "z9hG4bK-i", None),
        in_dialog(2_000, "ACK", 3, "z9hG4bK-a"),
        again(in_dialog(3_000, "ACK", 3, "z9hG4bK-a")),
    ];
    assert_eq!(hits(&AckCseqMatchesInvite, &msgs).len(), 2, "one per arrival");
}
