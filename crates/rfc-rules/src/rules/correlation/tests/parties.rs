//! A user agent that sends from one socket and listens on another is ONE party
//! to every rule of the family.

use super::*;

use super::super::parties::Parties;
use crate::verdict::RuleId;

/// The socket A sends from; A's Via sent-by, and its listening socket, is `A`.
const A_SEND: &str = "10.0.0.1:40000";
/// The socket B answers from; B takes requests at `B`.
const B_SEND: &str = "10.0.0.2:40000";

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
    &f.iter().find(|x| x.rule == rule && x.anchor == anchor).expect("an occasion").decision
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
/// socket (§18.2.2): their From-tag is A's own and their CSeq one A sent.
#[test]
fn responses_at_the_sent_by_answer_the_requests_of_the_other_socket() {
    let msgs = cancelled_from_two_sockets();
    let f = family(&msgs);
    assert!(f.iter().all(|x| !x.violated()), "{f:?}");
    for anchor in [3, 4] {
        for rule in [RuleId::MidDialogTags, RuleId::ResponseCorrelation] {
            let d = decision_at(&f, rule, anchor);
            assert!(matches!(d, Decision::Compliant), "{rule:?} at {anchor}: {d:?}");
        }
    }
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
    let d = decision_at(&f, RuleId::MidDialogTags, 3);
    assert!(matches!(d, Decision::Compliant), "the BYE: {d:?}");
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

/// The three ends of a proxied INVITE, each keyed as its own sender.
fn parties_of(msgs: &[Msg]) -> Vec<(String, String)> {
    let parties = Parties::of(msgs);
    [&msgs[0], &msgs[1], &msgs[2]].map(|m| parties.sender(m)).to_vec()
}

/// A proxy's two hops carry two branches (§16.6), so its answers tie each hop
/// to itself and the caller, proxy and callee stay three parties.
#[test]
fn a_proxys_hops_stay_three_parties() {
    const P: &str = "10.0.0.9:5060";
    let msgs = vec![
        req(1_000, A, P, "INVITE", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1),
        req(1_500, P, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-2", "sip:alice@h", "at", None, 2),
        resp(2_000, B, P, 200, 1, "INVITE", "z9hG4bK-2", "at", Some("bt"), 2),
        resp(2_500, P, A, 200, 1, "INVITE", "z9hG4bK-1", "at", Some("bt"), 1),
    ];
    let ends = parties_of(&msgs);
    assert_eq!(ends.iter().map(|(ua, _)| ua.as_str()).collect::<Vec<_>>(), [A, P, B]);
}

/// A branch carried on two hops (a forwarder adding no Via) ties nothing.
#[test]
fn a_branch_carried_between_two_pairs_ties_nothing() {
    const P: &str = "10.0.0.9:5060";
    let msgs = vec![
        req(1_000, A, P, "INVITE", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1),
        req(1_500, P, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-1", "sip:alice@h", "at", None, 1),
        resp(2_000, B, P, 200, 1, "INVITE", "z9hG4bK-1", "at", Some("bt"), 1),
        resp(2_500, P, A, 200, 1, "INVITE", "z9hG4bK-1", "at", Some("bt"), 1),
    ];
    let ends = parties_of(&msgs);
    assert_eq!(ends.iter().map(|(ua, _)| ua.as_str()).collect::<Vec<_>>(), [A, P, B]);
}
