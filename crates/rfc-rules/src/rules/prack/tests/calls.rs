//! A view carries many calls, and each party's PRACK ledgers are per call: a
//! fact on one call settles nothing on another, even under the same socket,
//! CSeq, To tag and `RSeq`.

use super::*;

/// `m` moved onto `call`.
fn on_call(mut m: Msg, call: &str) -> Msg {
    m.call_id = call.to_string();
    m
}

/// An unreadable RAck on one call says nothing about the PRACKs the same
/// socket owes on another.
#[test]
fn an_unreadable_rack_on_another_call_leaves_the_obligation_decided() {
    let mut garbled = prack(500_000, UAC, UAS, 2, 9, 9, "tx");
    garbled.call_id = "c0".to_string();
    let head = String::from_utf8(garbled.head.take().unwrap_or_default()).unwrap_or_default();
    garbled.head = Some(head.replace("RAck: 9 9 INVITE", "RAck: garbage").into_bytes());
    let msgs = vec![
        garbled,
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
        ok200(1_300_000, 1, "tb"),
    ];
    let f = decide(&UnackedReliableProvisional, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
}

/// §7.2: a PRACK naming an INVITE its sender opened only on another call
/// names nothing it opened on its own.
#[test]
fn a_rack_naming_an_invite_of_another_call_is_undecided() {
    let msgs = vec![
        on_call(invite(1_000_000, UAC, UAS, 1), "c0"),
        on_call(reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"), "c0"),
        prack(1_200_000, UAC, UAS, 2, 5, 1, "tb"),
    ];
    let f = decide(&RackWithoutKnownInvite, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(
            f[0].decision,
            Decision::Undecidable(
                "the emitter opened no INVITE on this call — nothing to correlate"
            )
        ),
        "{:?}",
        f[0].decision
    );
}

/// §4: a provisional the UAC sends on another call, with the same CSeq, To
/// tag and `RSeq`, is not this one passed on: the PRACK stays owed.
#[test]
fn a_like_provisional_sent_on_another_call_forwards_nothing() {
    let msgs = vec![
        invite(1_000_000, UAC, UAS, 1),
        reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
        on_call(reliable(1_150_000, UAC, PROXY, 180, 1, 5, "tb"), "c0"),
        ok200(1_300_000, 1, "tb"),
    ];
    let f = decide(&UnackedReliableProvisional, &msgs);
    let owed = f.iter().find(|f| f.anchor == 1).expect("the UAC's obligation on its call");
    assert!(owed.violated(), "{f:?}");
}

/// §4: provisionals taken on another call under the same To tag are no part
/// of this call's `RSeq` order.
#[test]
fn rseq_order_is_read_per_call() {
    let msgs = vec![
        on_call(invite(1_000_000, UAC, UAS, 1), "c0"),
        on_call(reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"), "c0"),
        invite(1_200_000, UAC, UAS, 1),
        reliable(1_300_000, UAS, UAC, 180, 1, 7, "tb"),
        prack(1_400_000, UAC, UAS, 2, 7, 1, "tb"),
    ];
    let f = decide(&NoPrackOfOutOfOrderRseq, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// §3: each call's early dialog numbers its own `RSeq`s, whatever To tag
/// the other call's carries: two first provisionals are no step.
#[test]
fn rseq_contiguity_is_read_per_call() {
    let msgs = vec![
        on_call(reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"), "c0"),
        reliable(1_200_000, UAS, UAC, 180, 1, 9, "tb"),
    ];
    assert!(
        decide(&NonContiguousRseq, &msgs).is_empty(),
        "{:?}",
        decide(&NonContiguousRseq, &msgs)
    );
}
