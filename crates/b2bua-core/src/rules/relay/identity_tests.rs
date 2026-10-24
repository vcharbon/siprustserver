//! The call's other owner, stated on the Via of every request the B2BUA sends
//! for the call (ADR-0055 §8): the front proxy signs it into the leg's cookie,
//! so every leg of one call names the same pair of owners. Which owner is the
//! other is the shell's to state (`Owners`).

use sip_message::header::ParamValue;

use super::identity::{leg_via, CallMarks};
use crate::config::B2buaConfig;
use crate::owners::Owners;

/// The `bak` param of the Via the B2BUA stamps on a leg's request of a call
/// whose other owner is `other`.
fn stated(other: Option<&str>) -> Option<String> {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..B2buaConfig::default() };
    let owners = Owners { other: other.map(str::to_string), ..Owners::default() };
    let marks = CallMarks {
        call_ref: "w0|c|t",
        incarnation_mark: "k",
        is_emergency: false,
        owners: &owners,
    };
    let via = leg_via(&config, marks, "b-1", None, "z9hG4bKowner".to_string());
    via.param("bak").and_then(ParamValue::as_str).map(str::to_string)
}

#[test]
fn the_other_owner_rides_the_via() {
    assert_eq!(stated(Some("w1")).as_deref(), Some("w1"));
}

#[test]
fn a_call_with_one_owner_states_none() {
    assert_eq!(stated(None), None);
}

#[test]
fn the_marks_of_a_call_name_the_other_owner_its_owners_state() {
    use crate::test_call::{invite, src};
    let config = B2buaConfig { self_ordinal: "w0".into(), ..B2buaConfig::default() };
    let invite = invite("w0", "w1", "owners");
    let mut ids = sip_ids::TurnIds::fixed(1);
    let call = crate::initial_invite::build_initial_call(&invite, src(), &config, &mut ids, 0);
    let owners = Owners { other: Some("w1".into()), ..Owners::default() };
    assert_eq!(CallMarks::of(&call, &owners).owners.other.as_deref(), Some("w1"));
}
