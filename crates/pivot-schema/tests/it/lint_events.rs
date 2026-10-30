//! Lint of a lane event a send awaits (§6.7g), on the base document of
//! [`super::lint`].

use serde_json::json;

use super::lint::{assert_clean, assert_fires};

#[test]
fn a_send_awaiting_a_lane_event_lints_clean() {
    let awaiting = |d: &mut serde_json::Value| {
        d["flow"][0]["awaits"] = json!("decision-request-in-flight");
    };
    assert_clean("awaits/on-expect", awaiting);
    assert_clean("awaits/empty", awaiting);
}

#[test]
fn an_expect_awaiting_a_lane_event_is_refused() {
    assert_fires("awaits/on-expect", |d| {
        d["flow"][1]["awaits"] = json!("decision-request-in-flight");
    });
}

#[test]
fn an_awaited_event_names_a_token() {
    assert_fires("awaits/empty", |d| d["flow"][0]["awaits"] = json!(" "));
}

#[test]
fn a_captured_document_awaits_no_lane_event() {
    assert_fires("subset/awaits", |d| {
        d["case"]["origin"] = json!("capture");
        d["flow"][0]["awaits"] = json!("decision-request-in-flight");
    });
}
