//! A user agent that sends from one socket and listens on another is ONE party
//! to every rule of the family, and a response sent past its Via ties no one.

use super::*;

use super::super::parties::Parties;
use crate::verdict::RuleId;

/// The socket A sends from; A's Via sent-by, and its listening socket, is `A`.
const A_SEND: &str = "10.0.0.1:40000";
/// The socket B answers from; B takes requests at `B`.
const B_SEND: &str = "10.0.0.2:40000";
/// A proxy between A and B.
const P: &str = "10.0.0.9:5060";

/// Every rule of the family over `msgs`.
fn family(msgs: &[Msg]) -> Vec<Finding> {
    let rules: [&dyn Obligation; 10] = [
        &ResponseEchoesRequestVia,
        &ResponseCorrelation,
        &MidDialogTags,
        &PeerUriStable,
        &CancelRequestUri,
        &CancelViaBranch,
        &TagConsistency,
        &NoToTagOnInitialRequest,
        &InDialogToTag,
        &InDialogFromTag,
    ];
    rules.iter().flat_map(|r| run(*r, msgs)).collect()
}

/// The decision on the occasion `rule` found at `anchor`.
fn decision_at(f: &[Finding], rule: RuleId, anchor: usize) -> &Decision {
    &f.iter()
        .find(|x| x.rule == rule && x.anchor == anchor)
        .unwrap_or_else(|| panic!("{rule:?} has an occasion at {anchor}: {f:?}"))
        .decision
}

/// `msg` with its top Via naming `sent_by`.
fn via_sent_by(mut msg: Msg, sent_by: &str) -> Msg {
    let head = String::from_utf8(msg.head.take().expect("a head")).expect("text");
    let head =
        head.replacen("Via: SIP/2.0/UDP 10.0.0.1:5060", &format!("Via: SIP/2.0/UDP {sent_by}"), 1);
    msg.head = Some(head.into_bytes());
    msg
}

/// A sends its INVITE and CANCEL from `A_SEND`; B answers every one at `A`.
fn cancelled_from_two_sockets() -> Vec<Msg> {
    vec![
        req(1_000, A_SEND, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        req(3_000, A_SEND, B, "CANCEL", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(4_000, B, A, 200, 1, "CANCEL", "z9hG4bK-i", "at", Some("bt"), 1),
        resp(5_000, B, A, 487, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        req(
            6_000,
            A_SEND,
            B,
            "ACK",
            "sip:bob@h",
            1,
            "z9hG4bK-i",
            "sip:alice@h",
            "at",
            Some("bt"),
            1,
        ),
    ]
}

/// The responses reaching A's sent-by answer requests A sent from its other
/// socket (§18.2.2): their From-tag is A's own, their CSeq one A sent, their
/// Via stack the one A's request carried.
#[test]
fn responses_at_the_sent_by_answer_the_requests_of_the_other_socket() {
    let f = family(&cancelled_from_two_sockets());
    assert!(f.iter().all(|x| !x.violated()), "{f:?}");
    for anchor in [3, 4] {
        for rule in
            [RuleId::MidDialogTags, RuleId::ResponseCorrelation, RuleId::ResponseEchoesRequestVia]
        {
            let d = decision_at(&f, rule, anchor);
            assert!(matches!(d, Decision::Compliant), "{rule:?} at {anchor}: {d:?}");
        }
    }
}

#[test]
fn a_grown_via_stack_at_the_sent_by_is_violated() {
    let mut msgs = cancelled_from_two_sockets();
    msgs[4] = resp(5_000, B, A, 487, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 2);
    let f = run(&ResponseEchoesRequestVia, &msgs);
    let d = decision_at(&f, RuleId::ResponseEchoesRequestVia, 4);
    assert!(matches!(d, Decision::Violated(Evidence::ResponseViaDiverged { .. })), "{d:?}");
}

/// A confirms the dialog at `A` and sends its BYE from `A_SEND`, naming the
/// tag the 200 at its other socket carried (§12.2.1.1).
#[test]
fn a_request_from_the_sending_socket_names_the_dialog_the_listening_socket_confirmed() {
    let msgs = vec![
        req(1_000, A_SEND, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        req(
            3_000,
            A_SEND,
            B,
            "ACK",
            "sip:bob@h",
            1,
            "z9hG4bK-a",
            "sip:alice@h",
            "at",
            Some("bt"),
            1,
        ),
        req(
            4_000,
            A_SEND,
            B,
            "BYE",
            "sip:bob@h",
            2,
            "z9hG4bK-b",
            "sip:alice@h",
            "at",
            Some("bt"),
            1,
        ),
        resp(5_000, B, A, 200, 2, "BYE", "z9hG4bK-b", "at", Some("bt"), 1),
    ];
    let f = family(&msgs);
    assert!(f.iter().all(|x| !x.violated()), "{f:?}");
    let d = decision_at(&f, RuleId::InDialogToTag, 3);
    assert!(matches!(d, Decision::Compliant), "the BYE: {d:?}");
}

/// B takes the INVITE and the BYE at `B` and answers from `B_SEND`: the BYE
/// names the tag B minted on that other socket.
fn answered_from_two_sockets(bye_to_tag: &str) -> Vec<Msg> {
    vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B_SEND, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        req(3_000, A, B, "ACK", "sip:bob@h", 1, "z9hG4bK-a", "sip:alice@h", "at", Some("bt"), 1),
        req(
            4_000,
            A,
            B,
            "BYE",
            "sip:bob@h",
            2,
            "z9hG4bK-b",
            "sip:alice@h",
            "at",
            Some(bye_to_tag),
            1,
        ),
        resp(5_000, B_SEND, A, 200, 2, "BYE", "z9hG4bK-b", "at", Some(bye_to_tag), 1),
    ]
}

#[test]
fn a_request_at_the_listening_socket_names_the_tag_the_answering_socket_minted() {
    let f = family(&answered_from_two_sockets("bt"));
    assert!(f.iter().all(|x| !x.violated()), "{f:?}");
    for rule in [RuleId::MidDialogTags, RuleId::InDialogFromTag] {
        let d = decision_at(&f, rule, 3);
        assert!(matches!(d, Decision::Compliant), "{rule:?} on the BYE: {d:?}");
    }
}

#[test]
fn a_foreign_tag_at_a_two_socket_party_is_still_violated() {
    let f = run(&MidDialogTags, &answered_from_two_sockets("nope"));
    let Decision::Violated(Evidence::DialogTagForeign { tag, local_tags, .. }) =
        decision_at(&f, RuleId::MidDialogTags, 3)
    else {
        panic!("tags evidence: {f:?}")
    };
    assert_eq!((tag.as_str(), local_tags.as_slice()), ("nope", ["bt".to_string()].as_slice()));
}

/// B's provisional leaves `B_SEND` and its final `B`: one UAS, so the final
/// flipping the provisional's tag is its flip (§17.2.1).
#[test]
fn a_tag_flipped_across_two_sockets_of_one_uas_is_violated() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B_SEND, A, 180, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        resp(3_000, B, A, 486, 1, "INVITE", "z9hG4bK-i", "at", Some("other"), 1),
        req(4_000, A, B, "ACK", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", Some("other"), 1),
    ];
    let f = run(&TagConsistency, &msgs);
    let d = decision_at(&f, RuleId::TagConsistency, 2);
    assert!(matches!(d, Decision::Violated(Evidence::UasTagFlipped { .. })), "{d:?}");
}

/// A's re-INVITE leaves a third socket, whose answer reaches A's sent-by: it
/// is A's request inside A's dialog, not a first request carrying a To-tag.
#[test]
fn a_request_from_a_third_socket_of_a_party_is_not_its_first() {
    const A_THIRD: &str = "10.0.0.1:40001";
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 200, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
        req(3_000, A, B, "ACK", "sip:bob@h", 1, "z9hG4bK-a", "sip:alice@h", "at", Some("bt"), 1),
        req(
            4_000,
            A_THIRD,
            B,
            "INVITE",
            "sip:bob@h",
            2,
            "z9hG4bK-r",
            "sip:alice@h",
            "at",
            Some("bt"),
            1,
        ),
        resp(5_000, B, A, 200, 2, "INVITE", "z9hG4bK-r", "at", Some("bt"), 1),
        req(6_000, A, B, "ACK", "sip:bob@h", 2, "z9hG4bK-ra", "sip:alice@h", "at", Some("bt"), 1),
    ];
    let f = run(&NoToTagOnInitialRequest, &msgs);
    assert!(f.iter().all(|x| x.anchor != 3), "the re-INVITE is no initial request: {f:?}");
}

/// The three ends of a proxied INVITE, each keyed as its own sender.
fn senders(msgs: &[Msg]) -> Vec<String> {
    let parties = Parties::of(msgs);
    msgs.iter().map(|m| parties.sender(m).0).collect()
}

/// A proxy's two hops carry two branches (§16.6), so its answers tie each hop
/// to itself and the caller, proxy and callee stay three parties.
#[test]
fn a_proxys_hops_stay_three_parties() {
    let msgs = vec![
        req(1_000, A, P, "INVITE", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1),
        via_sent_by(
            req(1_500, P, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-2", "sip:alice@h", "at", None, 2),
            P,
        ),
        via_sent_by(resp(2_000, B, P, 200, 1, "INVITE", "z9hG4bK-2", "at", Some("bt"), 2), P),
        resp(2_500, P, A, 200, 1, "INVITE", "z9hG4bK-1", "at", Some("bt"), 1),
    ];
    assert_eq!(senders(&msgs), [A, P, B, P]);
}

/// A branch carried on two hops (a forwarder adding no Via) ties nothing.
#[test]
fn a_branch_carried_between_two_pairs_ties_nothing() {
    let msgs = vec![
        req(1_000, A, P, "INVITE", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1),
        req(1_500, P, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1),
        resp(2_000, B, A, 200, 1, "INVITE", "z9hG4bK-1", "at", Some("bt"), 1),
    ];
    assert_eq!(senders(&msgs), [A, P, B]);
}

/// B answers the proxy's hop straight to the caller, past the address its Via
/// names: the response ties no one, and the caller sent no such Via stack.
#[test]
fn a_response_sent_past_its_via_ties_no_one() {
    let msgs = vec![
        req(1_000, A, P, "INVITE", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1),
        via_sent_by(
            req(1_500, P, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-2", "sip:alice@h", "at", None, 2),
            P,
        ),
        via_sent_by(resp(2_000, B, A, 200, 1, "INVITE", "z9hG4bK-2", "at", Some("bt"), 2), P),
    ];
    assert_eq!(senders(&msgs), [A, P, B]);
    let f = run(&ResponseEchoesRequestVia, &msgs);
    let d = decision_at(&f, RuleId::ResponseEchoesRequestVia, 2);
    assert!(matches!(d, Decision::Violated(Evidence::ResponseViaDiverged { .. })), "{d:?}");
}

/// B's answering socket ties to the one that took its request whatever A's
/// Via names: the sent-by is a name no view resolves, and the BYE still names
/// the tag B minted.
#[test]
fn the_answering_side_ties_under_a_named_sent_by() {
    let msgs: Vec<Msg> = answered_from_two_sockets("bt")
        .into_iter()
        .map(|m| via_sent_by(m, "alice.example:5060"))
        .collect();
    let f = run(&MidDialogTags, &msgs);
    let d = decision_at(&f, RuleId::MidDialogTags, 3);
    assert!(matches!(d, Decision::Compliant), "{d:?}");
}

/// The `rport` and `received` a response carries are the answering side's
/// stamp: a 486 sent to a socket A's request never named ties that socket to
/// no one.
#[test]
fn a_response_at_a_stamped_rport_the_sender_never_wrote_ties_no_one() {
    const ELSEWHERE: &str = "10.0.0.1:40001";
    let msgs = vec![
        via_sent_by(
            req(
                1_000,
                A_SEND,
                B,
                "INVITE",
                "sip:bob@h",
                1,
                "z9hG4bK-i",
                "sip:alice@h",
                "at",
                None,
                1,
            ),
            "10.0.0.1:5060;rport",
        ),
        via_sent_by(
            resp(2_000, B, ELSEWHERE, 486, 1, "INVITE", "z9hG4bK-i", "at", Some("bt"), 1),
            "10.0.0.1:5060;rport=40001;received=10.0.0.1",
        ),
    ];
    let parties = Parties::of(&msgs);
    assert_eq!(parties.taker(&msgs[1]).0, ELSEWHERE);
    assert_eq!(parties.sender(&msgs[0]).0, A_SEND);
}
