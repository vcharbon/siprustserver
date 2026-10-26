//! The ring deadline arms on a dialled leg's first 18x to the INVITE that
//! dialled it, and on nothing else.

use std::time::Duration;

use call::helpers::{join_attempt, open_attempt};
use call::testkit::{dialled_leg, stage_leg};
use call::{AttemptCause, Call, Deadline, LegState, Ring, TimerKey, TimerType};

use super::*;

const RING_MS: u64 = 2_345;

/// A call whose leg `b-1`, its INVITE sent at 0, dialled for an attempt
/// ringing `ring`, reads `state`.
fn call(ring: Deadline, state: LegState) -> Call {
    use crate::test_call::{invite, src};
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let invite = invite("ring-deadline");
    let call = crate::initial_invite::build_initial_call(
        &invite,
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut leg = dialled_leg("b-1", "own");
    stage_leg(&mut leg, state);
    let mut call = call::helpers::add_originated_b_leg(call, leg, 0);
    let (attempt, _) = open_attempt(&mut call, AttemptCause::Route, ring);
    join_attempt(&mut call, attempt, "b-1");
    call
}

/// The ring deadline `call` holds on `b-1`.
fn deadline(call: &Call) -> Option<i64> {
    let key = TimerKey::leg(TimerType::NoAnswer, "b-1");
    call.timers.iter().find(|t| t.key == key).map(|t| t.fire_at)
}

fn arm(call: &mut Call, cseq: i64, now_ms: i64) {
    let config = B2buaConfig::default();
    arm_at_first_18x(&config, call, &mut HandlerEffects::new(), "b-1", cseq, now_ms);
}

fn ringing() -> Deadline {
    Deadline::After(Duration::from_millis(RING_MS))
}

#[test]
fn the_first_18x_arms_the_ring_from_its_own_instant() {
    let mut call = call(ringing(), LegState::Trying);
    arm(&mut call, 1, 700);
    assert_eq!(deadline(&call), Some(700 + RING_MS as i64));
}

#[test]
fn a_leg_that_rang_before_is_not_armed_again() {
    let mut call = call(ringing(), LegState::Early);
    call.b_legs[0].ring = Ring::Rang;
    arm(&mut call, 1, 700);
    assert_eq!(deadline(&call), None);
}

#[test]
fn an_ended_leg_arms_nothing() {
    let mut call = call(ringing(), LegState::Terminated);
    arm(&mut call, 1, 700);
    assert_eq!(deadline(&call), None);
}

#[test]
fn a_leg_ringing_from_its_invite_since_a_restore_takes_its_real_18x() {
    let mut call = call(ringing(), LegState::Trying);
    restore(&mut call);
    assert_eq!(deadline(&call), Some(RING_MS as i64), "run from the INVITE");
    arm(&mut call, 1, 4_000);
    assert_eq!(deadline(&call), Some(4_000 + RING_MS as i64), "re-armed from the 18x");
}

fn restore(call: &mut Call) {
    arm_unrung_after_restore(&B2buaConfig::default(), call);
}

/// A restored leg with no 18x on record runs its ring deadline from its
/// INVITE, and reads as not having rung.
#[test]
fn a_restore_runs_an_unrung_legs_deadline_from_its_invite_and_leaves_it_silent() {
    let mut call = call(ringing(), LegState::Early);
    call.b_legs[0].outbound_invite_mut().expect("dialled").sent_at_ms = 6_000;
    restore(&mut call);
    assert_eq!(call.b_legs[0].outbound_invite().map(|i| i.sent_at_ms), Some(6_000));
    assert_eq!(call.b_legs[0].ring, Ring::Silent, "no 18x is on record");
    assert_eq!(deadline(&call), Some(6_000 + RING_MS as i64));
}

#[test]
fn a_restore_arms_nothing_of_a_leg_that_rang_or_holds_its_deadline() {
    let mut rang = call(ringing(), LegState::Early);
    rang.b_legs[0].ring = Ring::Rang;
    restore(&mut rang);
    assert_eq!(deadline(&rang), None);
    assert_eq!(rang.b_legs[0].ring, Ring::Rang);

    let mut armed = call(ringing(), LegState::Early);
    arm(&mut armed, 1, 700);
    armed.b_legs[0].ring = Ring::Silent;
    restore(&mut armed);
    assert_eq!(deadline(&armed), Some(700 + RING_MS as i64), "the armed deadline stands");
}

#[test]
fn a_restore_arms_nothing_of_an_answered_cancelled_or_ended_leg_or_one_ringing_under_none() {
    for state in [LegState::Confirmed, LegState::Terminated] {
        let mut call = call(ringing(), state);
        restore(&mut call);
        assert_eq!(deadline(&call), None, "{state:?}");
    }
    let mut cancelled = call(ringing(), LegState::Trying);
    cancelled.b_legs[0].outbound_invite_mut().expect("dialled").cancel_sent = true;
    restore(&mut cancelled);
    assert_eq!(deadline(&cancelled), None, "a CANCEL is on record");

    let mut off = call(Deadline::Off, LegState::Trying);
    restore(&mut off);
    assert_eq!(deadline(&off), None, "the attempt rings under no deadline");
    assert_eq!(off.b_legs[0].ring, Ring::Silent);
}

#[test]
fn an_18x_to_another_invite_arms_nothing() {
    let mut call = call(ringing(), LegState::Trying);
    arm(&mut call, 2, 700);
    assert_eq!(deadline(&call), None);
}

#[test]
fn an_answered_leg_arms_nothing() {
    let mut call = call(ringing(), LegState::Confirmed);
    arm(&mut call, 1, 700);
    assert_eq!(deadline(&call), None);
}

#[test]
fn an_attempt_whose_ring_is_off_arms_nothing() {
    let mut call = call(Deadline::Off, LegState::Trying);
    arm(&mut call, 1, 700);
    assert_eq!(deadline(&call), None);
}

#[test]
fn a_leg_no_attempt_dialled_arms_nothing() {
    let mut call = call(ringing(), LegState::Trying);
    call.b_legs[0].attempt = None;
    arm(&mut call, 1, 700);
    assert_eq!(deadline(&call), None);
}

#[test]
fn a_ring_past_the_invite_bound_is_held_inside_it() {
    let mut call = call(Deadline::After(Duration::from_secs(150)), LegState::Trying);
    call.b_legs[0].outbound_invite_mut().expect("dialled").sent_at_ms = 1_000;
    let config = B2buaConfig::default();
    let ceiling_ms =
        (config.invite_txn_timeout_sec - B2buaConfig::NO_ANSWER_CANCEL_MARGIN_SEC) * 1000;
    arm(&mut call, 1, 20_000);
    assert_eq!(deadline(&call), Some(1_000 + ceiling_ms), "held to the bound less its margin");
}
