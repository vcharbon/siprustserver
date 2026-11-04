//! The property lane of the decision core (ADR-0052, Consequences): a call's
//! turns run with no runtime, over generated event sequences, and a turn is
//! a function of its inputs — whatever a peer or the decision backend sends,
//! the turn returns a result, the same one for the same inputs, and never
//! panics.
//!
//! A case runs a script ([`Step`]) over a call dialled toward bob —
//! answers, hang-ups, a re-INVITE, an UPDATE, a reliable provisional and its
//! PRACK, a CANCEL, a REFER, the decision consults — each step's SIP text
//! built against the call as it stands, its sequence edited (a step dropped,
//! repeated or swapped), one step's text mutated ([`wire_mutations::mutate`]),
//! the decision answers' payloads generated; each turn goes through the
//! shared turn driver ([`b2bua_core::turn_driver`]): the timer screen, the
//! dialog gate's verdict, the rule chain and the landing pass as the shell
//! runs them.
//!
//! - [`every_script_turns_the_same_twice`] (default lane): every script,
//!   unmutated, ends its call where it should with no defect noted, and two
//!   runs agree;
//! - [`every_edited_script_turns_the_same_twice`] (default lane): every
//!   script with each step dropped, repeated or swapped with the next;
//! - [`a_step_replaced_by_its_own_text_turns_the_same`] (default lane): the
//!   replacement the fuzz target drives replays the script it replaces in;
//! - [`admit_reports_cross_their_event`] (default lane): an admit result's
//!   event carries back the report it was built from;
//! - [`a_session_timer_rides_both_legs`] (default lane): alice's session
//!   timer reaches bob, and bob's taking part reaches alice;
//! - [`every_decision_routes_the_caller_invite`] (default lane): alice's
//!   INVITE under every decision of [`invite::decisions`] dials bob with what
//!   the decision states, or is refused where its destination is;
//! - [`structured_invites_never_panic`] (slow lane): every deterministic
//!   mutation of alice's INVITE under every decision is refused or dials a
//!   b-leg INVITE that reads back and refreezes to itself;
//! - [`structured_corpus_never_panics`] (slow lane): every deterministic
//!   mutation of every SIP step of every script;
//! - [`random_cases_never_panic`] (slow lane): proptest over stacked random
//!   mutations, sequence edits and generated decision payloads, each case
//!   run twice and the runs compared, failures persisted under
//!   `proptest-regressions/` beside this file.

use proptest::collection::vec;
use proptest::prelude::*;
use wire_mutations::lane::{on_lane_stack, runner};
use wire_mutations::{mutate, Mutation};

use b2bua_core::limiter::report::{admit_report_of, LimiterAdmitResult};
use call::{AdmitOutcome, AdmitReport, CallModelState, LimiterEntry, LimiterHeld};

pub(crate) mod drive;
pub(crate) mod invite;
pub(crate) mod scripts;

use b2bua_core::turn_driver::{Driver, Session, Step};
use scripts::{scripts, Script};

/// Cases of the proptest properties.
const CASES: u32 = 512;

/// The requests (`leg METHOD`) a script's run sends among others: what its
/// steps ask the stack to carry or answer.
fn sends(name: &str) -> &'static [&'static str] {
    match name {
        "answered-bob-hangs-up" => &["a BYE"],
        "answered-alice-hangs-up" => &["b-1 BYE"],
        "reinvited" => &["b-1 ACK", "b-1 INVITE", "b-1 BYE"],
        "updated" => &["b-1 UPDATE", "a BYE"],
        "reliably-provisioned" => &["b-1 PRACK", "b-1 BYE"],
        "cancelled" => &["b-1 CANCEL"],
        "referred-then-refused" => &["b-1 NOTIFY", "a BYE"],
        "session-timed" => &["b-1 INVITE", "b-1 BYE"],
        "early-media-fake-prack" => &["b-1 PRACK", "b-1 BYE"],
        "referred-then-transferred" => &["b-2 INVITE", "a INVITE", "b-1 BYE", "b-2 BYE"],
        _ => &[],
    }
}

/// The responses (`leg STATUS`) a script's run sends, in order, where they
/// show its service at work: bob's early media shown to alice as a bare 180,
/// or promoted to her 200; the last answers her BYE.
fn answers(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "early-media-drop-sdp" | "early-media-keep-sdp" => {
            Some(&["a 180", "a 180", "a 200", "a 200"])
        }
        "early-media-fake-prack" => Some(&["a 180", "a 200", "a 200"]),
        "early-media-promoted" => Some(&["a 200", "a 200"]),
        _ => None,
    }
}

/// Every script, unmutated, ends its call where it should with no defect
/// noted, sends what its steps ask for, and two runs of it agree turn by
/// turn.
#[test]
fn every_script_turns_the_same_twice() {
    on_lane_stack(|| {
        for Script { name, setup, steps: script } in scripts() {
            let driver = drive::driver(setup);
            let dialled = driver.dial();
            let first = drive::run(&dialled, &script, usize::MAX, &[]);
            let again = drive::run(&dialled, &script, usize::MAX, &[]);
            assert_eq!(first.turns.len(), script.len(), "{name}: every step turns");
            assert_eq!(first.turns, again.turns, "{name}: a turn is a function of its inputs");
            assert_eq!(first.defects, Vec::<String>::new(), "{name}: no turn notes a defect");
            for request in sends(name) {
                assert!(
                    first.sent.iter().any(|s| s == request),
                    "{name}: {request}: {:?}",
                    first.sent
                );
            }
            if let Some(answers) = answers(name) {
                let answered: Vec<String> = first
                    .responses
                    .iter()
                    .map(|(leg, r)| format!("{leg} {}", r.status()))
                    .collect();
                assert_eq!(answered, answers, "{name}");
            }
            let ended = matches!(
                first.call.state,
                CallModelState::Terminating | CallModelState::Terminated
            );
            // A rerouted call rings carol; an unanswered one consults on its
            // failover, as its route carries a callback context.
            let lives = matches!(name, "refused-then-rerouted" | "ringing-unanswered");
            assert_eq!(ended, !lives, "{name}: {:?}", first.call.state);
        }
    });
}

/// A step's SIP text replaced by the text it carries turns as the script
/// does: what the fuzz target's seeds hold replays the call they came from.
#[test]
fn a_step_replaced_by_its_own_text_turns_the_same() {
    on_lane_stack(|| {
        for Script { name, setup, steps: script } in scripts() {
            let driver = drive::driver(setup);
            let dialled = driver.dial();
            let seed = drive::run(&dialled, &script, usize::MAX, &[]);
            for (at, raw) in &seed.raws {
                let replaced = drive::run_replacing(&dialled, &script, *at, raw);
                assert_eq!(replaced.turns, seed.turns, "{name} step {at}");
            }
        }
    });
}

/// An edit of a script's sequence: a step dropped, a step repeated, two
/// steps swapped.
#[derive(Clone, Copy, Debug)]
enum Edit {
    Drop(usize),
    Repeat(usize),
    Swap(usize, usize),
}

/// `script` with `edit` applied; an edit naming a step past its end leaves
/// it as it is.
fn edited(script: &[Step], edit: Edit) -> Vec<Step> {
    let mut steps = script.to_vec();
    match edit {
        Edit::Drop(i) if i < steps.len() => {
            steps.remove(i);
        }
        Edit::Repeat(i) if i < steps.len() => {
            let step = steps[i].clone();
            steps.insert(i, step);
        }
        Edit::Swap(i, j) if i < steps.len() && j < steps.len() => steps.swap(i, j),
        _ => {}
    }
    steps
}

/// Every script with each step dropped, repeated, or swapped with the next
/// turns, and two runs of it agree turn by turn.
#[test]
fn every_edited_script_turns_the_same_twice() {
    on_lane_stack(|| {
        for Script { name, setup, steps: script } in scripts() {
            let driver = drive::driver(setup);
            let dialled = driver.dial();
            for i in 0..script.len() {
                for edit in [Edit::Drop(i), Edit::Repeat(i), Edit::Swap(i, i + 1)] {
                    let steps = edited(&script, edit);
                    let first = drive::run(&dialled, &steps, usize::MAX, &[]);
                    let again = drive::run(&dialled, &steps, usize::MAX, &[]);
                    assert_eq!(first.turns, again.turns, "{name} {edit:?}");
                }
            }
        }
    });
}

/// Every deterministic mutation of every distinct SIP text of every script
/// turns: a text two scripts send alike is mutated in the first only.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_corpus_never_panics() {
    on_lane_stack(|| {
        let mut seen = std::collections::BTreeSet::new();
        for Script { name, setup, steps: script } in scripts() {
            let driver = drive::driver(setup);
            let dialled = driver.dial();
            let seed = drive::run(&dialled, &script, usize::MAX, &[]);
            for (at, raw) in seed.raws.iter().filter(|(_, raw)| seen.insert(raw.clone())) {
                for (family, mutations) in mutate::families(raw) {
                    for m in mutations {
                        // A run reads the dialled call and builds its own
                        // state: a panic leaves nothing half-written behind.
                        let mutated =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                drive::run(&dialled, &script, *at, std::slice::from_ref(&m))
                            }));
                        assert!(mutated.is_ok(), "{name} step {at} {family} {m:?} panicked");
                    }
                }
            }
        }
    });
}

/// alice's session timer rides the INVITE that dials bob, and bob's taking
/// part reaches alice's 200 (RFC 4028 §8, §9).
#[test]
fn a_session_timer_rides_both_legs() {
    on_lane_stack(|| {
        let script = scripts().into_iter().find(|s| s.name == "session-timed");
        let script = script.expect("the session-timed script");
        let driver = drive::driver(script.setup);
        let dialled = driver.dial();
        let dialling = String::from_utf8_lossy(drive::bob_invite(&dialled).image()).into_owned();
        assert!(dialling.contains("Session-Expires: 1800"), "{dialling}");
        let run = drive::run(&dialled, &script.steps, usize::MAX, &[]);
        let answer = run.responses.iter().find(|(leg, r)| leg == "a" && r.status() == 200);
        let answer = answer.expect("alice's 200");
        let answer = String::from_utf8_lossy(answer.1.image()).into_owned();
        assert!(answer.contains("Session-Expires: 1800"), "{answer}");
        assert!(answer.contains("Require: timer"), "{answer}");
    });
}

/// What the b-leg INVITE of each dialling decision carries of it.
fn states(name: &str) -> &'static [&'static str] {
    match name {
        "stated-headers" => &[
            "Subject: routed\r\n",
            "Organization: lane\r\n",
            "Call-Info: <http://example.com/i>\r\n",
            "User-Agent: lane\r\n",
        ],
        "attached-parts" => {
            &["multipart/mixed", "application/pidf+xml", "Content-ID: <lane@example.com>"]
        }
        "body-replaced" => &["a=inactive"],
        _ => &[],
    }
}

/// alice's INVITE under every decision dials bob, its b-leg INVITE carrying
/// what the decision states, and the call answered and ended over it notes
/// no defect; a destination the admission refuses refuses the call.
#[test]
fn every_decision_routes_the_caller_invite() {
    on_lane_stack(|| {
        let raw = drive::alice_invite_text(drive::Setup::Plain);
        let answered = scripts().into_iter().find(|s| s.name == "answered-bob-hangs-up");
        let answered = answered.expect("the answered script").steps;
        for decision in invite::decisions() {
            let name = decision.name;
            let Some(dialled) = invite::dial_checked(&raw, &decision) else {
                assert!(!decision.dials, "{name}: alice's INVITE dials bob");
                continue;
            };
            let wire = String::from_utf8_lossy(drive::bob_invite(&dialled).image()).into_owned();
            for stated in states(name) {
                assert!(wire.contains(stated), "{name}: {stated:?} in\n{wire}");
            }
            let run = drive::run(&dialled, &answered, usize::MAX, &[]);
            assert_eq!(run.defects, Vec::<String>::new(), "{name}");
        }
    });
}

/// Every deterministic mutation of alice's INVITE under every decision is
/// refused or dials a b-leg INVITE that reads back and refreezes to itself.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_invites_never_panic() {
    on_lane_stack(|| {
        let raw = drive::alice_invite_text(drive::Setup::Plain);
        let decisions = invite::decisions();
        for (family, mutations) in mutate::families(&raw) {
            for m in mutations {
                let input = mutate::apply(&raw, &m);
                for decision in &decisions {
                    let dialled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        invite::dial_checked(&input, decision).is_some()
                    }));
                    assert!(dialled.is_ok(), "{} {family} {m:?} panicked", decision.name);
                }
            }
        }
    });
}

/// A decision answer's payload: the keys the folds read, each with a value
/// of any JSON kind.
fn payload() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        any::<u64>().prop_map(serde_json::Value::from),
        (-2.0f64..2.0).prop_map(serde_json::Value::from),
        "[ -~]{0,12}".prop_map(serde_json::Value::from),
    ];
    let value = leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..4).prop_map(serde_json::Value::from),
            vec(("[a-z_]{1,8}", inner), 0..4)
                .prop_map(|kv| serde_json::Value::Object(kv.into_iter().collect())),
        ]
    });
    let key = prop_oneof![
        Just("code"),
        Just("reason"),
        Just("status"),
        Just("destination"),
        Just("new_ruri"),
        Just("contacts"),
        Just("update_headers"),
        Just("service_ext"),
        Just("no_answer_ms"),
        Just("features"),
        Just("failed_leg_id"),
        Just("origin"),
        Just("label"),
        Just("reject_code"),
        Just("reject_reason"),
        Just("new_refer_to"),
        Just("callback_context"),
    ];
    vec((key, value), 0..6).prop_map(|kv| {
        serde_json::Value::Object(kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    })
}

/// The outcome a decision answer folds back under.
fn outcome() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("failover"),
        Just("terminate"),
        Just("reject"),
        Just("redirect"),
        Just("relay"),
        Just("error"),
        Just("allow"),
    ]
}

/// A sequence edit of a script of up to eight steps.
fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (0..8usize).prop_map(Edit::Drop),
        (0..8usize).prop_map(Edit::Repeat),
        (0..8usize, 0..8usize).prop_map(|(i, j)| Edit::Swap(i, j)),
    ]
}

/// Up to four stacked mutations of one step of any script, the decision
/// answers generated, turn, and two runs of the case agree.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_cases_never_panic() {
    on_lane_stack(|| {
        let scripts = scripts();
        let drivers: Vec<Driver> = scripts.iter().map(|s| drive::driver(s.setup)).collect();
        let dialled: Vec<Session> = drivers.iter().map(Driver::dial).collect();
        let strategy = (
            0..scripts.len(),
            0..8usize,
            vec(mutate::mutation(), 1..=4),
            proptest::option::of((outcome(), payload())),
            proptest::option::of(edit()),
        );
        let mut runner = runner("tests/it/turn_lane/proptest-regressions/cases.txt", CASES);
        let result = runner.run(&strategy, |(s, at, mutations, answer, edit)| {
            let script: Vec<Step> = scripts[s]
                .steps
                .iter()
                .map(|step| match (step, &answer) {
                    (Step::FailureAnswers { .. }, Some((outcome, payload))) => {
                        Step::FailureAnswers { outcome, payload: payload.clone() }
                    }
                    (Step::ReferAnswers { .. }, Some((outcome, payload))) => {
                        Step::ReferAnswers { outcome, payload: payload.clone() }
                    }
                    _ => step.clone(),
                })
                .collect();
            let script = edit.map_or(script.clone(), |edit| edited(&script, edit));
            let mutations: &[Mutation] = &mutations;
            let first = drive::run(&dialled[s], &script, at, mutations);
            let again = drive::run(&dialled[s], &script, at, mutations);
            prop_assert_eq!(first.turns, again.turns);
            Ok(())
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}

fn entries() -> impl Strategy<Value = Vec<LimiterEntry>> {
    vec(("[a-z]{1,6}", any::<i64>()).prop_map(|(id, limit)| LimiterEntry { id, limit }), 0..4)
}

fn held() -> impl Strategy<Value = LimiterHeld> {
    (any::<u64>(), entries()).prop_map(|(change, entries)| LimiterHeld { change, entries })
}

fn admit_outcome() -> impl Strategy<Value = AdmitOutcome> {
    prop_oneof![
        Just(AdmitOutcome::Admitted),
        ("[a-z]{1,6}", held())
            .prop_map(|(limiter_id, held)| AdmitOutcome::Rejected { limiter_id, held }),
        held().prop_map(|held| AdmitOutcome::Superseded { held }),
        Just(AdmitOutcome::Released),
        Just(AdmitOutcome::Unavailable),
        Just(AdmitOutcome::NotSent),
    ]
}

/// An admit result's event carries back the report it was built from,
/// whatever the report states.
#[test]
fn admit_reports_cross_their_event() {
    let strategy = ("[a-z#]{1,12}", any::<u64>(), entries(), admit_outcome(), "[a-z-]{0,8}");
    let mut runner = runner("tests/it/turn_lane/proptest-regressions/admit_reports.txt", CASES);
    let result = runner.run(&strategy, |(key, change, entries, outcome, correlation_id)| {
        let report = AdmitReport { key, change, entries, outcome };
        let event =
            LimiterAdmitResult { call_ref: "lane".into(), correlation_id, report: report.clone() }
                .into_event();
        prop_assert_eq!(admit_report_of(&event), Some(report));
        Ok(())
    });
    if let Err(e) = result {
        panic!("{e}");
    }
}
