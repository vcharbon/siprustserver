//! The dialog table's verdict on a request that reaches a dialog already
//! ended, as the wire record's [`SentBeforeEnd`] states whether it crossed
//! that end: the table hands the fact the final or the BYE that ended the
//! dialog, and which side sent it.

use super::*;

/// The callee `B1`'s 487 to the caller `A1`'s INVITE numbered 1.
const PEERS_487: DialogEnd =
    DialogEnd { ending: Ending::Final { invite_cseq: 1, status: 487 }, by_peer: true };
/// The UAS `B1`'s own 487 to the caller `A1`'s INVITE numbered 1.
const OWN_487: DialogEnd = DialogEnd { by_peer: false, ..PEERS_487 };
/// The caller `A1`'s BYE numbered 3.
const PEERS_BYE_3: DialogEnd = DialogEnd { ending: Ending::Bye { cseq: 3 }, by_peer: true };

/// A wire record that shows every request crossing `end` when `crossed`,
/// and none else.
fn crossing(end: DialogEnd, crossed: bool) -> SentBeforeEnd {
    SentBeforeEnd::new(move |_, e| crossed && e == end)
}

/// A caller `A1` whose INVITE (CSeq 1) the callee `B1` rang under its tag,
/// then ended 487, reading `wire`.
fn caller_ended_by_the_callees_487(wire: SentBeforeEnd) -> DialogTable {
    let mut t = DialogTable::new(BackwardsCseq::Verdict);
    t.set_sent_before_end(wire);
    t.note_sent_request(&request("INVITE", 1, "A1", None));
    assert_eq!(t.note_received_response(&response(180, "INVITE", 1, "A1", "B1"), 0), None);
    assert_eq!(t.note_received_response(&response(487, "INVITE", 1, "A1", "B1"), 0), None);
    t
}

/// RFC 3261 §12.1.2, §17.1.2.2: the callee's early UPDATE whose lost first
/// copy left before the callee sent its 487 goes again past that 487. The
/// caller refuses it 481 and it is no finding; a repeat is not judged again.
#[test]
fn a_request_crossing_the_peers_own_final_is_refused_and_no_finding() {
    let mut t = caller_ended_by_the_callees_487(crossing(PEERS_487, true));
    let v = t.judge_received(&request("UPDATE", 1, "B1", Some("A1")), 0).expect("refused");
    assert_eq!(v.kind, VerdictKind::Crossed { by: Some(487), broke: None });
    assert_eq!(v.rule(), DIALOG_ENDED);
    assert_eq!(v.kind.refusal(), REFUSED_481);
    assert!(!v.kind.is_finding());
    assert_eq!(
        v.to_string(),
        "UPDATE From-tag B1 To-tag A1 names a dialog a 487 final ended, crossing that final"
    );
    assert_eq!(t.judge_received(&request("UPDATE", 1, "B1", Some("A1")), 0), None, "a repeat");
}

/// The same UPDATE that the wire record does not show crossing the callee's
/// 487 is a finding.
#[test]
fn a_request_after_the_peers_own_final_that_crossed_nothing_is_a_finding() {
    let mut t = caller_ended_by_the_callees_487(crossing(PEERS_487, false));
    let v = t.judge_received(&request("UPDATE", 1, "B1", Some("A1")), 0).expect("a verdict");
    assert_eq!(v.kind, VerdictKind::Ended { by: Some(487) });
    assert!(v.kind.is_finding());
}

/// RFC 3261 §17.2.1: once the caller's ACK showed she saw the UAS's own 487,
/// her early UPDATE is a finding, unless the wire record shows it a
/// retransmission whose lost first copy left before she read that 487.
#[test]
fn a_request_crossing_the_uas_own_seen_final_is_refused_and_no_finding() {
    for (crossed, kind) in [
        (true, VerdictKind::Crossed { by: Some(487), broke: None }),
        (false, VerdictKind::Ended { by: Some(487) }),
    ] {
        let mut t = ringing_uas(BackwardsCseq::Verdict);
        t.set_sent_before_end(crossing(OWN_487, crossed));
        t.note_sent_response(&response(487, "INVITE", 1, "A1", "B1"), 0);
        assert_eq!(t.judge_received(&request("ACK", 1, "A1", Some("B1")), 0), None);
        let v = t.judge_received(&request("UPDATE", 2, "A1", Some("B1")), 0).expect("a verdict");
        assert_eq!(v.kind, kind, "crossed: {crossed}");
        assert_eq!(v.kind.refusal(), REFUSED_481);
    }
}

/// RFC 3261 §15.1.2, §17.1.2.2: the caller's INFO numbered 2 lost on its
/// way, her BYE numbered 3 skips over it and ends the dialog; the INFO's
/// retransmission crosses that BYE. Judged as on the dialog before it, it
/// goes backwards, which the loss explains: refused 481, no finding. With
/// the wire record showing no crossing, it is a finding.
#[test]
fn a_request_crossing_the_peers_own_bye_is_refused_and_no_finding() {
    for (crossed, finding) in [(true, false), (false, true)] {
        let mut t = ringing_uas(BackwardsCseq::Verdict);
        t.set_sent_before_end(crossing(PEERS_BYE_3, crossed));
        t.set_lost_on_the_way(LostOnTheWay::new(|_, _, _| vec![(2, "INFO".to_string())]));
        t.note_sent_response(&response(200, "INVITE", 1, "A1", "B1"), 0);
        let bye = t.judge_received(&request("BYE", 3, "A1", Some("B1")), 0).expect("a skip");
        assert!(!bye.kind.is_finding(), "{bye}");
        t.note_sent_response(&response(200, "BYE", 3, "A1", "B1"), 0);
        let v = t.judge_received(&request("INFO", 2, "A1", Some("B1")), 0).expect("a verdict");
        assert_eq!(v.kind.is_finding(), finding, "{v}");
        assert_eq!(v.kind.refusal(), REFUSED_481);
        if crossed {
            let back = VerdictKind::CseqBackwards { cseq: 2, last: 3 };
            let explained = VerdictKind::LossExplained { broke: Box::new(back) };
            assert_eq!(v.kind, VerdictKind::Crossed { by: None, broke: Some(Box::new(explained)) });
        }
    }
}
