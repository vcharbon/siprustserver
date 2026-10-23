//! The screen delivers a fire of the instance the ledger holds and takes it
//! off; it drops a superseded or disarmed one, or one whose scope ended, and
//! leaves the ledger alone.

use call::helpers::arm_timer;
use call::{Call, TimerKey, TimerType};

use super::*;

fn call() -> Call {
    use crate::test_call::{invite, src};
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let invite = invite("w0", "w1", "timer-screen");
    crate::initial_invite::build_initial_call(
        &invite,
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    )
}

fn fire(key: &TimerKey, generation: u64, call: &Call) -> CallEvent {
    CallEvent::Timer {
        key: key.clone(),
        generation,
        call_ref: call.call_ref.clone(),
        incarnation: Some(call.incarnation().to_string()),
    }
}

#[test]
fn a_fire_of_the_held_instance_is_delivered_and_leaves_the_ledger() {
    let mut call = call();
    let key = TimerKey::leg(TimerType::KeepaliveTimeout, "a");
    let armed = arm_timer(&mut call, key.clone(), 1_000);
    let event = fire(&key, armed.generation, &call);
    assert_eq!(screen(&mut call, &event, 0, &mut Default::default()), Ok(()));
    assert!(call.timers.is_empty(), "the fired instance left the ledger");
}

#[test]
fn a_fire_of_a_replaced_instance_is_dropped() {
    let mut call = call();
    let key = TimerKey::call(TimerType::GlobalDuration);
    let first = arm_timer(&mut call, key.clone(), 1_000);
    let second = arm_timer(&mut call, key.clone(), 2_000);
    let event = fire(&key, first.generation, &call);
    assert_eq!(screen(&mut call, &event, 0, &mut Default::default()), Err(Dropped::Superseded));
    assert_eq!(call.timers, vec![second], "the live instance stays armed");
}

/// A ledger that holds the timer of a leg that has since ended: the fire
/// reaches no rule.
#[test]
fn a_fire_whose_leg_ended_is_dropped() {
    let mut call = call();
    let mut leg = call::testkit::dialled_leg("b-1", "own");
    call::testkit::stage_leg(&mut leg, call::LegState::Terminated);
    call.b_legs.push(leg);
    let key = TimerKey::leg(TimerType::NoAnswer, "b-1");
    let armed = arm_timer(&mut call, key.clone(), 1_000);
    let event = fire(&key, armed.generation, &call);
    let screened = screen(&mut call, &event, 0, &mut Default::default());
    assert!(screened.is_err(), "the fire of an ended leg reaches no rule: {screened:?}");
}

/// The same for the timer of an attempt that has since ended.
#[test]
fn a_fire_whose_attempt_ended_is_dropped() {
    let mut call = call();
    let (id, _) =
        call::helpers::open_attempt(&mut call, call::AttemptCause::Route, call::Deadline::Off);
    let key = TimerKey::attempt(TimerType::NoAnswer, id);
    let armed = arm_timer(&mut call, key.clone(), 1_000);
    call::helpers::end_attempt(&mut call, id, call::AttemptOutcome::Failed);
    let event = fire(&key, armed.generation, &call);
    let screened = screen(&mut call, &event, 0, &mut Default::default());
    assert!(screened.is_err(), "the fire of an ended attempt reaches no rule: {screened:?}");
}

#[test]
fn a_fire_of_a_disarmed_key_is_dropped() {
    let mut call = call();
    let key = TimerKey::leg(TimerType::KeepaliveTimeout, "a");
    let armed = arm_timer(&mut call, key.clone(), 1_000);
    call::helpers::disarm_timer(&mut call, &key);
    let event = fire(&key, armed.generation, &call);
    assert_eq!(screen(&mut call, &event, 0, &mut Default::default()), Err(Dropped::Disarmed));
}
