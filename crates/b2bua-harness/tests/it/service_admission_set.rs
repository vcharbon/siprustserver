//! A service replaces the call's admission set and is told the outcome
//! ([`RuleAction::ReplaceAdmissionSet`], ADR-0040).
//!
//! The `admitset` service arms a timer at setup; when it fires it sends the
//! admits of a plan named by the called user part, each a whole set, and
//! records every `limiter-admit-result` it reads: the correlation id, the
//! outcome, and the set the call holds as the rule reads it (the router has
//! applied the admit before any rule reads its event). The limiter is a real
//! store behind the production client, with one witness hold per id, and a
//! fault wrapper where a scenario needs one.
//!
//! Stated here: an admitted set is held and read by the service; a refusal on
//! a cap keeps the set held; two admits of one turn are ordered by their
//! change numbers, so the older landing last is superseded and changes
//! nothing; an answer lost after its admit landed is repaired by the next
//! refresh answer; a result landing on a call already gone releases the key;
//! an admit reaching a limiter restarted empty before the call's refresh
//! re-registers the call's held set and checks only the ids it adds; a call
//! moved onto a set runs uncounted while the limiter does not hold it all:
//! after a refusal, until it is moved onto a held set or ends, and after a
//! lost answer, until the next refresh states the set.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::{CallLimiter, LimiterEntry};
use b2bua::metrics::{LimiterFailure, LimiterOp};
use b2bua_harness::limiter::doubles::{
    answer_nth_admit_late, delay_admits_after, delay_nth_admit, unavailable_on_nth, without_breaker,
};
use b2bua_harness::{settle_until, B2buaSut, LimiterLeak, WitnessRig};
use call_limiter::wire::AdmitEntry;
use call_limiter::{AdmitResult, LimiterConfig};
use scenario_harness::callflow::{hangup, ANSWER_SDP, OFFER_SDP};
use scenario_harness::Harness;

/// What the service read on one result: `(correlation id, outcome, the ids
/// the call holds, the call)`.
type Read = (String, String, Vec<String>, String);

mod admitset {
    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceSeed,
        TimerDelay,
    };
    use b2bua::{define_service, sm_rule, CallEvent};
    use call::{LimiterEntry, TimerType};

    use super::{Read, READS};

    const KICK: TimerType = TimerType::service(ADMITSET, "kick");
    /// The next step of a plan that has one ([`then`]).
    const NEXT: TimerType = TimerType::service(ADMITSET, "next");

    define_service! {
        id: "admitset",
        machine: ADMITSET,
        states: AsState { Working, Awaiting },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(AsState::Working.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(ADMITSET, "kick"),
                    delay: TimerDelay::secs(1),
                    leg_id: None,
                },
            ]))
        },
        rules: [ kick(), next(), result() ],
    }

    fn e(id: &str, limit: i64) -> LimiterEntry {
        LimiterEntry { id: id.into(), limit }
    }

    /// One admit of a plan: its correlation id, its whole set, and whether it
    /// moves the call onto the set.
    pub type Step = (&'static str, Vec<LimiterEntry>, bool);

    /// The plan the called user part names: each admit, sent in this order in
    /// one turn.
    pub fn plan(user: &str) -> Vec<Step> {
        match user {
            "refused" => vec![("set-1", vec![e("x", 10), e("y", 1)], false)],
            "moved" | "moved-to-end" => vec![("set-1", vec![e("x", 10), e("y", 1)], true)],
            "moved-lost" => vec![("set-1", vec![e("y", 10), e("z", 10)], true)],
            "moved-empty" => vec![("set-1", Vec::new(), true)],
            "restarted" => vec![("set-1", vec![e("x", 2), e("y", 10)], false)],
            "ordered" => vec![
                ("older", vec![e("x", 10)], false),
                ("newer", vec![e("y", 10), e("z", 10)], false),
            ],
            _ => vec![("set-1", vec![e("y", 10), e("z", 10)], false)],
        }
    }

    /// The admit a plan sends once the result of `after` is read and a while
    /// has passed, if any.
    pub fn then(user: &str, after: &str) -> Option<Step> {
        match (user, after) {
            ("moved", "set-1") => Some(("set-2", vec![e("x", 10), e("z", 10)], false)),
            ("moved", "set-2") => Some(("set-3", vec![e("x", 10), e("z", 10)], true)),
            _ => None,
        }
    }

    fn replace((corr, entries, moves_call): Step) -> RuleAction {
        RuleAction::ReplaceAdmissionSet { correlation_id: corr.to_string(), entries, moves_call }
    }

    /// The correlation id of the last result `user`'s plan read.
    fn last_read(user: &str) -> String {
        let reads = READS.lock().unwrap();
        reads.get(user).and_then(|r| r.last()).map(|r| r.0.clone()).unwrap_or_default()
    }

    /// The called user part of the call's INVITE.
    pub fn user(ctx: &RuleContext) -> String {
        let uri = &ctx.call.a_leg_invite().uri;
        uri.trim_start_matches("sip:").split('@').next().unwrap_or_default().to_string()
    }

    fn kick() -> RuleDefinition {
        sm_rule! {
            id: "admitset-kick",
            machine: ADMITSET,
            active: [ AsState::Working ],
            transitions: [ AsState::Working => AsState::Awaiting ],
            effects: [],
            matcher: Match::timer().timer_type(KICK),
            handle: |ctx: &RuleContext| {
                let mut actions: Vec<RuleAction> = plan(&user(ctx)).into_iter().map(replace).collect();
                actions.push(RuleAction::SetState { machine: ADMITSET, to: AsState::Awaiting.label() });
                Some(RuleHandleResult::new(actions))
            },
        }
    }

    fn next() -> RuleDefinition {
        sm_rule! {
            id: "admitset-next",
            machine: ADMITSET,
            active: [ AsState::Awaiting ],
            transitions: [],
            effects: [],
            matcher: Match::timer().timer_type(NEXT),
            handle: |ctx: &RuleContext| {
                let user = user(ctx);
                Some(RuleHandleResult::new(then(&user, &last_read(&user)).into_iter().map(replace).collect()))
            },
        }
    }

    fn result() -> RuleDefinition {
        sm_rule! {
            id: "admitset-result",
            machine: ADMITSET,
            active: [ AsState::Awaiting ],
            transitions: [],
            effects: [ b2bua::rules::Effect::GuardTimer { timer: NEXT, label: "the plan's next admit" } ],
            matcher: Match::internal_event().topic("limiter-admit-result"),
            handle: |ctx: &RuleContext| {
                if let CallEvent::InternalEvent { outcome, payload, .. } = ctx.event {
                    let corr = payload["correlation_id"].as_str().unwrap_or_default().to_string();
                    let held = ctx.call.limiter_held().iter().map(|e| e.id.clone()).collect();
                    let next = then(&user(ctx), &corr).is_some();
                    let read: Read = (corr, outcome.clone(), held, ctx.call_ref.to_string());
                    READS.lock().unwrap().entry(user(ctx)).or_default().push(read);
                    if next {
                        return Some(RuleHandleResult::new(vec![RuleAction::ScheduleTimer {
                            timer_type: NEXT,
                            delay: TimerDelay::secs(2),
                            leg_id: None,
                        }]));
                    }
                }
                Some(RuleHandleResult::new(vec![]))
            },
        }
    }
}

/// Every result the service read, by the plan (the called user part) it ran.
static READS: Mutex<BTreeMap<String, Vec<Read>>> = Mutex::new(BTreeMap::new());

fn reads(plan: &str) -> Vec<Read> {
    READS.lock().unwrap().get(plan).cloned().unwrap_or_default()
}

/// How the wrapper treats the service's admits (every admit after the
/// initial route's, counted from 1).
#[derive(Clone, Copy)]
enum Fault {
    /// Forward every admit as it is.
    None,
    /// Hold the first service admit back for a while before forwarding it.
    DelayFirst(Duration),
    /// Forward the first service admit, then lose its answer.
    LoseFirstAnswer,
    /// Forward the first service admit, then answer it only after a while.
    AnswerFirstLate(Duration),
    /// Forward every service admit after a short local step, which the
    /// wrapped client's own timer does not count (a client arming its timer
    /// after a name lookup).
    StartsLate(Duration),
}

impl Fault {
    /// `inner` with the fault applied to the service's admits, after the
    /// `initial` admits the initial route sends (0 or 1); like every fault,
    /// without a circuit breaker.
    fn apply(self, initial: usize, inner: Arc<dyn CallLimiter>) -> Arc<dyn CallLimiter> {
        let first_service = initial + 1;
        match self {
            Fault::None => without_breaker(inner),
            Fault::DelayFirst(d) => delay_nth_admit(first_service, d, inner),
            Fault::LoseFirstAnswer => unavailable_on_nth(first_service, true, inner),
            Fault::AnswerFirstLate(d) => answer_nth_admit_late(first_service, d, inner),
            Fault::StartsLate(d) => delay_admits_after(initial, d, inner),
        }
    }
}

/// Every call routes to bob holding `route` (at cap 10).
fn routing(route: &'static [&'static str]) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter =
                    route.iter().map(|id| LimiterEntry { id: (*id).into(), limit: 10 }).collect();
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

struct Scene {
    h: Harness,
    alice: scenario_harness::Agent,
    bob: scenario_harness::Agent,
    rig: WitnessRig,
    b2bua: B2buaSut,
}

async fn scene(name: &str, route: &'static [&'static str], fault: Fault) -> Scene {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let rig = WitnessRig::serve(LimiterConfig::default(), Duration::from_secs(2), None).await;
    let limiter = fault.apply(usize::from(!route.is_empty()), rig.client.clone());
    let b2bua = B2buaSut::builder(routing(route))
        .services(vec![admitset::service_def()])
        .limiter(limiter)
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.limiter_refresh_sec = 5;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    Scene { h, alice, bob, rig, b2bua }
}

impl Scene {
    /// Alice calls `plan`@bob and bob answers.
    async fn call(&self, plan: &str) -> scenario_harness::Dialog {
        let uri = format!("sip:{plan}@127.0.0.1:5070");
        let mut call = self
            .alice
            .invite(&self.bob)
            .with_sdp(OFFER_SDP)
            .ruri(uri)
            .through(self.b2bua.addr)
            .send()
            .await;
        let mut uas = self.bob.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        call.expect(200).await;
        let dialog = call.ack().await;
        self.bob.receive("ACK").await;
        dialog
    }

    /// The worker's `b2bua_limiter_uncounted_calls` gauge.
    fn uncounted_calls(&self) -> u64 {
        self.b2bua.metrics().limiter().uncounted_calls()
    }

    async fn end(self, mut dialog: scenario_harness::Dialog) {
        hangup(&mut dialog, &self.bob).await;
        settle_until(|| self.b2bua.is_reaped()).await;
        self.rig.expect_drained("the call's release frees what the limiter holds").await;
        self.b2bua.assert_fully_reaped();
        let _ = self.h.finish().await;
    }
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// The service replaces `[x]` with `[y, z]`: the limiter holds the new set,
/// and the service reads the admitted outcome with the call already holding
/// it.
#[tokio::test(start_paused = true)]
async fn a_service_replaces_the_admission_set_and_reads_the_outcome() {
    let s = scene("service-admission-set-replaced", &["x"], Fault::None).await;
    let dialog = s.call("replaced").await;
    s.rig.expect_holds([1, 0, 0], "the route's set").await;
    s.rig.expect_holds([0, 1, 1], "the service's set replaced it").await;
    settle_until(|| !reads("replaced").is_empty()).await;
    let [(corr, outcome, held, _)] = &reads("replaced")[..] else { panic!("one result") };
    assert_eq!((corr.as_str(), outcome.as_str()), ("set-1", "admitted"));
    assert_eq!(held, &ids(&["y", "z"]), "the call holds what was admitted when the rule reads it");
    s.end(dialog).await;
}

/// The service's set adds `y`, which is at its cap: the admit is refused, the
/// call keeps `[x]`, and the service reads the refusal with the call still
/// holding `[x]`.
#[tokio::test(start_paused = true)]
async fn a_refused_replacement_keeps_the_set_held() {
    let s = scene("service-admission-set-refused", &["x"], Fault::None).await;
    let dialog = s.call("refused").await;
    settle_until(|| !reads("refused").is_empty()).await;
    let [(corr, outcome, held, _)] = &reads("refused")[..] else { panic!("one result") };
    assert_eq!((corr.as_str(), outcome.as_str()), ("set-1", "rejected"));
    assert_eq!(held, &ids(&["x"]), "a refusal keeps the set held");
    s.rig.expect_holds([1, 0, 0], "nothing moved").await;
    assert_eq!(s.uncounted_calls(), 0, "a refused set the call is not moved onto runs nothing");
    s.end(dialog).await;
}

/// The service moves the call onto `[x, y]`, refused on `y`'s cap: the call
/// stays counted on `[x]` and runs uncounted on `y`. An admitted change that
/// does not move the call (`[x, z]`) leaves it so: it still runs on `y`. Moved
/// onto `[x, z]`, admitted, it runs on a held set.
#[tokio::test(start_paused = true)]
async fn a_call_moved_onto_a_refused_set_runs_uncounted_until_moved_onto_a_held_one() {
    let s = scene("service-admission-set-moved-refused", &["x"], Fault::None).await;
    let dialog = s.call("moved").await;
    settle_until(|| !reads("moved").is_empty()).await;
    let [(corr, outcome, held, call_ref)] = &reads("moved")[..] else { panic!("one result") };
    assert_eq!((corr.as_str(), outcome.as_str()), ("set-1", "rejected"));
    assert_eq!(held, &ids(&["x"]), "a refusal keeps the set held");
    let call = s.b2bua.live_call(call_ref).expect("the call");
    assert!(call.limiter.counted() && !call.limiter.fail_open() && call.limiter.runs_uncounted());
    assert_eq!(s.uncounted_calls(), 1, "the call runs on y, which the limiter does not hold");

    s.h.advance(Duration::from_secs(3)).await;
    settle_until(|| reads("moved").len() == 2).await;
    let (corr, outcome, held, _) = &reads("moved")[1];
    assert_eq!((corr.as_str(), outcome.as_str(), held), ("set-2", "admitted", &ids(&["x", "z"])));
    assert_eq!(s.uncounted_calls(), 1, "an admitted change the call is not moved onto");

    s.h.advance(Duration::from_secs(3)).await;
    settle_until(|| reads("moved").len() == 3).await;
    let (corr, outcome, _, _) = &reads("moved")[2];
    assert_eq!((corr.as_str(), outcome.as_str()), ("set-3", "admitted"));
    assert_eq!(s.uncounted_calls(), 0, "the call runs on [x, z], which the limiter holds");
    s.rig.expect_holds([1, 0, 1], "the admitted set").await;
    s.end(dialog).await;
}

/// As above with no later change: the call runs uncounted on `y` until its
/// end, which leaves the gauge.
#[tokio::test(start_paused = true)]
async fn a_call_moved_onto_a_refused_set_runs_uncounted_until_it_ends() {
    let s = scene("service-admission-set-moved-to-end", &["x"], Fault::None).await;
    let mut dialog = s.call("moved-to-end").await;
    settle_until(|| !reads("moved-to-end").is_empty()).await;
    assert_eq!(reads("moved-to-end")[0].1, "rejected");
    s.h.advance(Duration::from_secs(10)).await;
    assert_eq!(s.uncounted_calls(), 1, "the call runs on y, refreshes and all");
    hangup(&mut dialog, &s.bob).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(s.uncounted_calls(), 0, "the call's end leaves the gauge");
    s.rig.expect_drained("the call's release frees what the limiter holds").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// The service moves the call onto `[y, z]`; the admit lands and its answer
/// is lost: the call runs uncounted until the next refresh states the set
/// the limiter holds.
#[tokio::test(start_paused = true)]
async fn a_moved_call_whose_answer_is_lost_is_counted_again_by_the_next_refresh() {
    let s = scene("service-admission-set-moved-lost", &["x"], Fault::LoseFirstAnswer).await;
    let dialog = s.call("moved-lost").await;
    settle_until(|| !reads("moved-lost").is_empty()).await;
    assert_eq!(reads("moved-lost")[0].1, "unavailable");
    assert_eq!(s.uncounted_calls(), 1, "held is [x]: the call runs on y and z uncounted");
    for _ in 0..10 {
        if s.uncounted_calls() == 0 {
            break;
        }
        s.h.advance(Duration::from_secs(1)).await;
    }
    assert_eq!(s.uncounted_calls(), 0, "the refresh answer states [y, z]");
    s.end(dialog).await;
}

/// A call routed with no limiter, which holds and owes nothing, is moved
/// onto an empty set: no admit leaves, the service reads `not_sent`, and the
/// call runs on nothing uncounted.
#[tokio::test(start_paused = true)]
async fn a_move_onto_nothing_sends_no_admit() {
    let s = scene("service-admission-set-moved-empty", &[], Fault::None).await;
    let dialog = s.call("moved-empty").await;
    settle_until(|| !reads("moved-empty").is_empty()).await;
    let [(corr, outcome, held, call_ref)] = &reads("moved-empty")[..] else { panic!("one result") };
    assert_eq!((corr.as_str(), outcome.as_str(), held), ("set-1", "not_sent", &ids(&[])));
    assert_eq!(s.b2bua.metrics().limiter().requests_total(LimiterOp::Admit), 0, "no admit left");
    let call = s.b2bua.live_call(call_ref).expect("the call");
    assert_eq!(call.limiter.runs_on(), Some(&[][..]));
    assert!(call.limiter.owed_release().is_none() && !call.limiter.runs_uncounted());
    assert_eq!(s.uncounted_calls(), 0);
    s.end(dialog).await;
}

/// Two admits of one turn: the older (`[x]`) is held back and lands after the
/// newer (`[y, z]`). The limiter refuses it as superseded, so the newer set
/// stays; the service reads both outcomes, and the call holds the newer set
/// throughout.
#[tokio::test(start_paused = true)]
async fn an_older_admit_landing_last_is_superseded_and_changes_nothing() {
    let fault = Fault::DelayFirst(Duration::from_millis(500));
    let s = scene("service-admission-set-ordered", &["x"], fault).await;
    let dialog = s.call("ordered").await;
    settle_until(|| reads("ordered").len() == 1).await;
    s.h.advance(Duration::from_secs(1)).await;
    settle_until(|| reads("ordered").len() == 2).await;
    let got: Vec<(String, String, Vec<String>)> =
        reads("ordered").into_iter().map(|(c, o, h, _)| (c, o, h)).collect();
    assert_eq!(
        got,
        [
            ("newer".to_string(), "admitted".to_string(), ids(&["y", "z"])),
            ("older".to_string(), "superseded".to_string(), ids(&["y", "z"])),
        ]
    );
    s.rig.expect_holds([0, 1, 1], "the newer set stays").await;
    s.end(dialog).await;
}

/// The admit lands and its answer is lost: the call still holds `[x]` when
/// the service reads the outcome; the next refresh answer states `[y, z]`,
/// which becomes the call's held set.
#[tokio::test(start_paused = true)]
async fn a_lost_answer_is_repaired_by_the_next_refresh() {
    let s = scene("service-admission-set-lost-answer", &["x"], Fault::LoseFirstAnswer).await;
    let dialog = s.call("lost").await;
    settle_until(|| !reads("lost").is_empty()).await;
    let [(corr, outcome, held, call_ref)] = &reads("lost")[..] else { panic!("one result") };
    assert_eq!((corr.as_str(), outcome.as_str()), ("set-1", "unavailable"));
    assert_eq!(held, &ids(&["x"]), "held is what the limiter last stated");
    s.rig.expect_holds([0, 1, 1], "the admit landed").await;
    let held_now = || s.b2bua.live_call(call_ref).map(|c| c.limiter.held_ids()).unwrap_or_default();
    for _ in 0..10 {
        if held_now() == ids(&["y", "z"]) {
            break;
        }
        s.h.advance(Duration::from_secs(1)).await;
    }
    assert_eq!(held_now(), ids(&["y", "z"]), "the refresh answer states the landed set");
    let call = s.b2bua.live_call(call_ref).expect("the call");
    assert!(call.limiter.counted() && !call.limiter.fail_open());
    s.end(dialog).await;
}

/// The limiter restarts empty after the call's route admitted `[x]`, and a
/// call admitted since takes `x` to its cap of 2 beside the witness. The
/// service's `[x, y]` reaches the limiter before the call's refresh: the
/// admit carries the call's held `[x]`, which the restarted limiter
/// re-registers, so `x` is kept, not added, and only `y` is checked. The call
/// stays counted on the admitted set, and its release frees it.
#[tokio::test(start_paused = true)]
async fn an_admit_reaching_a_restarted_limiter_keeps_the_call_s_own_ids() {
    let mut s = scene("service-admission-set-restarted", &["x"], Fault::None).await;
    let mut dialog = s.call("restarted").await;
    s.rig.expect_holds([1, 0, 0], "the route's set").await;

    // ── the limiter restarts empty, before the service's admit and the
    //    call's first refresh ──────────────────────────────────────────────
    let dead = s.rig.restart().await;
    let since = s.rig.store.admit("since", 1, &[AdmitEntry { id: "x".into(), limit: 100 }], false);
    assert_eq!(since, AdmitResult::Admitted);
    assert_eq!(s.rig.store.held("x"), 2, "x at its cap of 2: the witness and the call since");

    settle_until(|| !reads("restarted").is_empty()).await;
    let [(corr, outcome, held, call_ref)] = &reads("restarted")[..] else { panic!("one result") };
    assert_eq!(
        (corr.as_str(), outcome.as_str()),
        ("set-1", "admitted"),
        "x is the call's own: only y is checked"
    );
    assert_eq!(held, &ids(&["x", "y"]), "the call holds the admitted set");
    assert_eq!(s.rig.store.stats().admit_reregistered_calls, 1);
    s.rig.expect_holds([2, 1, 0], "x: the call and the call since; y: the call").await;
    let call = s.b2bua.live_call(call_ref).expect("the call");
    assert!(call.limiter.counted() && !call.limiter.fail_open());

    s.rig.store.release(&["since"]);
    hangup(&mut dialog, &s.bob).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.rig.expect_drained("the call's release frees what the limiter holds").await;
    // The SUT's ledger reads the dead store, frozen with the route's set and
    // the witnesses.
    assert_eq!(dead.stats().current_total, 4);
    s.b2bua.assert_fully_reaped_leaving(LimiterLeak {
        unreleased: 0,
        stored: dead.stats().current_total,
        queued: Vec::new(),
    });
    let _ = s.h.finish().await;
}

/// A call holding nothing, whose service admit lands and whose answer comes
/// back after the call ended: the call releases its key at its end (owed
/// from the turn that sent the admit), and the result reaching no call
/// releases the key once more, the router's own release of a report no call
/// took. Nothing is left behind.
#[tokio::test(start_paused = true)]
async fn a_result_landing_on_a_gone_call_releases_the_key() {
    let fault = Fault::AnswerFirstLate(Duration::from_secs(1));
    let s = scene("service-admission-set-gone", &[], fault).await;
    let mut dialog = s.call("gone").await;
    s.rig.expect_holds([0, 1, 1], "the admit landed, its answer on its way").await;
    hangup(&mut dialog, &s.bob).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.rig.expect_holds([0, 0, 0], "the call's release at its end freed the set").await;
    let at_end = s.rig.store.stats().releases_total;
    s.h.advance(Duration::from_secs(2)).await;
    settle_until(|| s.b2bua.limiter_releases_waiting() == 0).await;
    assert!(reads("gone").is_empty(), "no call was left to read the result");
    assert_eq!(
        s.rig.store.stats().releases_total,
        at_end + 1,
        "the result reaching no call released the key"
    );
    s.rig.expect_drained("nothing is left behind").await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// The limiter does not answer the service's admit, and the client arms its
/// timer a little after the router sent the admit: the client's own budget
/// still runs out before the router's cap, so the admit counts as a limiter
/// failure (the one the breaker reads), and the service reads it as
/// `unavailable`.
#[tokio::test(start_paused = true)]
async fn a_stalled_service_admit_counts_as_a_limiter_failure() {
    let fault = Fault::StartsLate(Duration::from_millis(50));
    let s = scene("service-admission-set-stalled", &["x"], fault).await;
    let dialog = s.call("stalled").await;
    let limiter = b2bua_harness::WITNESS_LIMITER_ADDR.parse().unwrap();
    let before =
        s.b2bua.metrics().limiter().failures_total(LimiterOp::Admit, LimiterFailure::Timeout);
    s.rig.http.apply_fault(http_net::Fault::Stall { dst: limiter });
    for _ in 0..40 {
        if !reads("stalled").is_empty() {
            break;
        }
        s.h.advance(Duration::from_millis(200)).await;
    }
    assert_eq!(reads("stalled").first().map(|r| r.1.as_str()), Some("unavailable"));
    assert_eq!(
        s.b2bua.metrics().limiter().failures_total(LimiterOp::Admit, LimiterFailure::Timeout),
        before + 1,
        "the client's timeout answered the admit, not the router's cap",
    );
    s.rig.http.apply_fault(http_net::Fault::Resume { dst: limiter });
    s.end(dialog).await;
}
