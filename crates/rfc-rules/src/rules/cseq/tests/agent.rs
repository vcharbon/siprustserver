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

/// RFC 3263 §4.3: a request that drew a 503 is retried, identical but for
/// its branch, at the agent's other socket: the same request, not a reuse.
#[test]
fn a_retry_after_a_503_to_the_uas_s_other_socket_is_no_reuse() {
    let mut msgs = opened();
    msgs.push(in_dialog(4_000, "BYE", 2, "z9hG4bK-b1"));
    msgs.push(rsp(4_500, 503, 2, "BYE", "z9hG4bK-b1"));
    msgs.push(at_second(in_dialog(5_000, "BYE", 2, "z9hG4bK-b2")));
    assert!(hits(&CseqInDialogOrder, &msgs).is_empty(), "{:?}", order(&msgs));
}

/// A request that drew a final other than 503 was served: the same number on
/// a new transaction to the agent's other socket is a reuse.
#[test]
fn a_cseq_spent_on_a_served_request_is_reused_at_the_other_socket() {
    let mut msgs = opened();
    msgs.push(in_dialog(4_000, "INFO", 2, "z9hG4bK-n"));
    msgs.push(rsp(4_500, 200, 2, "INFO", "z9hG4bK-n"));
    msgs.push(at_second(in_dialog(5_000, "INFO", 2, "z9hG4bK-n2")));
    assert_eq!(hits(&CseqInDialogOrder, &msgs).len(), 1, "{:?}", order(&msgs));
}
