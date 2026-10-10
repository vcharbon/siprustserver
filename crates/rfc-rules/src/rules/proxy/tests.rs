//! The rule's OWN semantics under a CLOSED observation: what the budget is,
//! what a retransmission does to it, and what the wire cannot place.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{
    NoTarget404, Proxy100TryingNotForwarded, Proxy100WithinGrace, StrictRouteRewriteHandled,
    StrictRouteShuffleOnSend, PROXY_100_GRACE_US,
};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

fn invite(at_us: u64, cseq: u32) -> Msg {
    Msg {
        at_us,
        src: ALICE.to_string(),
        dst: BOB.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: "INVITE".to_string() },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: "INVITE".to_string(),
        from_tag: Some("at".to_string()),
        to_tag: None,
        via_branch: Some("z9hG4bK-i".to_string()),
        head: None,
        body: None,
    }
}

fn trying(at_us: u64, cseq: u32) -> Msg {
    Msg {
        at_us,
        src: BOB.to_string(),
        dst: ALICE.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status: 100 },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: "INVITE".to_string(),
        from_tag: Some("at".to_string()),
        to_tag: None,
        via_branch: Some("z9hG4bK-i".to_string()),
        head: None,
        body: None,
    }
}

fn obs(msgs: &[Msg]) -> Observation {
    let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
    let mut last_us = 0;
    for m in msgs {
        last_us = last_us.max(m.at_us);
        for ep in [&m.src, &m.dst] {
            let at = endpoint_last_us.entry(ep.clone()).or_default();
            *at = (*at).max(m.at_us);
        }
    }
    Observation { last_us, endpoint_last_us, closed: true }
}

fn eval(msgs: &[Msg]) -> Vec<Finding> {
    Proxy100TryingNotForwarded.eval(&WireView { msgs, obs: &obs(msgs) })
}

fn hits(msgs: &[Msg]) -> Vec<Finding> {
    eval(msgs).into_iter().filter(Finding::violated).collect()
}

/// One INVITE, one 100: the budget is met, and it is one occasion.
#[test]
fn one_trying_per_invite_is_compliant() {
    let msgs = [invite(1_000, 1), trying(2_000, 1)];
    let all = eval(&msgs);
    assert_eq!(all.len(), 1, "one occasion, the transaction: {all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
    assert_eq!(all[0].taker, ALICE, "read at the endpoint that opened the transaction");
}

/// A second 100 against one INVITE is a downstream copy the proxy should
/// have absorbed, and it charges the hop that forwarded it.
#[test]
fn a_trying_in_excess_of_the_invites_sent_is_violated() {
    let msgs = [invite(1_000, 1), trying(2_000, 1), trying(3_000, 1)];
    let f = hits(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, BOB, "the hop that forwarded it is charged");
    assert_eq!(f[0].taker, ALICE);
    assert_eq!(f[0].anchor, 2, "the occasion rests on the first excess 100");
    let Decision::Violated(Evidence::ExtraTryingForwarded { trying_taken, invites_sent, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*trying_taken, *invites_sent), (2, 1));
}

/// A retransmitted INVITE is owed the replayed 100 (§17.2.1): the budget
/// grows with the ladder, and only the copy past it is forwarded.
#[test]
fn a_replayed_trying_per_retransmitted_invite_is_owed() {
    let ladder = [
        invite(1_000, 1),
        trying(2_000, 1),
        invite(3_000, 1),
        trying(4_000, 1),
        invite(5_000, 1),
        trying(6_000, 1),
    ];
    assert!(hits(&ladder).is_empty(), "one per INVITE sent: {:?}", hits(&ladder));

    let mut extra = ladder.to_vec();
    extra.push(trying(7_000, 1));
    let f = hits(&extra);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::ExtraTryingForwarded { trying_taken, invites_sent, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*trying_taken, *invites_sent), (4, 3));
}

/// Three stray copies are ONE defect on ONE transaction, and the count is
/// on the finding.
#[test]
fn several_stray_copies_are_one_occasion() {
    let msgs = [invite(1_000, 1), trying(2_000, 1), trying(3_000, 1), trying(4_000, 1)];
    let f = hits(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].anchor, 2, "anchored where the count stopped being owed");
    let Decision::Violated(Evidence::ExtraTryingForwarded { trying_taken, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*trying_taken, 3, "every copy the taker took is counted");
}

/// A 100 on a transaction this vantage never saw opened opens no occasion.
#[test]
fn a_trying_for_an_unseen_invite_opens_no_occasion() {
    assert!(eval(&[trying(1_000, 9)]).is_empty());
}

/// The transactions are keyed per call: a second call's 100 is not this
/// call's excess.
#[test]
fn separate_calls_do_not_alias() {
    let other = |mut m: Msg| {
        m.call_id = "c2".to_string();
        m
    };
    let msgs =
        [invite(1_000, 1), trying(2_000, 1), other(invite(3_000, 1)), other(trying(4_000, 1))];
    assert!(hits(&msgs).is_empty(), "{:?}", hits(&msgs));
}

// ── NoTarget404 (RFC 3261 §16.3) ────────────────────────────────────────

const PROXY: &str = "127.0.0.1:5080";

/// A request into the proxy, or out of it, on a caller-chosen branch.
fn hop_request(at_us: u64, src: &str, dst: &str, branch: &str, cseq: u32) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: "INVITE".to_string() },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: "INVITE".to_string(),
        from_tag: Some("at".to_string()),
        to_tag: None,
        via_branch: Some(branch.to_string()),
        head: None,
        body: None,
    }
}

fn hop_response(at_us: u64, src: &str, dst: &str, status: u16, branch: &str, cseq: u32) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: "INVITE".to_string(),
        from_tag: Some("at".to_string()),
        to_tag: Some("bt".to_string()),
        via_branch: Some(branch.to_string()),
        head: None,
        body: None,
    }
}

/// The rule's findings charged to `endpoint` — what the live consumer keeps
/// (violated, at the vantage bind). Any endpoint that takes a request and
/// answers it without forwarding is judged here; scoping the rule to
/// DECLARED proxies is the consumer's `{Proxy}` subject, not the body's.
fn charged_to(endpoint: &str, msgs: &[Msg]) -> Vec<Finding> {
    NoTarget404
        .eval(&WireView { msgs, obs: &obs(msgs) })
        .into_iter()
        .filter(|f| f.emitter == endpoint)
        .collect()
}

/// 404 is the answer §16.3 names, so the occasion is met.
#[test]
fn a_404_without_forwarding_is_compliant() {
    let msgs = [
        hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
        hop_response(2_000, PROXY, ALICE, 404, "z9hG4bK-in", 1),
    ];
    let f = charged_to(PROXY, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, PROXY, "the proxy that resolved nothing is charged");
    assert_eq!(f[0].taker, ALICE);
}

/// Any other error final names an outcome that did not happen.
#[test]
fn another_error_final_without_forwarding_is_violated() {
    let msgs = [
        hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
        hop_response(2_000, PROXY, ALICE, 500, "z9hG4bK-in", 1),
    ];
    let f = charged_to(PROXY, &msgs).into_iter().filter(Finding::violated).collect::<Vec<_>>();
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::NoTargetFinal { status, method, branch, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((*status, method.as_str(), branch.as_str()), (500, "INVITE", "z9hG4bK-in"));
}

/// A §16.6 proxy mints a FRESH branch on the leg it forwards, so the
/// forward is correlated by Call-ID + CSeq — never by branch.
#[test]
fn a_forward_on_a_fresh_branch_resolves_a_target() {
    let msgs = [
        hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
        hop_request(2_000, PROXY, BOB, "z9hG4bK-out", 1),
        hop_response(3_000, BOB, PROXY, 486, "z9hG4bK-out", 1),
        hop_response(4_000, PROXY, ALICE, 486, "z9hG4bK-in", 1),
    ];
    assert!(charged_to(PROXY, &msgs).is_empty(), "{:?}", charged_to(PROXY, &msgs));
    assert_eq!(
        charged_to(BOB, &msgs).len(),
        1,
        "the far UAS did reject without forwarding — the consumer's {{Proxy}} \
             subject is what keeps a UA off this rule, never the body"
    );
}

/// Even where the forwarded leg rode another vantage, a final that MATCHES
/// one the endpoint took on the same transaction is a relayed rejection.
#[test]
fn a_relayed_downstream_rejection_is_not_a_no_target_outcome() {
    let msgs = [
        hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
        hop_response(2_000, BOB, PROXY, 486, "z9hG4bK-other", 1),
        hop_response(3_000, PROXY, ALICE, 486, "z9hG4bK-in", 1),
    ];
    assert!(charged_to(PROXY, &msgs).is_empty(), "{:?}", charged_to(PROXY, &msgs));
}

/// A non-error final states no resolution failure, and an unanswered
/// request states no outcome at all.
#[test]
fn only_an_error_final_opens_the_occasion() {
    let served = [
        hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
        hop_response(2_000, PROXY, ALICE, 200, "z9hG4bK-in", 1),
    ];
    assert!(charged_to(PROXY, &served).is_empty());
    let silent = [hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1)];
    assert!(charged_to(PROXY, &silent).is_empty());
}

// ── StrictRouteRewriteHandled (RFC 3261 §16.4) ──────────────────────────

/// A request crossing a hop on `branch`, with a caller-chosen Request-URI
/// and Route row in the head the §16.4 rule reads.
fn routed(at_us: u64, src: &str, dst: &str, branch: &str, request_uri: &str, route: &str) -> Msg {
    let head = format!(
        "INVITE {request_uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@h>;tag=at\r\n\
             To: <sip:bob@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             {route}\r\n"
    );
    let mut m = hop_request(at_us, src, dst, branch, 1);
    m.head = Some(head.into_bytes());
    m
}

/// The rule's findings charged to `endpoint`. Every hop that TAKES a
/// strict-routed request owes the rewrite, so a two-hop trace judges both;
/// scoping to DECLARED proxies is the consumer's `{Proxy}` subject, never
/// the body's.
fn strict_at(endpoint: &str, msgs: &[Msg]) -> Vec<Finding> {
    StrictRouteRewriteHandled
        .eval(&WireView { msgs, obs: &obs(msgs) })
        .into_iter()
        .filter(|f| f.emitter == endpoint)
        .collect()
}

fn strict(msgs: &[Msg]) -> Vec<Finding> {
    strict_at(PROXY, msgs)
}

const STRICT_ROUTE: &str = "Route: <sip:strict@h>\r\n";

/// A LOOSE first route is the §16.12 path this rule says nothing about.
#[test]
fn a_loose_first_route_is_no_occasion() {
    let msgs = [routed(1_000, ALICE, PROXY, "z9hG4bK-in", "sip:bob@h", "Route: <sip:p@h;lr>\r\n")];
    assert!(strict(&msgs).is_empty(), "{:?}", strict(&msgs));
}

/// The tripwire: a strict-routed request taken and never forwarded at all.
#[test]
fn a_strict_route_never_forwarded_is_violated() {
    let msgs = [routed(1_000, ALICE, PROXY, "z9hG4bK-in", "sip:bob@h", STRICT_ROUTE)];
    let f = strict(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, PROXY, "the box that took it owes the rewrite");
    assert_eq!(f[0].taker, ALICE);
    let Decision::Violated(Evidence::StrictRouteNotRewritten {
        first_route,
        forwarded_request_uri,
        branch,
        ..
    }) = &f[0].decision
    else {
        panic!("strict-route evidence: {:?}", f[0].decision)
    };
    assert_eq!(first_route, "sip:strict@h");
    assert!(forwarded_request_uri.is_empty(), "nothing was forwarded on the branch");
    assert_eq!(branch, "z9hG4bK-in");
}

/// The §16.4 swap performed: the strict Route URI became the forwarded
/// Request-URI.
#[test]
fn a_strict_route_swapped_into_the_request_uri_is_compliant() {
    let msgs = [
        routed(1_000, ALICE, PROXY, "z9hG4bK-in", "sip:bob@h", STRICT_ROUTE),
        routed(2_000, PROXY, BOB, "z9hG4bK-in", "sip:strict@h", "Route: <sip:bob@h>\r\n"),
    ];
    let f = strict(&msgs);
    assert_eq!(f.len(), 1, "one occasion, the request taken: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(
        strict_at(BOB, &msgs).len(),
        1,
        "the next hop took a strict-routed request too — its own occasion",
    );
}

/// Forwarded verbatim: the request reaches a hop the strict URI did not
/// name, and the evidence carries both URIs.
#[test]
fn a_strict_route_forwarded_verbatim_is_violated() {
    let msgs = [
        routed(1_000, ALICE, PROXY, "z9hG4bK-in", "sip:bob@h", STRICT_ROUTE),
        routed(2_000, PROXY, BOB, "z9hG4bK-in", "sip:bob@h", STRICT_ROUTE),
    ];
    let f = strict(&msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::StrictRouteNotRewritten {
        first_route,
        forwarded_request_uri,
        ..
    }) = &f[0].decision
    else {
        panic!("strict-route evidence: {:?}", f[0].decision)
    };
    assert_eq!(
        (first_route.as_str(), forwarded_request_uri.as_str()),
        ("sip:strict@h", "sip:bob@h")
    );
}

/// A request strict-routed TO the proxy (§12.2.1.1: its Request-URI is the
/// proxy's Record-Route URI) is forwarded with its last Route as the
/// Request-URI (§16.4), under the proxy's own Via (§16.6 step 8).
#[test]
fn a_request_addressed_to_the_proxy_takes_its_last_route() {
    let taken =
        |route: &str| routed(1_000, ALICE, PROXY, "z9hG4bK-in", &format!("sip:{PROXY}"), route);
    let forward = |request_uri: &str, route: &str| {
        let mut m = routed(2_000, PROXY, BOB, "z9hG4bK-out", request_uri, route);
        let head = String::from_utf8(m.head.take().unwrap()).unwrap().replace(
            "branch=z9hG4bK-out\r\n",
            "branch=z9hG4bK-out\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-in\r\n",
        );
        m.head = Some(head.into_bytes());
        m
    };
    let target = "Route: <sip:bob@h>\r\n";
    let swapped = [taken(target), forward("sip:bob@h", "")];
    let f = strict(&swapped);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

    let verbatim = [taken(target), forward(&format!("sip:{PROXY}"), target)];
    let f = strict(&verbatim);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::StrictRouteNotRewritten {
        first_route,
        forwarded_request_uri,
        ..
    }) = &f[0].decision
    else {
        panic!("strict-route evidence: {:?}", f[0].decision)
    };
    assert_eq!(first_route, "sip:bob@h", "the last Route is owed");
    assert_eq!(forwarded_request_uri, &format!("sip:{PROXY}"));

    // A strict next hop left in the set takes the target's place (§16.6
    // step 6), the target riding the tail.
    let two = "Route: <sip:next@h>\r\nRoute: <sip:bob@h>\r\n";
    let f = strict(&[taken(two), forward("sip:next@h", target)]);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A Request-URI at the proxy's own address under a loose Route naming the
/// proxy is loose routing (§16.4 pops that Route), not a strict route: a
/// preloaded outbound proxy serving the domain (§8.1.2), or an element
/// whose Contact names itself.
#[test]
fn a_loose_route_to_the_proxy_itself_is_no_occasion() {
    let route = format!("Route: <sip:{PROXY};lr>\r\n");
    for request_uri in [format!("sip:bob@{PROXY}"), format!("sip:bob@{PROXY};x=1")] {
        let taken = routed(1_000, ALICE, PROXY, "z9hG4bK-in", &request_uri, &route);
        let forward = routed(2_000, PROXY, BOB, "z9hG4bK-in", &format!("sip:bob@{BOB}"), "");
        let f = strict(&[taken, forward]);
        assert!(f.is_empty(), "loose routing to {request_uri} is no strict occasion: {f:?}");
    }
}

/// A Request-URI at the proxy's address over a Route set that ends in a
/// loose entry is loose routing toward that entry: a strict-routed request
/// carries the remote target, never a loose route, last (§12.2.1.1).
#[test]
fn a_route_set_ending_loose_is_no_strict_occasion() {
    let loose_next = "Route: <sip:p2@127.0.0.1:5096;lr>\r\n";
    let taken = routed(1_000, ALICE, PROXY, "z9hG4bK-in", &format!("sip:bob@{PROXY}"), loose_next);
    let forward = routed(2_000, PROXY, BOB, "z9hG4bK-in", &format!("sip:bob@{PROXY}"), loose_next);
    let f = strict(&[taken, forward]);
    assert!(f.is_empty(), "loose routing past the proxy opened a strict occasion: {f:?}");
}

/// An RFC 2543 UA strict-routes to a loose proxy's own `;lr` Record-Route
/// URI: the Request-URI names the proxy and no loose top Route does, so
/// §16.4 owes the last Route as the forward's Request-URI, and an
/// unrewritten forward is charged. With a loose router left in the Route
/// set the owed URI is still the target, which only §16.4 names.
#[test]
fn a_strict_route_to_the_proxys_own_loose_uri_is_rewritten() {
    let own = format!("sip:{PROXY};lr");
    let target = "Route: <sip:bob@h>\r\n";
    let taken = |route: &str| routed(1_000, ALICE, PROXY, "z9hG4bK-in", &own, route);
    let forward = |request_uri: &str, route: &str| {
        let mut m = routed(2_000, PROXY, BOB, "z9hG4bK-out", request_uri, route);
        let head = String::from_utf8(m.head.take().unwrap()).unwrap().replace(
            "branch=z9hG4bK-out\r\n",
            "branch=z9hG4bK-out\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-in\r\n",
        );
        m.head = Some(head.into_bytes());
        m
    };

    let f = strict(&[taken(target), forward(&own, target)]);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::StrictRouteNotRewritten { first_route, .. }) = &f[0].decision
    else {
        panic!("an unrewritten forward is charged: {:?}", f[0].decision)
    };
    assert_eq!(first_route, "sip:bob@h", "§16.4: the last Route is owed");

    let via_loose = "Route: <sip:p2@h;lr>\r\nRoute: <sip:bob@h>\r\n";
    let f = strict(&[taken(via_loose), forward(&own, via_loose)]);
    assert_eq!(f.len(), 1, "a loose router before the target hides no occasion: {f:?}");
    assert!(matches!(f[0].decision, Decision::Violated(_)), "{:?}", f[0].decision);

    let f = strict(&[taken(via_loose), forward("sip:bob@h", "Route: <sip:p2@h;lr>\r\n")]);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The header block is what says a request was strict-routed at all: a
/// vantage that carried none opens NO occasion, rather than an undecidable
/// one against every request on the wire.
#[test]
fn a_request_without_header_bytes_is_no_occasion() {
    let msgs = [hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1)];
    assert!(strict(&msgs).is_empty(), "{:?}", strict(&msgs));
}

// ── Proxy100WithinGrace (RFC 3261 §16.7) ────────────────────────────────

/// A final bob sent on the INVITE's own transaction.
fn answered(at_us: u64, status: u16) -> Msg {
    Msg {
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

/// §16.7 wants the 100 only where the answer is slow: a hop that rejected
/// inside the grace owes nobody a Trying.
#[test]
fn a_final_inside_the_grace_discharges_it() {
    let f = grace(&[invite(1_000, 1), answered(1_000 + PROXY_100_GRACE_US, 486)]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
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

// ---- strict-route-shuffle-on-send -----------------------------------

/// A request the hop forwarded, stating `route` at the top of its Route set
/// (`""` = no Route header at all).
fn forwarded(route: &str) -> Msg {
    let row = if route.is_empty() { String::new() } else { format!("Route: {route}\r\n") };
    let head = format!(
        "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-r\r\n\
             {row}\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    );
    let mut m = invite(1_000, 1);
    m.head = Some(head.into_bytes());
    m
}

fn shuffle(msgs: &[Msg]) -> Vec<Finding> {
    StrictRouteShuffleOnSend.eval(&WireView { msgs, obs: &obs(msgs) })
}

#[test]
fn a_loose_topmost_route_is_compliant() {
    let f = shuffle(&[forwarded("<sip:proxy@127.0.0.1;lr>")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

#[test]
fn a_strict_topmost_route_is_violated() {
    let f = shuffle(&[forwarded("<sip:proxy@127.0.0.1>")]);
    let Decision::Violated(Evidence::HeaderValueRejected { header, value, expected, .. }) =
        &f[0].decision
    else {
        panic!("strict-route evidence: {:?}", f[0].decision)
    };
    assert_eq!(header, "Route");
    assert!(value.contains("proxy@127.0.0.1"), "{value}");
    assert!(expected.contains(";lr"), "{expected}");
}

/// The swap leaves the request it pushed down at the top of the Route set:
/// paired with the request the hop took, a Request-URI that is a strict
/// route that request named is the swap having run.
#[test]
fn a_swapped_forward_paired_with_what_it_took_is_compliant() {
    let taken = |route: &str| {
        let mut m = forwarded(route);
        m.src = "127.0.0.1:5050".to_string();
        m.dst = ALICE.to_string();
        m.via_branch = Some("z9hG4bK-r".to_string());
        m
    };
    let sent = |request_uri: &str| {
        let head = format!(
            "INVITE {request_uri} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-fwd\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5050;branch=z9hG4bK-r\r\n\
                 Route: <sip:bob@127.0.0.1:5070>\r\n\
                 From: <sip:alice@127.0.0.1>;tag=at\r\n\
                 To: <sip:bob@127.0.0.1>\r\n\
                 Call-ID: c1\r\n\
                 CSeq: 1 INVITE\r\n\
                 Content-Length: 0\r\n\r\n"
        );
        let mut m = invite(2_000, 1);
        m.via_branch = Some("z9hG4bK-fwd".to_string());
        m.head = Some(head.into_bytes());
        m
    };
    let strict_hop = "<sip:strict@127.0.0.1>";
    let f = shuffle(&[taken(strict_hop), sent("sip:strict@127.0.0.1")]);
    let swap = f.iter().find(|f| f.anchor == 1).expect("the forward is an occasion");
    assert!(matches!(swap.decision, Decision::Compliant), "{:?}", swap.decision);

    let f = shuffle(&[taken(strict_hop), sent("sip:other@127.0.0.1")]);
    let swap = f.iter().find(|f| f.anchor == 1).expect("the forward is an occasion");
    assert!(matches!(swap.decision, Decision::Violated(_)), "not the route it named");
}

/// A request `src` sent `dst` on Via `branches` (top first), carrying
/// `routes`.
fn hop_msg(
    at_us: u64,
    src: &str,
    dst: &str,
    request_uri: &str,
    branches: &[&str],
    routes: &[&str],
) -> Msg {
    let mut head = format!("INVITE {request_uri} SIP/2.0\r\n");
    for (i, branch) in branches.iter().enumerate() {
        head += &format!("Via: SIP/2.0/UDP 127.0.0.1:{};branch={branch}\r\n", 6000 + i);
    }
    for route in routes {
        head += &format!("Route: {route}\r\n");
    }
    head += "From: <sip:alice@127.0.0.1>;tag=at\r\nTo: <sip:bob@127.0.0.1>;tag=bt\r\n\
                 Call-ID: c1\r\nCSeq: 2 INVITE\r\nContent-Length: 0\r\n\r\n";
    let mut m = hop_request(at_us, src, dst, branches[0], 2);
    m.head = Some(head.into_bytes());
    m
}

const HOP: &str = "127.0.0.1:5080";
const S1: &str = "<sip:s1@127.0.0.1:5095>";
const S2: &str = "<sip:s2@127.0.0.1:5096>";
const TARGET: &str = "<sip:bob@127.0.0.1:5070>";

/// What `HOP` forwarded on the branch it took, sent to s2.
fn forward_of(request_uri: &str, routes: &[&str]) -> Msg {
    hop_msg(2_000, HOP, "127.0.0.1:5096", request_uri, &["z9hG4bK-out", "z9hG4bK-in"], routes)
}

fn violated_at(f: &[Finding], anchor: usize) -> bool {
    f.iter().any(|f| f.anchor == anchor && matches!(f.decision, Decision::Violated(_)))
}

/// §16.6 step 6 lifts the FIRST strict Route, not any one of them.
#[test]
fn a_shuffle_lifting_a_later_strict_route_is_violated() {
    let taken = hop_msg(1_000, ALICE, HOP, "sip:bob@127.0.0.1:5070", &["z9hG4bK-in"], &[S1, S2]);
    let sent = forward_of("sip:s2@127.0.0.1:5096", &[S1, TARGET]);
    assert!(violated_at(&shuffle(&[taken, sent]), 1), "the wrong strict route was lifted");
}

/// The §16.4 rewrite ran but the step 6 swap did not: the strict hop left
/// on top was owed the Request-URI.
#[test]
fn a_skipped_shuffle_after_the_strict_route_restore_is_violated() {
    let taken = hop_msg(1_000, ALICE, HOP, &format!("sip:{HOP}"), &["z9hG4bK-in"], &[S2, TARGET]);
    let sent = forward_of("sip:bob@127.0.0.1:5070", &[S2]);
    assert!(violated_at(&shuffle(&[taken, sent]), 1), "the swap never ran");
}

/// The proxy pops its own loose Route (§16.4), and the first strict Route
/// left is owed the Request-URI: both strict-route rules charge lifting
/// another one.
#[test]
fn a_wrong_lift_after_popping_its_own_loose_route_is_violated() {
    let own = format!("<sip:{HOP};lr>");
    let taken = hop_msg(
        1_000,
        ALICE,
        HOP,
        "sip:bob@127.0.0.1:5070",
        &["z9hG4bK-in"],
        &[own.as_str(), S1, S2],
    );
    let sent = forward_of("sip:s2@127.0.0.1:5096", &[S1, TARGET]);
    let msgs = [taken, sent];
    assert!(violated_at(&shuffle(&msgs), 1), "shuffle-on-send charges the wrong lift");
    let rewrite = StrictRouteRewriteHandled.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert!(violated_at(&rewrite, 0), "the rewrite was owed s1: {rewrite:?}");

    let right = forward_of("sip:s1@127.0.0.1:5095", &[S2, TARGET]);
    let msgs = [msgs[0].clone(), right];
    let rewrite = StrictRouteRewriteHandled.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
    assert!(
        rewrite.iter().any(|f| f.anchor == 0 && matches!(f.decision, Decision::Compliant)),
        "{rewrite:?}"
    );
}

/// A request stating no route set is routed through nothing, and a vantage
/// with no header block cannot say whether it stated one.
#[test]
fn a_routeless_or_byte_less_request_is_no_occasion() {
    assert!(shuffle(&[forwarded("")]).is_empty());
    assert!(shuffle(&[invite(1_000, 1)]).is_empty());
}
