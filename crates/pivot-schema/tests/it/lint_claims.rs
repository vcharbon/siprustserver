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

#[test]
fn a_call_records_claim_check_holds_the_check_shape() {
    assert_fires("checks/value-mismatch", |d| {
        d["postconditions"] = json!({ "claims": { "call-records": {
            "count": 1, "checks": [{ "field": "d", "op": "exists", "value": "x" }] } } });
    });
}
