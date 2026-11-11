//! A party holding several sockets is one user agent on its call: the
//! INVITE it opened from one socket, the provisionals it took at another and
//! the PRACKs it sent or took at either are one ledger. Each fixture picks its
//! sockets so the agent's name is not the socket a per-socket reading would
//! have filed the fact under.

use super::*;

/// alice's sending socket; her agent is named by `UAC`, which sorts first.
const UAC_SENDS: &str = "10.0.0.1:60000";
/// A proxy's listening socket and its sending one.
const PROXY_SENDS: &str = "10.0.0.4:60000";

/// alice's INVITE from her sending socket, answered at `UAC`, her sent-by.
fn two_socket_invite() -> Msg {
    via(invite(1_000_000, UAC_SENDS, UAS, 1), INV_BRANCH, UAC)
}

/// A reliable provisional bob sends alice's sent-by on the INVITE's branch.
fn reliable_to_uac(at_us: u64, rseq: u64) -> Msg {
    via(reliable(at_us, UAS, UAC, 180, 1, rseq, "tb"), INV_BRANCH, UAC)
}

/// alice's PRACK of `rseq` from her sending socket.
fn prack_from_second(at_us: u64, rseq: u64, branch: &str) -> Msg {
    via(prack(at_us, UAC_SENDS, UAS, 2, rseq, 1, "tb"), branch, UAC)
}

/// The response `status` to the PRACK on `branch`, from `src` to `dst`.
fn prack_status(at_us: u64, src: &str, dst: &str, status: u16, branch: &str) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\n\
             CSeq: 2 PRACK\r\nContent-Length: 0\r\n\r\n"
    );
    let mut m = msg(at_us, src, dst, Kind::Response { status }, 2, Some("tb"), head);
    m.cseq_method = "PRACK".to_string();
    via(m, branch, dst)
}

/// §7.2: the PRACK alice sends from the socket her INVITE left from names
/// the INVITE her agent opened.
#[test]
fn a_two_socket_uac_s_rack_names_its_own_invite() {
    let msgs = vec![
        two_socket_invite(),
        reliable_to_uac(1_100_000, 5),
        prack_from_second(1_150_000, 5, "z9hG4bK-p"),
    ];
    let f = decide(&RackWithoutKnownInvite, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// §4: the provisionals alice's sent-by took are the order her PRACK from
/// the other socket is judged against.
#[test]
fn a_two_socket_uac_s_prack_out_of_rseq_order_is_violated() {
    let msgs = vec![
        two_socket_invite(),
        reliable_to_uac(1_100_000, 5),
        reliable_to_uac(1_110_000, 7),
        prack_from_second(1_150_000, 7, "z9hG4bK-p"),
    ];
    let f = decide(&NoPrackOfOutOfOrderRseq, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAC_SENDS);
}

/// §4: a 100 alice's sent-by took is never PRACKed, from either socket.
#[test]
fn a_two_socket_uac_pracking_a_100_is_violated() {
    let trying_head = "SIP/2.0 100 Trying\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>\r\n\
                       CSeq: 1 INVITE\r\nRequire: 100rel\r\nRSeq: 1\r\nContent-Length: 0\r\n\r\n";
    let trying =
        msg(1_100_000, UAS, UAC, Kind::Response { status: 100 }, 1, None, trying_head.to_string());
    let msgs = vec![
        two_socket_invite(),
        via(trying, INV_BRANCH, UAC),
        prack_from_second(1_150_000, 1, "z9hG4bK-p"),
    ];
    let f = decide(&NoPrackOf100Trying, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAC_SENDS, "the socket the INVITE left from is charged");
}

/// A proxy that takes alice's PRACK at its listening socket and forwards it
/// from its sending one; bob's answer to the forwarded PRACK ties the two.
fn proxied_prack() -> Vec<Msg> {
    vec![
        two_socket_invite(),
        reliable_to_uac(1_100_000, 5),
        via(prack(1_150_000, UAC, PROXY, 2, 5, 1, "tb"), "z9hG4bK-p1", UAC),
        via(prack(1_160_000, PROXY_SENDS, UAS, 2, 5, 1, "tb"), "z9hG4bK-p2", PROXY),
        prack_status(1_170_000, UAS, PROXY, 200, "z9hG4bK-p2"),
    ]
}

/// §3: a PRACK the proxy forwards from its other socket was forwarded.
#[test]
fn a_prack_forwarded_from_a_proxy_s_other_socket_was_proxied() {
    let msgs = proxied_prack();
    let f = decide(&UnmatchedPrackProxied, &msgs);
    let at_proxy = f.iter().find(|f| f.emitter == PROXY).expect("the proxy's occasion");
    assert!(matches!(at_proxy.decision, Decision::Compliant), "{f:?}");
}

/// The PRACK the proxy forwards is marked relayed: the same RAck reached its
/// agent first.
#[test]
fn a_prack_forwarded_from_a_proxy_s_other_socket_is_relayed() {
    let msgs = proxied_prack();
    let f = decide(&RackWithoutKnownInvite, &msgs);
    let forwarded = f.iter().find(|f| f.anchor == 3).expect("the forwarded PRACK's occasion");
    assert!(forwarded.relayed, "{f:?}");
}

/// §3: a PRACK reaching the socket of an agent whose other socket took the
/// provisional is answerable there.
#[test]
fn a_prack_at_the_other_socket_of_the_provisional_s_taker_is_matched() {
    let msgs = vec![
        via(invite(1_000_000, PROXY_SENDS, UAS, 1), INV_BRANCH, PROXY),
        via(reliable(1_100_000, UAS, PROXY, 180, 1, 5, "tb"), INV_BRANCH, PROXY),
        via(prack(1_150_000, UAC, PROXY_SENDS, 2, 5, 1, "tb"), "z9hG4bK-p", UAC),
    ];
    let f = decide(&UnmatchedPrackProxied, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// §3 at a UAS answering from a second socket: the PRACK it takes there
/// matches the provisional it sent on the INVITE its first socket took, and
/// a second PRACK of that RSeq, taken at the first socket after the 2xx to
/// the other, matches nothing left unacknowledged.
#[test]
fn a_two_socket_uas_answers_pracks_across_its_sockets() {
    const UAS_SENDS: &str = "10.0.0.2:40000";
    let mut provisional = reliable_to_uac(1_100_000, 5);
    provisional.src = UAS_SENDS.to_string();
    let mut second = prack_from_second(1_300_000, 5, "z9hG4bK-p2");
    second.cseq = 3;
    let msgs = vec![
        two_socket_invite(),
        provisional,
        via(prack(1_150_000, UAC_SENDS, UAS_SENDS, 2, 5, 1, "tb"), "z9hG4bK-p1", UAC),
        prack_status(1_160_000, UAS_SENDS, UAC, 200, "z9hG4bK-p1"),
        second,
        prack_status(1_310_000, UAS, UAC, 200, "z9hG4bK-p2"),
    ];
    let f = decide(&Prack2xxOr481, &msgs);
    assert_eq!(f.len(), 2, "{f:?}");
    let at = |anchor| f.iter().find(|f| f.anchor == anchor).expect("an occasion");
    assert!(matches!(at(3).decision, Decision::Compliant), "matched at the other socket: {f:?}");
    assert!(at(5).violated(), "already acknowledged: a 481 was owed: {f:?}");
}

/// §3 / §4: the PRACK alice sends from her other socket to bob's other
/// socket acknowledges the offer bob's provisional carried before his 2xx.
#[test]
fn a_prack_between_two_socket_parties_precedes_the_2xx() {
    const UAC_LOW: &str = "10.0.0.1:40000";
    const UAS_LOW: &str = "10.0.0.2:40000";
    let mut offer = fork_resp(1_100_000, 183, &reliable_rows(5), "tb", true);
    offer.src = UAS_LOW.to_string();
    let mut answer = inv_resp(1_300_000, 200, "", false);
    answer.src = UAS_LOW.to_string();
    let msgs = vec![
        via(invite(1_000_000, UAC_LOW, UAS, 1), INV_BRANCH, UAC),
        offer,
        via(prack(1_150_000, UAC_LOW, UAS_LOW, 2, 5, 1, "tb"), "z9hG4bK-p", UAC),
        answer,
    ];
    let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
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
