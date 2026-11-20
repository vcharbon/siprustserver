//! Lint of a document's claims (§10), on the base document of
//! [`super::lint`]: the record expectation stated as a `call-records` claim is
//! the expectation, stated once and as a `{ count, checks }`.

use serde_json::json;

use super::lint::{assert_clean, assert_fires};

#[test]
fn a_call_records_claim_states_the_record_expectation() {
    let claimed = |d: &mut serde_json::Value| {
        d["postconditions"] = json!({ "claims": { "call-records": { "count": 1 } } });
    };
    assert_clean("postconditions/cdr-absent-needs-reason", claimed);
    assert_clean("postconditions/call-records-claim", claimed);
}

#[test]
fn a_call_records_claim_stated_twice_is_refused() {
    assert_fires("postconditions/call-records-claim", |d| {
        d["postconditions"] =
            json!({ "cdr": { "count": 1 }, "claims": { "call-records": { "count": 1 } } });
    });
}

#[test]
fn a_call_records_claim_of_another_shape_is_refused() {
    assert_fires("postconditions/call-records-claim", |d| {
        d["postconditions"] = json!({ "claims": { "call-records": { "lines": 1 } } });
    });
}

/// An accessor in the call-records claim is resolved like any other; another
/// collector's claim is opaque to the run, so its strings are never read as
/// accessors.
#[test]
fn only_the_call_records_claim_is_scanned_for_accessors() {
    assert_fires("accessor/leg-unknown", |d| {
        d["postconditions"] = json!({ "claims": { "call-records": {
            "count": 1, "checks": [{ "field": "d", "op": "eq", "value": "${leg:Z.call-id}" }] } } });
    });
    assert_clean("accessor/leg-unknown", |d| {
        d["postconditions"] = json!({ "claims": {
            "call-records": { "count": 1 },
            "call-traces": { "match": "${leg:Z.call-id}" } } });
    });
}

#[test]
fn a_call_records_claim_check_holds_the_check_shape() {
    assert_fires("checks/value-mismatch", |d| {
        d["postconditions"] = json!({ "claims": { "call-records": {
            "count": 1, "checks": [{ "field": "d", "op": "exists", "value": "x" }] } } });
    });
}
