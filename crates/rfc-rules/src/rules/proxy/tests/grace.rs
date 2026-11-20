//! Proxy100WithinGrace (RFC 3261 §16.7).

use super::*;

/// A final bob sent on the INVITE's own transaction.
fn answered(at_us: u64, status: u16) -> Msg {
    Msg {
        refused: false,
        at_us,
        src: BOB.to_string(),
        dst: ALICE.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status },
        call_id: "c1".to_string(),
        cseq: 1,
        cseq_method: "INVITE".to_string(),
        from_tag: Some("at".to_string()),
        to_tag: Some("bt".to_string()),
        via_branch: Some("z9hG4bK-i".to_string()),
        head: None,
        body: None,
    }
}

fn grace(msgs: &[Msg]) -> Vec<Finding> {
    Proxy100WithinGrace.eval(&WireView { msgs, obs: &obs(msgs) })
}

/// The 100 went out: the obligation is met, and the endpoint charged is the
/// one that TOOK the INVITE.
#[test]
fn a_trying_on_the_transaction_is_compliant() {
    let f = grace(&[invite(1_000, 1), trying(2_000, 1)]);
    assert_eq!(f.len(), 1, "one occasion, the INVITE taken: {f:?}");
    assert_eq!(f[0].rule, RuleId::Proxy100WithinGrace);
    assert_eq!(f[0].emitter, BOB, "the hop that took the INVITE is charged");
    assert_eq!(f[0].taker, ALICE);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A hop that answers from another socket than the one it took the INVITE at
/// said it was trying on that INVITE's transaction.
#[test]
fn a_trying_from_a_second_socket_of_the_hop_is_compliant() {
    let mut second = trying(2_000, 1);
    second.src = "127.0.0.1:41000".to_string();
    let f = grace(&[invite(1_000, 1), second]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, BOB, "the socket that took the INVITE is charged");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// §16.7 wants the 100 only where the answer is slow: a hop that rejected
/// inside the grace owes nobody a Trying.
#[test]
fn a_final_inside_the_grace_discharges_it() {
    let f = grace(&[invite(1_000, 1), answered(1_000 + PROXY_100_GRACE_US, 486)]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// An INVITE the strict reading refuses is answered 400 or dropped: the hop
/// owes no Trying for it.
#[test]
fn a_refused_invite_owes_no_trying() {
    let mut refused = invite(1_000, 1);
    refused.refused = true;
    assert!(grace(&[refused]).is_empty());
}

/// Past the grace with no 100, the finding names the Δ actually observed.
#[test]
fn a_final_past_the_grace_is_violated_with_its_delta() {
    let late = 1_000 + PROXY_100_GRACE_US + 1;
    let f = grace(&[invite(1_000, 1), answered(late, 486)]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::TryingNotSentInGrace {
        first_final_after_us,
        grace_us,
        branch,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*first_final_after_us, Some(PROXY_100_GRACE_US + 1));
    assert_eq!(*grace_us, PROXY_100_GRACE_US);
    assert_eq!(branch.as_str(), "z9hG4bK-i");
}

/// Nothing went out at all — and a CLOSED observation says so.
#[test]
fn silence_on_a_closed_observation_is_violated() {
    let f = grace(&[invite(1_000, 1)]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::TryingNotSentInGrace { first_final_after_us, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*first_final_after_us, None, "no response was sent");
}

/// An OPEN observation that stopped inside the grace shows truncation, not
/// a hop holding its tongue.
#[test]
fn silence_inside_the_grace_of_an_open_observation_is_undecidable() {
    let msgs = [invite(1_000, 1)];
    let open = Observation { last_us: 1_100, closed: false, ..Observation::default() };
    let f = Proxy100WithinGrace.eval(&WireView { msgs: &msgs, obs: &open });
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(!f[0].decided(), "{:?}", f[0].decision);
}

/// A retransmitted INVITE is the same transaction: the 100 the hop already
/// sent still discharges it, and it stays ONE occasion.
#[test]
fn a_retransmitted_invite_reuses_the_occasion() {
    let f = grace(&[invite(1_000, 1), trying(2_000, 1), invite(3_000, 1)]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}
