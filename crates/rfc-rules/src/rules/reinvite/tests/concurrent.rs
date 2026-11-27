//! `concurrent-re-invite-500-or-491` (RFC 3261 §14.2).

use super::*;

/// The sender re-INVITEs while its own earlier INVITE has no final: §14.2's
/// first clause owes 500 with `Retry-After`, so a 491 is the violation.
#[test]
fn a_same_sender_race_answered_491_is_violated() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(3_000, BOB, ALICE, 491, "z9hG4bK-2", 3, "at", "bt", ""),
            rsp(4_000, BOB, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
        ],
    );
    assert_eq!(f.len(), 1, "one occasion, the racing re-INVITE: {f:?}");
    assert_eq!(f[0].rule, RuleId::ConcurrentReInvite500Or491);
    assert_eq!(f[0].emitter, BOB, "the UAS that owed the 500 is charged");
    assert_eq!(f[0].taker, ALICE);
    let Decision::Violated(Evidence::ConcurrentReInvite { answered_status, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*answered_status, 491);
}

/// A re-INVITE the strict reading refuses is neither an occasion nor in
/// progress: its 400 is owed, and a later re-INVITE races nothing.
#[test]
fn a_refused_reinvite_opens_no_race() {
    let mut refused = req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt");
    refused.refused = true;
    let mut racing = req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt");
    racing.refused = true;
    let msgs = [
        refused,
        racing,
        req(3_000, ALICE, BOB, "INVITE", "z9hG4bK-3", 4, "at", "bt"),
        rsp(4_000, BOB, ALICE, 200, "z9hG4bK-3", 4, "at", "bt", ""),
    ];
    assert!(eval(&ConcurrentReInvite500Or491, &msgs).is_empty());
}

/// A re-INVITE crossing one of the other party's at a hop that carries
/// both directions is §14.2's second clause: 491 meets it.
#[test]
fn a_crossing_race_answered_491_is_compliant() {
    let f = eval(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-a", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-b", 2, "bt", "at"),
            rsp(3_000, BOB, ALICE, 491, "z9hG4bK-b", 2, "bt", "at", ""),
            rsp(4_000, BOB, ALICE, 200, "z9hG4bK-a", 2, "at", "bt", ""),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A UAS that answers from another socket than the one it takes INVITEs at
/// completed each re-INVITE: sequential ones never race.
#[test]
fn sequential_re_invites_to_a_two_socket_uas_are_no_race() {
    const BOB_SENDS: &str = "10.0.0.2:41000";
    let f = eval(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            rsp(2_000, BOB_SENDS, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
            req(3_000, ALICE, BOB, "ACK", "z9hG4bK-k1", 2, "at", "bt"),
            req(4_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(5_000, BOB_SENDS, ALICE, 200, "z9hG4bK-2", 3, "at", "bt", ""),
            req(6_000, ALICE, BOB, "ACK", "z9hG4bK-k2", 3, "at", "bt"),
        ],
    );
    assert!(f.is_empty(), "no re-INVITE raced another: {f:?}");
}

/// A UAS that answers from another socket than the one it took a re-INVITE
/// at still has that re-INVITE in progress when the sender's next one reaches
/// the second socket: §14.2's 500 is owed there.
#[test]
fn a_re_invite_at_a_two_socket_uas_s_other_socket_races_the_pending_one() {
    const BOB_SENDS: &str = "10.0.0.2:41000";
    let msgs = [
        req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
        rsp(1_500, BOB_SENDS, ALICE, 180, "z9hG4bK-1", 2, "at", "bt", ""),
        req(2_000, ALICE, BOB_SENDS, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
        rsp(3_000, BOB_SENDS, ALICE, 200, "z9hG4bK-2", 3, "at", "bt", ""),
        req(3_500, ALICE, BOB_SENDS, "ACK", "z9hG4bK-k2", 3, "at", "bt"),
        rsp(4_000, BOB_SENDS, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
        req(4_500, ALICE, BOB, "ACK", "z9hG4bK-k1", 2, "at", "bt"),
    ];
    let f = hits(&ConcurrentReInvite500Or491, &msgs);
    assert_eq!(f.len(), 1, "{:?}", eval(&ConcurrentReInvite500Or491, &msgs));
    assert_eq!(f[0].emitter, BOB_SENDS, "the socket that took the racing re-INVITE is charged");
}

/// At a hop carrying both directions, a re-INVITE that finds its sender's
/// own INVITE AND a crossing one in flight is judged against its sender's:
/// 500 with Retry-After is owed and a 491 is the violation, whichever
/// pending branch sorts first.
#[test]
fn a_race_over_both_a_same_sender_and_a_crossing_invite_owes_the_500() {
    let msgs = |status: u16, head: &str| {
        vec![
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-0", 2, "bt", "at"),
            rsp(3_000, BOB, ALICE, 491, "z9hG4bK-0", 2, "bt", "at", ""),
            req(2_500, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(4_000, BOB, ALICE, status, "z9hG4bK-2", 3, "at", "bt", head),
            rsp(5_000, BOB, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
        ]
    };
    let on_the_newcomer =
        |f: Vec<Finding>| -> Vec<Finding> { f.into_iter().filter(|f| f.cseq == 3).collect() };
    let compliant =
        on_the_newcomer(eval(&ConcurrentReInvite500Or491, &msgs(500, "Retry-After: 3\r\n")));
    assert_eq!(compliant.len(), 1, "{compliant:?}");
    assert!(matches!(compliant[0].decision, Decision::Compliant), "{:?}", compliant[0].decision);
    let refused_491 = on_the_newcomer(hits(&ConcurrentReInvite500Or491, &msgs(491, "")));
    assert_eq!(refused_491.len(), 1, "{refused_491:?}");
}

/// The crossing owes 491, not the 500 of a same-sender race.
#[test]
fn a_crossing_race_answered_500_is_violated() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-a", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-b", 2, "bt", "at"),
            rsp(3_000, BOB, ALICE, 500, "z9hG4bK-b", 2, "bt", "at", "Retry-After: 3\r\n"),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
}

/// Answering the race 200 serves two offers at once — the violation, with
/// both transactions named.
#[test]
fn a_racing_re_invite_answered_200_is_violated() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(3_000, BOB, ALICE, 200, "z9hG4bK-2", 3, "at", "bt", ""),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::ConcurrentReInvite {
        branch,
        pending_invite_branch,
        pending_invite_msg,
        answered_status,
        retry_after,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(branch.as_str(), "z9hG4bK-2");
    assert_eq!(pending_invite_branch.as_str(), "z9hG4bK-1");
    assert_eq!(*pending_invite_msg, 0, "the INVITE it collided with");
    assert_eq!((*answered_status, *retry_after), (200, false));
    assert_eq!(f[0].anchor, 1, "the occasion rests on the racing re-INVITE");
}

/// §14.2's other legal answer: 500 CARRYING `Retry-After`. A bare 500 is
/// not it.
#[test]
fn a_500_discharges_only_with_retry_after() {
    let race = |head: &str| {
        vec![
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(3_000, BOB, ALICE, 500, "z9hG4bK-2", 3, "at", "bt", head),
        ]
    };
    assert!(hits(&ConcurrentReInvite500Or491, &race("Retry-After: 5\r\n")).is_empty());
    assert_eq!(hits(&ConcurrentReInvite500Or491, &race("")).len(), 1);
}

/// The 100 Trying precedes a compliant 500 — only the transaction's FINAL
/// answers §14.2, so the provisional must not stand in for it.
#[test]
fn a_100_trying_does_not_stand_in_for_the_final() {
    assert!(hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(3_000, BOB, ALICE, 100, "z9hG4bK-2", 3, "at", "bt", ""),
            rsp(4_000, BOB, ALICE, 500, "z9hG4bK-2", 3, "at", "bt", "Retry-After: 3\r\n"),
            rsp(5_000, BOB, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
        ],
    )
    .is_empty());
}

/// §17.2.1: a Timer-A retransmission of an ALREADY-ANSWERED re-INVITE
/// crosses its own 491 on the wire. It draws the recorded final again — it
/// is not a new transaction and must not re-enter the in-progress window,
/// or the next genuine re-INVITE's compliant 200 reads as a race.
#[test]
fn a_post_final_retransmit_does_not_re_enter_the_window() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            rsp(2_000, BOB, ALICE, 491, "z9hG4bK-1", 2, "at", "bt", ""),
            req(3_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(4_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(5_000, BOB, ALICE, 200, "z9hG4bK-2", 3, "at", "bt", ""),
        ],
    );
    assert!(f.is_empty(), "{f:?}");
}

/// A retransmission of an INVITE still in flight is the same act again,
/// never a second transaction to serialise against.
#[test]
fn an_in_flight_retransmit_is_no_race() {
    assert!(eval(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
        ],
    )
    .is_empty());
}

/// The race is read on the UNORDERED dialog: a callee-initiated re-INVITE
/// crossing the caller's is exactly the §14.2 collision, tags reversed and
/// all. BOB takes the first and SENDS the reversed one; only what it TOOK is
/// judged here.
#[test]
fn crossing_re_invites_collide_at_the_uas_whichever_way_they_point() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-a", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-b", 2, "bt", "at"),
            rsp(3_000, BOB, ALICE, 200, "z9hG4bK-b", 2, "bt", "at", ""),
        ],
    );
    assert_eq!(f.len(), 1, "the reversed re-INVITE races the first: {f:?}");
}

/// An INVITE with only one tag creates a dialog rather than riding one:
/// §14.2 has nothing to serialise it against.
#[test]
fn an_initial_invite_is_no_race() {
    assert!(eval(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 1, "at", ""),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 2, "at", ""),
        ],
    )
    .is_empty());
}

/// A race the observation stopped on top of settles nothing: silence
/// inside the window is truncation, not a missing answer. Closed, the window
/// collapses and the absence decides.
#[test]
fn an_unanswered_race_inside_the_window_is_undecidable() {
    let msgs = [
        req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
        req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
    ];
    let open = eval_open(&ConcurrentReInvite500Or491, &msgs);
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(!open[0].decided(), "{:?}", open[0].decision);
    assert_eq!(hits(&ConcurrentReInvite500Or491, &msgs).len(), 1);
}

/// alice's second re-INVITE, racing her first, and her CANCEL of it on its
/// branch (RFC 3261 §9.1).
fn a_cancelled_race(cancel_at_us: u64) -> [Msg; 3] {
    [
        req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
        req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
        req(cancel_at_us, ALICE, BOB, "CANCEL", "z9hG4bK-2", 3, "at", "bt"),
    ]
}

/// bob's answer to the CANCEL on the racing re-INVITE's branch.
fn cancel_answered(at_us: u64, status: u16) -> Msg {
    Msg {
        cseq_method: "CANCEL".to_string(),
        ..rsp(at_us, BOB, ALICE, status, "z9hG4bK-2", 3, "at", "bt", "")
    }
}

/// A CANCEL bob matched before he answered the racing re-INVITE leaves it one
/// final, 487 (RFC 3261 §9.2), whichever of the 487 and the CANCEL's 200
/// leaves first: §14.2 is not reached.
#[test]
fn a_racing_re_invite_cancelled_before_its_final_is_answered_487() {
    let racing_487 = rsp(4_000, BOB, ALICE, 487, "z9hG4bK-2", 3, "at", "bt", "");
    for (case, answers) in [
        ("the CANCEL's 200 first", [cancel_answered(3_500, 200), racing_487.clone()]),
        ("the 487 first", [racing_487.clone(), cancel_answered(4_500, 200)]),
    ] {
        let msgs: Vec<Msg> = a_cancelled_race(3_000).into_iter().chain(answers).collect();
        let f = eval(&ConcurrentReInvite500Or491, &msgs);
        assert_eq!(f.len(), 1, "{case}: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{case}: {:?}", f[0].decision);
    }
}

/// The 487 answers §14.2 only behind a CANCEL bob matched before it: with no
/// CANCEL, with the CANCEL refused 481, with the CANCEL arriving after the
/// 487, with a 2xx after the CANCEL, or with a CANCEL the strict reading
/// refuses before the 487 and an admitted one after it, the race is still
/// charged.
#[test]
fn a_racing_re_invite_s_487_without_a_matched_cancel_before_it_is_violated() {
    let racing = |status| rsp(4_000, BOB, ALICE, status, "z9hG4bK-2", 3, "at", "bt", "");
    let [first, second, cancel] = a_cancelled_race(3_000);
    let [.., late_cancel] = a_cancelled_race(5_000);
    let malformed_cancel = Msg { refused: true, ..cancel.clone() };
    let cases = [
        (
            "a refused CANCEL before the 487, an admitted one after it",
            vec![
                first.clone(),
                second.clone(),
                malformed_cancel,
                cancel_answered(3_500, 400),
                racing(487),
                late_cancel.clone(),
                cancel_answered(5_500, 200),
            ],
            487,
        ),
        ("no CANCEL", vec![first.clone(), second.clone(), racing(487)], 487),
        (
            "the CANCEL refused 481",
            vec![
                first.clone(),
                second.clone(),
                cancel.clone(),
                cancel_answered(3_500, 481),
                racing(487),
            ],
            487,
        ),
        (
            "the CANCEL after the 487",
            vec![
                first.clone(),
                second.clone(),
                racing(487),
                late_cancel,
                cancel_answered(5_500, 200),
            ],
            487,
        ),
        (
            "a 2xx after the CANCEL",
            vec![first, second, cancel, cancel_answered(3_500, 200), racing(200)],
            200,
        ),
    ];
    for (case, msgs, status) in cases {
        let f = hits(&ConcurrentReInvite500Or491, &msgs);
        assert_eq!(f.len(), 1, "{case}: {f:?}");
        let Decision::Violated(Evidence::ConcurrentReInvite { answered_status, .. }) =
            &f[0].decision
        else {
            panic!("{case}: {:?}", f[0].decision)
        };
        assert_eq!(*answered_status, status, "{case}");
    }
}
