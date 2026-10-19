//! The property lane of the decision core (ADR-0052, Consequences): a call's
//! turns run with no runtime, over generated event sequences, and a turn is
//! a function of its inputs — whatever a peer or the decision backend sends,
//! the turn returns a result, the same one for the same inputs, and never
//! panics.
//!
//! A case runs a script ([`drive::Step`]) over a call dialled toward bob,
//! each step's SIP text built against the call as it stands, one step's text
//! mutated ([`wire_mutations::mutate`]), the decision answers' payloads
//! generated; each turn goes through the timer screen, the rule chain and
//! the landing pass as the shell runs them.
//!
//! - [`every_script_turns_the_same_twice`] (default lane): every script,
//!   unmutated, ends its call where it should, and two runs agree;
//! - [`admit_reports_cross_their_event`] (default lane): an admit result's
//!   event carries back the report it was built from;
//! - [`structured_corpus_never_panics`] (slow lane): every deterministic
//!   mutation of every SIP step of every script;
//! - [`random_cases_never_panic`] (slow lane): proptest over stacked random
//!   mutations and generated decision payloads, each case run twice and the
//!   runs compared, failures persisted under `proptest-regressions/` beside
//!   this file.

use proptest::collection::vec;
use proptest::prelude::*;
use wire_mutations::lane::{on_lane_stack, runner};
use wire_mutations::{mutate, Mutation};

use b2bua_core::limiter::report::{admit_report_of, LimiterAdmitResult};
use call::{AdmitOutcome, AdmitReport, CallModelState, LimiterEntry, LimiterHeld, TimerType};

mod drive;

use drive::Step;

/// Cases of the proptest properties.
const CASES: u32 = 512;

/// The scripts every case starts from, by name.
fn scripts() -> Vec<(&'static str, Vec<Step>)> {
    let route = serde_json::json!({
        "destination": {"host": "127.0.0.1", "port": 5071},
        "new_ruri": "sip:carol@127.0.0.1:5071",
        "features": serde_json::to_value(b2bua_core::decision::default_platform_features())
            .unwrap_or_default(),
    });
    vec![
        (
            "answered-bob-hangs-up",
            vec![Step::BobAnswers(180), Step::BobAnswers(200), Step::BobByes],
        ),
        (
            "answered-alice-hangs-up",
            vec![Step::BobAnswers(183), Step::BobAnswers(200), Step::AliceByes],
        ),
        (
            "refused-then-rejected",
            vec![
                Step::BobAnswers(180),
                Step::BobAnswers(486),
                Step::FailureAnswers {
                    outcome: "reject",
                    payload: serde_json::json!({"code": 480, "reason": "Temporarily Unavailable"}),
                },
            ],
        ),
        (
            "refused-then-rerouted",
            vec![
                Step::BobAnswers(486),
                Step::FailureAnswers { outcome: "failover", payload: route },
            ],
        ),
        (
            "refused-then-redirected",
            vec![
                Step::BobAnswers(486),
                Step::FailureAnswers {
                    outcome: "redirect",
                    payload: serde_json::json!({
                        "code": 302,
                        "contacts": [{"uri": "sip:carol@127.0.0.1:5071", "q": 0.5}],
                    }),
                },
            ],
        ),
        ("ringing-unanswered", vec![Step::BobAnswers(180), Step::Fires(TimerType::NoAnswer)]),
        ("setup-unanswered", vec![Step::Fires(TimerType::SetupTimeout)]),
    ]
}

/// Every script, unmutated, ends its call where it should, and two runs of
/// it agree turn by turn.
#[test]
fn every_script_turns_the_same_twice() {
    on_lane_stack(|| {
        let dialled = drive::dialled();
        for (name, script) in scripts() {
            let first = drive::run(&dialled, &script, usize::MAX, &[]);
            let again = drive::run(&dialled, &script, usize::MAX, &[]);
            assert_eq!(first.turns.len(), script.len(), "{name}: every step turns");
            assert_eq!(first.turns, again.turns, "{name}: a turn is a function of its inputs");
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

/// Every deterministic mutation of every SIP step of every script turns.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_corpus_never_panics() {
    on_lane_stack(|| {
        let dialled = drive::dialled();
        for (name, script) in scripts() {
            let seed = drive::run(&dialled, &script, usize::MAX, &[]);
            for (at, raw) in &seed.raws {
                for (family, mutations) in mutate::families(raw) {
                    for m in mutations {
                        let mutated = std::panic::catch_unwind(|| {
                            drive::run(&dialled, &script, *at, std::slice::from_ref(&m))
                        });
                        assert!(mutated.is_ok(), "{name} step {at} {family} {m:?} panicked");
                    }
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
    ]
}

/// Up to four stacked mutations of one step of any script, the decision
/// answers generated, turn, and two runs of the case agree.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_cases_never_panic() {
    on_lane_stack(|| {
        let dialled = drive::dialled();
        let scripts = scripts();
        let strategy = (
            0..scripts.len(),
            0..3usize,
            vec(mutate::mutation(), 1..=4),
            proptest::option::of((outcome(), payload())),
        );
        let mut runner = runner("tests/it/turn_lane/proptest-regressions/cases.txt", CASES);
        let result = runner.run(&strategy, |(s, at, mutations, answer)| {
            let script: Vec<Step> = scripts[s]
                .1
                .iter()
                .map(|step| match (step, &answer) {
                    (Step::FailureAnswers { .. }, Some((outcome, payload))) => {
                        Step::FailureAnswers { outcome, payload: payload.clone() }
                    }
                    _ => step.clone(),
                })
                .collect();
            let mutations: &[Mutation] = &mutations;
            let first = drive::run(&dialled, &script, at, mutations);
            let again = drive::run(&dialled, &script, at, mutations);
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
