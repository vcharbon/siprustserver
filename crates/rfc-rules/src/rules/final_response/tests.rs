//! The rule's OWN semantics under a CLOSED observation — what the live
//! adapter's relay-lane and subject policy cannot state: which emissions
//! form one transaction, and what the wire alone settles about them.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{No1xxAfterFinal, SingleFinalPerServerTxn};

const UAC: &str = "10.0.0.1:5060";
const UAS: &str = "10.0.0.2:5060";

/// A response the UAS sent on `branch` for a `method` transaction.
fn rsp(at_us: u64, status: u16, branch: &str, method: &str) -> Msg {
    Msg {
        at_us,
        src: UAS.to_string(),
        dst: UAC.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status },
        call_id: "c1".to_string(),
        cseq: 1,
        cseq_method: method.to_string(),
        from_tag: Some("fa".to_string()),
        to_tag: Some("tb".to_string()),
        via_branch: Some(branch.to_string()),
        head: None,
        body: None,
    }
}

/// The INVITE-transaction response every test but the method ones uses.
fn inv(at_us: u64, status: u16) -> Msg {
    rsp(at_us, status, "z9hG4bK-1", "INVITE")
}

/// The same message again on the wire — what §17.2.1 obliges, not a second
/// answer.
fn again(mut m: Msg) -> Msg {
    m.repeat = true;
    m
}

fn on_call(mut m: Msg, call_id: &str) -> Msg {
    m.call_id = call_id.to_string();
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

fn eval(msgs: &[Msg]) -> Vec<crate::verdict::Finding> {
    SingleFinalPerServerTxn.eval(&WireView { msgs, obs: &obs(msgs) })
}

/// The violation: the transaction answered 487, the caller ACKed and moved
/// on, and a later decision authored a 480 on the same transaction.
#[test]
fn a_second_final_of_another_status_is_violated() {
    let f = eval(&[inv(1_000, 487), inv(3_000, 480)]);
    assert_eq!(f.len(), 1, "one occasion, the transaction: {f:?}");
    assert_eq!(f[0].rule, RuleId::SingleFinalPerServerTxn);
    assert_eq!(f[0].emitter, UAS, "the endpoint that answered twice is charged");
    assert_eq!(f[0].taker, UAC);
    assert!(!f[0].relayed);
    assert_eq!(f[0].anchor, 1, "the occasion rests on the divergent final");
    let Decision::Violated(Evidence::MultipleFinals {
        first_status,
        second_status,
        divergent,
        gap_us,
        method,
        branch,
        ..
    }) = &f[0].decision
    else {
        panic!("multiple-finals evidence: {:?}", f[0].decision)
    };
    assert_eq!((*first_status, *second_status), (487, 480));
    assert_eq!(divergent.as_slice(), [480]);
    assert_eq!(*gap_us, 2_000);
    assert_eq!((method.as_str(), branch.as_str()), ("INVITE", "z9hG4bK-1"));
}

/// One answer has nothing to disagree with: no occasion at all.
#[test]
fn a_single_final_is_no_occasion() {
    assert!(eval(&[inv(1_000, 200)]).is_empty());
}

/// §17.2.1 Timer G repeats the non-2xx final and §13.3.1.4 the 2xx — a
/// message marked as that repeat is the same answer, so the transaction
/// still has only one and is no occasion.
#[test]
fn a_retransmitted_final_is_not_a_second_answer() {
    let f = eval(&[inv(1_000, 486), again(inv(2_000, 486)), again(inv(3_000, 486))]);
    assert!(f.is_empty(), "{f:?}");
}

/// A second final the vantage did NOT mark a repeat still costs an
/// occasion — and answering the same status again is compliant, the
/// denominator a report reads the violation rate against.
#[test]
fn a_second_final_of_the_same_status_is_compliant() {
    let f = eval(&[inv(1_000, 486), inv(2_000, 486)]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// Every divergent status is reported exactly once, ALL on one finding: the
/// occasion is the transaction, so a retransmitted offence adds nothing and
/// a genuinely third status joins the same evidence rather than opening a
/// second occasion.
#[test]
fn every_divergent_status_is_reported_exactly_once_on_one_finding() {
    let retransmitted = eval(&[inv(1_000, 487), inv(2_000, 480), again(inv(3_000, 480))]);
    assert_eq!(retransmitted.len(), 1, "{retransmitted:?}");
    let Decision::Violated(Evidence::MultipleFinals { divergent, .. }) = &retransmitted[0].decision
    else {
        panic!("{:?}", retransmitted[0].decision)
    };
    assert_eq!(divergent.as_slice(), [480], "one recording of the offending status");
    assert_eq!(retransmitted[0].anchor, 1, "the FIRST copy of the offending final");

    let three = eval(&[inv(1_000, 487), inv(2_000, 480), again(inv(3_000, 480)), inv(4_000, 486)]);
    assert_eq!(three.len(), 1, "a third status joins the occasion: {three:?}");
    let Decision::Violated(Evidence::MultipleFinals { divergent, second_status, .. }) =
        &three[0].decision
    else {
        panic!("{:?}", three[0].decision)
    };
    assert_eq!(divergent.as_slice(), [480, 486], "both, in emission order");
    assert_eq!(*second_status, 480, "the offence is where the answers first parted");
    assert_eq!(three[0].anchor, 1);
}

/// A repeat the vantage failed to mark is still one recording: the same
/// status never enters the evidence twice.
#[test]
fn an_unmarked_duplicate_offence_is_recorded_once() {
    let f = eval(&[inv(1_000, 487), inv(2_000, 480), inv(3_000, 480)]);
    let Decision::Violated(Evidence::MultipleFinals { divergent, .. }) = &f[0].decision else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(divergent.as_slice(), [480]);
}

/// A 1xx is not a final: the transaction below answered once, and the
/// provisional that followed is a different obligation's finding.
#[test]
fn a_provisional_after_the_final_is_not_judged() {
    assert!(eval(&[inv(1_000, 200), inv(2_000, 183)]).is_empty());
}

/// A CANCEL reuses its INVITE's branch (§9.1): the `200 (CANCEL)` and the
/// `487 (INVITE)` are two server transactions, each answered once — folding
/// them on the branch alone would invent a violation.
#[test]
fn a_cancel_200_and_its_invite_487_are_two_transactions() {
    let f = eval(&[rsp(1_000, 200, "z9hG4bK-c", "CANCEL"), rsp(2_000, 487, "z9hG4bK-c", "INVITE")]);
    assert!(f.is_empty(), "{f:?}");
}

/// A view carries many calls, and fixtures reuse hard-coded branches: the
/// Call-ID keeps two singly-answered transactions from folding into one.
#[test]
fn two_calls_reusing_one_branch_are_distinct_transactions() {
    let f = eval(&[on_call(inv(1_000, 200), "call-a"), on_call(inv(2_000, 486), "call-b")]);
    assert!(f.is_empty(), "{f:?}");
}

/// §17.2.2: the one-final rule is not INVITE-specific — a BYE server
/// transaction answered twice with different statuses is a violation.
#[test]
fn a_non_invite_server_transaction_is_judged() {
    let f = eval(&[rsp(1_000, 200, "z9hG4bK-n", "BYE"), rsp(2_000, 481, "z9hG4bK-n", "BYE")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
}

/// 64·T1 over UDP, in µs: Timer J, Timer H and Timer L.
const T1_64_US: u64 = 32_000_000;
/// T4 over UDP, in µs: Timer I.
const T4_US: u64 = 5_000_000;

/// A request the UAC sent the UAS on `branch` for a `method` transaction:
/// the arrival that opens a server transaction (§17.2.3).
fn req(at_us: u64, branch: &str, method: &str) -> Msg {
    let mut m = rsp(at_us, 0, branch, method);
    std::mem::swap(&mut m.src, &mut m.dst);
    m.kind = Kind::Request { method: method.to_string() };
    m
}

/// §17.2.2, §17.2.3: a non-INVITE server transaction ends at Timer J, 64·T1
/// after its final, and a new one exists only once a request on its key
/// arrives after that end. A final past Timer J with no such request is the
/// old transaction's second final; one after such a request is the new
/// transaction's one final.
#[test]
fn a_non_invite_server_transaction_ends_at_timer_j() {
    let cancel = |at_us, status| rsp(at_us, status, "z9hG4bK-c", "CANCEL");
    let copy = |at_us| req(at_us, "z9hG4bK-c", "CANCEL");
    let ends = 1_000 + T1_64_US;
    let inside = eval(&[cancel(1_000, 200), cancel(ends - 1, 481)]);
    assert!(inside.len() == 1 && inside[0].violated(), "inside Timer J: {inside:?}");

    let unasked = eval(&[cancel(1_000, 200), cancel(ends + 1_000, 481)]);
    assert!(unasked.len() == 1 && unasked[0].violated(), "no request opened one: {unasked:?}");

    let early_copy = eval(&[cancel(1_000, 200), copy(ends - 1), cancel(ends + 1_000, 481)]);
    assert!(early_copy.len() == 1 && early_copy[0].violated(), "inside J: {early_copy:?}");

    let asked = eval(&[cancel(1_000, 200), copy(ends), cancel(ends + 1_000, 481)]);
    assert!(asked.is_empty(), "two transactions, each answered once: {asked:?}");

    // The new transaction is judged on its own: its own second final, of
    // another status, is its own violation.
    let later = ends + 1_000;
    let f = eval(&[cancel(1_000, 200), copy(ends), cancel(later, 481), cancel(later + 1_000, 500)]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::MultipleFinals { first_status, second_status, .. }) =
        &f[0].decision
    else {
        panic!("multiple-finals evidence: {:?}", f[0].decision)
    };
    assert_eq!((*first_status, *second_status), (481, 500));
}

/// §17.2.1: an INVITE server transaction answered non-2xx ends at Timer H
/// then Timer I, 64·T1 + T4 after its final at the latest; one answered 2xx
/// at 64·T1, the end of the 2xx's retransmission (§13.3.1.4). A later final
/// with no INVITE copy arrived after that end is still its second final.
#[test]
fn an_invite_server_transaction_ends_with_its_own_timers() {
    let copy = |at_us| req(at_us, "z9hG4bK-1", "INVITE");
    let ends = 1_000 + T1_64_US + T4_US;
    let inside = eval(&[inv(1_000, 486), copy(ends - 1), inv(ends, 480)]);
    assert!(inside.len() == 1 && inside[0].violated(), "inside Timer I: {inside:?}");
    let past = eval(&[inv(1_000, 486), copy(ends), inv(ends + 1_000, 480)]);
    assert!(past.is_empty(), "past Timer I, a new INVITE: {past:?}");

    let ends = 1_000 + T1_64_US;
    let unasked = eval(&[inv(1_000, 200), inv(ends + 1_000, 408)]);
    assert!(unasked.len() == 1 && unasked[0].violated(), "a 2xx then a 408: {unasked:?}");
    let inside = eval(&[inv(1_000, 200), copy(ends - 1), inv(ends, 481)]);
    assert!(inside.len() == 1 && inside[0].violated(), "inside 64·T1: {inside:?}");
    let past = eval(&[inv(1_000, 200), copy(ends), inv(ends + 1_000, 481)]);
    assert!(past.is_empty(), "past 64·T1, a new INVITE: {past:?}");
}

/// Without a branch the finals cannot be split into transactions, so an
/// endpoint that answered more than once at this vantage is UNDECIDABLE —
/// the rule never guesses which answers belonged together.
#[test]
fn finals_without_a_via_branch_are_undecidable() {
    let branchless = |m: Msg| {
        let mut m = m;
        m.via_branch = None;
        m
    };
    let f = eval(&[branchless(inv(1_000, 487)), branchless(inv(2_000, 480))]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(f[0].decision, Decision::Undecidable("no via branch at this vantage")),
        "{:?}",
        f[0].decision
    );
    assert!(!f[0].decided());

    // One branchless final is no occasion either: there is nothing the
    // branch would have had to settle.
    assert!(eval(&[branchless(inv(1_000, 200))]).is_empty());
}

// ── No1xxAfterFinal (RFC 3261 §13.3.1.1 / §17.2.1) ──────────────────────

/// A provisional presenting a chosen early dialog.
fn early(at_us: u64, status: u16, to_tag: &str) -> Msg {
    let mut m = inv(at_us, status);
    m.to_tag = Some(to_tag.to_string());
    m
}

fn late_1xx(msgs: &[Msg]) -> Vec<crate::verdict::Finding> {
    No1xxAfterFinal.eval(&WireView { msgs, obs: &obs(msgs) })
}

/// The violation: 180, then the 200 that resolved the offer, then a NEW 181
/// on a transaction that is already complete.
#[test]
fn a_new_provisional_after_the_final_is_violated() {
    let f = late_1xx(&[early(1_000, 180, "bt"), inv(2_000, 200), early(3_000, 181, "bt")]);
    assert_eq!(f.len(), 2, "both provisionals are occasions: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[1].rule, RuleId::No1xxAfterFinal);
    assert_eq!(f[1].emitter, UAS, "the transaction's UAS side is charged");
    assert_eq!(f[1].anchor, 2, "the occasion rests on the late provisional");
    let Decision::Violated(Evidence::LateProvisional {
        status,
        completed_by_status,
        completed_by_msg,
        gap_us,
        branch,
        ..
    }) = &f[1].decision
    else {
        panic!("{:?}", f[1].decision)
    };
    assert_eq!((*status, *completed_by_status, *completed_by_msg), (181, 200, 1));
    assert_eq!(*gap_us, 1_000);
    assert_eq!(branch.as_str(), "z9hG4bK-1");
}

/// A provisional is identified by `(status, To tag)`, so a retransmitted
/// final and a retransmitted provisional — one first emitted PRE-final,
/// whose copy the vantage recorded after — never fire.
#[test]
fn retransmitted_finals_and_provisionals_are_silent() {
    assert!(late_1xx(&[
        early(1_000, 180, "bt"),
        inv(2_000, 200),
        inv(3_000, 200),
        early(4_000, 180, "bt"),
    ])
    .iter()
    .all(|f| !f.violated()));
}

/// Two distinct early dialogs before the winner: the multi-fork happy
/// path, both compliant.
#[test]
fn several_early_dialogs_before_the_final_are_compliant() {
    let f = late_1xx(&[early(1_000, 180, "bt1"), early(2_000, 183, "bt2"), inv(3_000, 200)]);
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(f.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{f:?}");
}

/// A 100 Trying establishes no early dialog: §17.2.1 has the transaction
/// replay it to a retransmitted INVITE, so it is never an occasion.
#[test]
fn a_100_trying_after_the_final_is_not_judged() {
    assert!(late_1xx(&[inv(1_000, 200), inv(2_000, 100)]).is_empty());
}

/// Two calls reusing one branch are two transactions: a provisional on the
/// second is not "after" the first's final.
#[test]
fn two_calls_reusing_one_branch_do_not_fold() {
    let f =
        late_1xx(&[on_call(inv(1_000, 200), "call-a"), on_call(early(2_000, 180, "bt"), "call-b")]);
    assert!(f.iter().all(|f| !f.violated()), "{f:?}");
}

/// Without a branch nothing says which final a provisional came after: no
/// occasion at all.
#[test]
fn a_branchless_provisional_is_no_occasion() {
    let mut late = early(2_000, 180, "bt");
    late.via_branch = None;
    assert!(late_1xx(&[inv(1_000, 200), late]).is_empty());
}
