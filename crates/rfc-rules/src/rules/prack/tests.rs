//! The family's OWN semantics: what an adapter's event fixtures cannot
//! state — the closed-observation ruling (D2), the fork partition, and
//! each rule's undecidable gates.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{
    Delay2xxOnUnackedReliable1xxWithSdp, NoNewReliable1xxAfterFinal,
    NoOverlappingReliableProvisionals, NoPrackOf100Trying, NoPrackOfOutOfOrderRseq,
    NoReliable1xxOnInDialog, NonContiguousRseq, Prack2xxOr481, PrackAnswers1xxOffer,
    RackWithoutKnownInvite, ReliableNeedsClientOptIn, RequireReliable1xxOnRequire,
    UnackedReliableProvisional, UnmatchedPrackProxied,
};

const UAC: &str = "10.0.0.1:5060";
const UAS: &str = "10.0.0.2:5060";

fn msg(
    at_us: u64,
    src: &str,
    dst: &str,
    kind: Kind,
    cseq: u32,
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
        call_id: "c1".to_string(),
        cseq,
        cseq_method: "INVITE".to_string(),
        via_branch: None,
        from_tag: Some("fa".to_string()),
        to_tag: to_tag.map(str::to_string),
        head: Some(head.into_bytes()),
        body: None,
    }
}

/// An INVITE offering `100rel`, from `src`.
fn invite(at_us: u64, src: &str, dst: &str, cseq: u32) -> Msg {
    let head = format!(
        "INVITE sip:bob@h SIP/2.0\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>\r\n\
             CSeq: {cseq} INVITE\r\nSupported: 100rel\r\nContent-Length: 0\r\n\r\n"
    );
    msg(at_us, src, dst, Kind::Request { method: "INVITE".to_string() }, cseq, None, head)
}

/// A reliable provisional the UAS sends on early dialog `to_tag`.
fn reliable(
    at_us: u64,
    src: &str,
    dst: &str,
    status: u16,
    cseq: u32,
    rseq: u64,
    to_tag: &str,
) -> Msg {
    let head = format!(
        "SIP/2.0 {status} Ringing\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} INVITE\r\nRequire: 100rel\r\nRSeq: {rseq}\r\nContent-Length: 0\r\n\r\n"
    );
    msg(at_us, src, dst, Kind::Response { status }, cseq, Some(to_tag), head)
}

/// A PRACK whose `RAck` names `(rseq, rack_cseq)`.
fn prack(
    at_us: u64,
    src: &str,
    dst: &str,
    cseq: u32,
    rseq: u64,
    rack_cseq: u32,
    to_tag: &str,
) -> Msg {
    let head = format!(
        "PRACK sip:bob@h SIP/2.0\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} PRACK\r\nRAck: {rseq} {rack_cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    let mut m = msg(
        at_us,
        src,
        dst,
        Kind::Request { method: "PRACK".to_string() },
        cseq,
        Some(to_tag),
        head,
    );
    m.cseq_method = "PRACK".to_string();
    m
}

/// The INVITE final that releases the transaction.
fn ok200(at_us: u64, cseq: u32, to_tag: &str) -> Msg {
    let head = format!(
        "SIP/2.0 200 OK\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    msg(at_us, UAS, UAC, Kind::Response { status: 200 }, cseq, Some(to_tag), head)
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

/// A reliable 180 followed 20 ms later by the 200 OK, never PRACKed: a
/// final arriving soon after the provisional does not excuse the PRACK
/// (RFC 3262 §4). That reading is the MESSAGES', so an open and a closed
/// observation reach the same verdict: the census over a capture and the
/// live audit over a run decide one sequence alike.
#[test]
fn a_fast_final_does_not_excuse_the_prack_whatever_the_observation() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
        ok200(1_120_000, 1, "tb"),
    ];
    for closed in [false, true] {
        let o = obs(&msgs, closed);
        let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
        assert_eq!(f.len(), 1);
        assert!(f[0].violated(), "closed={closed}: the final came after it: {:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC);
    }
}

/// The responder BYEs the early dialog its reliable 180 rode (a callee's
/// deviation, RFC 3261 §15): nothing rides that dialog after it
/// (§15.1.2), so no PRACK is owed; the 487 that follows changes nothing.
#[test]
fn a_responders_bye_on_the_early_dialog_leaves_its_provisional_owed_nothing() {
    let bye_head = "BYE sip:a@h SIP/2.0\r\nFrom: <sip:b@h>;tag=tb\r\nTo: <sip:a@h>;tag=fa\r\n\
                        CSeq: 1 BYE\r\nContent-Length: 0\r\n\r\n"
        .to_string();
    let mut bye = msg(
        1_110_000,
        UAS,
        UAC,
        Kind::Request { method: "BYE".to_string() },
        1,
        Some("fa"),
        bye_head,
    );
    bye.cseq_method = "BYE".to_string();
    bye.from_tag = Some("tb".to_string());
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
        bye,
        final_status(1_200_000, 487, 1, "tb"),
    ];
    let o = obs(&msgs, true);
    let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
    assert!(f.is_empty(), "no occasion: {f:?}");
}

/// A PRACK the UAC sent before the final reached it may be seen after that
/// final at the vantage: the two crossed in flight, and the PRACK still
/// discharges the obligation.
#[test]
fn a_prack_crossing_the_final_in_flight_is_compliant() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 183, 1, 5, "tb"),
        final_status(1_120_000, 487, 1, "tb"),
        prack(1_121_000, UAC, UAS, 2, 5, 1, "tb"),
    ];
    for closed in [false, true] {
        let o = obs(&msgs, closed);
        let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
        assert_eq!(f.len(), 1);
        assert!(
            matches!(f[0].decision, Decision::Compliant),
            "closed={closed}: {:?}",
            f[0].decision
        );
    }
}

/// RFC 3262 §4: a reliable provisional crossing the UAC's CANCEL is owed
/// its PRACK even when the 487 follows within milliseconds.
#[test]
fn a_provisional_crossing_the_cancel_is_owed_its_prack_whatever_the_final_delay() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        cancel(1_295_000, 1),
        reliable(1_324_000, UAS, UAC, 183, 1, 1, "tb"),
        final_status(1_419_000, 487, 1, "tb"),
    ];
    for closed in [false, true] {
        let o = obs(&msgs, closed);
        let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "closed={closed}: {:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC);
    }
}

/// A responder's retransmission of its reliable provisional is a second
/// copy the UAC RECEIVED, not one it passed on: the UAC that never PRACKs
/// either copy is charged (RFC 3262 §3, §4).
#[test]
fn a_retransmitted_provisional_does_not_make_its_taker_a_hop() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
        reliable(1_600_000, UAS, UAC, 180, 1, 5, "tb"),
        ok200(3_000_000, 1, "tb"),
    ];
    for closed in [false, true] {
        let o = obs(&msgs, closed);
        let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "closed={closed}: {:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC);
    }
}

/// A hop that takes the provisional and SENDS it on is no UAC: its
/// obligation stays undecidable, whoever PRACKs downstream.
#[test]
fn a_hop_that_sends_the_provisional_on_is_not_charged() {
    const HOP: &str = "10.0.0.3:5060";
    let msgs = vec![
        invite(1_000_000, UAC, HOP, 1),
        invite(1_010_000, HOP, UAS, 1),
        reliable(1_100_000, UAS, HOP, 180, 1, 5, "tb"),
        reliable(1_110_000, HOP, UAC, 180, 1, 5, "tb"),
        ok200(3_000_000, 1, "tb"),
    ];
    let o = obs(&msgs, true);
    let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
    let at_hop: Vec<_> = f.iter().filter(|f| f.emitter == HOP).collect();
    assert_eq!(at_hop.len(), 1, "{f:?}");
    assert!(matches!(at_hop[0].decision, Decision::Undecidable(_)), "{:?}", at_hop[0].decision);
}

/// Each side of a dialog numbers its own CSeq space (RFC 3261 §12.2.1.1):
/// the far end's re-INVITE may carry the number the initial INVITE did. Its
/// sender opened THAT INVITE — the From tag tells the two apart — so it
/// owes the PRACK of the reliable provisional answering it.
#[test]
fn a_reverse_reinvite_reusing_the_initial_cseq_is_its_senders_to_prack() {
    let from = |mut m: Msg, tag: &str| {
        m.from_tag = Some(tag.to_string());
        m
    };
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        ok200(1_100_000, 1, "tb"),
        from(invite(5_000_000, UAS, UAC, 1), "tb"),
        from(reliable(5_100_000, UAC, UAS, 183, 1, 9, "fa"), "tb"),
        from(final_status(9_000_000, 504, 1, "fa"), "tb"),
    ];
    let o = obs(&msgs, true);
    let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAS, "the re-INVITE's sender owed the PRACK");
}

/// RFC 3262 §4: a reliable provisional whose `RSeq` is not one higher than
/// the last taken on its dialog MUST NOT be PRACKed, so leaving it
/// unacknowledged is compliant; the in-order one before it is still owed.
#[test]
fn an_out_of_order_provisional_is_owed_no_prack() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 183, 1, 5, "tb"),
        prack(1_150_000, UAC, UAS, 2, 5, 1, "tb"),
        reliable(1_200_000, UAS, UAC, 183, 1, 7, "tb"),
        final_status(1_300_000, 487, 1, "tb"),
    ];
    let o = obs(&msgs, true);
    let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(f.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{f:?}");
}

/// A CLOSED observation still decides an absence at its end: no final
/// answered the INVITE, the stream simply stopped, and the drained
/// harness had nothing in flight. An OPEN one calls that truncation.
#[test]
fn a_closed_observation_decides_an_unanswered_provisional_at_its_end() {
    let msgs = vec![invite(1_000_000, UAC, UAS, 1), reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb")];
    let open = obs(&msgs, false);
    let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &open });
    assert!(matches!(f[0].decision, Decision::Undecidable(_)), "{:?}", f[0].decision);
    let closed = obs(&msgs, true);
    let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &closed });
    assert!(f[0].violated(), "closed: nothing was in flight: {:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAC, "the UAC owed the PRACK");
    assert_eq!(f[0].rule, RuleId::UnackedReliableProvisional);
}

/// An endpoint hosting two calls numbers each call's CSeq on its own: the
/// INVITE another call opened earlier under the same number neither makes
/// this UAC a hop, nor does that call's final release this transaction.
#[test]
fn another_calls_invite_and_final_say_nothing_of_this_ones_prack() {
    let on = |mut m: Msg, call: &str| {
        m.call_id = call.to_string();
        m
    };
    let msgs = vec![
        on(invite(500_000, UAC, UAS, 1), "other"),
        on(invite(1_000_000, UAC, UAS, 1), "c1"),
        on(reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"), "c1"),
        on(ok200(1_150_000, 1, "tz"), "other"),
        on(ok200(3_000_000, 1, "tb"), "c1"),
    ];
    let closed = obs(&msgs, true);
    let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "this call's UAC never PRACKed: {:?}", f[0].decision);
}

/// The final, here a `487`.
fn final_status(at_us: u64, status: u16, cseq: u32, to_tag: &str) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    msg(at_us, UAS, UAC, Kind::Response { status }, cseq, Some(to_tag), head)
}

/// The UAC's CANCEL of its INVITE.
fn cancel(at_us: u64, cseq: u32) -> Msg {
    let head = format!(
        "CANCEL sip:bob@h SIP/2.0\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>\r\n\
             CSeq: {cseq} CANCEL\r\nContent-Length: 0\r\n\r\n"
    );
    let mut m =
        msg(at_us, UAC, UAS, Kind::Request { method: "CANCEL".to_string() }, cseq, None, head);
    m.cseq_method = "CANCEL".to_string();
    m
}

/// RFC 3262 §4: a CANCEL does not end the INVITE transaction, so a reliable
/// provisional taken after it, the final still a window away, is owed its
/// PRACK — in an open observation and a closed one alike.
#[test]
fn a_reliable_provisional_taken_after_the_cancel_is_still_owed_its_prack() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        cancel(1_050_000, 1),
        reliable(1_100_000, UAS, UAC, 183, 1, 5, "tb"),
        final_status(2_600_000, 487, 1, "tb"),
    ];
    for closed in [false, true] {
        let o = obs(&msgs, closed);
        let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "closed={closed}: {:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC);
    }
}

/// A final that reached the UAC before the provisional leaves nothing to
/// PRACK: no occasion.
#[test]
fn a_provisional_after_the_final_is_no_occasion() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        final_status(1_100_000, 487, 1, "tb"),
        reliable(1_150_000, UAS, UAC, 183, 1, 5, "tb"),
    ];
    let o = obs(&msgs, true);
    let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &o });
    assert!(f.is_empty(), "{f:?}");
}

/// The PRACK discharges the obligation whatever else follows.
#[test]
fn a_pracked_provisional_is_compliant() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
        prack(1_150_000, UAC, UAS, 2, 5, 1, "tb"),
        ok200(1_200_000, 1, "tb"),
    ];
    let closed = obs(&msgs, true);
    let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// `rack-without-known-invite`: the RAck CSeq-num is copied from the
/// acknowledged provisional, so a PRACK naming its own CSeq charges its
/// sender and the finding names the INVITE it should have quoted.
#[test]
fn a_rack_naming_the_pracks_own_cseq_charges_its_sender() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 183, 1, 12_800_221, "tb"),
        prack(1_150_000, UAC, UAS, 101, 12_800_221, 101, "tb"),
    ];
    let closed = obs(&msgs, true);
    let f = RackWithoutKnownInvite.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, UAC, "the PRACK's sender is at fault");
    let Decision::Violated(Evidence::UnknownRack { rack_cseq, known_cseqs, .. }) = &f[0].decision
    else {
        panic!("rack evidence: {:?}", f[0].decision)
    };
    assert_eq!(*rack_cseq, 101);
    assert_eq!(known_cseqs, &vec![1], "the INVITE the sender did open");
}

/// A PRACK whose RAck names the INVITE its sender opened is compliant; one
/// sent by an endpoint that opened no INVITE here is undecidable, never a
/// hit — the vantage has nothing to correlate against.
#[test]
fn a_rack_is_compliant_on_a_known_invite_and_undecidable_with_none() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 7),
        reliable(1_100_000, UAS, UAC, 180, 7, 5, "tb"),
        prack(1_150_000, UAC, UAS, 2, 5, 7, "tb"),
    ];
    let closed = obs(&msgs, true);
    let f = RackWithoutKnownInvite.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

    let orphan = vec![prack(1_150_000, UAC, UAS, 2, 5, 7, "tb")];
    let closed = obs(&orphan, true);
    let f = RackWithoutKnownInvite.eval(&WireView { msgs: &orphan, obs: &closed });
    assert_eq!(f.len(), 1);
    assert!(matches!(f[0].decision, Decision::Undecidable(_)), "{:?}", f[0].decision);
}

/// `no-overlapping-reliable-provisionals`: the second provisional waits for
/// the first one's PRACK — and a retransmission of an RSeq already sent is
/// no occasion at all.
#[test]
fn a_second_provisional_before_the_prack_charges_the_uas() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
        reliable(1_150_000, UAS, UAC, 180, 1, 5, "tb"), // retransmission
        reliable(1_200_000, UAS, UAC, 183, 1, 6, "tb"),
    ];
    let closed = obs(&msgs, true);
    let f = NoOverlappingReliableProvisionals.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1, "one occasion: the 183, not the retransmit: {f:?}");
    assert_eq!(f[0].emitter, UAS);
    assert_eq!(f[0].anchor, 3);
    let Decision::Violated(Evidence::Overlapping { unacked_rseq, rseq, .. }) = &f[0].decision
    else {
        panic!("overlap evidence: {:?}", f[0].decision)
    };
    assert_eq!((*unacked_rseq, *rseq), (5, 6));
}

/// The PRACK of the first provisional releases the UAS to send the next.
#[test]
fn a_second_provisional_after_the_prack_is_compliant() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
        prack(1_150_000, UAC, UAS, 2, 5, 1, "tb"),
        reliable(1_200_000, UAS, UAC, 183, 1, 6, "tb"),
    ];
    let closed = obs(&msgs, true);
    let f = NoOverlappingReliableProvisionals.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// `non-contiguous-rseq`: a gap charges the UAS at the offending
/// provisional, and each fork numbers its own early dialog — two forks
/// starting at the same RSeq is not a step backwards.
#[test]
fn an_rseq_gap_charges_the_uas_and_forks_number_independently() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
        reliable(1_200_000, UAS, UAC, 183, 1, 8, "tb"),
    ];
    let closed = obs(&msgs, true);
    let f = NonContiguousRseq.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].anchor, 2, "the offending message is the 183");
    let Decision::Violated(Evidence::RseqGap { prior_rseq, rseq, .. }) = &f[0].decision else {
        panic!("gap evidence: {:?}", f[0].decision)
    };
    assert_eq!((*prior_rseq, *rseq), (5, 8));

    let forked = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 183, 1, 1, "f1"),
        reliable(1_120_000, UAS, UAC, 183, 1, 1, "f2"),
    ];
    let closed = obs(&forked, true);
    assert!(
        NonContiguousRseq.eval(&WireView { msgs: &forked, obs: &closed }).is_empty(),
        "each fork's first provisional seeds its own RSeq space"
    );
}

/// `no-prack-of-out-of-order-rseq`: the offending message is the PRACK the
/// UAC sent for a provisional that jumped the gap; an in-order PRACK on the
/// same dialog stays compliant.
#[test]
fn pracking_an_out_of_order_provisional_charges_the_uac_at_the_prack() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 1, "tb"),
        prack(1_150_000, UAC, UAS, 2, 1, 1, "tb"),
        reliable(1_200_000, UAS, UAC, 183, 1, 5, "tb"),
        prack(1_250_000, UAC, UAS, 3, 5, 1, "tb"),
    ];
    let closed = obs(&msgs, true);
    let f = NoPrackOfOutOfOrderRseq.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "in order: {:?}", f[0].decision);
    assert_eq!(f[1].anchor, 4, "the offending message is the second PRACK");
    assert_eq!(f[1].emitter, UAC);
    let Decision::Violated(Evidence::OutOfOrderRack { rack_rseq, expected_rseq, .. }) =
        &f[1].decision
    else {
        panic!("order evidence: {:?}", f[1].decision)
    };
    assert_eq!((*rack_rseq, *expected_rseq), (5, 2));
}

/// A PRACK naming an RSeq no provisional on this view carried to its sender
/// says nothing about order — undecidable, never a hit.
#[test]
fn a_prack_for_an_unseen_rseq_is_undecidable_on_order() {
    let msgs = vec![invite(1_000_000, UAC, UAS, 1), prack(1_150_000, UAC, UAS, 2, 9, 1, "tb")];
    let closed = obs(&msgs, true);
    let f = NoPrackOfOutOfOrderRseq.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1);
    assert!(matches!(f[0].decision, Decision::Undecidable(_)), "{:?}", f[0].decision);
}

// ── the §3 negotiation half ─────────────────────────────────────────────
//
// These rules pair a request an endpoint TOOK on a top-Via branch with the
// responses it SENT there, so their fixtures carry a branch (the ones above
// deliberately do not — the obligation half never keys on one).

const INV_BRANCH: &str = "z9hG4bK-i";
const PRACK_BRANCH: &str = "z9hG4bK-p";
const UPDATE_BRANCH: &str = "z9hG4bK-u";
const SDP: &str = "v=0\r\no=- 1 1 IN IP4 h\r\ns=-\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\n";

/// The body rows a message carries, as a rule reads them off the head.
fn body_rows(body: bool) -> String {
    match body {
        true => format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n", SDP.len()),
        false => "Content-Length: 0\r\n".to_string(),
    }
}

/// The reliable-provisional markers §3 requires of BOTH halves.
fn reliable_rows(rseq: u64) -> String {
    format!("Require: 100rel\r\nRSeq: {rseq}\r\n")
}

/// An INVITE the UAS takes, with caller-chosen option-tag rows, To tag and
/// body.
fn invite_on(at_us: u64, extra: &str, to_tag: Option<&str>, body: bool) -> Msg {
    let to = match to_tag {
        Some(t) => format!("<sip:b@h>;tag={t}"),
        None => "<sip:b@h>".to_string(),
    };
    let head = format!(
        "INVITE sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={INV_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: {to}\r\nCSeq: 1 INVITE\r\n{extra}{}\r\n",
        body_rows(body)
    );
    let mut m =
        msg(at_us, UAC, UAS, Kind::Request { method: "INVITE".to_string() }, 1, to_tag, head);
    m.via_branch = Some(INV_BRANCH.to_string());
    m
}

/// An INVITE-transaction response the UAS sends.
fn inv_resp(at_us: u64, status: u16, extra: &str, body: bool) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={INV_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 1 INVITE\r\n{extra}{}\r\n",
        body_rows(body)
    );
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 1, Some("tb"), head);
    m.via_branch = Some(INV_BRANCH.to_string());
    m
}

/// An in-dialog UPDATE the UAS takes (RFC 3311) — a non-INVITE request, so
/// no provisional it draws may be reliable.
fn update_on(at_us: u64, extra: &str) -> Msg {
    let head = format!(
        "UPDATE sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={UPDATE_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 UPDATE\r\n{extra}\
             Content-Length: 0\r\n\r\n"
    );
    let mut m =
        msg(at_us, UAC, UAS, Kind::Request { method: "UPDATE".to_string() }, 2, Some("tb"), head);
    m.cseq_method = "UPDATE".to_string();
    m.via_branch = Some(UPDATE_BRANCH.to_string());
    m
}

/// The UAS's response on that UPDATE transaction.
fn update_resp(at_us: u64, status: u16, extra: &str) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={UPDATE_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 UPDATE\r\n{extra}\
             Content-Length: 0\r\n\r\n"
    );
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 2, Some("tb"), head);
    m.cseq_method = "UPDATE".to_string();
    m.via_branch = Some(UPDATE_BRANCH.to_string());
    m
}

/// A PRACK the UAC sends, its `RAck` spelled out.
fn prack_on(at_us: u64, rack: &str, body: bool) -> Msg {
    let head = format!(
        "PRACK sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={PRACK_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n{}\r\n",
        body_rows(body)
    );
    let mut m =
        msg(at_us, UAC, UAS, Kind::Request { method: "PRACK".to_string() }, 2, Some("tb"), head);
    m.cseq_method = "PRACK".to_string();
    m.via_branch = Some(PRACK_BRANCH.to_string());
    m
}

/// The UAS's response on that PRACK's own server transaction.
fn prack_answer(at_us: u64, status: u16) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={PRACK_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 PRACK\r\n\
             Content-Length: 0\r\n\r\n"
    );
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 2, Some("tb"), head);
    m.cseq_method = "PRACK".to_string();
    m.via_branch = Some(PRACK_BRANCH.to_string());
    m
}

/// The same message crossing the other way — the vantage of a hop that TOOK
/// what the builders above have it send.
fn inbound(mut m: Msg) -> Msg {
    std::mem::swap(&mut m.src, &mut m.dst);
    m
}

fn decide(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<crate::verdict::Finding> {
    rule.eval(&WireView { msgs, obs: &obs(msgs, true) })
}

/// `require-reliable-1xx-on-require`: the INVITE demanded the extension, so
/// a plain 18x is neither half of §3's disjunction — and the 420 rejecting
/// the extension discharges the transaction whenever on it it goes out.
#[test]
fn a_plain_18x_against_require_100rel_charges_the_uas() {
    let msgs = vec![
        invite_on(1_000_000, "Require: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, "", false),
    ];
    let f = decide(&RequireReliable1xxOnRequire, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, UAS);
    assert_eq!(f[0].anchor, 1, "the occasion rests on the provisional");
    let Decision::Violated(Evidence::Unreliable1xx { status, invite_msg, .. }) = &f[0].decision
    else {
        panic!("unreliable-1xx evidence: {:?}", f[0].decision)
    };
    assert_eq!((*status, *invite_msg), (180, 0));

    let reliable = vec![
        invite_on(1_000_000, "Require: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
    ];
    assert!(matches!(
        decide(&RequireReliable1xxOnRequire, &reliable)[0].decision,
        Decision::Compliant
    ));

    let rejected = vec![
        invite_on(1_000_000, "Require: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, "", false),
        inv_resp(1_200_000, 420, "Unsupported: 100rel\r\n", false),
    ];
    assert!(
        decide(&RequireReliable1xxOnRequire, &rejected).iter().all(|f| !f.violated()),
        "the 420 discharges the transaction",
    );
}

/// `reliable-needs-client-opt-in`: `Supported` alone licenses the PRACK
/// machinery; an INVITE this vantage never carried witnesses no
/// negotiation and settles nothing.
#[test]
fn a_reliable_1xx_without_client_opt_in_charges_the_uas() {
    let msgs = vec![
        invite_on(1_000_000, "", None, false),
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
    ];
    let f = decide(&ReliableNeedsClientOptIn, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAS);

    let opted = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
    ];
    assert!(matches!(decide(&ReliableNeedsClientOptIn, &opted)[0].decision, Decision::Compliant));

    let orphan = vec![inv_resp(1_100_000, 180, &reliable_rows(1), false)];
    assert!(matches!(
        decide(&ReliableNeedsClientOptIn, &orphan)[0].decision,
        Decision::Undecidable(_)
    ));
}

/// `no-reliable-1xx-on-in-dialog`: the METHOD of the request it answers is
/// what makes the provisional an offence (RFC 3262 §3 — "any method but
/// INVITE"), so an UPDATE is charged and a re-INVITE is not.
#[test]
fn a_reliable_1xx_to_a_non_invite_request_charges_the_uas() {
    let msgs = vec![
        update_on(1_000_000, "Supported: 100rel\r\n"),
        update_resp(1_100_000, 183, &reliable_rows(1)),
    ];
    let f = decide(&NoReliable1xxOnInDialog, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::InDialogReliable1xx { method, request_msg, .. }) =
        &f[0].decision
    else {
        panic!("non-INVITE evidence: {:?}", f[0].decision)
    };
    assert_eq!((method.as_str(), *request_msg), ("UPDATE", 0));

    let initial = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
    ];
    assert!(matches!(decide(&NoReliable1xxOnInDialog, &initial)[0].decision, Decision::Compliant));
}

/// A re-INVITE is an INVITE: RFC 3262 §3's To-tag prohibition is written
/// for a proxy ("unlike a UAS"), and RFC 6141 §4.6 has a UAS answer a
/// target-refresh re-INVITE with a reliable provisional.
#[test]
fn a_reliable_1xx_to_a_re_invite_is_compliant() {
    let msgs = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", Some("tb"), false),
        inv_resp(1_100_000, 183, &reliable_rows(1), false),
    ];
    let f = decide(&NoReliable1xxOnInDialog, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// `unmatched-prack-proxied`: charged to the endpoint that TOOK the PRACK,
/// and discharged either by an `RSeq` it had taken itself or by the same
/// `RAck` triple going back out.
#[test]
fn a_prack_matching_nothing_local_and_forwarded_nowhere_charges_its_taker() {
    let msgs = vec![
        inbound(inv_resp(1_100_000, 180, &reliable_rows(1), false)),
        prack_on(1_200_000, "9 1 INVITE", false),
    ];
    let f = decide(&UnmatchedPrackProxied, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, UAS, "the endpoint that took the PRACK is charged");
    let Decision::Violated(Evidence::PrackAbsorbed { rack_rseq, known_rseqs, .. }) = &f[0].decision
    else {
        panic!("absorbed evidence: {:?}", f[0].decision)
    };
    assert_eq!((*rack_rseq, known_rseqs.as_slice()), (9, &[1][..]));

    let matched = vec![
        inbound(inv_resp(1_100_000, 180, &reliable_rows(1), false)),
        prack_on(1_200_000, "1 1 INVITE", false),
    ];
    assert!(matches!(decide(&UnmatchedPrackProxied, &matched)[0].decision, Decision::Compliant));

    let forwarded = vec![
        inbound(inv_resp(1_100_000, 180, &reliable_rows(1), false)),
        prack_on(1_200_000, "9 1 INVITE", false),
        inbound(prack_on(1_250_000, "9 1 INVITE", false)),
    ];
    let out = decide(&UnmatchedPrackProxied, &forwarded);
    assert!(
        out.iter().filter(|f| f.emitter == UAS).all(|f| !f.violated()),
        "forwarded, not absorbed: {out:?}",
    );
}

/// `prack-2xx-or-481`: the answer is the UAS's own RSeq state read back,
/// and `matched` is read as of the PRACK's arrival.
#[test]
fn a_prack_draws_2xx_on_a_match_and_481_without_one() {
    let wrong = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        prack_on(1_200_000, "1 1 INVITE", false),
        prack_answer(1_250_000, 481),
    ];
    let f = decide(&Prack2xxOr481, &wrong);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::PrackAnsweredWrongly { status, rack_matched, .. }) =
        &f[0].decision
    else {
        panic!("prack-answer evidence: {:?}", f[0].decision)
    };
    assert_eq!((*status, *rack_matched), (481, true));

    let right = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        prack_on(1_200_000, "1 1 INVITE", false),
        prack_answer(1_250_000, 200),
    ];
    assert!(matches!(decide(&Prack2xxOr481, &right)[0].decision, Decision::Compliant));

    let unmatched = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        prack_on(1_200_000, "9 1 INVITE", false),
        prack_answer(1_250_000, 200),
    ];
    let f = decide(&Prack2xxOr481, &unmatched);
    assert!(f[0].violated(), "no match owes a 481: {:?}", f[0].decision);

    let rejected = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        prack_on(1_200_000, "9 1 INVITE", false),
        prack_answer(1_250_000, 481),
    ];
    assert!(matches!(decide(&Prack2xxOr481, &rejected)[0].decision, Decision::Compliant));
}

/// `prack-2xx-or-481`: the match is the whole §7.2 `RAck` — the right
/// `RSeq` under a CSeq-num of no INVITE this UAS answered reliably, or
/// under a method no reliable provisional answers, names nothing and owes
/// the 481.
#[test]
fn a_prack_matches_on_every_rack_token_not_the_rseq_alone() {
    let wrong_cseq = |status: u16| {
        vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            prack_on(1_200_000, "1 101 INVITE", false),
            prack_answer(1_250_000, status),
        ]
    };
    assert!(matches!(decide(&Prack2xxOr481, &wrong_cseq(481))[0].decision, Decision::Compliant));
    let f = decide(&Prack2xxOr481, &wrong_cseq(200));
    let Decision::Violated(Evidence::PrackAnsweredWrongly {
        status, rack_matched, rack_rseq, ..
    }) = &f[0].decision
    else {
        panic!("prack-answer evidence: {:?}", f[0].decision)
    };
    assert_eq!((*status, *rack_matched, *rack_rseq), (200, false, 1));

    let wrong_method = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        prack_on(1_200_000, "1 1 UPDATE", false),
        prack_answer(1_250_000, 481),
    ];
    assert!(matches!(decide(&Prack2xxOr481, &wrong_method)[0].decision, Decision::Compliant));
}

/// `prack-2xx-or-481`: the match is read within the early dialog the PRACK
/// names. Two forks number their `RSeq`s independently (§3, errata 4600),
/// so a sibling fork's live number PRACKed in this fork's dialog names
/// nothing here and owes the 481.
#[test]
fn a_sibling_forks_rseq_pracked_in_this_dialog_matches_nothing() {
    let cross_fork = |status: u16| {
        vec![
            fork_resp(1_100_000, 183, &reliable_rows(1), "f1", false),
            fork_resp(1_150_000, 183, &reliable_rows(200), "f2", false),
            fork_prack(1_200_000, "200 1 INVITE", "f1", PRACK_BRANCH),
            prack_answer(1_250_000, status),
        ]
    };
    assert!(matches!(decide(&Prack2xxOr481, &cross_fork(481))[0].decision, Decision::Compliant));
    let f = decide(&Prack2xxOr481, &cross_fork(200));
    let Decision::Violated(Evidence::PrackAnsweredWrongly { rack_matched, rack_rseq, .. }) =
        &f[0].decision
    else {
        panic!("prack-answer evidence: {:?}", f[0].decision)
    };
    assert_eq!((*rack_matched, *rack_rseq), (false, 200));

    let own_fork = vec![
        fork_resp(1_100_000, 183, &reliable_rows(1), "f1", false),
        fork_resp(1_150_000, 183, &reliable_rows(200), "f2", false),
        fork_prack(1_200_000, "200 1 INVITE", "f2", PRACK_BRANCH),
        prack_answer(1_250_000, 200),
    ];
    assert!(matches!(decide(&Prack2xxOr481, &own_fork)[0].decision, Decision::Compliant));
}

/// `delay-2xx-on-unacked-reliable-1xx-with-sdp`: ONE occasion — the 2xx —
/// naming every RSeq still outstanding; a bodiless provisional put no
/// description on the wire and is no occasion at all.
#[test]
fn the_2xx_waits_for_the_prack_of_an_offer_sent_reliably() {
    let early = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        inv_resp(1_200_000, 200, "", false),
    ];
    let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &early);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].anchor, 1, "the occasion rests on the 2xx");
    let Decision::Violated(Evidence::AnsweredOverUnackedOffer { unacked_rseqs, .. }) =
        &f[0].decision
    else {
        panic!("unacked-offer evidence: {:?}", f[0].decision)
    };
    assert_eq!(unacked_rseqs.as_slice(), &[1]);

    let pracked = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_150_000, "1 1 INVITE", true),
        inv_resp(1_200_000, 200, "", false),
    ];
    assert!(matches!(
        decide(&Delay2xxOnUnackedReliable1xxWithSdp, &pracked)[0].decision,
        Decision::Compliant
    ));

    let bodiless = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        inv_resp(1_200_000, 200, "", false),
    ];
    assert!(decide(&Delay2xxOnUnackedReliable1xxWithSdp, &bodiless).is_empty());

    let two = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        inv_resp(1_150_000, 183, &reliable_rows(2), true),
        inv_resp(1_200_000, 200, "", false),
    ];
    let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &two);
    assert_eq!(f.len(), 1, "one occasion, the 2xx: {f:?}");
    let Decision::Violated(Evidence::AnsweredOverUnackedOffer { unacked_rseqs, .. }) =
        &f[0].decision
    else {
        panic!("unacked-offer evidence: {:?}", f[0].decision)
    };
    assert_eq!(unacked_rseqs.as_slice(), &[1, 2], "both named on the one finding");
}

/// A retransmission of a PRACKed provisional crossing the acknowledgement
/// on the wire — RFC 3262 §3 has the UAS repeat until the PRACK reaches it
/// — carries the same `RSeq` and answers to the same PRACK: the 2xx behind
/// it owes nothing more.
#[test]
fn a_retransmission_after_the_prack_reopens_no_obligation() {
    let crossed = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_150_000, "1 1 INVITE", true),
        inv_resp(1_160_000, 183, &reliable_rows(1), true),
        inv_resp(1_200_000, 200, "", false),
    ];
    let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &crossed);
    assert_eq!(f.len(), 1, "one occasion, the 2xx: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

    // A repeat that was never PRACKed at all still leaves the offer open.
    let never = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        inv_resp(1_160_000, 183, &reliable_rows(1), true),
        inv_resp(1_200_000, 200, "", false),
    ];
    let Decision::Violated(Evidence::AnsweredOverUnackedOffer { unacked_rseqs, .. }) =
        &decide(&Delay2xxOnUnackedReliable1xxWithSdp, &never)[0].decision
    else {
        panic!("unacked-offer evidence")
    };
    assert_eq!(unacked_rseqs.as_slice(), &[1], "named once, not once per copy");
}

/// **A repeat cannot OPEN an obligation, but it MEETS one** — the wire
/// model's ruling, and load-bearing here: a vantage that lost the first
/// copy of a PRACK and kept the retransmission carries that copy as the
/// whole acknowledgement, so the 2xx behind it owes nothing and the answer
/// it drew is judged against the transaction it restates.
#[test]
fn a_repeat_prack_still_discharges_what_it_acknowledges() {
    let retransmitted = |at_us: u64| {
        let mut p = prack_on(at_us, "1 1 INVITE", false);
        p.repeat = true;
        p
    };
    let answered = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        retransmitted(1_150_000),
        inv_resp(1_200_000, 200, "", false),
    ];
    let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &answered);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

    let judged = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        retransmitted(1_150_000),
        prack_answer(1_200_000, 200),
    ];
    let f = decide(&Prack2xxOr481, &judged);
    assert_eq!(f.len(), 1, "the restated PRACK names the transaction: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A reliable provisional on a chosen early dialog — what a fork looks like
/// on the branch it shares with its sibling.
fn fork_resp(at_us: u64, status: u16, extra: &str, to_tag: &str, sdp: bool) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={INV_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\nCSeq: 1 INVITE\r\n\
             {extra}{}\r\n",
        body_rows(sdp)
    );
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 1, Some(to_tag), head);
    m.via_branch = Some(INV_BRANCH.to_string());
    m
}

/// The PRACK of one fork's provisional, on that fork's own dialog.
fn fork_prack(at_us: u64, rack: &str, to_tag: &str, branch: &str) -> Msg {
    let head = format!(
        "PRACK sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={branch}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\nCSeq: 2 PRACK\r\n\
             RAck: {rack}\r\nContent-Length: 0\r\n\r\n"
    );
    let mut m =
        msg(at_us, UAC, UAS, Kind::Request { method: "PRACK".to_string() }, 2, Some(to_tag), head);
    m.cseq_method = "PRACK".to_string();
    m.via_branch = Some(branch.to_string());
    m
}

/// Two forks answer one INVITE on ONE branch and number their `RSeq`s
/// independently (§12.1.2), so the winning fork's 2xx is judged against its
/// OWN offer and its own PRACK — never against the abandoned fork's copy of
/// the same number.
#[test]
fn each_fork_answers_for_its_own_offer() {
    let both_pracked = vec![
        fork_resp(1_100_000, 183, &reliable_rows(1), "f1", true),
        fork_prack(1_150_000, "1 1 INVITE", "f1", "z9hG4bK-p1"),
        fork_resp(1_200_000, 183, &reliable_rows(1), "f2", true),
        fork_prack(1_250_000, "1 1 INVITE", "f2", "z9hG4bK-p2"),
        fork_resp(1_300_000, 200, "", "f2", false),
    ];
    let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &both_pracked);
    assert_eq!(f.len(), 1, "one 2xx, one occasion: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

    let wrong_fork = vec![
        fork_resp(1_100_000, 183, &reliable_rows(1), "f1", true),
        fork_prack(1_150_000, "1 1 INVITE", "f1", "z9hG4bK-p1"),
        fork_resp(1_200_000, 183, &reliable_rows(1), "f2", true),
        fork_resp(1_300_000, 200, "", "f2", false),
    ];
    let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &wrong_fork);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "the winning fork's own offer is unacked: {:?}", f[0].decision);
}

/// A PRACK after the final is answered by its match (RFC 3262 §3-§4): no
/// registered rule charges a 481 to one naming nothing, and
/// a non-2xx to one naming a provisional sent is still charged.
#[test]
fn a_prack_after_the_final_draws_what_its_match_decides() {
    let unmatched = vec![
        inv_resp(1_100_000, 200, "", false),
        prack_on(1_200_000, "1 1 INVITE", false),
        prack_answer(1_250_000, 481),
    ];
    for rule in crate::all_rules() {
        let charged: Vec<_> = decide(rule.as_ref(), &unmatched)
            .into_iter()
            .filter(|f| f.violated() && f.anchor == 2)
            .collect();
        assert!(charged.is_empty(), "a 481 to a late PRACK naming nothing: {charged:?}");
    }

    let matched = vec![
        inv_resp(1_000_000, 180, &reliable_rows(1), false),
        inv_resp(1_100_000, 486, "", false),
        prack_on(1_200_000, "1 1 INVITE", false),
        prack_answer(1_250_000, 481),
    ];
    let f = decide(&Prack2xxOr481, &matched);
    assert!(
        f.iter().any(|f| f.violated()),
        "a late PRACK naming a provisional sent owes a 2xx: {f:?}"
    );
}

/// `no-new-reliable-1xx-after-final`: a fresh RSeq after the final is the
/// offence; a retransmission of one the transaction already used is not.
#[test]
fn a_new_reliable_1xx_after_the_final_charges_the_uas() {
    let msgs = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        inv_resp(1_200_000, 200, "", false),
        inv_resp(1_300_000, 183, &reliable_rows(2), false),
    ];
    let f = decide(&NoNewReliable1xxAfterFinal, &msgs);
    assert_eq!(f.len(), 2, "both provisionals are occasions: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    let Decision::Violated(Evidence::Reliable1xxAfterFinal { rseq, prior_final_status, .. }) =
        &f[1].decision
    else {
        panic!("stray-1xx evidence: {:?}", f[1].decision)
    };
    assert_eq!((*rseq, *prior_final_status), (2, 200));

    let retransmitted = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        inv_resp(1_200_000, 200, "", false),
        inv_resp(1_300_000, 180, &reliable_rows(1), false),
    ];
    assert!(decide(&NoNewReliable1xxAfterFinal, &retransmitted).iter().all(|f| !f.violated()));
}

/// `no-prack-of-100-trying`: the occasion is the bogus 100 the UAC took,
/// and a violated one rests on the PRACK it drew.
#[test]
fn pracking_a_100_trying_charges_the_uac_at_the_prack() {
    let msgs = vec![
        inv_resp(1_000_000, 100, &reliable_rows(7), false),
        prack_on(1_100_000, "7 1 INVITE", false),
    ];
    let f = decide(&NoPrackOf100Trying, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, UAC);
    assert_eq!(f[0].anchor, 1, "the occasion rests on the PRACK");
    let Decision::Violated(Evidence::PrackedTrying { rseq, trying_msg, .. }) = &f[0].decision
    else {
        panic!("pracked-trying evidence: {:?}", f[0].decision)
    };
    assert_eq!((*rseq, *trying_msg), (7, 0));

    let ignored = vec![inv_resp(1_000_000, 100, &reliable_rows(7), false)];
    let f = decide(&NoPrackOf100Trying, &ignored);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// `prack-answers-1xx-offer` reads the offer in its EARLY DIALOG: each fork
/// numbers its `RSeq`s on its own (RFC 3262 §3, errata 4600), so a PRACK on
/// one fork's dialog naming the number another fork's offer carried answers
/// no offer there — no occasion — while the offer's own PRACK still owes it.
#[test]
fn a_prack_naming_another_forks_offer_rseq_is_no_occasion() {
    let on = |mut m: Msg, tag: &str| {
        m.to_tag = Some(tag.to_string());
        m
    };
    let msgs = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        on(inv_resp(1_100_000, 183, &reliable_rows(1), true), "f1"),
        on(prack_on(1_150_000, "1 1 INVITE", true), "f1"),
        on(inv_resp(1_200_000, 183, &reliable_rows(200), true), "f2"),
        on(prack_on(1_250_000, "200 1 INVITE", true), "f2"),
        on(prack_on(1_300_000, "200 1 INVITE", false), "f1"),
    ];
    let f = decide(&PrackAnswers1xxOffer, &msgs);
    assert_eq!(f.len(), 2, "the two PRACKs of the two offers only: {f:?}");
    assert!(f.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{f:?}");
}

/// `prack-answers-1xx-offer`: the 1xx body is the OFFER only where the
/// INVITE carried none — the standard setup (offer in the INVITE, answer in
/// the 183, bodiless PRACK) is no occasion at all.
#[test]
fn a_bodiless_prack_answering_a_1xx_offer_charges_its_sender() {
    let msgs = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_200_000, "1 1 INVITE", false),
    ];
    let f = decide(&PrackAnswers1xxOffer, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, UAC, "the PRACK's sender owes the answer");
    let Decision::Violated(Evidence::PrackWithoutAnswer { rack_rseq, offer_1xx_msg, .. }) =
        &f[0].decision
    else {
        panic!("prack-answer evidence: {:?}", f[0].decision)
    };
    assert_eq!((*rack_rseq, *offer_1xx_msg), (1, 1));

    let answered = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_200_000, "1 1 INVITE", true),
    ];
    assert!(matches!(decide(&PrackAnswers1xxOffer, &answered)[0].decision, Decision::Compliant));

    let standard = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, true),
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_200_000, "1 1 INVITE", false),
    ];
    assert!(
        decide(&PrackAnswers1xxOffer, &standard).is_empty(),
        "the 183's body is the ANSWER (RFC 3264 §5), so the PRACK owes none",
    );
}

/// The extension is invisible to the obligation half: the five rules ported
/// before it decide the same shape the same way, whether or not the new
/// server-transaction facts are collected alongside.
#[test]
fn the_server_transaction_half_changes_no_obligation_verdict() {
    let msgs = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, &reliable_rows(5), false),
    ];
    let f = decide(&UnackedReliableProvisional, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "the UAC took it and never PRACKed: {:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAC);
}
