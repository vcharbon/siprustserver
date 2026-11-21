//! The legs a call is carried on (`carries_call`): the caller's leg and the
//! leg it is bridged to; with no live bridge, the adopted legs dialled for
//! it.

use crate::common::representative_call;
use call::helpers::*;
use call::model::*;

fn end_dialog(mut call: Call, leg_id: &str, state: DialogState) -> Call {
    update_leg(&mut call, leg_id, |l| l.dialogs.iter_mut().for_each(|d| d.state = state.clone()));
    call
}

/// `representative_call` (the caller bridged to `b-1`) with a media leg
/// `b-2` and an adopted leg `b-3` dialled beside the bridge, both confirmed.
fn call_with_side_legs() -> Call {
    let mut call = representative_call();
    let b1 = call.b_legs[0].clone();
    let side =
        |leg_id: &str, kind: Option<LegKind>| Leg { leg_id: leg_id.into(), kind, ..b1.clone() };
    call.b_legs.push(side("b-2", Some(LegKind::Media)));
    call.b_legs.push(side("b-3", None));
    call
}

/// The caller's leg and the leg it is bridged to carry the call; a media leg
/// and a leg dialled beside the bridge do not.
#[test]
fn the_bridged_legs_carry_the_call() {
    let call = call_with_side_legs();
    assert_eq!(call_carrying_legs(&call), vec!["a", "b-1"]);
    assert!(!carries_call(&call, "b-2"), "a media leg");
    assert!(!carries_call(&call, "b-3"), "a leg dialled beside the bridge");
    assert!(!carries_call(&call, "b-9"), "no such leg");
}

/// Bridged to no live leg (the setup, or a peer gone away), the caller's leg
/// and every adopted leg not going away carry the call; a media leg never
/// does, and nothing carries a terminating call.
#[test]
fn with_no_live_bridge_the_adopted_legs_carry_the_call() {
    let call = end_dialog(call_with_side_legs(), "b-1", DialogState::Ended(DialogEnd::PeerBye));
    assert_eq!(call_carrying_legs(&call), vec!["a", "b-3"], "the bridged peer went away");

    let mut call = call_with_side_legs();
    split_leg(&mut call, "a");
    assert_eq!(call_carrying_legs(&call), vec!["a", "b-1", "b-3"], "no bridge yet");

    call.state = CallModelState::Terminating;
    assert!(call_carrying_legs(&call).is_empty(), "a terminating call");
}
