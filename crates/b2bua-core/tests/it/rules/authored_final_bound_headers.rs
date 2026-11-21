//! A final the decision or a service authors for the caller's INVITE states no
//! header the transaction, a negotiation, an authentication exchange, a dialog
//! reference or the body binds (RFC 3261 §16.6, §20): a set of one is left out
//! and reported. The header a final's status is defined by is its author's to
//! state: a 401's challenge (§21.4.2, §22.1), a 407's (§21.4.8, §22.3).

use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;
use b2bua_core::rules::RelayedFinal;
use call::header_update::{HeaderUpdate, SipHeaderUpdates};

const CHALLENGE: &str = "Digest realm=\"example.com\", nonce=\"5ca1ab1e\"";

/// The stated-text skips `result` reports.
fn skips(result: &HandlerResult) -> Vec<String> {
    result
        .effects
        .buffered
        .iter()
        .filter_map(|e| match e {
            BufferedObservabilityEffect::StatedTextSkipped { detail } => Some(detail.clone()),
            _ => None,
        })
        .collect()
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

fn lines(resp: &sip_message::SipResponse, name: &str) -> Vec<String> {
    resp.raw(HeaderName::from(name)).map(str::to_string).collect()
}

/// `status` authored by the decision with `header_updates` set.
fn authored(status: u16, header_updates: &[(&str, &str)]) -> HandlerResult {
    execute_on(
        &test_call(),
        &[RuleAction::RespondToALeg {
            status,
            reason: "Refused".into(),
            header_updates: header_updates
                .iter()
                .map(|(name, value)| (name.to_string(), Some(value.to_string())))
                .collect(),
            contacts: vec![],
        }],
    )
}

#[test]
fn a_decision_authored_reject_states_no_bound_header() {
    let result = authored(
        486,
        &[("Require", "zz-ext"), ("Authorization", "Digest username=\"u\""), ("X-Kept", "fine")],
    );
    let resp = final_to_a(&result);
    assert_eq!(resp.status(), 486);
    assert!(lines(&resp, "Require").is_empty(), "{:?}", resp.headers());
    assert!(lines(&resp, "Authorization").is_empty(), "{:?}", resp.headers());
    assert_eq!(lines(&resp, "X-Kept"), ["fine"]);
    let skips = skips(&result);
    assert_eq!(skips.len(), 2, "{skips:?}");
    assert!(skips.iter().any(|s| s.contains("Require")), "{skips:?}");
    assert!(skips.iter().any(|s| s.contains("Authorization")), "{skips:?}");
}

#[test]
fn a_401_the_decision_authors_states_its_challenge_and_a_403_none() {
    let challenged = authored(401, &[("WWW-Authenticate", CHALLENGE)]);
    assert_eq!(lines(&final_to_a(&challenged), "WWW-Authenticate"), [CHALLENGE]);
    assert!(skips(&challenged).is_empty(), "{:?}", skips(&challenged));

    let forbidden = authored(403, &[("WWW-Authenticate", CHALLENGE)]);
    assert!(lines(&final_to_a(&forbidden), "WWW-Authenticate").is_empty());
    assert_eq!(skips(&forbidden).len(), 1, "{:?}", skips(&forbidden));
}

/// `status` the stack rejects the caller's INVITE with, under the decision's
/// `header_updates` sets.
fn rejected(status: u16, header_updates: &[(&str, &str)]) -> HandlerResult {
    let call = test_call();
    let a_invite = crate::caller_of(&call);
    let updates: SipHeaderUpdates = header_updates
        .iter()
        .map(|(name, value)| (name.to_string(), HeaderUpdate::Set(vec![value.to_string()])))
        .collect();
    b2bua_core::initial_invite::reject_call(
        call,
        &a_invite,
        status,
        None,
        Some(&updates),
        &[],
        &sip_message::generators::CapabilitySet::default(),
        0,
        call::TerminationCause::Admission,
    )
}

#[test]
fn a_407_reject_states_its_proxy_challenge_and_a_486_no_bound_header() {
    let challenged = rejected(407, &[("Proxy-Authenticate", CHALLENGE)]);
    assert_eq!(lines(&final_to_a(&challenged), "Proxy-Authenticate"), [CHALLENGE]);
    assert!(skips(&challenged).is_empty(), "{:?}", skips(&challenged));

    let busy = rejected(486, &[("Replaces", "other@192.0.2.1;to-tag=t;from-tag=f")]);
    assert!(lines(&final_to_a(&busy), "Replaces").is_empty());
    let skips = skips(&busy);
    assert_eq!(skips.len(), 1, "{skips:?}");
    assert!(skips[0].contains("Replaces"), "{skips:?}");
}

#[test]
fn a_service_authored_answer_states_no_bound_header() {
    let call = call_with_early_a_dialog("A1early");
    let event = CallEvent::sip(SipMessage::Request(invite()), "127.0.0.1:5060".parse().unwrap());
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &B2buaConfig::default(),
        &mut scope(1),
        &[RuleAction::AnswerALegNewDialog {
            status: 200,
            reason: "OK".into(),
            body: None,
            to_tag: None,
            header_updates: vec![
                ("Require".into(), Some("zz-ext".into())),
                ("X-Kept".into(), Some("fine".into())),
            ],
            relayed: RelayedFinal::none(),
        }],
    );
    let resp = final_to_a(&result);
    assert_eq!(resp.status(), 200);
    assert!(lines(&resp, "Require").is_empty(), "{:?}", resp.headers());
    assert_eq!(lines(&resp, "X-Kept"), ["fine"]);
    let skips = skips(&result);
    assert_eq!(skips.len(), 1, "{skips:?}");
    assert!(skips[0].contains("Require"), "{skips:?}");
}
