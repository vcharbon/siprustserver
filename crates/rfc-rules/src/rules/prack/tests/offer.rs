//! An offer carried by a reliable provisional.

use super::*;

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

    let framed_none = vec![
        framing_no_description(inv_resp(1_100_000, 183, &reliable_rows(1), true)),
        inv_resp(1_200_000, 200, "", false),
    ];
    assert!(
        decide(&Delay2xxOnUnackedReliable1xxWithSdp, &framed_none).is_empty(),
        "a body framing no description holds no 2xx",
    );

    let unrecorded = vec![
        unrecorded_body(inv_resp(1_100_000, 183, &reliable_rows(1), true)),
        inv_resp(1_200_000, 200, "", false),
    ];
    assert!(matches!(
        decide(&Delay2xxOnUnackedReliable1xxWithSdp, &unrecorded)[0].decision,
        Decision::Undecidable(_)
    ));

    let unrecorded_beside_pracked = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_150_000, "1 1 INVITE", true),
        unrecorded_body(inv_resp(1_160_000, 183, &reliable_rows(2), true)),
        inv_resp(1_200_000, 200, "", false),
    ];
    assert!(
        matches!(
            decide(&Delay2xxOnUnackedReliable1xxWithSdp, &unrecorded_beside_pracked)[0].decision,
            Decision::Undecidable(_)
        ),
        "an unPRACKed provisional of unknown body leaves the 2xx undecided",
    );

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

/// A UAS whose Contact is not the address it answers the INVITE from takes
/// the PRACK at its Contact (RFC 3262 §4: the PRACK is an in-dialog request
/// to the remote target). That PRACK acknowledges the offer on the early
/// dialog; one sent after the 2xx, one by another sender, or one naming
/// another INVITE's `RSeq` space does not.
#[test]
fn a_prack_taken_at_the_uas_contact_discharges_the_offer() {
    let pracked = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        via_contact(prack_on(1_150_000, "1 1 INVITE", true)),
        via_contact(prack_answer(1_160_000, 200)),
        inv_resp(1_200_000, 200, "", false),
    ];
    let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &pracked);
    assert_eq!(f.len(), 1, "one occasion, the 2xx: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

    let late = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        inv_resp(1_200_000, 200, "", false),
        via_contact(prack_on(1_250_000, "1 1 INVITE", true)),
    ];
    assert!(decide(&Delay2xxOnUnackedReliable1xxWithSdp, &late)[0].violated());

    let mut stranger = via_contact(prack_on(1_150_000, "1 1 INVITE", true));
    stranger.src = "10.0.0.9:5060".to_string();
    let other_sender = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        stranger,
        inv_resp(1_200_000, 200, "", false),
    ];
    assert!(decide(&Delay2xxOnUnackedReliable1xxWithSdp, &other_sender)[0].violated());

    let other_invite = vec![
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        via_contact(prack_on(1_150_000, "1 7 INVITE", true)),
        inv_resp(1_200_000, 200, "", false),
    ];
    assert!(decide(&Delay2xxOnUnackedReliable1xxWithSdp, &other_invite)[0].violated());
}

/// A proxy that did not record-route leaves the route set empty (RFC 3261
/// §16.6, §12.1.2): the UAC sends the PRACK straight to the UAS while the
/// provisional and the 2xx cross the proxy. The PRACK the UAS took
/// discharges its 2xx, and the one the UAC sent discharges the proxy's copy.
#[test]
fn a_prack_sent_past_a_proxy_discharges_the_offer() {
    let [offer, offer_relayed] = through_proxy(inv_resp(1_100_000, 183, &reliable_rows(1), true));
    let [ok, ok_relayed] = through_proxy(inv_resp(1_200_000, 200, "", false));
    let flow = vec![
        offer,
        offer_relayed,
        prack_on(1_150_000, "1 1 INVITE", true),
        prack_answer(1_160_000, 200),
        ok,
        ok_relayed,
    ];
    let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &flow);
    assert_eq!(f.len(), 2, "the UAS's 2xx and the proxy's copy: {f:?}");
    assert!(f.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{f:?}");
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

/// `prack-answers-1xx-offer`: the 1xx description is the OFFER only where
/// the INVITE carried none — the standard setup (offer in the INVITE, answer
/// in the 183, bodiless PRACK) is no occasion at all. Each side reads a
/// description, never a body: a body framing none is neither offer nor
/// answer, and a body the vantage did not record decides nothing.
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

    let invite_framing_none = vec![
        framing_no_description(invite_on(1_000_000, "Supported: 100rel\r\n", None, true)),
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_200_000, "1 1 INVITE", false),
    ];
    assert!(
        decide(&PrackAnswers1xxOffer, &invite_framing_none)[0].violated(),
        "an INVITE body framing no description is no offer: the 183's is",
    );

    let prack_framing_none = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        framing_no_description(prack_on(1_200_000, "1 1 INVITE", true)),
    ];
    assert!(
        decide(&PrackAnswers1xxOffer, &prack_framing_none)[0].violated(),
        "a PRACK body framing no description carries no answer",
    );

    let provisional_framing_none = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        framing_no_description(inv_resp(1_100_000, 183, &reliable_rows(1), true)),
        prack_on(1_200_000, "1 1 INVITE", false),
    ];
    assert!(
        decide(&PrackAnswers1xxOffer, &provisional_framing_none).is_empty(),
        "a provisional body framing no description makes no offer",
    );

    let undecided = |msgs: &[Msg]| {
        matches!(decide(&PrackAnswers1xxOffer, msgs)[0].decision, Decision::Undecidable(_))
    };
    let prack_unrecorded = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        unrecorded_body(prack_on(1_200_000, "1 1 INVITE", true)),
    ];
    assert!(undecided(&prack_unrecorded), "the PRACK's body is unknown");
    let invite_unrecorded = vec![
        unrecorded_body(invite_on(1_000_000, "Supported: 100rel\r\n", None, false)),
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_200_000, "1 1 INVITE", false),
    ];
    assert!(undecided(&invite_unrecorded), "whether the INVITE offered is unknown");
    let provisional_unrecorded = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        unrecorded_body(inv_resp(1_100_000, 183, &reliable_rows(1), true)),
        prack_on(1_200_000, "1 1 INVITE", false),
    ];
    assert!(undecided(&provisional_unrecorded), "whether the 183 offered is unknown");
}

/// `prack-answers-1xx-offer` reads each INVITE transaction on its own: the
/// offer is the INVITE's or the provisional's per CSeq (RFC 3264 §5), and a
/// re-INVITE's provisionals number their `RSeq`s afresh (RFC 3262 §3), the
/// `RAck` naming the INVITE they answer (§7.2).
#[test]
fn each_invite_transaction_carries_its_own_offer() {
    let reinvite = |at_us, body| {
        on_invite_cseq(invite_on(at_us, "Supported: 100rel\r\n", Some("tb"), body), 2)
    };
    let reinvite_reliable =
        |at_us, rseq| on_invite_cseq(inv_resp(at_us, 183, &reliable_rows(rseq), true), 2);

    let offerless_reinvite = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, true),
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_200_000, "1 1 INVITE", false),
        reinvite(2_000_000, false),
        reinvite_reliable(2_100_000, 2),
        prack_on(2_200_000, "2 2 INVITE", false),
    ];
    let f = decide(&PrackAnswers1xxOffer, &offerless_reinvite);
    assert_eq!(f.len(), 1, "the re-INVITE's 183 alone made an offer: {f:?}");
    assert!(f[0].violated(), "its bodiless PRACK carries no answer: {f:?}");

    let rseq_restarted = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 183, &reliable_rows(1), true),
        prack_on(1_200_000, "1 1 INVITE", true),
        reinvite(2_000_000, true),
        reinvite_reliable(2_100_000, 1),
        prack_on(2_200_000, "1 2 INVITE", false),
    ];
    let f = decide(&PrackAnswers1xxOffer, &rseq_restarted);
    assert!(
        f.iter().all(|f| !f.violated()),
        "the re-INVITE's 183 answers its offer, whatever RSeq it restarts at: {f:?}",
    );
}
