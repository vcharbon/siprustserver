//! The pair's OWN semantics: what the capture conformance pins in
//! `sip_pcap::rfc::ack` cannot state — the closed-observation collapse,
//! the UAS half, and the two rules' opposite verdicts on one trace.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{
    AckPreservesInviteRoute, AckRequireSubsetOfInvite, NoAckToDialogCreating2xx,
    Unacked2xxNotCleared, UnackedInviteNon2xxFinal,
};

const UAC: &str = "10.0.0.1:5060";
const UAS: &str = "10.0.0.2:5060";

fn msg(
    at_us: u64,
    src: &str,
    dst: &str,
    kind: Kind,
    cseq: u32,
    from_tag: Option<&str>,
    to_tag: Option<&str>,
) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind,
        call_id: "c1".to_string(),
        cseq,
        cseq_method: "INVITE".to_string(),
        via_branch: None,
        from_tag: from_tag.map(str::to_string),
        to_tag: to_tag.map(str::to_string),
        head: None,
        body: None,
    }
}

fn req(at_us: u64, src: &str, dst: &str, method: &str, cseq: u32, to_tag: Option<&str>) -> Msg {
    msg(at_us, src, dst, Kind::Request { method: method.to_string() }, cseq, Some("fa"), to_tag)
}

fn ok200(at_us: u64, cseq: u32) -> Msg {
    msg(at_us, UAS, UAC, Kind::Response { status: 200 }, cseq, Some("fa"), Some("tb"))
}

fn obs(msgs: &[Msg], closed: bool) -> Observation {
    let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
    let mut last_us = 0;
    for m in msgs {
        last_us = last_us.max(m.at_us);
        for ep in [&m.src, &m.dst] {
            let at = endpoint_last_us.entry(ep.clone()).or_default();
            *at = (*at).max(m.at_us);
        }
    }
    Observation { last_us, endpoint_last_us, closed }
}

/// An un-ACKed 2xx followed 10 ms later by end-of-stream: an OPEN
/// observation cannot tell absence from truncation; a CLOSED one decides
/// immediately, because the harness drained before stopping.
#[test]
fn a_closed_observation_collapses_the_ack_window() {
    let msgs = vec![req(1_000_000, UAC, UAS, "INVITE", 1, None), ok200(1_200_000, 1)];
    let open = obs(&msgs, false);
    let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &open });
    assert_eq!(f.len(), 1);
    assert!(
        matches!(f[0].decision, Decision::Undecidable(_)),
        "open: truncation, not absence: {:?}",
        f[0].decision
    );

    let closed = obs(&msgs, true);
    let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &closed });
    assert!(f[0].violated(), "closed: nothing was in flight: {:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAC, "the UAC owed the ACK");
}

/// An endpoint hosting two calls numbers each call's CSeq on its own: the
/// dialog-creating INVITE of one call says nothing about the 2xx to a
/// re-INVITE on the other, which shares its CSeq number and no dialog.
#[test]
fn a_dialog_creating_invite_of_another_call_opens_no_occasion() {
    let on = |mut m: Msg, call: &str| {
        m.call_id = call.to_string();
        m
    };
    let msgs = vec![
        // Call one: the endpoint dials, takes the 2xx and ACKs it.
        on(req(1_000_000, UAC, UAS, "INVITE", 1, None), "c1"),
        on(ok200(1_100_000, 1), "c1"),
        on(req(1_200_000, UAC, UAS, "ACK", 1, Some("tb")), "c1"),
        // Call two: the same endpoint re-INVITEs on a dialog it answered,
        // CSeq 1 of its own side, and takes a 2xx it never ACKs.
        on(req(2_000_000, UAC, UAS, "INVITE", 1, Some("tx")), "c2"),
        on(
            msg(2_100_000, UAS, UAC, Kind::Response { status: 200 }, 1, Some("fa"), Some("tx")),
            "c2",
        ),
    ];
    let closed = obs(&msgs, true);
    let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &closed });
    assert!(f.iter().all(|f| !f.violated()), "{f:?}");
    assert_eq!(f.len(), 1, "call one's 2xx is the one occasion: {f:?}");
}

/// A B2BUA passing the callee's 2xx on to a caller that never ACKs it holds
/// its own ACK behind the caller's: the withholding is the caller's, read
/// twice, so the B2BUA's half is undecided and the caller's is charged.
#[test]
fn a_b2buas_ack_held_behind_its_unacking_caller_is_undecided() {
    const B2BUA: &str = "10.0.0.5:5060";
    let on = |mut m: Msg, call: &str| {
        m.call_id = call.to_string();
        m
    };
    let msgs = vec![
        on(req(1_000_000, UAC, B2BUA, "INVITE", 1, None), "a"),
        on(req(1_010_000, B2BUA, UAS, "INVITE", 1, None), "b"),
        on(
            msg(1_200_000, UAS, B2BUA, Kind::Response { status: 200 }, 1, Some("fa"), Some("tb")),
            "b",
        ),
        on(
            msg(1_210_000, B2BUA, UAC, Kind::Response { status: 200 }, 1, Some("fa"), Some("ta")),
            "a",
        ),
    ];
    let closed = obs(&msgs, true);
    let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &closed });
    let of = |who: &str| f.iter().find(|f| f.emitter == who).expect("an occasion");
    assert!(of(UAC).violated(), "the caller withheld its ACK: {f:?}");
    assert!(
        matches!(of(B2BUA).decision, Decision::Undecidable(_)),
        "the B2BUA's ACK waits on the caller's: {f:?}"
    );
}

/// The B2BUA's hold is read off the call it relays, never off another one
/// it happens to carry: x's caller ACKed the answer relayed to her, so the
/// B2BUA's missing ACK on y is its own — an older call p whose 2xx nobody
/// ACKed says nothing about y.
#[test]
fn another_calls_unacked_2xx_does_not_excuse_a_b2buas_missing_ack() {
    const B2BUA: &str = "10.0.0.5:5060";
    const OTHER: &str = "10.0.0.9:5060";
    let on = |mut m: Msg, call: &str| {
        m.call_id = call.to_string();
        m
    };
    let ok = |at, src, dst, to: &str, call| {
        on(msg(at, src, dst, Kind::Response { status: 200 }, 1, Some("fa"), Some(to)), call)
    };
    let msgs = vec![
        on(req(500_000, OTHER, B2BUA, "INVITE", 1, None), "p"),
        on(req(1_000_000, UAC, B2BUA, "INVITE", 1, None), "x"),
        on(req(1_010_000, B2BUA, UAS, "INVITE", 1, None), "y"),
        ok(1_200_000, UAS, B2BUA, "ty", "y"),
        ok(1_210_000, B2BUA, UAC, "tx", "x"),
        on(req(1_300_000, UAC, B2BUA, "ACK", 1, Some("tx")), "x"),
        ok(1_400_000, B2BUA, OTHER, "tp", "p"),
    ];
    let closed = obs(&msgs, true);
    let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &closed });
    let b2bua = f.iter().find(|f| f.emitter == B2BUA).expect("the B2BUA's occasion on y");
    assert!(b2bua.violated(), "x was ACKed, so y's missing ACK is the B2BUA's: {f:?}");
}

/// One trace — a dialog-creating 2xx that is BYE'd but never ACKed — and
/// the pair states BOTH obligations: the UAC
/// broke §13.2.2.4 (a BYE is corroboration, never a discharge), and the
/// UAS met §13.3.1.4 (the BYE is exactly its clearing duty).
#[test]
fn a_byed_but_unacked_2xx_charges_the_uac_and_clears_the_uas() {
    let msgs = vec![
        req(1_000_000, UAC, UAS, "INVITE", 1, None),
        ok200(1_200_000, 1),
        // The UAS gives up per §13.3.1.4 and BYEs its own dialog — its
        // minted tag rides From, the caller's rides To (tags swapped).
        msg(
            34_000_000,
            UAS,
            UAC,
            Kind::Request { method: "BYE".to_string() },
            9,
            Some("tb"),
            Some("fa"),
        ),
    ];
    let closed = obs(&msgs, true);
    let view = WireView { msgs: &msgs, obs: &closed };

    let uac_half = NoAckToDialogCreating2xx.eval(&view);
    assert_eq!(uac_half.len(), 1);
    assert!(uac_half[0].violated(), "{:?}", uac_half[0].decision);
    assert_eq!(uac_half[0].emitter, UAC);
    let Decision::Violated(Evidence::NoAck { bye_after_us, bye_by, .. }) = &uac_half[0].decision
    else {
        panic!("ack evidence: {:?}", uac_half[0].decision)
    };
    assert_eq!(*bye_after_us, Some(32_800_000), "the BYE corroborates, not discharges");
    assert_eq!(bye_by.as_deref(), Some(UAS));

    let uas_half = Unacked2xxNotCleared.eval(&view);
    assert_eq!(uas_half.len(), 1);
    assert!(
        matches!(uas_half[0].decision, Decision::Compliant),
        "the UAS cleared its dialog: {:?}",
        uas_half[0].decision
    );
    assert_eq!(uas_half[0].emitter, UAS);
}

/// The UAS half's violation: a 2xx the UAS neither saw ACKed nor cleared,
/// with the observation closed — the silent answered-call leak.
#[test]
fn a_2xx_neither_acked_nor_byed_charges_the_uas_too() {
    let msgs = vec![req(1_000_000, UAC, UAS, "INVITE", 1, None), ok200(1_200_000, 1)];
    let closed = obs(&msgs, true);
    let f = Unacked2xxNotCleared.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].rule, RuleId::Unacked2xxNotCleared);
    assert_eq!(f[0].emitter, UAS, "the UAS owed the clearing BYE");
    assert_eq!(f[0].taker, UAC);
    assert!(!f[0].relayed);
    assert!(f[0].violated(), "{:?}", f[0].decision);
}

/// A re-INVITE's 2xx is the UAS half's business too (§13.3.1.4 confirms it
/// the same way) — while the UAC half stays dialog-creating-only.
#[test]
fn a_re_invites_unacked_2xx_is_the_uas_halfs_occasion_only() {
    let msgs = vec![
        req(1_000_000, UAC, UAS, "INVITE", 1, None),
        ok200(1_200_000, 1),
        req(1_250_000, UAC, UAS, "ACK", 1, Some("tb")),
        req(5_000_000, UAC, UAS, "INVITE", 2, Some("tb")),
        ok200(5_100_000, 2),
    ];
    let closed = obs(&msgs, true);
    let view = WireView { msgs: &msgs, obs: &closed };
    let uac_half = NoAckToDialogCreating2xx.eval(&view);
    assert_eq!(uac_half.len(), 1, "only the initial INVITE's 2xx: {uac_half:?}");
    assert!(matches!(uac_half[0].decision, Decision::Compliant));
    let uas_half = Unacked2xxNotCleared.eval(&view);
    assert_eq!(uas_half.len(), 2, "both 2xxs opened clearing obligations: {uas_half:?}");
    assert!(matches!(uas_half[0].decision, Decision::Compliant), "the initial was ACKed");
    assert!(uas_half[1].violated(), "the re-INVITE's was not: {:?}", uas_half[1].decision);
}

/// A hop that forwards a 2xx someone else originated carries the UAS
/// half's obligation only as `relayed` — consumer policy's to weigh.
#[test]
fn a_forwarding_hops_uncleared_2xx_is_marked_relayed() {
    const HOP: &str = "10.0.0.9:5060";
    let msgs = vec![
        req(1_000_000, UAC, HOP, "INVITE", 1, None),
        req(1_010_000, HOP, UAS, "INVITE", 1, None),
        msg(1_200_000, UAS, HOP, Kind::Response { status: 200 }, 1, Some("fa"), Some("tb")),
        msg(1_210_000, HOP, UAC, Kind::Response { status: 200 }, 1, Some("fa"), Some("tb")),
    ];
    let closed = obs(&msgs, true);
    let f = Unacked2xxNotCleared.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 2, "{f:?}");
    let by_emitter = |ep: &str| f.iter().find(|x| x.emitter == ep).unwrap();
    assert!(!by_emitter(UAS).relayed, "the far UAS originated its 2xx");
    assert!(by_emitter(HOP).relayed, "the hop passed it on");
}

// ---- what the ACK CARRIED (§13.2.2.4 / §17.1.1.3) --------------------

/// A request the UAC SENT on `branch`, carrying `extra` header rows in the
/// head the two carries rules read.
fn on_branch(at_us: u64, method: &str, branch: &str, extra: &str) -> Msg {
    let head = format!(
        "{method} sip:bob@10.0.0.2 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@h>;tag=fa\r\n\
             To: <sip:bob@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 {method}\r\n\
             {extra}\r\n"
    );
    let mut m = req(at_us, UAC, UAS, method, 1, None);
    m.via_branch = Some(branch.to_string());
    m.head = Some(head.into_bytes());
    m
}

fn requires(msgs: &[Msg]) -> Vec<Finding> {
    AckRequireSubsetOfInvite.eval(&WireView { msgs, obs: &obs(msgs, true) })
}

fn routes(msgs: &[Msg]) -> Vec<Finding> {
    AckPreservesInviteRoute.eval(&WireView { msgs, obs: &obs(msgs, true) })
}

/// The obligation met: the ACK requires a tag the INVITE required.
#[test]
fn an_ack_requiring_what_its_invite_required_is_compliant() {
    let f = requires(&[
        on_branch(1_000, "INVITE", "z9hG4bK-i", "Require: 100rel\r\n"),
        on_branch(3_000, "ACK", "z9hG4bK-i", "Require: 100rel\r\n"),
    ]);
    assert_eq!(f.len(), 1, "one occasion, the ACK: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAC, "the sender of the ACK is charged");
    assert_eq!(f[0].taker, UAS);
}

/// The violation: the ACK escalates the transaction with a tag the INVITE
/// never stated, and the evidence names exactly which.
#[test]
fn an_ack_requiring_more_than_its_invite_is_violated() {
    let f = requires(&[
        on_branch(1_000, "INVITE", "z9hG4bK-i", ""),
        on_branch(3_000, "ACK", "z9hG4bK-i", "Require: timer\r\n"),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].rule, RuleId::AckRequireSubsetOfInvite);
    assert_eq!(f[0].anchor, 1, "the occasion rests on the ACK");
    let Decision::Violated(Evidence::AckRequireNotSubset { extra_tags, branch, .. }) =
        &f[0].decision
    else {
        panic!("require evidence: {:?}", f[0].decision)
    };
    assert_eq!(extra_tags.as_slice(), ["timer"]);
    assert_eq!(branch, "z9hG4bK-i");
}

/// An ACK requiring nothing is the empty set, which is a subset of
/// anything: a MET occasion, so the report can rate the offence against
/// the ACKs that could have carried it.
#[test]
fn an_ack_requiring_nothing_is_a_met_occasion() {
    let f = requires(&[
        on_branch(1_000, "INVITE", "z9hG4bK-i", "Require: 100rel\r\n"),
        on_branch(3_000, "ACK", "z9hG4bK-i", ""),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The ACK of a 2xx rides its OWN branch, so it pairs with no INVITE and is
/// no occasion of either carries rule — a different MUST governs it.
#[test]
fn an_ack_on_its_own_branch_is_no_occasion() {
    let msgs = [
        on_branch(1_000, "INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"),
        on_branch(3_000, "ACK", "z9hG4bK-a", "Require: timer\r\n"),
    ];
    assert!(requires(&msgs).is_empty(), "{:?}", requires(&msgs));
    assert!(routes(&msgs).is_empty(), "{:?}", routes(&msgs));
}

/// A vantage that carried no header block for one of the two settles
/// nothing about what they said — UNDECIDABLE, never clean.
#[test]
fn a_carries_pair_without_header_bytes_is_undecidable() {
    let headless = |mut m: Msg| {
        m.head = None;
        m
    };
    let msgs = [
        headless(on_branch(1_000, "INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n")),
        on_branch(3_000, "ACK", "z9hG4bK-i", "Require: timer\r\n"),
    ];
    for f in [requires(&msgs), routes(&msgs)] {
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            matches!(f[0].decision, Decision::Undecidable("no header block at this vantage")),
            "{:?}",
            f[0].decision
        );
        assert!(!f[0].decided());
    }
}

/// A retransmitted ACK is the same act again: one occasion, tested by the
/// copy that first left the sender.
#[test]
fn a_retransmitted_ack_is_not_a_second_occasion() {
    let again = |mut m: Msg| {
        m.repeat = true;
        m
    };
    let msgs = [
        on_branch(1_000, "INVITE", "z9hG4bK-i", ""),
        on_branch(3_000, "ACK", "z9hG4bK-i", "Require: timer\r\n"),
        again(on_branch(4_000, "ACK", "z9hG4bK-i", "Require: timer\r\n")),
    ];
    assert_eq!(requires(&msgs).len(), 1, "{:?}", requires(&msgs));
    assert_eq!(routes(&msgs).len(), 1, "{:?}", routes(&msgs));
}

/// The obligation met: the non-2xx ACK walks the path its INVITE walked.
#[test]
fn an_ack_echoing_the_invites_route_is_compliant() {
    let f = routes(&[
        on_branch(1_000, "INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"),
        on_branch(3_000, "ACK", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"),
    ]);
    assert_eq!(f.len(), 1, "one occasion, the ACK: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAC, "the sender of the ACK is charged");
}

/// The violation: the ACK drops the INVITE's Route set, so it is routed by
/// a Request-URI no server transaction on that path is waiting on.
#[test]
fn an_ack_dropping_the_invites_route_is_violated() {
    let f = routes(&[
        on_branch(1_000, "INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"),
        on_branch(3_000, "ACK", "z9hG4bK-i", ""),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].rule, RuleId::AckPreservesInviteRoute);
    let Decision::Violated(Evidence::AckRouteDiverged {
        ack_routes, invite_routes, branch, ..
    }) = &f[0].decision
    else {
        panic!("ack-route evidence: {:?}", f[0].decision)
    };
    assert!(ack_routes.is_empty());
    assert_eq!(invite_routes.as_slice(), ["<sip:p1@h;lr>"]);
    assert_eq!(branch, "z9hG4bK-i");
}

/// The Route ROWS are compared in order: the same hops re-folded into one
/// row are a different path statement, and §17.1.1.3 asks for the
/// INVITE's.
#[test]
fn ack_route_rows_are_compared_in_wire_order() {
    let two = "Route: <sip:p1@h;lr>\r\nRoute: <sip:p2@h;lr>\r\n";
    let folded = routes(&[
        on_branch(1_000, "INVITE", "z9hG4bK-i", two),
        on_branch(3_000, "ACK", "z9hG4bK-i", "Route: <sip:p1@h;lr>, <sip:p2@h;lr>\r\n"),
    ]);
    assert!(folded[0].violated(), "{:?}", folded[0].decision);
    let copied = routes(&[
        on_branch(1_000, "INVITE", "z9hG4bK-i", two),
        on_branch(3_000, "ACK", "z9hG4bK-i", two),
    ]);
    assert!(matches!(copied[0].decision, Decision::Compliant), "{:?}", copied[0].decision);
}

// ── UnackedInviteNon2xxFinal (RFC 3261 §17.1.1.3) ───────────────────────

/// A message on a named branch between the two endpoints.
fn on(at_us: u64, src: &str, dst: &str, kind: Kind, cseq: u32, branch: &str) -> Msg {
    let mut m = msg(at_us, src, dst, kind, cseq, Some("fa"), Some("tb"));
    m.via_branch = Some(branch.to_string());
    m
}

fn reject(at_us: u64, status: u16, branch: &str) -> Msg {
    on(at_us, UAS, UAC, Kind::Response { status }, 1, branch)
}

fn took(at_us: u64, method: &str, branch: &str) -> Msg {
    on(at_us, UAC, UAS, Kind::Request { method: method.to_string() }, 1, branch)
}

fn unacked(msgs: &[Msg]) -> Vec<Finding> {
    UnackedInviteNon2xxFinal.eval(&WireView { msgs, obs: &obs(msgs, true) })
}

/// §17.1.1.3: the ACK of a non-2xx final rides the INVITE's own branch, so
/// it discharges that transaction — and a retransmitted final does not
/// re-open it.
#[test]
fn an_ack_on_the_invite_branch_discharges_the_reject() {
    let f = unacked(&[
        took(1_000, "INVITE", "z9hG4bK-i"),
        reject(2_000, 486, "z9hG4bK-i"),
        reject(3_000, 486, "z9hG4bK-i"),
        took(4_000, "ACK", "z9hG4bK-i"),
    ]);
    assert_eq!(f.len(), 1, "one occasion, the transaction: {f:?}");
    assert_eq!(f[0].rule, RuleId::UnackedInviteNon2xxFinal);
    assert_eq!(f[0].emitter, UAC, "the UAC that took the reject owes its ACK");
    assert_eq!(f[0].taker, UAS);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The defect: the reject went out and its mandatory ACK never came back.
#[test]
fn a_reject_that_is_never_acked_is_violated() {
    let f = unacked(&[took(1_000, "INVITE", "z9hG4bK-i"), reject(2_000, 486, "z9hG4bK-i")]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::UnackedReject { status, branch, invite_msg, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*status, branch.as_str(), *invite_msg), (486, "z9hG4bK-i", 0));
}

/// The offence is the ACK that never came, so [`Finding::emitter`] is the
/// party that OWED it: the UAC whose INVITE client transaction took the
/// reject (§17.1.1.3). The UAS that sent the reject and waited is the one
/// owed, the `taker`.
#[test]
fn a_never_acked_reject_is_charged_to_the_uac_that_owed_the_ack() {
    let f = unacked(&[took(1_000, "INVITE", "z9hG4bK-i"), reject(2_000, 487, "z9hG4bK-i")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAC, "the UAC that withheld the ACK is charged");
    assert_eq!(f[0].taker, UAS, "the UAS that sent the reject is the one owed");
}

/// A reject on a branch no INVITE in the view opened names no transaction,
/// and a 2xx is the dialog rules' business.
#[test]
fn a_reject_off_any_seen_branch_and_a_2xx_open_no_occasion() {
    assert!(unacked(&[
        // A reject whose branch no INVITE of the view carried.
        on(1_000, UAC, UAS, Kind::Request { method: "INVITE".to_string() }, 1, "z9hG4bK-u"),
        on(2_000, UAS, UAC, Kind::Response { status: 486 }, 1, "z9hG4bK-x"),
        // A 2xx on a branch this UAS IS the server of.
        took(3_000, "INVITE", "z9hG4bK-i"),
        reject(4_000, 200, "z9hG4bK-i"),
    ])
    .is_empty());
}

/// An ACK recorded BEFORE the final it would answer belongs to an earlier
/// transaction on that branch, not to the obligation this final opens.
#[test]
fn an_ack_preceding_the_reject_does_not_discharge_it() {
    let f = unacked(&[
        took(1_000, "INVITE", "z9hG4bK-i"),
        took(2_000, "ACK", "z9hG4bK-i"),
        reject(3_000, 486, "z9hG4bK-i"),
    ]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
}

/// An OPEN observation that stopped before the §17.2.1 give-up point shows
/// truncation; a CLOSED one decides at once.
#[test]
fn a_closed_observation_collapses_the_reject_window() {
    let msgs = [took(1_000, "INVITE", "z9hG4bK-i"), reject(2_000, 486, "z9hG4bK-i")];
    let open = UnackedInviteNon2xxFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs, false) });
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(!open[0].decided(), "{:?}", open[0].decision);
    assert!(unacked(&msgs)[0].violated());
}
