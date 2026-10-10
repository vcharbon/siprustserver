//! The UAS's `RSeq` space.

use super::*;

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
