//! The tags this stack shows the caller derive from the call's seed (ADR-0055
//! §4, RFC 3261 §19.3): any node holding the record derives the tag the
//! call's first owner showed, and two calls share none.

use b2bua_core::caller_tags;
use b2bua_core::turn_driver::Driver;

use crate::calls::bridged;

/// The record of a call bridged to bob.
fn bridged_record() -> call::Call {
    bridged(&Driver::new()).call().clone()
}

#[test]
fn every_holder_of_the_record_derives_the_same_caller_tags() {
    let call = bridged_record();
    let copy = call.clone();
    assert_eq!(caller_tags::own(&call), caller_tags::own(&copy));
    assert_eq!(
        caller_tags::fronting(&call, "b-1", "bobtag"),
        caller_tags::fronting(&copy, "b-1", "bobtag")
    );
    assert_eq!(caller_tags::of_leg(&call, "b-2"), caller_tags::of_leg(&copy, "b-2"));
}

#[test]
fn each_dialog_a_call_fronts_takes_a_tag_of_its_own() {
    let call = bridged_record();
    let tags = [
        caller_tags::own(&call),
        caller_tags::answering(&call),
        caller_tags::fronting(&call, "b-1", "bobtag"),
        caller_tags::fronting(&call, "b-1", "bobtag2"),
        caller_tags::of_leg(&call, "b-1"),
    ];
    for (i, t) in tags.iter().enumerate() {
        assert!(tags.iter().skip(i + 1).all(|u| u != t), "{tags:?}");
    }
}

/// Two calls' seeds differ: a property of the derivation over any seed.
#[test]
fn two_calls_derive_unrelated_tags() {
    let call = bridged_record();
    let mut other = call.clone();
    other.tag_seed = call.tag_seed.wrapping_add(1);
    assert_ne!(caller_tags::own(&call), caller_tags::own(&other));
}

/// The caller tag a service reads is the a-leg's own once minted, else the
/// one every holder of the record derives: the record before bob's 180
/// minted it names the same dialog the answered call does.
#[test]
fn a_service_reads_the_caller_tag_every_owner_derives() {
    let driver = Driver::new();
    let unshown = driver.dial().call().clone();
    assert_eq!(unshown.a_leg.own_tag, None, "no tag shown before bob's 180");
    let minted = bridged(&driver).call().clone();
    let own = minted.a_leg.own_tag.clone().expect("the tag shown to alice");
    assert_eq!(b2bua_sdk::rule_call::RuleCall::new(&minted).caller_tag(), own);
    let read = b2bua_sdk::rule_call::RuleCall::new(&unshown).caller_tag();
    assert_eq!(read, caller_tags::own(&unshown));
    assert_eq!(read, own, "the tag every owner derives is the one shown");
}
