//! The PRACK's own transaction.

use super::*;

/// The same message crossing the other way — the vantage of a hop that TOOK
/// what the fixture builders have it send.
fn inbound(mut m: Msg) -> Msg {
    std::mem::swap(&mut m.src, &mut m.dst);
    m
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

/// `prack-2xx-or-481`: a provisional an earlier PRACK of another transaction
/// drew a 2xx for is acknowledged, so a new PRACK naming it matches nothing
/// unacknowledged and owes the 481 (RFC 3262 §3).
#[test]
fn a_new_prack_of_an_acknowledged_provisional_draws_481() {
    let again = |m: Msg| {
        let mut m = m;
        m.via_branch = Some("z9hG4bK-second-prack".to_string());
        m
    };
    let second = |status: u16| {
        vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            prack_on(1_200_000, "1 1 INVITE", false),
            prack_answer(1_250_000, 200),
            again(prack_on(1_300_000, "1 1 INVITE", false)),
            again(prack_answer(1_350_000, status)),
        ]
    };
    let f = decide(&Prack2xxOr481, &second(481));
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(f.iter().all(|f| !f.violated()), "the first 200, the second 481: {f:?}");
    let f = decide(&Prack2xxOr481, &second(200));
    assert!(f[1].violated(), "a 2xx to a PRACK of an acknowledged provisional: {f:?}");

    let first_refused = vec![
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
        prack_on(1_200_000, "1 1 INVITE", false),
        prack_answer(1_250_000, 500),
        again(prack_on(1_300_000, "1 1 INVITE", false)),
        again(prack_answer(1_350_000, 200)),
    ];
    let f = decide(&Prack2xxOr481, &first_refused);
    assert!(!f[1].violated(), "a PRACK the UAS did not 2xx acknowledged nothing: {f:?}");
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
        let charged: Vec<_> = decide(rule.body, &unmatched)
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
