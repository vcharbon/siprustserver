//! `AckCseqMatchesInvite` (RFC 3261 §13.2.2.4).

use super::*;

/// Each ACK reuses its own INVITE's CSeq — the obligation met, twice.
#[test]
fn an_ack_reusing_its_invites_cseq_is_compliant() {
    let msgs = [
        req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
        in_dialog(2_000, "ACK", 1, "z9hG4bK-a1"),
        in_dialog(3_000, "INVITE", 4, "z9hG4bK-i2"),
        in_dialog(4_000, "ACK", 4, "z9hG4bK-a2"),
    ];
    let all = eval(&AckCseqMatchesInvite, &msgs);
    assert_eq!(all.len(), 2, "one occasion per ACK: {all:?}");
    assert!(all.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{all:?}");
}

/// The bug: an early PRACK/UPDATE advanced the dialog CSeq, so the 2xx ACK
/// lands at a number no INVITE used and a real UAS cannot match it to the
/// INVITE server transaction.
#[test]
fn an_ack_on_a_number_no_invite_used_is_violated() {
    let msgs =
        [req(1_000, "INVITE", 1, "z9hG4bK-i", None), in_dialog(2_000, "ACK", 3, "z9hG4bK-a")];
    let f = hits(&AckCseqMatchesInvite, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, ALICE, "the endpoint that sent the ACK is charged");
    assert_eq!(f[0].taker, BOB);
    let Decision::Violated(Evidence::AckCseqUnmatched { ack_cseq, invite_cseqs, from_tag, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*ack_cseq, 3);
    assert_eq!(invite_cseqs.as_slice(), [1], "the INVITE it owed");
    assert_eq!(from_tag.as_str(), "fa");
}

/// An ACK is judged against the INVITEs the stream had carried BY THEN, so
/// a stream with none yet settles nothing.
#[test]
fn an_ack_before_any_invite_is_undecidable() {
    let f = eval(&AckCseqMatchesInvite, &[in_dialog(1_000, "ACK", 9, "z9hG4bK-a")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(f[0].decision, Decision::Undecidable("no INVITE on this stream at this vantage")),
        "{:?}",
        f[0].decision
    );
}

/// The ACK rule reads every copy the taker took: §12.2.1.1's fold is not
/// its fold, so a retransmitted bad ACK is charged as often as it lands.
#[test]
fn a_retransmitted_ack_is_judged_every_time() {
    let msgs = [
        req(1_000, "INVITE", 1, "z9hG4bK-i", None),
        in_dialog(2_000, "ACK", 3, "z9hG4bK-a"),
        again(in_dialog(3_000, "ACK", 3, "z9hG4bK-a")),
    ];
    assert_eq!(hits(&AckCseqMatchesInvite, &msgs).len(), 2, "one per arrival");
}
