//! The family's OWN semantics under a CLOSED observation — what the capture
//! conformance pins in `sip_pcap::rfc::cancel` and the live adapter's lane
//! policy cannot state: the crossing race, how a branch pairs a CANCEL with
//! its INVITE, and what the wire alone settles about each pairing.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{CancelAfter1xx, CancelRouteEchoesInvite, No200AfterCancel, NoCancelAfterFinal};

const UAC: &str = "10.0.0.1:5060";
const UAS: &str = "10.0.0.2:5060";
/// The transaction every `req` / `rsp` rides unless a test re-homes it.
const B1: &str = "z9hG4bK-1";
const B2: &str = "z9hG4bK-2";

/// `m` re-homed onto the transaction named by top-Via `branch`.
fn on(branch: &str, mut m: Msg) -> Msg {
    m.via_branch = Some(branch.to_string());
    m
}

fn msg(at_us: u64, src: &str, dst: &str, kind: Kind, cseq_method: &str) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind,
        call_id: "c1".to_string(),
        cseq: 1,
        cseq_method: cseq_method.to_string(),
        via_branch: Some(B1.to_string()),
        from_tag: Some("fa".to_string()),
        to_tag: None,
        head: None,
        body: None,
    }
}

fn req(at_us: u64, method: &str) -> Msg {
    msg(at_us, UAC, UAS, Kind::Request { method: method.to_string() }, method)
}

fn rsp(at_us: u64, status: u16) -> Msg {
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, "INVITE");
    m.to_tag = Some("tb".to_string());
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

/// The violation, under a closed observation: the UAS took the CANCEL and
/// answered 200 anyway. No window collapses here — the offence is a message
/// PRESENT on the wire, so closing the observation changes nothing.
#[test]
fn a_2xx_after_the_cancel_is_violated_under_a_closed_observation() {
    let msgs = vec![req(1_000, "INVITE"), rsp(3_000, 180), req(4_000, "CANCEL"), rsp(6_000, 200)];
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "one occasion, the cancelled transaction: {f:?}");
    assert_eq!(f[0].rule, RuleId::No200AfterCancel);
    assert_eq!(f[0].emitter, UAS, "the UAS that answered is charged");
    assert_eq!(f[0].taker, UAC);
    assert!(!f[0].relayed);
    let Decision::Violated(Evidence::Cancelled { gap_us, status, .. }) = &f[0].decision else {
        panic!("cancel evidence: {:?}", f[0].decision)
    };
    assert_eq!((*gap_us, *status), (2_000, 200));
}

/// The crossing race stays COMPLIANT live: the 200 was already on the wire
/// when the CANCEL arrived, so the UAS had nothing to answer 487 to. The
/// occasion is still decided — the denominator a report reads the violation
/// rate against.
#[test]
fn a_2xx_that_crossed_the_cancel_stays_compliant_live() {
    let msgs = vec![req(1_000, "INVITE"), rsp(3_000, 180), rsp(4_000, 200), req(4_500, "CANCEL")];
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "the crossing costs an occasion: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[0].taker, UAC, "the party that sent the CANCEL is owed the 487");
}

/// The violation: the UAC took a final, ACKed it, and CANCELled the
/// transaction it had just completed. The 481 the far side owes such a
/// CANCEL is what the rule exists to predict.
#[test]
fn a_cancel_after_the_uac_acked_its_own_final_is_violated() {
    let msgs = vec![
        req(1_000, "INVITE"),
        rsp(3_000, 180),
        rsp(4_000, 480),
        req(4_500, "ACK"),
        req(5_000, "CANCEL"),
    ];
    let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "one occasion, the CANCEL: {f:?}");
    assert_eq!(f[0].emitter, UAC, "the UAC that cancelled is charged");
    assert_eq!(f[0].taker, UAS);
    let Decision::Violated(Evidence::LateCancel { since_final_us, final_status, ack_msg, .. }) =
        &f[0].decision
    else {
        panic!("late-cancel evidence: {:?}", f[0].decision)
    };
    assert_eq!((*since_final_us, *final_status, *ack_msg), (1_000, 480, 3));
}

/// The ordinary cancellation stays COMPLIANT: the transaction was still in
/// flight, which is the only state §9.1 lets a UAC cancel from. The
/// occasion is still decided — the denominator a report reads the rate
/// against.
#[test]
fn a_cancel_before_the_final_stays_compliant() {
    let msgs = vec![
        req(1_000, "INVITE"),
        rsp(3_000, 180),
        req(4_000, "CANCEL"),
        rsp(5_000, 487),
        req(5_500, "ACK"),
    ];
    let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "the ordinary cancel costs an occasion: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The crossing the rule refuses to charge: the final is in view before the
/// CANCEL, but the emitter never ACKed it, so nothing proves it had the
/// final in hand rather than passing it in flight. Under-reporting is the
/// contract.
#[test]
fn a_final_the_emitter_never_acked_is_a_crossing_not_a_violation() {
    let msgs = vec![req(1_000, "INVITE"), rsp(4_000, 480), req(4_100, "CANCEL")];
    let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// Two attempts that repeat ONE CSeq number, each on its own branch, are two
/// transactions (whether the repeat is itself a fault is
/// `cseq-in-dialog-order`'s call). The final that ended the first says
/// nothing about the second's CANCEL.
#[test]
fn a_final_of_an_earlier_attempt_does_not_charge_the_next_ones_cancel() {
    let msgs = vec![
        req(1_000, "INVITE"),
        rsp(2_000, 408),
        req(2_100, "ACK"),
        on(B2, req(2_200, "INVITE")),
        on(B2, rsp(3_000, 180)),
        on(B2, req(4_000, "CANCEL")),
    ];
    let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A CANCEL whose INVITE this vantage never saw the endpoint SEND is no
/// occasion: nothing says that endpoint is the transaction's UAC.
#[test]
fn a_cancel_without_a_witnessed_outgoing_invite_is_not_an_occasion() {
    let msgs = vec![rsp(4_000, 480), req(4_500, "ACK"), req(5_000, "CANCEL")];
    let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert!(f.is_empty(), "{f:?}");
}

/// A CANCEL for an INVITE this vantage never saw the endpoint take is no
/// occasion at all: nothing says that endpoint is the transaction's UAS.
#[test]
fn a_cancel_without_a_witnessed_invite_is_not_an_occasion() {
    let msgs = vec![req(4_000, "CANCEL"), rsp(6_000, 200)];
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert!(f.is_empty(), "{f:?}");
}

/// The live vantage carries MANY calls on one socket, and a UAS numbers
/// every call's initial INVITE CSeq 1. A CANCEL of one call says nothing
/// about the next call's answer: the Call-ID keeps the two transactions
/// apart, so the second call is no occasion of this rule at all.
#[test]
fn a_cancel_on_one_call_does_not_charge_the_next_calls_answer() {
    let on_call = |mut m: Msg, call_id: &str| {
        m.call_id = call_id.to_string();
        m
    };
    let msgs = vec![
        on_call(req(1_000, "INVITE"), "call-1"),
        on_call(rsp(3_000, 180), "call-1"),
        on_call(req(4_000, "CANCEL"), "call-1"),
        on_call(rsp(4_500, 487), "call-1"),
        // A second call to the same UAS socket, numbering its own CSeq 1.
        on_call(req(5_000, "INVITE"), "call-2"),
        on_call(rsp(6_000, 200), "call-2"),
    ];
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "only the cancelled call is an occasion: {f:?}");
    assert!(
        matches!(f[0].decision, Decision::Compliant),
        "the cancelled call answered 487: {:?}",
        f[0].decision
    );
}

/// A UAC re-offers its INVITE on the same Call-ID and CSeq number but a NEW
/// branch once its first attempt was cancelled, 487ed and ACKed: a new
/// transaction (§17.2.3). Its 200 answers no INVITE the UAS took a CANCEL
/// for, so the cancelled transaction is the only occasion and it is met.
#[test]
fn a_re_offer_on_a_new_branch_after_a_cancel_is_a_new_transaction() {
    let mut ok = on(B2, rsp(9_000, 200));
    ok.to_tag = Some("tb2".to_string());
    let mut ack_2xx = on("z9hG4bK-3", req(9_500, "ACK"));
    ack_2xx.to_tag = Some("tb2".to_string());
    let msgs = vec![
        req(1_000, "INVITE"),
        rsp(2_000, 180),
        req(3_000, "CANCEL"),
        rsp(3_500, 487),
        req(4_000, "ACK"),
        on(B2, req(6_000, "INVITE")),
        on(B2, rsp(7_000, 180)),
        ok,
        ack_2xx,
    ];
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "only the cancelled transaction is an occasion: {f:?}");
    assert!(
        matches!(f[0].decision, Decision::Compliant),
        "the cancelled transaction answered 487: {:?}",
        f[0].decision
    );
    assert_eq!(f[0].anchor, 2, "the occasion anchors on the CANCEL");
}

/// The re-offer races its predecessor's 487: the UAC sends the new-branch
/// INVITE before the first attempt's 487 lands, then cancels the re-offer
/// too. The 487 and ACK of the FIRST transaction completed nothing the
/// second CANCEL names, so both CANCELs are compliant.
#[test]
fn a_final_of_another_branch_does_not_complete_the_cancelled_transaction() {
    let msgs = vec![
        req(1_000, "INVITE"),
        req(2_000, "CANCEL"),
        on(B2, req(3_000, "INVITE")),
        rsp(3_500, 487),
        req(3_600, "ACK"),
        on(B2, rsp(4_000, 180)),
        on(B2, req(5_000, "CANCEL")),
        on(B2, rsp(5_500, 487)),
        on(B2, req(5_600, "ACK")),
    ];
    let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 2, "one occasion per CANCEL: {f:?}");
    for finding in &f {
        assert!(
            matches!(finding.decision, Decision::Compliant),
            "CANCEL at {} cancelled a live transaction: {:?}",
            finding.anchor,
            finding.decision
        );
    }
}

/// A 2xx's ACK rides a branch of its own (§13.2.2.4): it still proves the
/// UAC held the final before it cancelled the transaction the 2xx ended.
#[test]
fn a_cancel_after_the_uac_acked_a_2xx_on_its_own_branch_is_violated() {
    let mut ack = on("z9hG4bK-ack", req(4_500, "ACK"));
    ack.to_tag = Some("tb".to_string());
    let msgs =
        vec![req(1_000, "INVITE"), rsp(3_000, 180), rsp(4_000, 200), ack, req(5_000, "CANCEL")];
    let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "one occasion, the CANCEL: {f:?}");
    let Decision::Violated(Evidence::LateCancel { final_status, ack_msg, .. }) = &f[0].decision
    else {
        panic!("late-cancel evidence: {:?}", f[0].decision)
    };
    assert_eq!((*final_status, *ack_msg), (200, 3));
}

/// A non-2xx final's ACK rides the INVITE's branch (§17.1.1.3): an ACK the
/// UAC sent on ANOTHER branch acknowledged some other transaction's final
/// and proves nothing about this one.
#[test]
fn an_ack_on_another_branch_does_not_complete_a_non_2xx_transaction() {
    let msgs = vec![
        req(1_000, "INVITE"),
        rsp(4_000, 480),
        on(B2, req(4_500, "ACK")),
        req(5_000, "CANCEL"),
    ];
    let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A non-2xx final's ACK rides its INVITE's branch and acknowledges only
/// that transaction: it never stands for the ACK of a sibling attempt's
/// 2xx, whether the two finals carry distinct To tags or the same one.
#[test]
fn a_non_2xx_ack_on_a_sibling_branch_is_not_the_2xxs_ack() {
    for final_tag in ["t1", "t2"] {
        let tagged = |mut m: Msg, tag: &str| {
            m.to_tag = Some(tag.to_string());
            m
        };
        let msgs = vec![
            req(1_000, "INVITE"),
            req(2_000, "CANCEL"),
            on(B2, req(3_000, "INVITE")),
            tagged(on(B2, rsp(3_500, 200)), "t2"),
            tagged(rsp(4_000, 487), final_tag),
            tagged(req(4_100, "ACK"), final_tag),
            on(B2, req(5_000, "CANCEL")),
        ];
        let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert_eq!(f.len(), 2, "one occasion per CANCEL: {f:?}");
        for finding in &f {
            assert!(
                matches!(finding.decision, Decision::Compliant),
                "487 tagged {final_tag}: CANCEL at {} charged on another branch's ACK: {:?}",
                finding.anchor,
                finding.decision
            );
        }
    }
}

/// A UAS that answers a re-offer with the To tag its earlier 2xx on the
/// same Call-ID and CSeq carried originates that answer: a 2xx sighted
/// before it TOOK the re-offer's INVITE is not one it forwarded.
#[test]
fn a_reused_to_tag_on_a_re_offer_is_not_a_relay() {
    let msgs = vec![
        req(1_000, "INVITE"),
        rsp(2_000, 200),
        on("z9hG4bK-ack", req(2_100, "ACK")),
        on(B2, req(6_000, "INVITE")),
        on(B2, rsp(7_000, 180)),
        on(B2, req(8_000, "CANCEL")),
        on(B2, rsp(9_000, 200)),
    ];
    let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert_eq!(f.len(), 1, "the cancelled re-offer is the occasion: {f:?}");
    assert!(matches!(f[0].decision, Decision::Violated(_)), "{:?}", f[0].decision);
    assert!(!f[0].relayed, "the UAS originated its 200 to the re-offer: {f:?}");
}

/// A vantage that carried no top-Via branch cannot pair a CANCEL with its
/// INVITE, so neither side of the cancellation is an occasion — never a
/// guess on Call-ID and CSeq number.
#[test]
fn a_branchless_vantage_keys_no_cancellation() {
    let bare = |mut m: Msg| {
        m.via_branch = None;
        m
    };
    let msgs: Vec<Msg> = [
        req(1_000, "INVITE"),
        rsp(3_000, 180),
        req(4_000, "CANCEL"),
        rsp(6_000, 200),
        req(6_500, "ACK"),
        req(7_000, "CANCEL"),
    ]
    .into_iter()
    .map(bare)
    .collect();
    let view = WireView { msgs: &msgs, obs: &obs(&msgs) };
    assert!(No200AfterCancel.eval(&view).is_empty(), "{:?}", No200AfterCancel.eval(&view));
    assert!(NoCancelAfterFinal.eval(&view).is_empty(), "{:?}", NoCancelAfterFinal.eval(&view));
}

// ---- §9.1: the UAC's half of a cancellation --------------------------

/// A request the UAC SENT on `branch`, carrying `extra` header rows in the
/// head the §9.1 rules read.
fn sent(at_us: u64, method: &str, branch: &str, extra: &str) -> Msg {
    let head = format!(
        "{method} sip:bob@10.0.0.2 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@h>;tag=fa\r\n\
             To: <sip:bob@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 {method}\r\n\
             {extra}\r\n"
    );
    let mut m = msg(at_us, UAC, UAS, Kind::Request { method: method.to_string() }, method);
    m.via_branch = Some(branch.to_string());
    m.head = Some(head.into_bytes());
    m
}

/// A response the UAC TOOK back on `branch`.
fn took(at_us: u64, status: u16, branch: &str) -> Msg {
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, "INVITE");
    m.via_branch = Some(branch.to_string());
    m.to_tag = Some("tb".to_string());
    m
}

fn routes(msgs: &[Msg]) -> Vec<Finding> {
    CancelRouteEchoesInvite.eval(&WireView { msgs, obs: &obs(msgs) })
}

fn waited(msgs: &[Msg]) -> Vec<Finding> {
    CancelAfter1xx.eval(&WireView { msgs, obs: &obs(msgs) })
}

const P1: &str = "Route: <sip:p1@h;lr>\r\n";
const P2: &str = "Route: <sip:p2@h;lr>\r\n";

/// The obligation met: the CANCEL carries the INVITE's Route rows, so it
/// walks the path the INVITE walked.
#[test]
fn a_cancel_echoing_the_invites_route_is_compliant() {
    let f =
        routes(&[sent(1_000, "INVITE", "z9hG4bK-1", P1), sent(4_000, "CANCEL", "z9hG4bK-1", P1)]);
    assert_eq!(f.len(), 1, "one occasion, the CANCEL: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAC, "the sender of the CANCEL is charged");
    assert_eq!(f[0].taker, UAS);
}

/// The violation: the CANCEL states a different path from the INVITE's, and
/// the evidence names both sets and the branch that paired them.
#[test]
fn a_cancel_stating_another_path_is_violated() {
    let f =
        routes(&[sent(1_000, "INVITE", "z9hG4bK-1", P1), sent(4_000, "CANCEL", "z9hG4bK-1", P2)]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].rule, RuleId::CancelRouteEchoesInvite);
    assert_eq!(f[0].anchor, 1, "the occasion rests on the CANCEL");
    assert!(!f[0].relayed);
    let Decision::Violated(Evidence::CancelRouteDiverged {
        cancel_routes,
        invite_routes,
        branch,
        ..
    }) = &f[0].decision
    else {
        panic!("cancel-route evidence: {:?}", f[0].decision)
    };
    assert_eq!(cancel_routes.as_slice(), ["<sip:p2@h;lr>"]);
    assert_eq!(invite_routes.as_slice(), ["<sip:p1@h;lr>"]);
    assert_eq!(branch, "z9hG4bK-1");
}

/// The Route ROWS are compared in order: the same hops re-folded into one
/// row, or listed the other way round, are a different path statement, and
/// §9.1 asks the CANCEL for a copy.
#[test]
fn route_rows_are_compared_in_wire_order() {
    let folded = routes(&[
        sent(1_000, "INVITE", "z9hG4bK-1", &format!("{P1}{P2}")),
        sent(4_000, "CANCEL", "z9hG4bK-1", "Route: <sip:p1@h;lr>, <sip:p2@h;lr>\r\n"),
    ]);
    assert!(folded[0].violated(), "{:?}", folded[0].decision);
    let reversed = routes(&[
        sent(1_000, "INVITE", "z9hG4bK-1", &format!("{P1}{P2}")),
        sent(4_000, "CANCEL", "z9hG4bK-1", &format!("{P2}{P1}")),
    ]);
    assert!(reversed[0].violated(), "{:?}", reversed[0].decision);
    let copied = routes(&[
        sent(1_000, "INVITE", "z9hG4bK-1", &format!("{P1}{P2}")),
        sent(4_000, "CANCEL", "z9hG4bK-1", &format!("{P1}{P2}")),
    ]);
    assert!(matches!(copied[0].decision, Decision::Compliant), "{:?}", copied[0].decision);
}

/// A CANCEL routeless like its INVITE echoes it exactly — an empty set is a
/// set.
#[test]
fn two_routeless_messages_agree() {
    let f =
        routes(&[sent(1_000, "INVITE", "z9hG4bK-1", ""), sent(4_000, "CANCEL", "z9hG4bK-1", "")]);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A CANCEL this vantage never saw the same emitter open with an INVITE is
/// no occasion of either §9.1 rule: nothing pairs with it.
#[test]
fn a_cancel_without_a_witnessed_invite_is_no_occasion() {
    let msgs = [sent(4_000, "CANCEL", "z9hG4bK-1", P1)];
    assert!(routes(&msgs).is_empty(), "{:?}", routes(&msgs));
    assert!(waited(&msgs).is_empty(), "{:?}", waited(&msgs));

    // The branch is what §9.1 pairs on: an INVITE on another branch of the
    // same call pairs with nothing either.
    let other =
        [sent(1_000, "INVITE", "z9hG4bK-other", P1), sent(4_000, "CANCEL", "z9hG4bK-1", P2)];
    assert!(routes(&other).is_empty(), "{:?}", routes(&other));
}

/// A vantage that carried no header block for one of the two messages
/// settles nothing about what they said — UNDECIDABLE, never clean.
#[test]
fn a_pair_without_header_bytes_is_undecidable() {
    let headless = |mut m: Msg| {
        m.head = None;
        m
    };
    let f = routes(&[
        headless(sent(1_000, "INVITE", "z9hG4bK-1", P1)),
        sent(4_000, "CANCEL", "z9hG4bK-1", P2),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(f[0].decision, Decision::Undecidable("no header block at this vantage")),
        "{:?}",
        f[0].decision
    );
    assert!(!f[0].decided());
}

/// The §9.1 wait, honoured: a provisional was in hand when the CANCEL went
/// out, so the UAS has a server transaction to match it against.
#[test]
fn a_cancel_after_a_taken_provisional_is_compliant() {
    let f = waited(&[
        sent(1_000, "INVITE", "z9hG4bK-1", ""),
        took(3_000, 180, "z9hG4bK-1"),
        sent(4_000, "CANCEL", "z9hG4bK-1", ""),
    ]);
    assert_eq!(f.len(), 1, "one occasion, the CANCEL: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The eager CANCEL: nothing had answered the INVITE, so the UAS may not have
/// built the server transaction the CANCEL needs.
#[test]
fn an_eager_cancel_before_any_provisional_is_violated() {
    let f =
        waited(&[sent(1_000, "INVITE", "z9hG4bK-1", ""), sent(2_000, "CANCEL", "z9hG4bK-1", "")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].rule, RuleId::CancelAfter1xx);
    assert_eq!(f[0].emitter, UAC, "the sender of the CANCEL is charged");
    assert_eq!(f[0].anchor, 1);
    let Decision::Violated(Evidence::EagerCancel { since_invite_us, branch, .. }) = &f[0].decision
    else {
        panic!("eager-cancel evidence: {:?}", f[0].decision)
    };
    assert_eq!((*since_invite_us, branch.as_str()), (1_000, "z9hG4bK-1"));
}

/// No time discharges the wait: a CANCEL sent half a minute after its INVITE
/// with nothing answered is the same §9.1 breach as an eager one.
#[test]
fn a_pre_1xx_cancel_is_violated_however_late() {
    let f = waited(&[
        sent(1_000, "INVITE", "z9hG4bK-1", ""),
        sent(30_001_000, "CANCEL", "z9hG4bK-1", ""),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
}

/// ORDER IN THE VIEW is the whole of the "before" test: a provisional that
/// arrived after the CANCEL had already left does not excuse it — the
/// sender could not have been waiting for it.
#[test]
fn a_provisional_after_the_cancel_does_not_excuse_it() {
    let f = waited(&[
        sent(1_000, "INVITE", "z9hG4bK-1", ""),
        sent(2_000, "CANCEL", "z9hG4bK-1", ""),
        took(3_000, 180, "z9hG4bK-1"),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
}

/// A provisional belongs to the branch it came back on: one branch's 18x
/// discharges nothing on another branch of the same call.
#[test]
fn a_provisional_on_another_branch_discharges_nothing() {
    let f = waited(&[
        sent(1_000, "INVITE", "z9hG4bK-1", ""),
        sent(1_100, "INVITE", "z9hG4bK-2", ""),
        took(2_000, 180, "z9hG4bK-1"),
        sent(3_000, "CANCEL", "z9hG4bK-2", ""),
        sent(3_100, "CANCEL", "z9hG4bK-1", ""),
    ]);
    assert_eq!(f.len(), 2, "one occasion per CANCEL: {f:?}");
    assert!(f[0].violated(), "the fork with no answer: {:?}", f[0].decision);
    assert_eq!(f[0].anchor, 3);
    assert!(matches!(f[1].decision, Decision::Compliant), "{:?}", f[1].decision);
}

/// A CANCEL the vantage marked a repeat is the same act again — §9.1 is
/// tested once, by the copy that first left the sender.
#[test]
fn a_retransmitted_cancel_is_not_a_second_occasion() {
    let again = |mut m: Msg| {
        m.repeat = true;
        m
    };
    let msgs = [
        sent(1_000, "INVITE", "z9hG4bK-1", P1),
        sent(2_000, "CANCEL", "z9hG4bK-1", P2),
        again(sent(3_000, "CANCEL", "z9hG4bK-1", P2)),
    ];
    assert_eq!(routes(&msgs).len(), 1, "{:?}", routes(&msgs));
    assert_eq!(waited(&msgs).len(), 1, "{:?}", waited(&msgs));
}
