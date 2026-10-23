//! The call's other owner, stated on the Via of every request the B2BUA sends
//! for the call (ADR-0055 §8): the front proxy signs it into the leg's cookie,
//! so every leg of one call names the same pair of owners.

use sip_message::header::ParamValue;

use super::identity::{leg_via, CallMarks};
use crate::config::B2buaConfig;

/// The `bak` param of the Via the B2BUA stamps on a leg's request of a call
/// owned by `owners`, sent by the worker `sender`.
fn stated(owners: Option<(&str, &str)>, sender: &str) -> Option<String> {
    let config = B2buaConfig { self_ordinal: sender.into(), ..B2buaConfig::default() };
    let marks =
        CallMarks { call_ref: "w0|c|t", incarnation_mark: "k", is_emergency: false, owners };
    let via = leg_via(&config, marks, "b-1", "z9hG4bKowner".to_string());
    via.param("bak").and_then(ParamValue::as_str).map(str::to_string)
}

#[test]
fn the_primary_states_the_calls_backup() {
    assert_eq!(stated(Some(("w0", "w1")), "w0").as_deref(), Some("w1"));
}

#[test]
fn a_copy_on_the_backup_states_the_primary() {
    assert_eq!(stated(Some(("w0", "w1")), "w1").as_deref(), Some("w0"));
}

#[test]
fn a_call_with_one_owner_states_none() {
    assert_eq!(stated(Some(("w0", "")), "w0"), None);
    assert_eq!(stated(None, "w0"), None);
}

#[test]
fn the_marks_of_a_call_name_its_owners() {
    use crate::test_call::{invite, src};
    let config = B2buaConfig { self_ordinal: "w0".into(), ..B2buaConfig::default() };
    let invite = invite("w0", "w1", "owners");
    let mut ids = sip_ids::TurnIds::fixed(1);
    let call = crate::initial_invite::build_initial_call(&invite, src(), &config, &mut ids, 0);
    assert_eq!(CallMarks::of(&call).owners, Some(("w0", "w1")));
}
