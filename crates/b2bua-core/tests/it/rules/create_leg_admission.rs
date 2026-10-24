//! The rule-driven `create-leg` destination gate and the refusals of a leg
//! that cannot be built.
//!
//! The `apply_route` decision-boundary gate is wired-tested e2e in
//! `b2bua-harness/tests/destination_allowlist_gate.rs`; this pins the OTHER
//! admission site — the rule-path `ActionExecutor::CreateLeg` branch a service
//! reaches in production via REFER (`transfer-http-allow`) or the announcement
//! service when call-control hands back a bogus transfer/media-server host.
//! Driving `ActionExecutor` directly reaches the reject branch that no harness
//! create-leg exercises (every one routes to the IP literal `127.0.0.1`, which
//! classifies `IpLiteral` and never rejects).
//!
//! A refused create-leg emits NO b-leg outbound, ends the call gracefully, and
//! writes a `Reject` CDR carrying `admission_reject host=<host>` (`HandlerEffects`
//! has no span channel). IP literals and the `["*"]` wildcard admit regardless
//! of the suffix list.

use super::*;
use call::{CallModelState, CdrEventType};

/// One `create-leg` action toward `host:port`, all overrides at their
/// default.
fn create_leg(host: &str, port: u16) -> RuleAction {
    RuleAction::CreateLeg {
        destination: (host.into(), port),
        new_ruri: None,
        new_from: None,
        new_to: None,
        callback_context: None,
        body_override: None,
        header_updates: vec![],
        header_adds: vec![],
        kind: None,
        attempt: None,
    }
}

/// Run a single `create-leg` from the a-leg under `config`'s suffix list and
/// return the [`HandlerResult`]. A re-INVITE-shaped a-leg event is enough — the
/// gate only reads `destination` + the config.
fn run_create_leg(config: &B2buaConfig, host: &str, port: u16) -> HandlerResult {
    let call = test_call();
    let reinvite = invite();
    let event = CallEvent::sip(SipMessage::Request(reinvite), "127.0.0.1:5060".parse().unwrap());
    let mut id_gen = scope(1);
    let now_ms = 1_700_000_000_000;
    let mut exec = ActionExecutor {
        owners: b2bua_core::owners::Owners::none(),
        config,
        turn: &mut id_gen,
        now_ms,
    };
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 1_700_000_000_000,
        config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
        copy_served: false,
    };
    exec.execute(&[create_leg(host, port)], &call, &ctx)
}

// A rule routing to a non-IP non-suffixed host is rejected — no
// b-leg outbound, the call ends with nothing left to resolve, and a Reject CDR
// records the cause.
#[test]
fn create_leg_to_non_allow_listed_host_is_rejected() {
    let config = B2buaConfig::default(); // default suffix list [".svc.cluster.local"]
    let result = run_create_leg(&config, "kindlab", 5060);

    assert!(
        result.effects.outbound.is_empty(),
        "a rejected create-leg must emit no b-leg outbound (host never reaches the send path)"
    );
    assert!(result.call.b_legs.is_empty(), "no b-leg state is allocated on admission reject");
    let result = invariants::finalize(result);
    assert_eq!(result.call.state, CallModelState::Terminated, "the call is torn down");
    let reject = result
        .call
        .cdr_events
        .iter()
        .find(|e| e.event_type == CdrEventType::Reject)
        .expect("admission reject writes a Reject CDR event");
    assert_eq!(reject.status_code, Some(503));
    assert_eq!(
        reject.reason.as_deref(),
        Some("admission_reject host=kindlab"),
        "the Reject CDR carries the admission cause + host"
    );
}

/// A refused create-leg on an answered call ends it on the wire under the
/// `Admission` cause: each confirmed leg is BYEd, each ringing leg CANCELled,
/// and the rest of the rule's list (here a timer arm) does not run on the call
/// it left terminating.
#[test]
fn create_leg_refused_mid_call_ends_every_leg_on_the_wire() {
    let refusals = [
        ("admission", create_leg("kindlab", 5060)),
        (
            "unreadable address",
            RuleAction::CreateLeg {
                destination: ("10.0.1.5".into(), 5060),
                new_ruri: None,
                new_from: Some("not a uri".into()),
                new_to: None,
                callback_context: None,
                body_override: None,
                header_updates: vec![],
                header_adds: vec![],
                kind: None,
                attempt: None,
            },
        ),
    ];
    for (refusal, action) in refusals {
        let mut call = test_call();
        confirm_a(&mut call);
        let mut b = b_leg_pending();
        call::testkit::stage_leg(&mut b, LegState::Confirmed);
        with_dialog(&mut b, "btag");
        call = call::helpers::add_b_leg(call, b);
        let mut ringing =
            Leg { leg_id: "b-2".into(), call_id: "b2cid@x".into(), ..b_leg_pending() };
        // The INVITE `b-2` rings on, which its CANCEL names (RFC 3261 §9.1).
        let dest = call::HostPort { host: "10.0.0.2".into(), port: 5070 };
        let handle = call::SentRequest {
            branch: "z9hG4bK-b2".to_string(),
            image: invite().image().to_vec(),
            destination: dest,
        };
        call::testkit::dial_with(&mut ringing, handle);
        call = call::helpers::add_b_leg(call, ringing);
        let guard = TimerKey::call(TimerType::ReferOverallSafety);
        let result =
            execute_on(&call, &[action, RuleAction::arm(guard.clone(), TimerDelay::millis(5_000))]);

        let requests: Vec<(Method, &str)> = result
            .effects
            .outbound
            .iter()
            .filter_map(|e| match &e.body {
                b2bua_core::effects::OutboundBody::Request(r) => {
                    Some((r.method().clone(), e.leg_id.as_deref()?))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            requests,
            [(Method::Bye, "a"), (Method::Bye, "b-1"), (Method::Cancel, "b-2")],
            "{refusal}: every confirmed leg is BYEd, every ringing one CANCELled",
        );
        assert_eq!(result.call.b_legs.len(), 2, "{refusal}: no leg is created");
        assert_eq!(result.call.state, CallModelState::Terminating, "{refusal}");
        assert_eq!(
            result.call.termination.as_ref().map(|t| t.cause),
            Some(TerminationCause::Admission),
            "{refusal}",
        );
        assert!(
            !result.call.timers.iter().any(|t| t.key == guard),
            "{refusal}: the rest of the rule's list does not run: {:?}",
            result.call.timers,
        );
    }
}

// A create-leg to an IP literal is admitted regardless of the suffix
// list — the b-leg INVITE is emitted and the call is NOT terminated.
#[test]
fn create_leg_to_ip_literal_is_admitted_regardless_of_suffix_list() {
    let config = B2buaConfig::default(); // 10.0.1.5 ∉ [".svc.cluster.local"] but is an IP
    let result = run_create_leg(&config, "10.0.1.5", 5060);

    assert!(
        !result.effects.outbound.is_empty(),
        "an IP-literal create-leg is admitted: the b-leg INVITE is emitted"
    );
    assert_eq!(result.call.b_legs.len(), 1, "the b-leg is created");
    assert_ne!(
        result.call.state,
        CallModelState::Terminated,
        "an admitted create-leg does not terminate the call"
    );
    assert!(
        !result.call.cdr_events.iter().any(|e| e.event_type == CdrEventType::Reject),
        "no Reject CDR on an admitted create-leg"
    );
}

// The `*` wildcard in the allow-list lets any host through, the non-IP
// `kindlab` included.
#[test]
fn create_leg_to_any_host_is_admitted_under_wildcard_allow_list() {
    let config =
        B2buaConfig { worker_allowed_target_suffixes: vec!["*".into()], ..Default::default() };
    let result = run_create_leg(&config, "kindlab", 5060);

    assert!(
        !result.effects.outbound.is_empty(),
        "the wildcard admits even a non-IP host: the b-leg INVITE is emitted"
    );
    assert_eq!(result.call.b_legs.len(), 1, "the b-leg is created under the wildcard");
    assert_ne!(
        result.call.state,
        CallModelState::Terminated,
        "a wildcard-admitted create-leg does not terminate the call"
    );
}
