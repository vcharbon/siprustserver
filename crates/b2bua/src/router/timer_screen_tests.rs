//! The screen delivers a fire of the instance the ledger holds and takes it
//! off; it drops a superseded or disarmed one and leaves the ledger alone.

use call::helpers::arm_timer;
use call::{Call, TimerKey, TimerType};

use super::*;

fn call() -> Call {
    use crate::router::test_support::{invite, src};
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
    let key = TimerKey::leg(TimerType::NoAnswer, "b-1");
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

#[test]
fn a_fire_of_a_disarmed_key_is_dropped() {
    let mut call = call();
    let key = TimerKey::leg(TimerType::KeepaliveTimeout, "a");
    let armed = arm_timer(&mut call, key.clone(), 1_000);
    call::helpers::disarm_timer(&mut call, &key);
    let event = fire(&key, armed.generation, &call);
    assert_eq!(screen(&mut call, &event, 0, &mut Default::default()), Err(Dropped::Disarmed));
}
