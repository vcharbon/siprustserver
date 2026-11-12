use std::cell::Cell;

use super::Wiring;
use crate::Failure;

const INGRESS: &str = "127.0.0.2:5090";
const BACKEND: &str = "127.0.0.1:8713";

#[test]
fn a_sut_that_never_called_the_backend_is_named_once_and_refuses_every_later_cell() {
    let wiring = Wiring::default();
    assert_eq!(wiring.refusal(), None);
    let why = wiring.after_cell(true, true, || 0, INGRESS, BACKEND).expect("named");
    assert!(
        why.contains("SUT at 127.0.0.2:5090 never called the backend double at 127.0.0.1:8713"),
        "{why}"
    );
    assert_eq!(wiring.refusal(), Some(why));
    assert_eq!(wiring.after_cell(true, true, || 3, INGRESS, BACKEND), None, "judged once");
    assert!(wiring.refusal().is_some());
}

#[test]
fn a_sut_that_called_the_backend_is_wired_for_the_whole_batch() {
    let wiring = Wiring::default();
    assert_eq!(wiring.after_cell(true, true, || 2, INGRESS, BACKEND), None);
    assert_eq!(wiring.after_cell(true, true, || 0, INGRESS, BACKEND), None);
    assert_eq!(wiring.refusal(), None);
}

/// A first cell whose case asks the backend double nothing, or that never
/// drove the SUT, judges nothing: the cell after it is the one that does, and
/// the batch's exchanges are read once, for that cell alone.
#[test]
fn only_a_cell_that_drove_a_routed_case_judges_and_reads_the_exchanges_once() {
    let wiring = Wiring::default();
    let reads = &Cell::new(0);
    let count = |n: usize| {
        move || {
            reads.set(reads.get() + 1);
            n
        }
    };
    assert_eq!(wiring.after_cell(false, true, count(0), INGRESS, BACKEND), None);
    assert_eq!(wiring.after_cell(true, false, count(0), INGRESS, BACKEND), None);
    assert_eq!(wiring.refusal(), None);
    assert_eq!(reads.get(), 0);
    assert!(wiring.after_cell(true, true, count(0), INGRESS, BACKEND).is_some());
    assert_eq!(wiring.after_cell(true, true, count(0), INGRESS, BACKEND), None);
    assert_eq!(reads.get(), 1);
}

/// The cell line the batch writes opens with the prefix the batch client
/// stops on (`replay-batch.ts` `MISWIRED`).
#[test]
fn the_cell_line_opens_with_the_prefix_the_client_stops_on() {
    let why = Wiring::default().after_cell(true, true, || 0, INGRESS, BACKEND).expect("named");
    let line = Failure::Environment(why).to_string();
    assert!(line.starts_with("environment: lane wiring: "), "{line}");
}
