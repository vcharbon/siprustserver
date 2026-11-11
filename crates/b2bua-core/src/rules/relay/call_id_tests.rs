use sip_message::FirstIngressBound;

use super::call_id::dialled_call_id;

const BOUND: FirstIngressBound = FirstIngressBound::DEFAULT;

#[test]
fn a_leg_dialled_from_the_creator_space_is_numbered_by_its_ordinal() {
    assert_eq!(dialled_call_id("b-1", "abc@10.0.0.1", &BOUND).as_deref(), Some("1-abc@10.0.0.1"));
    assert_eq!(dialled_call_id("b-12", "abc@10.0.0.1", &BOUND).as_deref(), Some("12-abc@10.0.0.1"));
}

#[test]
fn the_other_space_numbers_apart_from_the_creator_space() {
    let creator = dialled_call_id("b-3", "abc", &BOUND).unwrap();
    let other = dialled_call_id("c-3", "abc", &BOUND).unwrap();
    assert_eq!(other, "1000000003-abc");
    assert_ne!(creator, other, "two copies acting at once never mint one Call-ID for two legs");
}

#[test]
fn a_derived_call_id_keeps_the_originators_as_its_tail() {
    let originator = "7-a84b4c76e66710@pc33.atlanta.example.com";
    let derived = dialled_call_id("b-2", originator, &BOUND).unwrap();
    let (n, tail) = derived.split_once('-').unwrap();
    assert!(n.bytes().all(|b| b.is_ascii_digit()) && tail == originator, "{derived}");
}

#[test]
fn a_leg_id_no_space_minted_has_no_derived_call_id() {
    assert_eq!(dialled_call_id("a", "abc", &BOUND), None);
    assert_eq!(dialled_call_id("x-1", "abc", &BOUND), None);
    assert_eq!(dialled_call_id("b-1-call-id", "abc", &BOUND), None);
}

#[test]
fn a_call_id_line_at_the_first_ingress_bound_is_derived_one_byte_past_it_is_not() {
    let fits = "123456789";
    let bound = FirstIngressBound { header_length: "Call-ID: 1-".len() + fits.len(), ..BOUND };
    assert_eq!(dialled_call_id("b-1", fits, &bound).as_deref(), Some("1-123456789"));
    assert_eq!(dialled_call_id("b-1", "1234567890", &bound), None);
}
