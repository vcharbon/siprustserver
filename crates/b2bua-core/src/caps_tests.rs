use b2bua_sdk::config::{AgeCap, CallCaps, Cap};
use call::{CallModelState, CdrEvent, CdrEventType};

use super::*;
use crate::test_call::turn;

fn call() -> Call {
    use crate::test_call::{invite, src};
    let config = crate::config::B2buaConfig::default();
    let mut ids = sip_ids::TurnIds::fixed(1);
    let mut call =
        crate::initial_invite::build_initial_call(&invite("caps"), src(), &config, &mut ids, 0);
    call.cdr_events.clear();
    call
}

fn caps() -> CallCaps {
    CallCaps { cdr_events: AgeCap { base: 2, per_hour: 1 }, ..CallCaps::DEFAULT }
}

fn add_cdr_events(call: &mut Call, n: usize) {
    for _ in 0..n {
        call.cdr_events.push(CdrEvent {
            event_type: CdrEventType::Bye,
            timestamp: 0,
            leg_id: "a".into(),
            status_code: None,
            reason: None,
            decision_ordinal: 0,
        });
    }
}

fn held(start: &Call, after: Call, checked: Checked) -> Result<HandlerResult, Tripped> {
    hold(&caps(), &Gauge::of(start), checked, &mut turn(1), HandlerResult::new(after))
}

#[test]
fn a_turn_growing_a_collection_past_its_cap_trips() {
    let start = call();
    let mut after = start.clone();
    add_cdr_events(&mut after, 3);
    let tripped = held(&start, after, Checked::All).unwrap_err();
    assert_eq!(tripped, Tripped { cap: Cap::CdrEvents, verdict: false });
}

#[test]
fn a_turn_reaching_a_cap_exactly_lands() {
    let start = call();
    let mut after = start.clone();
    add_cdr_events(&mut after, 2);
    assert!(held(&start, after, Checked::All).is_ok());
}

#[test]
fn a_call_already_past_a_cap_trips_only_when_a_turn_grows_it() {
    let mut start = call();
    add_cdr_events(&mut start, 5);
    assert!(held(&start, start.clone(), Checked::All).is_ok(), "nothing grew");
    let mut after = start.clone();
    add_cdr_events(&mut after, 1);
    assert!(held(&start, after, Checked::All).is_err(), "the turn grew it");
}

#[test]
fn an_age_cap_grows_with_the_granted_duration() {
    let start = call();
    let mut after = start.clone();
    after.features = Some(crate::decision::default_platform_features());
    add_cdr_events(&mut after, 3);
    // Granted one hour: 2 + 1 × 1.
    assert!(held(&start, after.clone(), Checked::All).is_ok());
    add_cdr_events(&mut after, 1);
    assert!(held(&start, after, Checked::All).is_err());
}

#[test]
fn a_verdict_turn_or_a_going_away_call_is_held_to_its_fuel_alone() {
    let start = call();
    let mut after = start.clone();
    add_cdr_events(&mut after, 10);
    let checked = Checked::FuelOnly { verdict: true };
    assert!(held(&start, after, checked).is_ok());

    let mut going = call();
    going.state = CallModelState::Terminating;
    let event = CallEvent::InternalEvent {
        call_ref: going.call_ref.clone(),
        topic: "x".into(),
        outcome: "y".into(),
        payload: serde_json::Value::Null,
        body: Vec::new(),
        incarnation: None,
    };
    assert_eq!(Checked::of(&going, &event), Checked::FuelOnly { verdict: false });
    assert_eq!(Checked::of(&call(), &event), Checked::All);
}

#[test]
fn a_turn_past_its_fuel_trips_whatever_its_collections() {
    let start = call();
    let mut scope = turn(1);
    assert!(scope.charge(2_000, 2_000));
    assert!(!scope.charge(1, 2_000), "the 2001st action trips");
    assert!(!scope.charge(0, 2_000), "a tripped turn charges nothing more");
    let checked = Checked::FuelOnly { verdict: true };
    let out = hold(&caps(), &Gauge::of(&start), checked, &mut scope, HandlerResult::new(start));
    assert_eq!(out.unwrap_err(), Tripped { cap: Cap::ActionsPerTurn, verdict: true });
}

#[test]
fn growing_across_half_a_cap_is_noted() {
    let mut start = call();
    add_cdr_events(&mut start, 1);
    let mut after = start.clone();
    add_cdr_events(&mut after, 1);
    let mut scope = turn(1);
    let out =
        hold(&caps(), &Gauge::of(&start), Checked::All, &mut scope, HandlerResult::new(after));
    assert!(out.is_ok());
    assert_eq!(scope.notes.caps_near(), &[Cap::CdrEvents]);
}

#[test]
fn a_service_value_is_measured_by_its_encoding() {
    let mut call = call();
    call::helpers::set_call_ext(&mut call, "svc", Some(serde_json::json!({ "k": "abcd" })));
    assert_eq!(Gauge::of(&call).get(Cap::ExtBytes), r#"{"k":"abcd"}"#.len() as u64);
}
