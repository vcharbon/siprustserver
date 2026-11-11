//! A UAS taking requests at two sockets holds one CSeq space per stream: the
//! stream is the user agent's, and a finding names the socket each request
//! reached.

use super::*;

/// A second socket of bob's, the one it answers from.
const BOB_SENDS: &str = "10.0.0.2:41000";

/// `m` addressed to, or sent from, bob's second socket.
fn at_second(mut m: Msg) -> Msg {
    if m.dst == BOB {
        m.dst = BOB_SENDS.to_string();
    } else {
        m.src = BOB_SENDS.to_string();
    }
    m
}

/// The dialog bob answers from his second socket, which alice then addresses.
fn opened() -> Vec<Msg> {
    vec![
        req(1_000, "INVITE", 1, "z9hG4bK-i", None),
        at_second(rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i")),
        at_second(in_dialog(3_000, "ACK", 1, "z9hG4bK-a")),
    ]
}

/// One CSeq carried by two new transactions reaching bob at his two sockets
/// is one reuse on one dialog.
#[test]
fn a_reuse_across_a_two_socket_uas_is_violated() {
    let mut msgs = opened();
    msgs.push(in_dialog(4_000, "INFO", 2, "z9hG4bK-n"));
    msgs.push(at_second(in_dialog(5_000, "BYE", 2, "z9hG4bK-b")));
    let f = hits(&CseqInDialogOrder, &msgs);
    assert_eq!(f.len(), 1, "{:?}", order(&msgs));
    assert_eq!(f[0].taker, BOB_SENDS, "the socket the reused CSeq reached is named");
}

/// The ACK bob takes at his second socket acknowledges the INVITE he took at
/// his first.
#[test]
fn an_ack_at_a_two_socket_uas_s_other_socket_matches_its_invite() {
    let f = eval(&AckCseqMatchesInvite, &opened());
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[0].taker, BOB_SENDS);
}
