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
