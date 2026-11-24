//! The fuzzer over the shipped basic call, alice calling bob directly: a seed
//! fixes the whole draw, the header budget bounds every INVITE, only legal
//! messages are drawn unless asked, and a fuzzed call runs to a legal ending
//! the RFC audit finds nothing in.

use std::sync::Arc;

use callshapes::shapes::{basic_call, default_binder};
use scenario_harness::actor::{
    run_built_actor_call, ActorCall, ActorScenario, Disposition, GoalStep, ScenarioVerdict,
};
use scenario_harness::realcall::{CallCtx, CallEnv, InvitePlan};
use scenario_harness::{Agent, Harness};

use crate::payload::ILLEGAL_BODY;
use crate::*;

fn env<'a>(alice: &'a Agent, bob: &'a Agent, token: &str) -> CallEnv<'a> {
    CallEnv::for_functional(alice, bob, None, bob.addr(), "X-Test", token)
}

fn basic(env: &CallEnv<'_>) -> ActorCall {
    basic_call(default_binder()).build(env).expect("the basic call builds")
}

fn payloads_always() -> FuzzConfig {
    FuzzConfig { payload_pct: 100, jitter_pct: 100, ..FuzzConfig::default() }
}

/// The plan of the call's INVITE.
fn invite_plan(call: &ActorCall) -> InvitePlan {
    call.actors
        .iter()
        .flat_map(|a| a.goals.iter())
        .find_map(|g| match &g.step {
            GoalStep::Invite { plan: Some(plan), .. } => Some(plan.clone()),
            _ => None,
        })
        .expect("the call has an INVITE")
}

/// The bytes the display name `d` adds to a From as sip-message writes it.
fn written(d: &str) -> usize {
    use sip_message::header::{From, NameAddr, Uri};
    let from = |addr: NameAddr| From::new(addr).to_string().len();
    let addr = NameAddr::new(Uri::sip("example.com"));
    from(addr.clone().with_display(d)) - from(addr)
}

/// The headers of the call's INVITE.
fn invite_headers(call: &ActorCall) -> Vec<(String, String)> {
    invite_plan(call).headers
}

/// Every goal of the call, its delay and step, in order.
fn goals(call: &ActorCall) -> Vec<String> {
    call.actors
        .iter()
        .flat_map(|a| a.goals.iter().map(move |g| format!("{} {:?} {:?}", a.role, g.delay, g.step)))
        .collect()
}

/// The content types of the INFOs the call sends.
fn info_types(call: &ActorCall) -> Vec<String> {
    call.actors
        .iter()
        .flat_map(|a| a.goals.iter())
        .filter_map(|g| match &g.step {
            GoalStep::InDialog { content_type, .. } => content_type.clone(),
            _ => None,
        })
        .collect()
}

async fn agents(name: &str) -> (Harness, Agent, Agent) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    (h, alice, bob)
}

#[tokio::test(start_paused = true)]
async fn a_seed_fixes_the_whole_perturbation() {
    let (h, alice, bob) = agents("callfuzz-determinism").await;
    let env = env(&alice, &bob, "t");
    let config = payloads_always();
    let fuzzed = |seed| {
        let mut call = basic(&env);
        let mut notes = Vec::new();
        perturb(&mut Rng::new(seed), &mut call, &config, &mut notes);
        (notes, invite_headers(&call), goals(&call))
    };
    for seed in 0..64 {
        assert_eq!(fuzzed(seed), fuzzed(seed), "seed {seed}");
    }
    let distinct: std::collections::BTreeSet<_> = (0..64).map(|s| fuzzed(s).0).collect();
    assert!(distinct.len() > 32, "seeds draw different calls: {distinct:?}");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_header_budget_bounds_every_invite() {
    let (h, alice, bob) = agents("callfuzz-budget").await;
    let env = env(&alice, &bob, "t");
    let budget = 700;
    let bytes = |header_budget| {
        (0..400)
            .map(|seed| {
                let mut call = basic(&env);
                let config = FuzzConfig { header_budget, ..payloads_always() };
                perturb(&mut Rng::new(seed), &mut call, &config, &mut Vec::new());
                let plan = invite_plan(&call);
                let p = &plan.presentation;
                plan.headers.iter().map(|(n, v)| n.len() + v.len() + 4).sum::<usize>()
                    + [&p.from_display, &p.to_display]
                        .into_iter()
                        .flatten()
                        .map(|d| written(d))
                        .sum::<usize>()
            })
            .collect::<Vec<_>>()
    };
    let bounded = bytes(budget);
    assert!(bounded.iter().all(|&b| b <= budget), "{bounded:?}");
    assert!(bounded.iter().any(|&b| b > budget / 2), "the budget is used: {bounded:?}");
    assert!(bytes(usize::MAX).iter().any(|&b| b > budget), "the draws pass it unbounded");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn only_legal_bodies_are_drawn_unless_asked() {
    let (h, alice, bob) = agents("callfuzz-legal").await;
    let env = env(&alice, &bob, "t");
    let types = |illegal| {
        (0..600)
            .flat_map(|seed| {
                let mut call = basic(&env);
                let config = FuzzConfig { illegal, ..payloads_always() };
                perturb(&mut Rng::new(seed), &mut call, &config, &mut Vec::new());
                info_types(&call)
            })
            .collect::<Vec<_>>()
    };
    let illegal_boundary = |t: &String| t.contains("boundary=\"b😀\"");
    let legal = types(false);
    assert!(!legal.is_empty(), "INFOs are drawn");
    assert!(!legal.iter().any(illegal_boundary), "{ILLEGAL_BODY} drawn unasked");
    assert!(types(true).iter().any(illegal_boundary), "{ILLEGAL_BODY} drawn when asked");
    h.finish().await;
}

/// A quiet party sends no odd INFO: the basic call's caller, the one party
/// with a BYE to send it ahead of, sends some unless quiet.
#[tokio::test(start_paused = true)]
async fn a_quiet_party_sends_no_odd_info() {
    let (h, alice, bob) = agents("callfuzz-quiet").await;
    let env = env(&alice, &bob, "t");
    let senders = |quiet| {
        let config = FuzzConfig { quiet, ..payloads_always() };
        (0..200)
            .flat_map(|seed| {
                let mut call = basic(&env);
                perturb(&mut Rng::new(seed), &mut call, &config, &mut Vec::new());
                call.actors
                    .iter()
                    .filter(|a| a.goals.iter().any(|g| matches!(g.step, GoalStep::InDialog { .. })))
                    .map(|a| a.role)
                    .collect::<Vec<_>>()
            })
            .collect::<std::collections::BTreeSet<&str>>()
    };
    assert_eq!(senders(&[]).into_iter().collect::<Vec<_>>(), ["alice"]);
    assert!(senders(&["alice"]).is_empty());
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_held_callee_rings_reliably_only_where_100rel_is_offered() {
    let (h, alice, bob) = agents("callfuzz-held").await;
    let env = env(&alice, &bob, "t");
    let mut held = 0;
    for (seed, reliable_ok) in (0..200).map(|s| (s, s % 2 == 0)) {
        let mut call = basic(&env);
        let mut rng = Rng::new(seed);
        let reliable = hold_callees(&mut rng, &mut call, &["bob"], reliable_ok, &mut Vec::new());
        let bob = call.actors.iter().find(|a| a.role == "bob").expect("bob");
        if !matches!(bob.disposition, Disposition::Held) {
            assert!(!reliable);
            continue;
        }
        held += 1;
        let steps: Vec<&GoalStep> = bob.goals.iter().map(|g| &g.step).collect();
        let [GoalStep::Ring { reliable: rings_reliably, .. }, GoalStep::Answer { .. }] = steps[..]
        else {
            panic!("a held callee rings then answers: {steps:?}");
        };
        assert_eq!(*rings_reliably, reliable);
        assert!(reliable_ok || !reliable, "seed {seed}: reliable without 100rel");
    }
    assert!(held > 60, "callees are held: {held}");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_train_replaces_the_ring_and_a_reliable_one_offers_100rel() {
    let (h, alice, bob) = agents("callfuzz-train").await;
    let env = env(&alice, &bob, "t");
    let mut reliable_trains = 0;
    for seed in 0..200 {
        let mut call = basic(&env);
        hold_callees(&mut Rng::new(seed), &mut call, &["bob"], false, &mut Vec::new());
        if call.actors.iter().all(|a| !matches!(a.disposition, Disposition::Held)) {
            continue;
        }
        draw_trains(seed, &mut call, 30, true, &mut Vec::new());
        let bob = call.actors.iter().find(|a| a.role == "bob").expect("bob");
        assert!(!bob.goals.iter().any(|g| matches!(g.step, GoalStep::Ring { .. })));
        let reliable =
            bob.goals.iter().any(|g| matches!(g.step, GoalStep::Provisional(p) if p.reliable));
        let offered = invite_headers(&call)
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("Supported") && v.contains("100rel"));
        assert_eq!(offered, reliable, "seed {seed}");
        reliable_trains += usize::from(reliable);
    }
    assert!(reliable_trains > 0);
    h.finish().await;
}

fn wrap(config: FuzzConfig) -> FuzzWrap {
    FuzzWrap {
        id: "fz_basic_call",
        inner: Arc::new(basic_call(default_binder())),
        policy: Policy {
            cancel_endings: Some(CANCEL_ENDINGS),
            callee_reliable: true,
            callee_roles: &["bob"],
        },
        config,
    }
}

#[tokio::test(start_paused = true)]
async fn a_wrapped_call_is_stamped_with_its_seed_and_recorded() {
    let (h, alice, bob) = agents("callfuzz-stamp").await;
    let config = FuzzConfig { run_seed: 77, ..FuzzConfig::default() };
    let call = wrap(config).build(&env(&alice, &bob, "tok-1")).expect("built");
    let call_id = call
        .actors
        .iter()
        .flat_map(|a| a.goals.iter())
        .find_map(|g| match &g.step {
            GoalStep::Invite { plan: Some(plan), .. } => plan.call_id.clone(),
            _ => None,
        })
        .expect("the INVITE states a Call-ID");
    let seed = config.seed_of("tok-1");
    assert_eq!(seed_of_call_id(&call_id), Some(seed));
    let drawn = drawn().lock().expect("registry").get("tok-1").cloned().expect("recorded");
    assert_eq!(drawn.seed, seed);
    assert_eq!(drawn.call_id, call_id);
    assert!(drawn.desc.starts_with("Wrap | basic_call"), "{}", drawn.desc);
    h.finish().await;
}

/// The names of the endings `call` accepts, in order.
fn accepted(call: &ActorCall) -> Vec<&'static str> {
    call.expect.branches().map(|b| b.name).collect()
}

/// A wrapped call that drew a CANCEL accepts the CANCEL races; one that drew
/// none keeps the basic call's own ending, its held callee answering.
#[tokio::test(start_paused = true)]
async fn only_a_drawn_cancel_widens_the_accepted_endings() {
    let (h, alice, bob) = agents("callfuzz-endings").await;
    let own = accepted(&basic(&env(&alice, &bob, "t")));
    let races: Vec<&str> = CANCEL_ENDINGS.iter().map(|b| b.name).collect();
    let (mut cancelled, mut kept) = (0, 0);
    for seed in 0..200u64 {
        let token = format!("endings-{seed}");
        let config = FuzzConfig { run_seed: seed, ..FuzzConfig::default() };
        let call = wrap(config).build(&env(&alice, &bob, &token)).expect("built");
        let desc = drawn().lock().expect("registry")[&token].desc.clone();
        if desc.contains("cancel+") {
            assert_eq!(accepted(&call), races, "{desc}");
            cancelled += 1;
        } else {
            assert_eq!(accepted(&call), own, "{desc}");
            kept += 1;
        }
    }
    assert!(cancelled > 20 && kept > 100, "cancelled {cancelled}, kept {kept}");
    h.finish().await;
}

/// Fuzzed basic calls, payloads and trains on, run alice to bob: each ends on
/// an accepted ending and the RFC audit charges nothing.
#[tokio::test(start_paused = true)]
async fn fuzzed_calls_run_to_a_legal_ending() {
    let config = FuzzConfig { train: true, ..payloads_always() };
    for seed in 0..24u64 {
        let name = format!("callfuzz-run-{seed}");
        let (h, alice, bob) = agents(&name).await;
        let env = env(&alice, &bob, &name);
        let call =
            wrap(FuzzConfig { run_seed: seed, ..config }).build(&env).expect("the call builds");
        let verdict = run_built_actor_call(call, &env, &CallCtx::new()).await;
        let (_report, findings) = h.finish_collecting().await;
        assert!(matches!(verdict, ScenarioVerdict::Success { .. }), "seed {seed}: {verdict:?}");
        let gating: Vec<_> = findings.iter().filter(|f| !f.advisory).collect();
        assert!(gating.is_empty(), "seed {seed}: {gating:?}");
    }
}

/// Offer modes and framings drawn on every call, trains on, alice to bob:
/// the delayed offer is answered in a PRACK or the ACK, a framed description
/// is read, and the RFC audit charges nothing.
#[tokio::test(start_paused = true)]
async fn every_offer_mode_runs_to_a_legal_ending() {
    let config = FuzzConfig { train: true, offer_pct: 100, ..payloads_always() };
    let mut delayed = 0;
    for seed in 0..48u64 {
        let name = format!("callfuzz-offer-{seed}");
        let (h, alice, bob) = agents(&name).await;
        let env = env(&alice, &bob, &name);
        let call =
            wrap(FuzzConfig { run_seed: seed, ..config }).build(&env).expect("the call builds");
        delayed +=
            usize::from(call.actors.iter().any(|a| a.role == "alice" && !a.media.makes_offer()));
        let verdict = run_built_actor_call(call, &env, &CallCtx::new()).await;
        let (_report, findings) = h.finish_collecting().await;
        assert!(matches!(verdict, ScenarioVerdict::Success { .. }), "seed {seed}: {verdict:?}");
        let gating: Vec<_> = findings.iter().filter(|f| !f.advisory).collect();
        assert!(gating.is_empty(), "seed {seed}: {gating:?}");
    }
    assert!(delayed > 10, "{delayed} of 48 calls made no offer in the INVITE");
}

/// A call whose party holds an ACK or an answer may leave a renegotiation
/// refused 491 once its retries ran out (RFC 3261 §14.1); one holding nothing
/// may not, and an INFO requiring an unknown extension draws its 420.
#[tokio::test(start_paused = true)]
async fn the_finals_a_draw_accepts_follow_its_holds_and_requirements() {
    let h = Harness::new("callfuzz-refusals");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let mut call = basic(&env(&alice, &bob, "refusals"));
    let none = crate::stamp::refusals(&call);
    assert!(none.is_empty(), "nothing held: {none:?}");
    let caller = call.actors.iter_mut().find(|a| a.role == "alice").expect("alice");
    caller.delayed.push(sip_message::DelayedAutomatic::ack_after(1000));
    let bye = caller.goals.iter().position(|g| matches!(g.step, GoalStep::Bye)).expect("a BYE");
    caller.goals.insert(
        bye,
        scenario_harness::actor::Goal::new(
            scenario_harness::actor::Barrier::None,
            GoalStep::InDialog {
                method: sip_message::generators::InDialogMethod::Info,
                content_type: None,
                body: None,
                headers: vec![("Require".into(), crate::payload::UNKNOWN_EXTENSION.into())],
            },
        ),
    );
    let held = crate::stamp::refusals(&call);
    let owed = [
        (Requester::Either, "INVITE", 491),
        (Requester::Either, "UPDATE", 491),
        (Requester::Caller, "INFO", 420),
    ];
    for owed in owed {
        assert!(held.contains(&owed), "{owed:?} accepted: {held:?}");
    }
    h.finish().await;
}

/// A refusal drawn on the callee answers the caller's request, a hold's 487
/// too; one drawn on the caller answers a callee leg's.
#[tokio::test(start_paused = true)]
async fn a_drawn_refusal_is_owed_to_the_refusing_partys_peer() {
    let (h, alice, bob) = agents("callfuzz-refusal-side").await;
    let mut call = basic(&env(&alice, &bob, "side"));
    let refuse = scenario_harness::actor::answers::Answer {
        hold: std::time::Duration::from_secs(1),
        refuse: Some(488),
    };
    let callee = call.actors.iter_mut().find(|a| a.role == "bob").expect("bob");
    callee.answers.reinvite = refuse;
    let owed = crate::stamp::refusals(&call);
    assert!(owed.contains(&(Requester::Caller, "INVITE", 488)), "{owed:?}");
    assert!(owed.contains(&(Requester::Caller, "INVITE", 487)), "{owed:?}");
    assert!(!owed.contains(&(Requester::Callee, "INVITE", 488)), "{owed:?}");
    let caller = call.actors.iter_mut().find(|a| a.role == "alice").expect("alice");
    caller.answers.update = refuse;
    let owed = crate::stamp::refusals(&call);
    assert!(owed.contains(&(Requester::Callee, "UPDATE", 488)), "{owed:?}");
    h.finish().await;
}

/// An INFO requires an extension of a proxy only on a proxied path, where it
/// is drawn too; a UAS ignores `Proxy-Require`.
#[tokio::test(start_paused = true)]
async fn proxy_require_is_drawn_only_on_a_proxied_path() {
    let (h, alice, bob) = agents("callfuzz-proxy-require").await;
    let env = env(&alice, &bob, "t");
    let proxy_requires = |proxied| {
        (0..200).any(|seed| {
            let mut call = basic(&env);
            let config = FuzzConfig { unknown_require_pct: 100, proxied, ..payloads_always() };
            perturb(&mut Rng::new(seed), &mut call, &config, &mut Vec::new());
            call.actors.iter().flat_map(|a| a.goals.iter()).any(|g| match &g.step {
                GoalStep::InDialog { headers, .. } => {
                    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("Proxy-Require"))
                }
                _ => false,
            })
        })
    };
    assert!(!proxy_requires(false), "no proxy, no Proxy-Require");
    assert!(proxy_requires(true), "a proxied path draws it");
    h.finish().await;
}

mod identity;
