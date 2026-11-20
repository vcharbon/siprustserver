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

/// What `bob` met of the PRACKs, by CSeq number: when each copy of the PRACK
/// reached him, the RSeq it acknowledged, and when each of his 200s to it left.
#[derive(Debug, Default)]
struct PrackTimes {
    rseq: Option<u32>,
    arrived_ms: Vec<u64>,
    answered_ms: Vec<u64>,
}

fn prack_times(h: &Harness, bob: &Agent) -> std::collections::BTreeMap<u32, PrackTimes> {
    let mut by_cseq = std::collections::BTreeMap::<u32, PrackTimes>::new();
    for e in h.wire_entries() {
        match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Request(r)) if e.to == bob.addr() && r.method() == "PRACK" => {
                let t = by_cseq.entry(r.cseq().seq()).or_default();
                t.rseq =
                    r.header::<sip_message::header::RAck>().and_then(Result::ok).map(|a| a.rseq());
                t.arrived_ms.extend(e.received_ms);
            }
            Ok(SipMessage::Response(r))
                if e.from == bob.addr() && r.status() == 200 && r.cseq().method() == "PRACK" =>
            {
                by_cseq.entry(r.cseq().seq()).or_default().answered_ms.push(e.sent_ms);
            }
            _ => {}
        }
    }
    by_cseq
}

/// The RSeqs of bob's reliable provisionals, in the order he sent them.
fn sent_rseqs(h: &Harness, bob: &Agent) -> Vec<u32> {
    let mut v: Vec<u32> = h
        .wire_entries()
        .iter()
        .filter(|e| e.from == bob.addr())
        .filter_map(|e| match CustomParser::new().parse(&e.raw).ok()? {
            SipMessage::Response(r) => {
                r.header::<sip_message::header::RSeq>().and_then(Result::ok).map(|v| v.value())
            }
            SipMessage::Request(_) => None,
        })
        .collect();
    v.dedup();
    v
}

/// Each PRACK's 200 leaves its provisional's hold after the PRACK arrived,
/// and once (RFC 3262 §3: the PRACK acknowledges on arrival, its 200 closes
/// only its own transaction). The 700 ms hold outlasts T1, so the caller's
/// Timer E sends the PRACK again (RFC 3261 §17.1.2.2): the held transaction
/// absorbs the copy and still answers once.
#[tokio::test(start_paused = true)]
async fn a_held_prack_200_leaves_its_hold_after_the_prack_and_once() {
    let h = Harness::new("actor-trains-prack-hold");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let holds = [700u64, 150];
    let train = vec![
        prov("f1", 183, true, true, holds[0]),
        prov("f1", 180, false, true, holds[1]),
        fin(200, Some(ForkWinner::Tag("f1"))),
    ];
    let call = plan(vec![caller(&alice, &bob, true), held(&bob, train)]);
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the held-PRACK call settles, got {verdict:?}");

    let rseqs = sent_rseqs(&h, &bob);
    assert_eq!(rseqs.len(), holds.len(), "two reliable provisionals: {rseqs:?}");
    let times = prack_times(&h, &bob);
    assert_eq!(times.len(), holds.len(), "one PRACK per reliable provisional: {times:?}");
    for t in times.values() {
        let at = rseqs.iter().position(|r| Some(*r) == t.rseq).expect("a PRACK names an RSeq");
        let hold = holds[at];
        let arrived = *t.arrived_ms.first().expect("the PRACK reached bob");
        assert_eq!(t.answered_ms.len(), 1, "one 200 per PRACK: {t:?}");
        let answered = t.answered_ms[0];
        assert!(
            (arrived + hold..arrived + hold + 5).contains(&answered),
            "the 200 leaves {hold} ms after its PRACK: {t:?}"
        );
        if hold > 500 {
            assert!(t.arrived_ms.len() >= 2, "a hold past T1 sees the PRACK again: {t:?}");
        }
    }
    h.finish().await;
}

/// Bob answers, BYEs alice 300 ms later, and holds his PRACK's 200 for 2 s,
/// which keeps the call open past alice's own hang-up goal 1 s after the
/// answer. Bob's BYE already ended her dialog (RFC 3261 §15.1.2): her goal
/// sends nothing, the wire carries his BYE alone, and the call settles.
#[tokio::test(start_paused = true)]
async fn a_bye_goal_the_peer_s_bye_met_sends_nothing() {
    let h = Harness::new("actor-trains-peer-bye-first");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let both = Barrier::AllConfirmed(&["alice", "bob"]);
    let mut alice_spec = caller(&alice, &bob, false);
    alice_spec.goals.push(Goal::new(both.clone(), GoalStep::Bye).after(Duration::from_secs(1)));
    let train = vec![
        prov("f1", 183, true, true, 2000),
        fin(200, Some(ForkWinner::Tag("f1"))),
        Goal::new(both, GoalStep::Bye).after(Duration::from_millis(300)),
    ];
    let call = plan(vec![alice_spec, held(&bob, train)]);
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the call settles on bob's BYE, got {verdict:?}");
    let byes: Vec<String> = h
        .wire_entries()
        .iter()
        .filter(|e| e.raw.starts_with(b"BYE "))
        .map(|e| format!("{} -> {}", e.from, e.to))
        .collect();
    assert_eq!(byes, vec![format!("{} -> {}", bob.addr(), alice.addr())], "bob's BYE alone");
    h.finish().await;
}
