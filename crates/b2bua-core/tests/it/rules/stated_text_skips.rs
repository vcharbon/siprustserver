//! Stated text the wire cannot carry whole (ADR-0053) never costs the message
//! it rides on: an authored final still reaches the caller, its reason phrase
//! the status's default and the line left out; a BYE still leaves its leg
//! without the line. Each skip is reported for the model-defect count.

use super::body_descriptors::{bridged, sent_request, values};
use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;

/// The value that would start a header line of its own.
const SPLIT: &str = "ok\r\nVia: SIP/2.0/UDP 192.0.2.66:5060;branch=z9hG4bKinjected";

/// How many stated-text skips `result` reports.
fn skips(result: &HandlerResult) -> usize {
    result
        .effects
        .buffered
        .iter()
        .filter(|e| matches!(e, BufferedObservabilityEffect::StatedTextSkipped { .. }))
        .count()
}

/// The final the caller was sent.
fn final_to_a(result: &HandlerResult) -> sip_message::SipResponse {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("a"))
        .find_map(|e| match &e.body {
            OutboundBody::Response(r) if r.status() >= 200 => Some(r.clone()),
            _ => None,
        })
        .expect("a final to the caller")
}

#[test]
fn an_authored_final_with_a_line_break_still_reaches_the_caller() {
    let result = execute_on(
        &test_call(),
        &[RuleAction::RespondToALeg {
            status: 486,
            reason: format!("Busy{SPLIT}"),
            header_updates: vec![
                ("X-Split".into(), Some(SPLIT.into())),
                ("X-Kept".into(), Some("fine".into())),
            ],
            contacts: vec![],
        }],
    );
    let resp = final_to_a(&result);
    assert_eq!(resp.status(), 486);
    assert_eq!(resp.reason(), "Busy Here", "the status's default phrase");
    assert_eq!(resp.raw(HeaderName::from("X-Kept")).collect::<Vec<_>>(), ["fine"]);
    assert_eq!(resp.raw(HeaderName::from("X-Split")).count(), 0);
    assert_eq!(resp.via().len(), 1);
    assert_eq!(skips(&result), 2, "the phrase and the line");
}

#[test]
fn the_stacks_reject_falls_back_to_the_default_phrase() {
    let call = test_call();
    let a_invite = crate::caller_of(&call);
    let result = b2bua_core::initial_invite::reject_call(
        call,
        &a_invite,
        503,
        Some(format!("Down{SPLIT}")),
        None,
        &[],
        &sip_message::generators::CapabilitySet::default(),
        &mut scope(3).ids,
        0,
        call::TerminationCause::Admission,
    );
    let resp = final_to_a(&result);
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.reason(), "Service Unavailable");
    assert_eq!(skips(&result), 1);
}

#[test]
fn a_bye_with_a_line_break_in_a_stated_line_still_leaves() {
    let result = execute_on(
        &bridged(),
        &[RuleAction::SendRequestToLeg {
            leg_id: "b-1".into(),
            method: "BYE".into(),
            body: None,
            headers: vec![
                ("Reason".into(), format!("Q.850;cause=16{SPLIT}")),
                ("X-Kept".into(), "fine".into()),
            ],
        }],
    );
    let bye = sent_request(&result, "b-1");
    assert_eq!(bye.method(), "BYE");
    assert_eq!(values(&bye, "X-Kept"), ["fine"]);
    assert!(values(&bye, "Reason").is_empty());
    assert_eq!(bye.via().len(), 1);
    assert_eq!(skips(&result), 1);
}

#[test]
fn a_service_reported_skip_is_counted() {
    let result = execute_on(
        &test_call(),
        &[RuleAction::ReportStatedTextSkipped { detail: "INFO part line".into() }],
    );
    assert_eq!(skips(&result), 1);
}
