//! The tags this stack shows the caller derive from the call's seed (ADR-0055
//! §4, RFC 3261 §19.3): any node holding the record derives the tag the
//! call's first owner showed, and two calls share none.

use b2bua_core::caller_tags;

use super::evidence_rules::bridged;

#[test]
fn every_holder_of_the_record_derives_the_same_caller_tags() {
    let call = bridged();
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
    let call = bridged();
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

#[test]
fn two_calls_derive_unrelated_tags() {
    let call = bridged();
    let mut other = call.clone();
    other.tag_seed = call.tag_seed.wrapping_add(1);
    assert_ne!(caller_tags::own(&call), caller_tags::own(&other));
}

/// The caller tag a service reads is the a-leg's own once minted, else the
/// one every holder of the record derives: a copy that never saw the
/// provisional names the same dialog.
#[test]
fn a_service_reads_the_caller_tag_every_owner_derives() {
    let minted = bridged();
    assert_eq!(b2bua_sdk::rule_call::RuleCall::new(&minted).caller_tag(), "b2bua-a");
    let mut unshown = minted.clone();
    unshown.a_leg.own_tag = None;
    let read = b2bua_sdk::rule_call::RuleCall::new(&unshown).caller_tag();
    assert_eq!(read, caller_tags::own(&unshown));
    assert_eq!(read, b2bua_sdk::rule_call::RuleCall::new(&unshown.clone()).caller_tag());
}
