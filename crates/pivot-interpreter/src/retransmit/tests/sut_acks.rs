//! The ACKs the system owes the copies of a 2xx the run sent (RFC 3261
//! §13.2.2.4).

use super::*;

/// The SUT's ACK ladder to our 2xx, in µs: our 2xx at 0 declaring one repeat,
/// sent at `copy_at`; the SUT's ACK at `ack_at`, re-sent at `reacks`.
fn sut_acks(copy_at: u64, ack_at: u64, reacks: &[u64], declared: Option<u32>) -> Vec<Failure> {
    let ok = b"SIP/2.0 200 OK\r\nContact: <sip:fixture@192.0.2.98>\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c9\r\nCSeq: 4 INVITE\r\n\r\n";
    let ack = b"ACK sip:b@h SIP/2.0\r\nMax-Forwards: 70\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c9\r\nCSeq: 4 ACK\r\n\r\n";
    let mut events: Vec<(u64, bool)> = vec![(copy_at, true), (ack_at, false)];
    events.extend(reacks.iter().map(|&at| (at, false)));
    events.sort();
    let mut repeats = Repeats::new();
    repeats.claim("s1", "B", LadderSide::Send, Some(1), &[492], ok, 0);
    let mut acked = false;
    for (at, ours) in events {
        if ours {
            repeats.note("B", ok, at);
        } else if !acked {
            repeats.claim("s2", "B", LadderSide::Expect, declared, &[], ack, at);
            acked = true;
        } else {
            repeats.note("B", ack, at);
        }
    }
    repeats.mismatches(RUN_END)
}

/// The SUT owes one ACK per copy of our 2xx it receives. A copy that reached
/// it before its ACK left is answered by that one ACK, so the capture's second
/// ACK is not owed.
#[test]
fn one_ack_after_both_copies_of_our_2xx_owes_no_repeat() {
    assert!(sut_acks(492_000, 530_000, &[], Some(1)).is_empty(), "the copy came first");
    assert!(sut_acks(492_000, 400_000, &[495_000], Some(1)).is_empty(), "the copy came after");
}

/// A copy we sent after the SUT's ACK reached us is owed an ACK of its own,
/// whatever the document declared: ignoring it stays a mismatch.
#[test]
fn a_copy_of_our_2xx_past_the_sut_s_ack_is_owed_an_ack() {
    for declared in [Some(1), None] {
        assert!(
            matches!(
                sut_acks(492_000, 400_000, &[], declared).as_slice(),
                [Failure::RetransmitCountMismatch { step, expected: 1, observed: 0, .. }]
                    if step == "s2"
            ),
            "declared {declared:?}"
        );
    }
}

/// A copy sent inside two hops before the SUT's ACK reached us crossed that
/// ACK: the SUT may have taken it before or after its ACK left, so both one
/// ACK and two are the RFC's. A second re-ACK is not.
#[test]
fn a_copy_that_crossed_the_sut_s_ack_may_draw_one_more_ack_or_none() {
    assert!(sut_acks(492_000, 500_000, &[], Some(1)).is_empty(), "none drawn");
    assert!(sut_acks(492_000, 500_000, &[503_000], Some(1)).is_empty(), "one drawn");
    assert!(
        !sut_acks(492_000, 500_000, &[503_000, 504_000], Some(1)).is_empty(),
        "two re-ACKs for one crossing copy"
    );
}

/// Our 2xx sent once and ACKed, then `reacks` repeats of that ACK the SUT
/// sent with no copy of our 2xx to draw them, the document declaring `declared`.
fn reacks_with_no_copy(reacks: usize, declared: Option<u32>) -> Vec<Failure> {
    let ok = b"SIP/2.0 200 OK\r\nContact: <sip:fixture@192.0.2.98>\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c9\r\nCSeq: 4 INVITE\r\n\r\n";
    let ack = b"ACK sip:b@h SIP/2.0\r\nMax-Forwards: 70\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c9\r\nCSeq: 4 ACK\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s1", "B", LadderSide::Send, Some(0), &[], ok, 0);
    repeats.claim("s2", "B", LadderSide::Expect, declared, &[], ack, 400_000);
    for k in 0..reacks {
        repeats.note("B", ack, 900_000 + 100_000 * k as u64);
    }
    repeats.mismatches(RUN_END)
}

/// A re-ACK the capture itself declares, which a relaying system repeats
/// byte-identically from its other leg (RFC 3261 §13.2.2.4 there), is the
/// capture's own reproduced, though no copy of our 2xx drew it.
#[test]
fn a_declared_re_ack_with_no_copy_of_our_2xx_is_the_capture_s_own() {
    assert!(reacks_with_no_copy(1, Some(1)).is_empty(), "{:#?}", reacks_with_no_copy(1, Some(1)));
}

/// Past the capture's count, or with none declared, a re-ACK no copy of our
/// 2xx drew stays a mismatch.
#[test]
fn a_re_ack_past_the_declared_count_with_no_copy_stays_a_mismatch() {
    for (reacks, declared) in [(2, Some(1)), (1, None), (1, Some(0))] {
        assert!(
            matches!(
                reacks_with_no_copy(reacks, declared).as_slice(),
                [Failure::RetransmitCountMismatch { step, expected: 0, .. }] if step == "s2"
            ),
            "{reacks} re-ACKs, declared {declared:?}"
        );
    }
}
