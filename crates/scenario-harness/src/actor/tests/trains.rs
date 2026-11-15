//! A held callee's provisional trains (RFC 3262 §3): several provisionals per
//! early dialog, interleaved across forks, reliable ones numbered per dialog
//! and PRACKed across one another, and a final racing the PRACKs it may.

use std::time::Duration;

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::actor::*;
use crate::realcall::{CallCtx, InvitePlan};
use crate::{Agent, Harness, ANSWER_SDP, OFFER_SDP};

fn caller(alice: &Agent, bob: &Agent, bye: bool) -> ActorSpec {
    let plan = InvitePlan {
        via: bob.addr(),
        from: None,
        to: None,
        ruri: None,
        call_id: None,
        headers: vec![("Supported".to_string(), "100rel".to_string())],
        rewrite: Default::default(),
    };
    let mut goals =
        vec![Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: Some(plan) })];
    if bye {
        goals.push(Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye));
    }
    ActorSpec {
        role: "alice",
        agent: alice.clone(),
        disposition: Disposition::Caller,
        media: MediaState::offer(OFFER_SDP),
        goals,
        invite_targets: vec![("bob", bob.clone())],
        via: None,
        feed: CtxFeed::default(),
        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

fn held(bob: &Agent, goals: Vec<Goal>) -> ActorSpec {
    ActorSpec {
        role: "bob",
        agent: bob.clone(),
        disposition: Disposition::Held,
        media: MediaState::answer(ANSWER_SDP),
        goals,
        invite_targets: vec![],
        via: None,
        feed: CtxFeed::default(),
        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

fn plan(actors: Vec<ActorSpec>) -> CallPlan {
    CallPlan {
        actors,
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

/// One provisional on `fork`, its PRACK answered `hold_ms` after arrival.
fn prov(fork: &'static str, status: u16, sdp: bool, reliable: bool, hold_ms: u64) -> Goal {
    Goal::new(
        Barrier::None,
        GoalStep::Provisional(Provisional {
            fork: Some(fork),
            status,
            sdp,
            reliable,
            prack_answer_after: Duration::from_millis(hold_ms),
        }),
    )
}

fn fin(status: u16, winner: Option<ForkWinner>) -> Goal {
    Goal::new(Barrier::None, GoalStep::Final(HeldFinal { status, winner }))
}

/// What `agent` took, in arrival order: `(status or method, CSeq method, To tag, RSeq)`.
fn taken(agent: &Agent) -> Vec<(String, String, String, Option<u32>)> {
    agent
        .wire_view()
        .iter()
        .filter_map(|e| match CustomParser::new().parse(&e.raw).ok()? {
            SipMessage::Response(r) => Some((
                r.status().to_string(),
                r.cseq().method().as_str().to_string(),
                r.to().tag().unwrap_or_default().to_string(),
                r.header::<sip_message::header::RSeq>().and_then(Result::ok).map(|v| v.value()),
            )),
            SipMessage::Request(r) => Some((
                r.method().as_str().to_string(),
                r.cseq().method().as_str().to_string(),
                r.to().tag().unwrap_or_default().to_string(),
                None,
            )),
        })
        .collect()
}

/// The most PRACKs the caller had unanswered at once: she PRACKs each reliable
/// provisional on arrival, and each PRACK's 200 closes one.
fn most_pracks_in_flight(alice: &Agent) -> usize {
    let (mut open, mut most) = (0usize, 0usize);
    for (what, method, _, rseq) in taken(alice) {
        if rseq.is_some() {
            open += 1;
        } else if what == "200" && method == "PRACK" {
            open = open.saturating_sub(1);
        }
        most = most.max(open);
    }
    most
}

/// Two forks interleave 180s and 183s, reliable and not; each fork's reliable
/// provisionals wait for the PRACK of the last, number on from their own
/// first RSeq, and their PRACKs' 200s come late, so the PRACKs of both forks
/// are in flight at once. The winner answers once its offer is PRACKed.
#[tokio::test(start_paused = true)]
async fn interleaved_trains_hold_pracks_of_two_forks_in_flight() {
    let h = Harness::new("actor-trains-interleaved");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let train = vec![
        prov("f1", 180, false, false, 0),
        prov("f1", 183, false, true, 300),
        prov("f2", 183, true, false, 0),
        prov("f2", 180, false, true, 300),
        prov("f1", 183, false, true, 200),
        prov("f2", 183, true, true, 0),
        prov("f1", 180, true, false, 0),
        fin(200, Some(ForkWinner::Tag("f2"))),
    ];
    let call = plan(vec![caller(&alice, &bob, true), held(&bob, train)]);
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the train call settles, got {verdict:?}");

    let rseqs = |tag: &str| -> Vec<u32> {
        let mut v: Vec<u32> = taken(&alice)
            .into_iter()
            .filter(|(_, m, t, _)| m == "INVITE" && t == tag)
            .filter_map(|(_, _, _, r)| r)
            .collect();
        v.dedup();
        v
    };
    for tag in ["f1", "f2"] {
        let r = rseqs(tag);
        assert_eq!(r.len(), 2, "{tag} sent two reliable provisionals: {r:?}");
        assert_eq!(r[1], r[0] + 1, "{tag} numbers on from its own first RSeq");
    }
    assert!(most_pracks_in_flight(&alice) >= 2, "{:?}", taken(&alice));
    h.finish().await;
}

/// A non-2xx final need not wait for PRACKs (RFC 3262 §3): it leaves right
/// behind two forks' reliable provisionals, the caller's PRACKs cross it, and
/// the callee still answers them 200.
#[tokio::test(start_paused = true)]
async fn a_reject_crosses_the_pracks_of_two_forks() {
    let h = Harness::new("actor-trains-reject-race");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let train =
        vec![prov("f1", 183, true, true, 0), prov("f2", 183, true, true, 100), fin(486, None)];
    let call = plan(vec![caller(&alice, &bob, false), held(&bob, train)]);
    let (ctx, obs) = (CallCtx::new(), ObservedState::new());
    let accepted = ("alice", Expect::one(Branch::rejected(486)));
    let run = run_call_accepting(call, accepted, obs, &ctx, Duration::from_secs(5), None).await;
    assert!(run.is_ok(), "the rejected call settles, got {run:?}");
    let seen = taken(&alice);
    let reject = seen.iter().position(|(s, ..)| s == "486").expect("the 486 arrives");
    let prack_200 = seen.iter().rposition(|(s, m, ..)| s == "200" && m == "PRACK");
    assert!(prack_200.is_some_and(|p| p > reject), "a PRACK is answered after the final: {seen:?}");
    h.finish().await;
}

/// A 2xx waits only for its own dialog's offer (RFC 3262 §3): with the
/// winner's later reliable provisional carrying no session description and
/// another fork's offer unPRACKed, it leaves at once and those PRACKs cross it.
#[tokio::test(start_paused = true)]
async fn a_2xx_crosses_pracks_its_dialog_s_offer_does_not_await() {
    let h = Harness::new("actor-trains-2xx-race");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let train = vec![
        prov("f1", 183, true, true, 0),
        prov("f1", 180, false, true, 200),
        prov("f2", 183, true, true, 200),
        fin(200, Some(ForkWinner::Tag("f1"))),
    ];
    let call = plan(vec![caller(&alice, &bob, true), held(&bob, train)]);
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the racing 2xx call settles, got {verdict:?}");
    let seen = taken(&alice);
    let answer = seen.iter().position(|(s, m, ..)| s == "200" && m == "INVITE").expect("a 2xx");
    let before = &seen[..answer];
    let reliable = before.iter().filter(|(.., rseq)| rseq.is_some()).count();
    let answered = before.iter().filter(|(s, m, ..)| s == "200" && m == "PRACK").count();
    assert_eq!(reliable - answered, 2, "two PRACKs cross the 2xx: {seen:?}");
    h.finish().await;
}
