//! A client transaction's timeout on a call: `handle-timeout` on a pending,
//! cancelling, ending or ended leg, and the timeout of a non-INVITE request
//! record read by its origin and the method table (ADR-0055 §2).

use super::fixtures::*;
use super::*;
use b2bua_core::rule_fixture::{context, Varied};

/// The `handle-timeout` rule's output for a b-leg INVITE transaction timeout
/// naming `leg_id` on `call`.
fn handle_timeout_result(call: &call::Call, leg_id: &str) -> Vec<RuleAction> {
    timeout_result(call, leg_id, "INVITE")
}

/// The `handle-timeout` rule's output for a transaction timeout of `method`
/// on `leg_id` of `call`.
fn timeout_result(call: &call::Call, leg_id: &str, method: &str) -> Vec<RuleAction> {
    handled(call, leg_id, method, 1, Some("handle-timeout")).1
}

/// The rule that handles a timeout of the request `method` numbered `cseq` on
/// `leg_id` of `call`, and its actions: `only` that rule, or the first ranked
/// rule that takes it, as the engine selects.
fn handled(
    call: &call::Call,
    leg_id: &str,
    method: &str,
    cseq: u32,
    only: Option<&str>,
) -> (&'static str, Vec<RuleAction>) {
    let event = CallEvent::Timeout {
        branch: format!("z9hG4bK{leg_id}"),
        call_ref: Some(call.call_ref.clone()),
        leg_id: Some(leg_id.into()),
        method: Some(method.into()),
        cseq,
        destination: None,
        timeout_kind: call::TxnTimeoutKind::Response,
        incarnation: None,
    };
    // The engine settles the record the timeout names before the rules read it.
    let record = call::helpers::record_with_branch(call, leg_id, &format!("z9hG4bK{leg_id}"))
        .filter(|(_, r)| r.outbound_cseq == i64::from(cseq) && r.method == method)
        .map(Box::new);
    let resolved = call::helpers::Resolved { dialog: None, record };
    let config = B2buaConfig::default();
    let ctx = context(call, &event, leg_id, &config).resolving(&resolved);
    let rules = default_rules();
    pick_ranked(&rules, call, &ctx)
        .into_iter()
        .filter(|r| only.is_none_or(|id| r.id == id))
        .find_map(|r| (r.handle)(&ctx).map(|out| (r.id, out.actions)))
        .expect("a rule handles the timeout")
}

#[test]
fn invite_timeout_on_a_cancelling_leg_resolves_locally_without_consult() {
    // A pending b-leg whose CANCEL is in flight owes nothing to its dead
    // INVITE transaction: the timeout resolves the leg locally — no
    // /call/failure consult, no BeginTermination re-arm of the safety timer.
    let mut call = test_call();
    call.callback_context = Some("cb".into());
    call.state = CallModelState::Terminating;
    let mut b = b_leg_pending();
    cancel_sent(&mut b);
    call = call::helpers::add_b_leg(call, b);
    let actions = handle_timeout_result(&call, "b-1");
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::TerminateLeg { leg_id, end: LegEnd::Abandon }] if leg_id == "b-1"
        ),
        "going-away leg resolves locally on its timeout, got {actions:?}",
    );
}

/// The `handle-timeout` rule's output for a transaction timeout of `method`
/// on the a-leg of `call`.
fn a_leg_timeout_result(call: &call::Call, method: &str) -> Vec<RuleAction> {
    timeout_result(call, "a", method)
}

/// A terminating call's leg awaiting the answer to its BYE (`ByeSent`) is
/// resolved by the BYE's own transaction timeout, and by nothing else: the
/// liveness OPTIONS whose silence opened the teardown times out later (Timer
/// F) while the BYE may still be answered.
#[test]
fn only_the_bye_timeout_resolves_a_bye_sent_leg() {
    let mut call = test_call();
    call.state = CallModelState::Terminating;
    confirm_a(&mut call);
    end_dialogs(&mut call.a_leg, DialogState::Ending(EndingBy::Bye));

    let actions = a_leg_timeout_result(&call, "OPTIONS");
    assert!(
        actions.is_empty(),
        "an OPTIONS timeout resolves nothing on a BYE-sent leg, got {actions:?}"
    );

    let actions = a_leg_timeout_result(&call, "BYE");
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::TerminateLeg { leg_id, end: LegEnd::OurByeTimedOut }] if leg_id == "a"
        ),
        "the BYE's timeout resolves the leg as a BYE timeout, got {actions:?}",
    );
}

/// A leg already ended keeps its end when a later request's transaction on
/// it times out (a NOTIFY on a subscription usage that outlived the dialog,
/// RFC 5057 §5.2): it is re-terminated with nothing more to record, which
/// answers what is still pending toward it. A leg whose CANCEL is in flight,
/// or a going-away leg with no end yet, is abandoned.
#[test]
fn a_timeout_on_an_ended_leg_keeps_its_disposition() {
    for ended in [
        DialogState::Ended(call::DialogEnd::PeerBye),
        DialogState::Ended(call::DialogEnd::OurBye { final_status: Some(200) }),
        DialogState::Ended(call::DialogEnd::OurBye { final_status: None }),
        DialogState::Ended(call::DialogEnd::Rejected(487)),
    ] {
        let mut call = test_call();
        confirm_a(&mut call);
        end_dialogs(&mut call.a_leg, ended.clone());
        if matches!(ended, DialogState::Ended(call::DialogEnd::Rejected(_))) {
            *call.a_leg.invite_final_sent_mut() = Some(487);
        }
        let actions = a_leg_timeout_result(&call, "NOTIFY");
        assert!(
            matches!(
                actions.as_slice(),
                [RuleAction::TerminateLeg { leg_id, end: LegEnd::Settle }] if leg_id == "a"
            ),
            "{ended:?}: re-terminated, end kept, got {actions:?}",
        );
    }
    let mut call = test_call();
    call.state = CallModelState::Terminating;
    let mut b = b_leg_pending();
    cancel_sent(&mut b);
    call = call::helpers::add_b_leg(call, b);
    let actions = handle_timeout_result(&call, "b-1");
    assert!(
        matches!(actions.as_slice(), [RuleAction::TerminateLeg { end: LegEnd::Abandon, .. }]),
        "a CANCEL in flight is still settled by the timeout, got {actions:?}",
    );
    let mut call = test_call();
    call.state = CallModelState::Terminating;
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let actions = handle_timeout_result(&call, "b-1");
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::TerminateLeg { leg_id, end: LegEnd::Abandon }] if leg_id == "b-1"
        ),
        "a going-away pending leg with no end is abandoned, got {actions:?}",
    );
}

#[test]
fn invite_timeout_on_a_live_pending_leg_still_consults() {
    // The going-away guard is narrow: a live pending b-leg's transaction
    // timeout keeps the failover consult.
    let mut call = test_call();
    call.callback_context = Some("cb".into());
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let actions = handle_timeout_result(&call, "b-1");
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::FailureAsyncHttp { .. })),
        "live pending b-leg timeout consults /call/failure, got {actions:?}",
    );
}

/// The caller's confirmed call, its dialog holding the request record of
/// `method` sent with CSeq `cseq` on the branch a timeout of the a-leg names,
/// relayed or this stack's own.
fn answered_call(method: &str, cseq: i64, origin: call::RequestOrigin) -> call::Call {
    let mut call = test_call();
    call.state = CallModelState::Active;
    let mut d = a_dialog(&call, "atag", DialogState::Confirmed);
    let sent = call::SentRequest {
        branch: "z9hG4bKa".into(),
        image: Vec::new(),
        destination: call::HostPort { host: "127.0.0.1".into(), port: 5060 },
    };
    d.requests.push(call::RequestRecord {
        origin,
        sent: Some(sent),
        ..call::RequestRecord::relayed(method, cseq, 4)
    });
    call.a_leg.dialogs.insert(d);
    call.a_leg.own_tag = Some("atag".into());
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
    call
}

/// The timeout of a NOTIFY this stack sent itself on a live leg ends its
/// subscription alone: no rule ends the call (RFC 5057 §5.2).
#[test]
fn an_own_notify_timeout_leaves_the_call() {
    let call = answered_call("NOTIFY", 8, call::RequestOrigin::Own);
    let (rule, actions) = handled(&call, "a", "NOTIFY", 8, None);
    assert_eq!(rule, "handle-timeout");
    assert!(actions.is_empty(), "{actions:?}");
}

/// The timeout of a PRACK this stack sent itself denies that acknowledgement
/// alone, whatever the leg: the call stays as it was.
#[test]
fn an_own_prack_timeout_leaves_the_call() {
    let mut call = answered_call("PRACK", 8, call::RequestOrigin::Own);
    let (_, actions) = handled(&call, "a", "PRACK", 8, None);
    assert!(actions.is_empty(), "{actions:?}");
    call.state = CallModelState::Terminating;
    end_dialogs(&mut call.a_leg, DialogState::Ending(EndingBy::Bye));
    let (_, actions) = handled(&call, "a", "PRACK", 8, None);
    assert!(actions.is_empty(), "on a leg going away too: {actions:?}");
}

/// The timeout of an INFO this stack sent itself ends the session.
#[test]
fn an_own_info_timeout_ends_the_session() {
    let call = answered_call("INFO", 8, call::RequestOrigin::Own);
    let (_, actions) = handled(&call, "a", "INFO", 8, None);
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::BeginTermination {
                cause: TerminationCause::Timeout(call::TimeoutKind::Transaction),
                ..
            }]
        ),
        "{actions:?}",
    );
}

/// A relayed request's timeout is the relayed exchange's: its record is
/// settled with no 408 to the originator (RFC 4320 §4.2) and the session
/// stands (RFC 5057 §5.2), whatever the method.
#[test]
fn a_relayed_request_timeout_leaves_the_call() {
    for method in ["NOTIFY", "INFO", "UPDATE", "MESSAGE", "OPTIONS", "REFER", "PRACK"] {
        let call = answered_call(method, 7, call::RequestOrigin::Relayed);
        let (rule, actions) = handled(&call, "a", method, 7, None);
        assert_eq!(rule, "handle-timeout");
        assert!(actions.is_empty(), "{method}: {actions:?}");
    }
}

/// A NOTIFY of this stack timing out on a leg already ended keeps
/// `handle-timeout`'s re-termination, its end as it was.
#[test]
fn an_own_notify_timeout_on_an_ended_leg_is_handle_timeouts() {
    let mut call = answered_call("NOTIFY", 8, call::RequestOrigin::Own);
    end_dialogs(&mut call.a_leg, DialogState::Ended(call::DialogEnd::PeerBye));
    let (rule, actions) = handled(&call, "a", "NOTIFY", 8, None);
    assert_eq!(rule, "handle-timeout");
    assert!(
        matches!(actions.as_slice(), [RuleAction::TerminateLeg { end: LegEnd::Settle, .. }]),
        "{actions:?}",
    );
}
