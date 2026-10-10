//! The UAC's PRACK: owed, ordered, aimed at a known INVITE, never of a 100.

use super::*;

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

/// The server-transaction half is invisible to the obligation half: the
/// obligation rules decide the same shape the same way, whether or not the
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
