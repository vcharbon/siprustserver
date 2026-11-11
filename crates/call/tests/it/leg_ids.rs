//! A leg id's Call-ID number and the identity of a dialled leg (ADR-0059).

use call::helpers::{leg_call_id_number, same_leg};

#[test]
fn a_leg_id_numbers_its_call_id_by_its_ordinal_and_space() {
    assert_eq!(leg_call_id_number("b-1"), Some(1));
    assert_eq!(leg_call_id_number("b-12"), Some(12));
    assert_eq!(leg_call_id_number("c-3"), Some(1_000_000_003), "the other space numbers apart");
    for unminted in ["a", "x-1", "b-", "b-1-call-id"] {
        assert_eq!(leg_call_id_number(unminted), None, "{unminted}");
    }
}

#[test]
fn a_leg_id_minted_twice_names_two_legs_by_their_from_tags() {
    let first = call::testkit::dialled_leg("c-2", "from-tag-1");
    let again = call::testkit::dialled_leg("c-2", "from-tag-2");
    assert!(same_leg(&first, &first.clone()));
    assert!(!same_leg(&first, &again), "each INVITE drew its own From-tag");
    assert!(!same_leg(&first, &call::testkit::dialled_leg("c-3", "from-tag-1")));
    let mut other_call_id = first.clone();
    other_call_id.call_id = "2-x".into();
    assert!(!same_leg(&first, &other_call_id), "one leg id and From-tag, another Call-ID");
}
