//! A callee's provisional as the rules read it: under an answered caller, on a
//! confirmed b-leg, and a transfer target's repeat.

use super::fixtures::*;
use super::*;

#[test]
fn a_provisional_under_an_answered_caller_is_absorbed_with_what_the_leg_is_owed() {
    // The caller's INVITE carries its 200; a b-leg dialled later rings. The
    // relay rule relays nothing toward her and still gives the ringing leg
    // its due: its early dialog (a teardown then CANCELs it) and the CDR event.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    call = call::helpers::add_b_leg(call, b_leg_pending());

    let (id, actions) = handle_from_b1(&call, &b_leg_ringing_event(false));
    assert_eq!(id, "relay-provisional");
    assert!(
        !actions.iter().any(|a| matches!(a, RuleAction::RelayToPeer { .. })),
        "nothing is relayed toward the answered caller: {actions:?}"
    );
    assert!(
        actions.iter().any(|a| matches!(
            a,
            RuleAction::TrackEarlyDialog { leg_id, .. } if leg_id == "b-1"
        )),
        "the ringing leg's early dialog is tracked: {actions:?}"
    );
    assert!(
        actions.iter().any(|a| matches!(
            a,
            RuleAction::AddCdrEvent { event_type: call::CdrEventType::Provisional, leg_id, status_code: Some(180), .. }
                if leg_id == "b-1"
        )),
        "the provisional is accounted: {actions:?}"
    );
    // …and the early dialog the provisional establishes (RFC 3261 §12.1.2):
    // the callee's tag, its Contact as the remote target, the recorded route.
    let result = execute_from_b1(&call, &b_leg_ringing_event(false), &actions);
    let dialog = result.call.b_legs[0]
        .dialogs
        .by_tags("svc", "bobtag")
        .expect("the early dialog carries the callee's tag");
    assert_eq!(dialog.state, DialogState::Early);
    assert!(
        dialog.remote_target.contains(CALLEE_RINGING_CONTACT),
        "the remote target is the provisional's Contact: {}",
        dialog.remote_target
    );
    assert!(
        dialog.route_set.iter().any(|r| r.contains(CALLEE_RINGING_ROUTE)),
        "the route set is the provisional's recorded route: {:?}",
        dialog.route_set
    );
    assert_eq!(result.call.b_legs[0].state(), LegState::Early);
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

#[test]
fn a_repeated_transfer_target_provisional_is_accounted_once() {
    // The REFER machine's C leg repeats a 180 the referrer was already
    // notified of: the NOTIFY is deduped and so is the CDR event — the leg's
    // due (Early) still rides.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    let mut referrer = b_leg_pending();
    call::testkit::stage_leg(&mut referrer, LegState::Confirmed);
    with_dialog(&mut referrer, "reftag");
    call = call::helpers::add_b_leg(call, referrer);
    let mut target = b_leg_pending();
    target.leg_id = "b-2".into();
    call = call::helpers::add_b_leg(call, target);
    call.transfer = Some(call::TransferState {
        phase: call::TransferPhase::CRinging,
        referrer_leg_id: "b-1".into(),
        refer_to_uri: "sip:carol@10.0.0.3".into(),
        effective_refer_to_uri: None,
        callback_context: None,
        c_leg_id: Some("b-2".into()),
        refer_cseq: Some(2),
        notify_event: "refer".into(),
        started_at_ms: 0,
        last_c_leg_provisional_status: Some(180),
        c_initial_sdp: None,
    });
    b2bua_core::rules::refer_transfer::project_cursor(&mut call);

    let (id, actions) = handle_from(&call, &b_leg_ringing_event(false), "b-2");
    assert_eq!(id, "transfer-c-1xx-absorb");
    let accounted = actions
        .iter()
        .filter(|a| {
            matches!(a, RuleAction::AddCdrEvent { event_type: call::CdrEventType::Provisional, .. })
        })
        .count();
    assert_eq!(accounted, 0, "a repeat of a status is not accounted again: {actions:?}");
    assert!(
        !actions.iter().any(|a| matches!(a, RuleAction::SendNotify { .. })),
        "the repeat draws no NOTIFY: {actions:?}"
    );
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
