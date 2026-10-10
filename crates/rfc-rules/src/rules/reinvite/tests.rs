//! The family's OWN semantics under a CLOSED observation — what the live
//! adapter's lane policy cannot state: which INVITEs are in flight
//! together, which tag orientation a race is read on, and what the wire
//! alone settles about an unanswered one.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::Obligation;
use super::{
    ConcurrentReInvite500Or491, FailedReinviteTearsDownDialog, NoReInviteWhileInviteInProgress,
    REINVITE_FINAL_WINDOW_US,
};

const ALICE: &str = "10.0.0.1:5060";
const BOB: &str = "10.0.0.2:5060";

/// A request on the wire, with caller-controlled tags and top-Via branch.
#[allow(clippy::too_many_arguments)]
fn req(
    at_us: u64,
    src: &str,
    dst: &str,
    method: &str,
    branch: &str,
    cseq: u32,
    from_tag: &str,
    to_tag: &str,
) -> Msg {
    Msg {
        at_us,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Request { method: method.to_string() },
        call_id: "c1".to_string(),
        cseq,
        cseq_method: method.to_string(),
        via_branch: Some(branch.to_string()),
        from_tag: Some(from_tag.to_string()),
        to_tag: (!to_tag.is_empty()).then(|| to_tag.to_string()),
        head: Some(b"Content-Length: 0\r\n\r\n".to_vec()),
        body: None,
    }
}

/// A response on the wire, answering an INVITE transaction on `branch`.
#[allow(clippy::too_many_arguments)]
fn rsp(
    at_us: u64,
    src: &str,
    dst: &str,
    status: u16,
    branch: &str,
    cseq: u32,
    from_tag: &str,
    to_tag: &str,
    head: &str,
) -> Msg {
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
        via_branch: Some(branch.to_string()),
        from_tag: Some(from_tag.to_string()),
        to_tag: (!to_tag.is_empty()).then(|| to_tag.to_string()),
        head: Some(format!("{head}Content-Length: 0\r\n\r\n").into_bytes()),
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

/// The same view with the observation left OPEN at `last_us`, so a window
/// gate is exercised rather than collapsed.
fn eval_open(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    let mut o = obs(msgs);
    o.closed = false;
    rule.eval(&WireView { msgs, obs: &o })
}

fn eval(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    rule.eval(&WireView { msgs, obs: &obs(msgs) })
}

fn hits(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
    eval(rule, msgs).into_iter().filter(Finding::violated).collect()
}

// ---- concurrent-re-invite-500-or-491 ---------------------------------

/// The sender re-INVITEs while its own earlier INVITE has no final: §14.2's
/// first clause owes 500 with `Retry-After`, so a 491 is the violation.
#[test]
fn a_same_sender_race_answered_491_is_violated() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(3_000, BOB, ALICE, 491, "z9hG4bK-2", 3, "at", "bt", ""),
            rsp(4_000, BOB, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
        ],
    );
    assert_eq!(f.len(), 1, "one occasion, the racing re-INVITE: {f:?}");
    assert_eq!(f[0].rule, RuleId::ConcurrentReInvite500Or491);
    assert_eq!(f[0].emitter, BOB, "the UAS that owed the 500 is charged");
    assert_eq!(f[0].taker, ALICE);
    let Decision::Violated(Evidence::ConcurrentReInvite { answered_status, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*answered_status, 491);
}

/// A re-INVITE crossing one of the other party's at a hop that carries
/// both directions is §14.2's second clause: 491 meets it.
#[test]
fn a_crossing_race_answered_491_is_compliant() {
    let f = eval(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-a", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-b", 2, "bt", "at"),
            rsp(3_000, BOB, ALICE, 491, "z9hG4bK-b", 2, "bt", "at", ""),
            rsp(4_000, BOB, ALICE, 200, "z9hG4bK-a", 2, "at", "bt", ""),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// At a hop carrying both directions, a re-INVITE that finds its sender's
/// own INVITE AND a crossing one in flight is judged against its sender's:
/// 500 with Retry-After is owed and a 491 is the violation, whichever
/// pending branch sorts first.
#[test]
fn a_race_over_both_a_same_sender_and_a_crossing_invite_owes_the_500() {
    let msgs = |status: u16, head: &str| {
        vec![
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-0", 2, "bt", "at"),
            rsp(3_000, BOB, ALICE, 491, "z9hG4bK-0", 2, "bt", "at", ""),
            req(2_500, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(4_000, BOB, ALICE, status, "z9hG4bK-2", 3, "at", "bt", head),
            rsp(5_000, BOB, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
        ]
    };
    let on_the_newcomer =
        |f: Vec<Finding>| -> Vec<Finding> { f.into_iter().filter(|f| f.cseq == 3).collect() };
    let compliant =
        on_the_newcomer(eval(&ConcurrentReInvite500Or491, &msgs(500, "Retry-After: 3\r\n")));
    assert_eq!(compliant.len(), 1, "{compliant:?}");
    assert!(matches!(compliant[0].decision, Decision::Compliant), "{:?}", compliant[0].decision);
    let refused_491 = on_the_newcomer(hits(&ConcurrentReInvite500Or491, &msgs(491, "")));
    assert_eq!(refused_491.len(), 1, "{refused_491:?}");
}

/// The crossing owes 491, not the 500 of a same-sender race.
#[test]
fn a_crossing_race_answered_500_is_violated() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-a", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-b", 2, "bt", "at"),
            rsp(3_000, BOB, ALICE, 500, "z9hG4bK-b", 2, "bt", "at", "Retry-After: 3\r\n"),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
}

/// Answering the race 200 serves two offers at once — the violation, with
/// both transactions named.
#[test]
fn a_racing_re_invite_answered_200_is_violated() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(3_000, BOB, ALICE, 200, "z9hG4bK-2", 3, "at", "bt", ""),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::ConcurrentReInvite {
        branch,
        pending_invite_branch,
        pending_invite_msg,
        answered_status,
        retry_after,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(branch.as_str(), "z9hG4bK-2");
    assert_eq!(pending_invite_branch.as_str(), "z9hG4bK-1");
    assert_eq!(*pending_invite_msg, 0, "the INVITE it collided with");
    assert_eq!((*answered_status, *retry_after), (200, false));
    assert_eq!(f[0].anchor, 1, "the occasion rests on the racing re-INVITE");
}

/// §14.2's other legal answer: 500 CARRYING `Retry-After`. A bare 500 is
/// not it.
#[test]
fn a_500_discharges_only_with_retry_after() {
    let race = |head: &str| {
        vec![
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(3_000, BOB, ALICE, 500, "z9hG4bK-2", 3, "at", "bt", head),
        ]
    };
    assert!(hits(&ConcurrentReInvite500Or491, &race("Retry-After: 5\r\n")).is_empty());
    assert_eq!(hits(&ConcurrentReInvite500Or491, &race("")).len(), 1);
}

/// The 100 Trying precedes a compliant 500 — only the transaction's FINAL
/// answers §14.2, so the provisional must not stand in for it.
#[test]
fn a_100_trying_does_not_stand_in_for_the_final() {
    assert!(hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(3_000, BOB, ALICE, 100, "z9hG4bK-2", 3, "at", "bt", ""),
            rsp(4_000, BOB, ALICE, 500, "z9hG4bK-2", 3, "at", "bt", "Retry-After: 3\r\n"),
            rsp(5_000, BOB, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
        ],
    )
    .is_empty());
}

/// §17.2.1: a Timer-A retransmission of an ALREADY-ANSWERED re-INVITE
/// crosses its own 491 on the wire. It draws the recorded final again — it
/// is not a new transaction and must not re-enter the in-progress window,
/// or the next genuine re-INVITE's compliant 200 reads as a race.
#[test]
fn a_post_final_retransmit_does_not_re_enter_the_window() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            rsp(2_000, BOB, ALICE, 491, "z9hG4bK-1", 2, "at", "bt", ""),
            req(3_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(4_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            rsp(5_000, BOB, ALICE, 200, "z9hG4bK-2", 3, "at", "bt", ""),
        ],
    );
    assert!(f.is_empty(), "{f:?}");
}

/// A retransmission of an INVITE still in flight is the same act again,
/// never a second transaction to serialise against.
#[test]
fn an_in_flight_retransmit_is_no_race() {
    assert!(eval(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
        ],
    )
    .is_empty());
}

/// The race is read on the UNORDERED dialog: a callee-initiated re-INVITE
/// crossing the caller's is exactly the §14.2 collision, tags reversed and
/// all.
#[test]
fn crossing_re_invites_collide_at_the_uas_whichever_way_they_point() {
    let f = hits(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-a", 2, "at", "bt"),
            // BOB is the taker of the first and the SENDER of the reversed
            // one; only what it TOOK is judged here.
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-b", 2, "bt", "at"),
            rsp(3_000, BOB, ALICE, 200, "z9hG4bK-b", 2, "bt", "at", ""),
        ],
    );
    assert_eq!(f.len(), 1, "the reversed re-INVITE races the first: {f:?}");
}

/// An INVITE with only one tag creates a dialog rather than riding one:
/// §14.2 has nothing to serialise it against.
#[test]
fn an_initial_invite_is_no_race() {
    assert!(eval(
        &ConcurrentReInvite500Or491,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 1, "at", ""),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 2, "at", ""),
        ],
    )
    .is_empty());
}

/// A race the observation stopped on top of settles nothing: silence
/// inside the window is truncation, not a missing answer.
#[test]
fn an_unanswered_race_inside_the_window_is_undecidable() {
    let msgs = [
        req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
        req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
    ];
    let open = eval_open(&ConcurrentReInvite500Or491, &msgs);
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(!open[0].decided(), "{:?}", open[0].decision);
    // Closed, the window collapses and the absence decides.
    assert_eq!(hits(&ConcurrentReInvite500Or491, &msgs).len(), 1);
}

// ---- no-re-invite-while-invite-in-progress ---------------------------

/// The clean shape: the prior INVITE reached Confirmed — 2xx AND the UAC's
/// ACK — before the next one went out.
#[test]
fn a_re_invite_after_the_ack_is_compliant() {
    let f = eval(
        &NoReInviteWhileInviteInProgress,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 1, "at", ""),
            rsp(2_000, BOB, ALICE, 200, "z9hG4bK-1", 1, "at", "bt", ""),
            req(3_000, ALICE, BOB, "ACK", "z9hG4bK-k", 1, "at", "bt"),
            req(4_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 2, "at", "bt"),
        ],
    );
    assert_eq!(f.len(), 1, "one occasion, the re-INVITE: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, ALICE, "the UAC that sent it is charged");
}

/// RFC 6026's *Accepted* interval: the 2xx has landed but the ACK has not
/// gone out, so the prior transaction is still outstanding and the
/// overtaking re-INVITE collides with it — on the INITIAL INVITE too, which
/// re-keys onto the dialog its own 2xx names.
#[test]
fn a_re_invite_overtaking_the_ack_is_violated() {
    for (label, first) in [
        ("re-INVITE", req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt")),
        ("initial INVITE", req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 1, "at", "")),
    ] {
        let cseq = first.cseq;
        let f = hits(
            &NoReInviteWhileInviteInProgress,
            &[
                first,
                rsp(2_000, BOB, ALICE, 200, "z9hG4bK-1", cseq, "at", "bt", ""),
                req(3_000, ALICE, BOB, "INVITE", "z9hG4bK-2", cseq + 1, "at", "bt"),
            ],
        );
        assert_eq!(f.len(), 1, "{label}: {f:?}");
        let Decision::Violated(Evidence::OverlappingReInvite {
            prior_accepted, prior_branch, ..
        }) = &f[0].decision
        else {
            panic!("{label}: {:?}", f[0].decision)
        };
        assert!(prior_accepted, "{label}: the prior 2xx is unACKed — Accepted");
        assert_eq!(prior_branch.as_str(), "z9hG4bK-1");
    }
}

/// A non-2xx final ends the transaction at the final itself: its ACK is the
/// transaction layer's and may never surface as a TU send, so the retry
/// that follows is clean.
#[test]
fn a_re_invite_after_a_non_2xx_final_is_compliant() {
    assert!(hits(
        &NoReInviteWhileInviteInProgress,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            rsp(2_000, BOB, ALICE, 491, "z9hG4bK-1", 2, "at", "bt", ""),
            req(3_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
        ],
    )
    .is_empty());
}

/// Nothing answered the first at all: the second overtakes a transaction
/// still in progress.
#[test]
fn a_re_invite_over_an_unanswered_one_is_violated() {
    let f = hits(
        &NoReInviteWhileInviteInProgress,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::OverlappingReInvite { prior_accepted, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert!(!prior_accepted, "no final arrived — in progress, not Accepted");
}

/// Crossing re-INVITEs relayed by ONE forwarding face are two requesters
/// with one INVITE each — legal glare, not one UAC overlapping. The
/// ORDERED key is what keeps them apart.
#[test]
fn relayed_crossing_glare_is_not_one_uac_overlapping() {
    const LB: &str = "10.0.0.9:5060";
    let f = hits(
        &NoReInviteWhileInviteInProgress,
        &[
            req(1_000, LB, BOB, "INVITE", "z9hG4bK-a1", 2, "at", "bt"),
            req(2_000, LB, ALICE, "INVITE", "z9hG4bK-b1", 2, "bt", "at"),
            rsp(3_000, BOB, LB, 491, "z9hG4bK-a1", 2, "at", "bt", ""),
            rsp(4_000, ALICE, LB, 491, "z9hG4bK-b1", 2, "bt", "at", ""),
        ],
    );
    assert!(f.is_empty(), "{f:?}");
}

/// Ordered keying must not blind the rule: a genuinely same-direction
/// overlap still fires exactly once with the peer's reversed re-INVITE
/// interleaved.
#[test]
fn a_same_direction_overlap_fires_amid_reversed_traffic() {
    const LB: &str = "10.0.0.9:5060";
    let f = hits(
        &NoReInviteWhileInviteInProgress,
        &[
            req(1_000, LB, ALICE, "INVITE", "z9hG4bK-b1", 2, "bt", "at"),
            req(2_000, LB, BOB, "INVITE", "z9hG4bK-a1", 2, "at", "bt"),
            req(3_000, LB, BOB, "INVITE", "z9hG4bK-a2", 3, "at", "bt"),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::OverlappingReInvite { branch, prior_branch, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((branch.as_str(), prior_branch.as_str()), ("z9hG4bK-a2", "z9hG4bK-a1"));
}

/// A retransmission reuses its own branch: the same request again, never a
/// second one to overlap with.
#[test]
fn a_retransmitted_re_invite_does_not_overlap_itself() {
    assert!(hits(
        &NoReInviteWhileInviteInProgress,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
        ],
    )
    .is_empty());
}

// ---- spiral copies ---------------------------------------------------

/// A proxy that a Record-Routed route set crosses twice (§16.3).
const P: &str = "10.0.0.9:5060";
/// The element the proxy routes the re-INVITE through before it comes back.
const X: &str = "10.0.0.7:5060";

/// `req` carrying `vias`, topmost first, as `branch@host` pairs: the top
/// one names the request's own branch.
fn via_req(at_us: u64, src: &str, dst: &str, cseq: u32, vias: &[(&str, &str)]) -> Msg {
    let mut m = req(at_us, src, dst, "INVITE", vias[0].0, cseq, "at", "bt");
    let rows: String = vias
        .iter()
        .map(|(branch, host)| format!("Via: SIP/2.0/UDP {host};branch={branch}\r\n"))
        .collect();
    m.head = Some(format!("{rows}Content-Length: 0\r\n\r\n").into_bytes());
    m
}

/// alice's re-INVITE crosses P, X and P again on its way to bob: P takes it
/// twice and sends it twice, each copy under alice's bottom Via.
fn spiral(bottom_of_the_copy: &str) -> Vec<Msg> {
    let alice = ("z9hG4bK-a", "10.0.0.1:5060");
    let first = ("z9hG4bK-p1", P);
    let at_x = ("z9hG4bK-x", X);
    let second = ("z9hG4bK-p2", P);
    let back = (bottom_of_the_copy, "10.0.0.1:5060");
    vec![
        via_req(1_000, ALICE, P, 2, &[alice]),
        via_req(2_000, P, X, 2, &[first, alice]),
        via_req(3_000, X, P, 2, &[at_x, first, back]),
        via_req(4_000, P, BOB, 2, &[second, at_x, first, back]),
        rsp(5_000, BOB, P, 200, "z9hG4bK-p2", 2, "at", "bt", ""),
        rsp(6_000, P, X, 200, "z9hG4bK-x", 2, "at", "bt", ""),
        rsp(7_000, X, P, 200, "z9hG4bK-p1", 2, "at", "bt", ""),
        rsp(8_000, P, ALICE, 200, "z9hG4bK-a", 2, "at", "bt", ""),
    ]
}

/// The copy P takes back from X keeps alice's bottom Via: the re-INVITE in
/// flight at P again, not one racing it.
#[test]
fn a_spiral_copy_taken_again_is_no_race() {
    let f = eval(&ConcurrentReInvite500Or491, &spiral("z9hG4bK-a"));
    assert!(f.is_empty(), "no occasion: {f:?}");
}

/// The copy P sends on to bob is the re-INVITE it already sent to X.
#[test]
fn a_spiral_copy_sent_again_is_no_overlap() {
    let f = hits(&NoReInviteWhileInviteInProgress, &spiral("z9hG4bK-a"));
    assert!(f.is_empty(), "{f:?}");
}

/// Under another bottom branch the INVITE P takes is another request,
/// whatever its path: the race and the overlap stand.
#[test]
fn another_bottom_branch_is_another_invite() {
    let msgs = spiral("z9hG4bK-a2");
    assert_eq!(hits(&ConcurrentReInvite500Or491, &msgs).len(), 1);
    assert_eq!(hits(&NoReInviteWhileInviteInProgress, &msgs).len(), 1);
}

/// A UA that reuses its bottom branch for a second re-INVITE, a higher
/// CSeq, routed to P by another path, sends another request: the race and
/// the overlap stand.
#[test]
fn a_reused_bottom_branch_under_a_higher_cseq_is_another_invite() {
    let alice = ("z9hG4bK-a", "10.0.0.1:5060");
    let msgs = vec![
        via_req(1_000, ALICE, P, 2, &[alice]),
        via_req(2_000, P, X, 2, &[("z9hG4bK-p1", P), alice]),
        via_req(3_000, X, P, 3, &[("z9hG4bK-x", X), alice]),
        via_req(4_000, P, BOB, 3, &[("z9hG4bK-p2", P), ("z9hG4bK-x", X), alice]),
    ];
    assert_eq!(hits(&ConcurrentReInvite500Or491, &msgs).len(), 1);
    assert_eq!(hits(&NoReInviteWhileInviteInProgress, &msgs).len(), 1);
}

// ---- failed-reinvite-tears-down-dialog -------------------------------

/// The final arrived: whatever it said, §14.1's prior state was reported
/// on rather than silently dropped.
#[test]
fn a_re_invite_that_drew_a_final_is_compliant() {
    let f = eval(
        &FailedReinviteTearsDownDialog,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt"),
            rsp(2_000, BOB, ALICE, 183, "z9hG4bK-re", 2, "at", "bt", ""),
            rsp(3_000, BOB, ALICE, 488, "z9hG4bK-re", 2, "at", "bt", ""),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The defect: a provisional and then silence on a dialog nothing ended —
/// the prior session was destroyed by a failed re-INVITE.
#[test]
fn a_provisional_then_silence_is_violated() {
    let f = hits(
        &FailedReinviteTearsDownDialog,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt"),
            rsp(2_000, BOB, ALICE, 183, "z9hG4bK-re", 2, "at", "bt", ""),
        ],
    );
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, ALICE, "the re-INVITE's sender is charged");
    let Decision::Violated(Evidence::AbandonedReInvite { branch, provisional_status, .. }) =
        &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((branch.as_str(), *provisional_status), ("z9hG4bK-re", 183));
}

/// "Absent another cause": a BYE on the dialog is an independent teardown
/// that legitimately abandons the in-flight re-INVITE.
#[test]
fn a_byed_dialog_discharges_the_abandoned_re_invite() {
    assert!(hits(
        &FailedReinviteTearsDownDialog,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt"),
            rsp(2_000, BOB, ALICE, 100, "z9hG4bK-re", 2, "at", "bt", ""),
            req(3_000, ALICE, BOB, "BYE", "z9hG4bK-bye", 3, "at", "bt"),
        ],
    )
    .is_empty());
}

/// An INITIAL INVITE abandoned after a 100 is the transaction layer's
/// timeout, not a §14.1 prior-state breach: there is no prior state.
#[test]
fn an_abandoned_initial_invite_is_no_occasion() {
    assert!(eval(
        &FailedReinviteTearsDownDialog,
        &[
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-init", 1, "at", ""),
            rsp(2_000, BOB, ALICE, 100, "z9hG4bK-init", 1, "at", "", ""),
        ],
    )
    .is_empty());
}

/// A re-INVITE that drew NOTHING was never demonstrably live: §17.1.1.2's
/// timeout owns it, so it is not an occasion here at all.
#[test]
fn a_re_invite_that_drew_no_provisional_is_no_occasion() {
    assert!(eval(
        &FailedReinviteTearsDownDialog,
        &[req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt")],
    )
    .is_empty());
}

/// An observation that stopped inside the final-response window shows
/// truncation, not abandonment.
#[test]
fn silence_inside_the_final_window_is_undecidable() {
    let msgs = [
        req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt"),
        rsp(2_000, BOB, ALICE, 183, "z9hG4bK-re", 2, "at", "bt", ""),
    ];
    let open = eval_open(&FailedReinviteTearsDownDialog, &msgs);
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(!open[0].decided(), "{:?}", open[0].decision);

    // An open observation that DID outlive the window decides.
    let mut long = msgs.to_vec();
    long.push(req(
        2_000 + REINVITE_FINAL_WINDOW_US,
        ALICE,
        BOB,
        "OPTIONS",
        "z9hG4bK-o",
        9,
        "at",
        "bt",
    ));
    let decided = eval_open(&FailedReinviteTearsDownDialog, &long);
    assert!(decided.iter().any(Finding::violated), "{decided:?}");
}
