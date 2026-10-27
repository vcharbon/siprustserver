//! A callee's provisional as the rules read it: under an answered caller, on a
//! confirmed b-leg, and a transfer target's repeat.

use b2bua_core::turn_driver::{callee_tag, Driver, Session, Step};

use super::fixtures::*;
use super::*;

/// The record route a callee's provisional carries.
const RINGING_ROUTE: &str = "<sip:10.0.0.7:5060;lr>";

/// A call bridged to bob whose subscribed duration cap fired and whose
/// release consult rerouted it: the replacement leg `b-2` is dialled.
fn rerouted(driver: &Driver) -> Session<'_> {
    let mut call = crate::calls::bridged(driver);
    let reroute = serde_json::json!({
        "destination": {"host": "127.0.0.1", "port": 5071},
        "new_ruri": "sip:carol@127.0.0.1:5071",
    });
    call.run([
        Step::Fires(TimerType::GlobalDuration),
        Step::Internal {
            leg: "a",
            topic: "call-release-result",
            outcome: "reroute",
            payload: reroute,
        },
    ]);
    assert!(call.peer("b-2").is_some(), "the replacement leg is dialled");
    call
}

/// The caller's INVITE carries its 200; a b-leg dialled later — the release
/// reroute's replacement leg — rings. The relay rule relays nothing toward
/// her and still gives the ringing leg its due: its early dialog (a teardown
/// then CANCELs it) and the CDR event.
#[test]
fn a_provisional_under_an_answered_caller_is_absorbed_with_what_the_leg_is_owed() {
    let driver = Driver::new().with_route(|r| {
        r.subscriptions = vec![call::ReleaseEventKind::MaxCallDuration];
    });
    let mut call = rerouted(&driver);
    let accounted = call.call().cdr_events.len();
    let ringing = Step::AnswersWith("b-2", 180, vec![("Record-Route", RINGING_ROUTE)]);
    let turn = call.turned(ringing);
    assert_eq!(turn.rule, Some("relay-provisional"));
    assert!(turn.statuses_to("a").is_empty(), "nothing is relayed toward the answered caller");
    let record = call.call();
    let events = &record.cdr_events[accounted..];
    assert!(
        events.iter().any(|e| e.event_type == call::CdrEventType::Provisional
            && e.leg_id == "b-2"
            && e.status_code == Some(180)),
        "the provisional is accounted: {events:?}"
    );
    // …and the early dialog the provisional establishes (RFC 3261 §12.1.2):
    // the callee's tag, its Contact as the remote target, the recorded route.
    let leg = call::helpers::find_leg(record, "b-2").expect("the replacement leg");
    let dialog = leg.dialogs.iter().find(|d| d.peer_tag == callee_tag("b-2"));
    let dialog = dialog.expect("the early dialog carries the callee's tag");
    assert_eq!(dialog.state, DialogState::Early);
    let contact = b2bua_core::turn_driver::BOB;
    assert!(dialog.remote_target.contains(contact), "{}", dialog.remote_target);
    assert!(
        dialog.route_set.iter().any(|r| r.contains("10.0.0.7:5060")),
        "the route set is the provisional's recorded route: {:?}",
        dialog.route_set
    );
    assert_eq!(leg.state(), LegState::Early);
}

#[test]
fn a_bare_180_after_the_final_mints_no_mask_state() {
    // The bare-180 downgrade asked to ring an answered caller: refused at the
    // seam before the mask's own bookkeeping — no a-facing tag mapping, the
    // mask still unrelayed.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let event = b_leg_ringing_event(false);

    let result = execute_from_b1(
        &call,
        &event,
        &[RuleAction::RelayFirstBare180 { leg_id: "b-1".into(), b_tag: "bobtag".into() }],
    );
    assert!(provisionals_to_a(&result).is_empty(), "no 180 leaves: {:?}", result.effects.outbound);
    assert_eq!(late_provisional_refusals(&result), vec![(180, 200)]);
    let fronts: Vec<_> = result.call.a_leg.dialogs.iter().flat_map(|d| d.fronts.iter()).collect();
    assert!(fronts.is_empty(), "no caller-facing dialog fronts it: {fronts:?}");
    assert!(
        !call::helpers::relay_first_18x_first_relayed(&result.call),
        "the mask has relayed nothing"
    );
}

/// The REFER machine's C leg rings twice with the same status: the referrer
/// hears of no progress (RFC 3515 §3.10), the CDR accounts the status once,
/// and the leg's due (Early) still rides.
#[test]
fn a_repeated_transfer_target_provisional_is_accounted_once() {
    let driver = Driver::new();
    let mut call = crate::calls::bridged(&driver);
    let target = serde_json::json!({"destination": {"host": "127.0.0.1", "port": 5071}});
    call.run([
        Step::Sends("b-1", Method::Refer),
        Step::ReferAnswers { outcome: "allow", payload: target },
    ]);
    let provisionals = |c: &call::Call| {
        c.cdr_events.iter().filter(|e| e.event_type == call::CdrEventType::Provisional).count()
    };
    let before = provisionals(call.call());
    for ring in 0..2 {
        let turn = call.turned(Step::Answers("b-2", 180));
        assert_eq!(turn.rule, Some("transfer-c-1xx-absorb"));
        assert!(turn.request_to("b-1", "NOTIFY").is_none(), "ring {ring}: no NOTIFY of progress");
    }
    assert_eq!(provisionals(call.call()) - before, 1, "the status is accounted once");
    let leg = call::helpers::find_leg(call.call(), "b-2").expect("the target's leg");
    assert_eq!(leg.state(), LegState::Early, "the leg's due still rides");
}

#[test]
fn a_provisional_on_a_confirmed_b_leg_selects_no_relay_rule() {
    // A 1xx after the leg's own 2xx (a callee's late provisional, a fork the
    // proxy failed to cancel, a retransmission) answers a transaction that
    // took its final: no relay rule claims it, so nothing moves the leg back
    // to Early or rings the caller.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    let mut confirmed = b_leg_pending();
    call::testkit::stage_leg(&mut confirmed, LegState::Confirmed);
    with_dialog(&mut confirmed, "bobtag");
    call = call::helpers::add_b_leg(call, confirmed);

    let event = b_leg_ringing_event(false);
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "b-1", &config);
    let rules = default_rules();
    let ranked: Vec<&str> = pick_ranked(&rules, &call, &ctx).iter().map(|r| r.id).collect();
    assert!(ranked.is_empty(), "a provisional on a confirmed leg selects no rule, got {ranked:?}");
}
