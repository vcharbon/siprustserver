//! The dialog table's CSeq verdicts on a run that declares a loss model: a
//! CSeq out of the remote sequence that a loss on the way explains.

use super::*;

/// A confirmed UAS whose table reads that the network lost on their way to
/// it a copy of each [`request`] `losses` names by `(CSeq, method)` on the
/// dialog `A1`/`B1`.
fn uas_losing(losses: &'static [(u32, &'static str)]) -> DialogTable {
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    t.set_lost_on_the_way(LostOnTheWay::new(move |call_id, from, to| match (call_id, from, to) {
        (CALL, "A1", "B1") => losses.iter().map(|(c, m)| lost(*c, m)).collect(),
        _ => Vec::new(),
    }));
    t.note_sent_response(&response(200, "INVITE", 1, "A1", "B1"), 0);
    t
}

/// RFC 3261 §12.2.2, §17.1.2.2: the PRACK numbered 2 lost on its way, the
/// UPDATE numbered 3 skips over it and the PRACK's retransmission then goes
/// backwards; the loss explains both. The retransmission is still refused
/// 500, and neither is a finding.
#[test]
fn a_cseq_out_of_sequence_a_loss_explains_is_no_finding() {
    let mut t = uas_losing(&[(2, "PRACK")]);
    let v = t.judge_received(&request("UPDATE", 3, "A1", Some("B1")), 0).expect("the skip");
    let skip = VerdictKind::CseqSkip { cseq: 3, last: 1 };
    assert_eq!(v.kind, VerdictKind::LossExplained { broke: Box::new(skip) });
    assert_eq!(v.rule(), DIALOG_CSEQ_SKIP);
    assert!(!v.kind.is_finding());
    assert_eq!(v.kind.refusal(), None, "taken");
    let v = t.judge_received(&request("PRACK", 2, "A1", Some("B1")), 0).expect("backwards");
    let back = VerdictKind::CseqBackwards { cseq: 2, last: 3 };
    assert_eq!(v.kind, VerdictKind::LossExplained { broke: Box::new(back) });
    assert_eq!(v.rule(), DIALOG_CSEQ_BACKWARDS);
    assert!(!v.kind.is_finding());
    assert_eq!(v.kind.refusal(), Some((500, "Server Internal Error")));
    assert_eq!(
        v.to_string(),
        "PRACK CSeq 2 goes backwards on From-tag A1 (last remote CSeq 3), as the network's loss \
         of a first copy explains"
    );
}

/// A loss explains only what it caused: a skip over requests all lost is
/// explained, but a request going backwards whose own copy was not lost (a
/// lost request of another method had its number), or a skip over a lost
/// ACK (it carries its INVITE's number), stays a finding; and with no loss
/// read at all, none is explained.
#[test]
fn a_cseq_out_of_sequence_no_loss_explains_is_a_finding() {
    let mut t = uas_losing(&[(2, "PRACK"), (3, "UPDATE"), (5, "ACK")]);
    assert!(t
        .judge_received(&request("INFO", 4, "A1", Some("B1")), 0)
        .is_some_and(|v| !v.kind.is_finding()));
    let v = t.judge_received(&request("INFO", 3, "A1", Some("B1")), 0).expect("backwards");
    assert_eq!(v.kind, VerdictKind::CseqBackwards { cseq: 3, last: 4 });
    let v = t.judge_received(&request("INFO", 6, "A1", Some("B1")), 0).expect("a skip");
    assert_eq!(v.kind, VerdictKind::CseqSkip { cseq: 6, last: 4 });
    let mut t = ringing_uas(BackwardsCseq::Verdict);
    t.note_sent_response(&response(200, "INVITE", 1, "A1", "B1"), 0);
    let v = t.judge_received(&request("UPDATE", 3, "A1", Some("B1")), 0).expect("the skip");
    assert_eq!(v.kind, VerdictKind::CseqSkip { cseq: 3, last: 1 });
}

/// A loss explains only its own transaction going backwards: the INFO
/// numbered 2 lost once, its retransmission and the INFO numbered 3 taken in
/// order, a new transaction (another top-Via branch) reusing the number 2 is
/// a CSeq gone backwards and a finding (RFC 3261 §12.2.2, §17.2.3).
#[test]
fn a_new_transaction_reusing_a_lost_requests_cseq_is_a_finding() {
    let mut t = uas_losing(&[(2, "INFO")]);
    assert_eq!(t.judge_received(&request("INFO", 2, "A1", Some("B1")), 0), None);
    assert_eq!(t.judge_received(&request("INFO", 3, "A1", Some("B1")), 0), None);
    let reused = request_through("INFO", 2, "A1", Some("B1"), 1);
    let v = t.judge_received(&reused, 0).expect("backwards");
    assert_eq!(v.kind, VerdictKind::CseqBackwards { cseq: 2, last: 3 });
}

/// A skip is explained only when every number in its gap is a request lost on
/// the way: the INFO numbered 2 lost, an INFO numbered 5 over the never sent 3
/// and 4 is a CSeq skip and a finding (RFC 3261 §12.2.2).
#[test]
fn a_skip_over_numbers_no_loss_accounts_for_is_a_finding() {
    let mut t = uas_losing(&[(2, "INFO")]);
    let v = t.judge_received(&request("INFO", 5, "A1", Some("B1")), 0).expect("the skip");
    assert_eq!(v.kind, VerdictKind::CseqSkip { cseq: 5, last: 1 });
}
