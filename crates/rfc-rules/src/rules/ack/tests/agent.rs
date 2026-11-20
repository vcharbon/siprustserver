//! A party holding several sockets is one user agent: its obligations, its
//! untagged ACKs and a B2BUA's legs are read per agent on each call.

use super::*;

/// A B2BUA that takes its caller's INVITE at one socket, sends from a second
/// and takes its callee's answer at a third is one user agent across both
/// calls: its ACK still waits on its caller's.
#[test]
fn a_b2bua_listening_at_a_socket_per_call_holds_its_ack_behind_its_caller() {
    const B2BUA_A: &str = "10.0.0.5:5060";
    const B2BUA_SENDS: &str = "10.0.0.5:40000";
    const B2BUA_B: &str = "10.0.0.5:5080";
    let on = |mut m: Msg, call: &str| {
        m.call_id = call.to_string();
        m
    };
    let ok = |at_us, src, dst, to_tag| {
        msg(at_us, src, dst, Kind::Response { status: 200 }, 1, Some("fa"), Some(to_tag))
    };
    let msgs = vec![
        on(via(req(1_000_000, UAC, B2BUA_A, "INVITE", 1, None), "z9hG4bK-a", UAC), "a"),
        on(via(req(1_010_000, B2BUA_SENDS, UAS, "INVITE", 1, None), "z9hG4bK-b", B2BUA_B), "b"),
        on(via(ok(1_200_000, UAS, B2BUA_B, "tb"), "z9hG4bK-b", B2BUA_B), "b"),
        on(via(ok(1_210_000, B2BUA_SENDS, UAC, "ta"), "z9hG4bK-a", UAC), "a"),
    ];
    let closed = obs(&msgs, true);
    let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &closed });
    let of = |who: &str| f.iter().find(|f| f.emitter == who).expect("an occasion");
    assert!(of(UAC).violated(), "the caller withheld its ACK: {f:?}");
    assert!(
        matches!(of(B2BUA_SENDS).decision, Decision::Undecidable(
            "the endpoint answered another call whose caller never ACKed (a B2BUA's ACK waits on its caller's)"
        )),
        "the B2BUA's ACK waits on the caller's: {f:?}"
    );
}

/// An ACK naming no dialog that a two-socket UAC sent from its sent-by may be
/// the one its INVITE's 2xx owed.
#[test]
fn a_two_socket_uac_s_untagged_ack_from_its_sent_by_may_be_the_one() {
    const UAC_SENDS: &str = "10.0.0.1:60000";
    let msgs = vec![
        via(req(1_000_000, UAC_SENDS, UAS, "INVITE", 1, None), "z9hG4bK-i", UAC),
        via(ok200(1_200_000, 1), "z9hG4bK-i", UAC),
        req(1_210_000, UAC, UAS, "ACK", 1, None),
    ];
    let closed = obs(&msgs, true);
    let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(
            f[0].decision,
            Decision::Undecidable("an ACK the emitter sent names no dialog — it may be the one")
        ),
        "{:?}",
        f[0].decision
    );
}

/// The UAS half reads the untagged ACK the same way: from the UAC's sent-by,
/// it may be the one the 2xx its other socket's INVITE drew owed.
#[test]
fn a_two_socket_uac_s_untagged_ack_leaves_the_uas_obligation_undecided() {
    const UAC_SENDS: &str = "10.0.0.1:60000";
    let msgs = vec![
        via(req(1_000_000, UAC_SENDS, UAS, "INVITE", 1, None), "z9hG4bK-i", UAC),
        via(ok200(1_200_000, 1), "z9hG4bK-i", UAC),
        req(1_210_000, UAC, UAS, "ACK", 1, None),
    ];
    let closed = obs(&msgs, true);
    let f = Unacked2xxNotCleared.eval(&WireView { msgs: &msgs, obs: &closed });
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(
            f[0].decision,
            Decision::Undecidable("an ACK the taker sent names no dialog — it may be the one")
        ),
        "{:?}",
        f[0].decision
    );
}

/// Two nodes tied by a failover on a third call are not one B2BUA on the
/// calls at hand: B2's missing ACK still waits on the caller of the call B2
/// took, not on the later call B1 took.
#[test]
fn a_failover_tie_on_another_call_does_not_pick_the_relayed_call() {
    const B1: &str = "10.0.0.5:5060";
    const B2: &str = "10.0.0.6:5060";
    const CAROL: &str = "10.0.0.7:5060";
    let on = |mut m: Msg, call: &str| {
        m.call_id = call.to_string();
        m
    };
    let ok = |at_us, src, dst, to_tag| {
        msg(at_us, src, dst, Kind::Response { status: 200 }, 1, Some("fa"), Some(to_tag))
    };
    let msgs = vec![
        on(via(req(100_000, CAROL, B1, "INVITE", 1, None), "z9hG4bK-d", CAROL), "d"),
        on(via(ok(200_000, B2, CAROL, "td"), "z9hG4bK-d", CAROL), "d"),
        on(req(210_000, CAROL, B1, "ACK", 1, Some("td")), "d"),
        on(via(req(1_000_000, UAC, B2, "INVITE", 1, None), "z9hG4bK-a", UAC), "a"),
        on(via(req(2_000_000, CAROL, B1, "INVITE", 1, None), "z9hG4bK-c", CAROL), "c"),
        on(via(req(3_000_000, B2, UAS, "INVITE", 1, None), "z9hG4bK-b", B2), "b"),
        on(via(ok(4_000_000, UAS, B2, "tb"), "z9hG4bK-b", B2), "b"),
        on(via(ok(5_000_000, B2, UAC, "ta"), "z9hG4bK-a", UAC), "a"),
        on(via(ok(6_000_000, B1, CAROL, "tc"), "z9hG4bK-c", CAROL), "c"),
        on(req(6_010_000, CAROL, B1, "ACK", 1, Some("tc")), "c"),
    ];
    let closed = obs(&msgs, true);
    let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &closed });
    let on_b = f.iter().find(|f| f.anchor == 6).expect("B2's obligation on its dialled call");
    assert_eq!(on_b.emitter, B2);
    assert!(
        matches!(on_b.decision, Decision::Undecidable(
            "the endpoint answered another call whose caller never ACKed (a B2BUA's ACK waits on its caller's)"
        )),
        "B2's ACK waits on the caller of the call it took: {f:?}"
    );
}
