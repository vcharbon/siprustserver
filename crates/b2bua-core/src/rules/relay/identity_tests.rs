//! The call's owners, stated on the Via of every request the B2BUA sends for
//! the call (ADR-0055 §8): the other owner, or on a leg's dialog-creating
//! request the call's primary and backup. The front proxy signs them into the
//! leg's cookie, so every leg of one call names the same pair of owners
//! whichever of them dials it. Which owner is which is the shell's to state
//! (`Owners`).

use call::RelayOwner;
use sip_message::header::ParamValue;

use super::identity::{dialling_via, leg_via, CallMarks, RequestFacts};
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
    let invite = invite("owners");
    let mut ids = sip_ids::TurnIds::fixed(1);
    let call = crate::initial_invite::build_initial_call(&invite, src(), &config, &mut ids, 0);
    let owners = Owners { other: Some("w1".into()), ..Owners::default() };
    assert_eq!(CallMarks::of(&call, &owners).owners.other.as_deref(), Some("w1"));
}

/// The `pri` and `bak` params of the Via `this` stamps on the INVITE that
/// dials a leg of a call `primary` and `backup` own.
fn dialling(this: &str, primary: &str, backup: &str) -> (Option<String>, Option<String>) {
    let config = B2buaConfig { self_ordinal: this.into(), ..B2buaConfig::default() };
    let other = if this == primary { backup } else { primary };
    let owners = Owners {
        this: RelayOwner::new(this, "boot-1"),
        other: Some(other.into()),
        primary: Some(primary.into()),
        backup: Some(backup.into()),
        copy_served: this != primary,
    };
    let marks = CallMarks {
        call_ref: "w0|c|t",
        incarnation_mark: "k",
        is_emergency: false,
        owners: &owners,
    };
    let facts = RequestFacts { method: "INVITE", uri: "sip:bob@192.0.2.1", offer: false };
    let via = dialling_via(&config, marks, "b-1", facts, "z9hG4bKdial".to_string());
    let param = |name: &str| via.param(name).and_then(ParamValue::as_str).map(str::to_string);
    (param("pri"), param("bak"))
}

#[test]
fn the_primary_dialling_a_leg_states_itself_and_its_backup() {
    assert_eq!(dialling("w0", "w0", "w1"), (Some("w0".into()), Some("w1".into())));
}

#[test]
fn a_copy_dialling_a_leg_states_the_calls_primary_and_itself_as_backup() {
    assert_eq!(dialling("w1", "w0", "w1"), (Some("w0".into()), Some("w1".into())));
}

/// An in-dialog request states the other owner alone.
#[test]
fn an_in_dialog_request_states_no_primary() {
    let config = B2buaConfig { self_ordinal: "w1".into(), ..B2buaConfig::default() };
    let owners = Owners {
        this: RelayOwner::new("w1", "boot-1"),
        other: Some("w0".into()),
        primary: Some("w0".into()),
        backup: Some("w1".into()),
        copy_served: true,
    };
    let marks = CallMarks {
        call_ref: "w0|c|t",
        incarnation_mark: "k",
        is_emergency: false,
        owners: &owners,
    };
    let via = leg_via(&config, marks, "b-1", None, "z9hG4bKreinvite".to_string());
    assert_eq!(via.param("pri"), None);
    assert_eq!(via.param("bak").and_then(ParamValue::as_str), Some("w0"));
}
