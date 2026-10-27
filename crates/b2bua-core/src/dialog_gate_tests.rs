//! The dialog gate's table (ADR-0041): which dialog state takes which request,
//! and what a caller-facing dialog the caller was never shown names.

use b2bua_sdk::event::CallEvent;
use call::{Call, CallModelState, Dialog, DialogEnd, DialogState, EndingBy, RetiredDialog};
use sip_message::{Method, SipMessage, SipRequest};

use super::{admission, note_in_order, request_verdict, Admission, Verdict};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::test_call::{invite, src};

/// The caller's tag for the in-dialog requests below: the From-tag of `invite`.
const CALLER_TAG: &str = "alicetag";

fn dialog(state: DialogState) -> Dialog {
    Dialog::new("ours", CALLER_TAG, state, 1)
}

/// A retired dialog; `Some(settled)`: this stack released it with a BYE,
/// its record open until that BYE settled.
fn retired(release: Option<bool>) -> DialogState {
    let bye = call::RequestRecord::own(
        "BYE",
        2,
        call::SentRequest {
            branch: "z9hG4bK-release".into(),
            image: Vec::new(),
            destination: call::HostPort { host: "10.0.0.2".into(), port: 5070 },
        },
        32_000,
        Default::default(),
    );
    let open_requests = if release == Some(false) { vec![bye] } else { vec![] };
    DialogState::Retired(RetiredDialog { open_requests })
}

const METHODS: [Method; 12] = [
    Method::Invite,
    Method::Bye,
    Method::Options,
    Method::Prack,
    Method::Info,
    Method::Message,
    Method::Notify,
    Method::Update,
    Method::Refer,
    Method::Subscribe,
    Method::Publish,
    Method::Ack,
];

/// What `state` makes of each method.
fn row(state: DialogState) -> Vec<(Method, Admission)> {
    let d = dialog(state);
    METHODS
        .into_iter()
        .map(|m| {
            let a = admission(Some(&d), &m);
            (m, a)
        })
        .collect()
}

/// The methods `state` makes `admission` of.
fn methods(state: DialogState, wanted: Admission) -> Vec<Method> {
    row(state).into_iter().filter(|(_, a)| *a == wanted).map(|(m, _)| m).collect()
}

#[test]
fn a_live_dialog_passes_every_request() {
    for state in [DialogState::Early, DialogState::Confirmed] {
        assert_eq!(methods(state.clone(), Admission::Pass), METHODS.to_vec(), "{state:?}");
    }
}

#[test]
fn an_ending_dialog_takes_the_crossing_bye_absorbs_reports_and_terminates_the_rest() {
    let ending = [
        DialogState::Ending(EndingBy::Bye),
        DialogState::Ending(EndingBy::Cancel),
        retired(Some(false)),
    ];
    for state in ending {
        let passed: Vec<_> = methods(state.clone(), Admission::Pass);
        assert_eq!(
            passed.iter().filter(|m| **m != Method::Ack).collect::<Vec<_>>(),
            [&Method::Bye, &Method::Prack],
            "{state:?}: the crossing BYE and the PRACK reach their rules"
        );
        assert_eq!(
            methods(state.clone(), Admission::Absorb),
            [Method::Options, Method::Info, Method::Message, Method::Notify],
            "{state:?}: answered 200 by the gate"
        );
        assert_eq!(
            methods(state.clone(), Admission::Terminate),
            [Method::Invite, Method::Update, Method::Refer, Method::Subscribe, Method::Publish],
            "{state:?}: answered 487 (RFC 3261 §21.4.25)"
        );
    }
}

#[test]
fn an_ended_or_retired_dialog_refuses_all_but_its_table_exceptions() {
    let refused_but = |state: DialogState, taken: &[Method]| {
        for (method, admission) in row(state.clone()) {
            let expected =
                if taken.contains(&method) { Admission::Pass } else { Admission::Refuse };
            assert_eq!(admission, expected, "{state:?} {method}");
        }
    };
    let by_our_bye = |final_status| DialogState::Ended(DialogEnd::OurBye { final_status });
    refused_but(DialogState::Ended(DialogEnd::Rejected(487)), &[Method::Prack]);
    refused_but(DialogState::Ended(DialogEnd::PeerBye), &[Method::Ack]);
    refused_but(by_our_bye(Some(200)), &[Method::Ack]);
    refused_but(by_our_bye(None), &[Method::Ack]);
    refused_but(retired(None), &[]);
    refused_but(retired(Some(true)), &[]);
    assert_eq!(admission(None, &Method::Bye), Admission::Refuse, "no dialog takes nothing");
}

/// A call whose caller-facing dialog `ours` is early, `shown` to the caller
/// or not.
fn call_with_caller_dialog(shown: bool) -> Call {
    let mut call = build_initial_call(
        &invite("gate"),
        src(),
        &B2buaConfig::default(),
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut d = dialog(DialogState::Early);
    d.shown = shown;
    d.peer_cseq = Some(1);
    call.a_leg.dialogs.insert(d);
    call.a_leg.own_tag = Some("ours".into());
    call
}

/// The caller's request of `method` and CSeq `cseq` naming the dialog `ours`.
fn caller_request(method: Method, cseq: u32) -> SipRequest {
    let inv = invite("gate");
    inv.thaw()
        .with_method(method.clone())
        .set(inv.to().clone().with_tag("ours"))
        .set(sip_message::header::CSeq::new(cseq, method))
        .freeze()
        .expect("an in-dialog request of the same dialog is complete")
}

/// A caller dialog never shown names nothing; an ACK naming the caller's
/// INVITE, which the record holds unanswered, is evidence of an answer the
/// record lacks (ADR-0055 §4) and passes with no dialog resolved, while one
/// naming another INVITE is dropped.
#[test]
fn a_caller_facing_dialog_the_caller_was_never_shown_names_nothing() {
    let unshown = call_with_caller_dialog(false);
    assert_eq!(request_verdict(&unshown, "a", &caller_request(Method::Info, 2)), Verdict::NoDialog);
    assert_eq!(
        request_verdict(&unshown, "a", &caller_request(Method::Ack, 1)),
        Verdict::Pass(call::helpers::Resolved::default())
    );
    assert_eq!(request_verdict(&unshown, "a", &caller_request(Method::Ack, 2)), Verdict::Drop);

    let shown = call_with_caller_dialog(true);
    let Verdict::Pass(resolved) = request_verdict(&shown, "a", &caller_request(Method::Info, 2))
    else {
        panic!("a shown early dialog takes an INFO");
    };
    assert_eq!(resolved.dialog.map(|d| d.our_tag), Some("ours".to_string()));
}

#[test]
fn a_request_below_its_dialogs_last_cseq_is_out_of_order() {
    let mut call = call_with_caller_dialog(true);
    call.a_leg.dialogs.by_tags_mut("ours", CALLER_TAG).expect("the dialog").peer_cseq = Some(5);
    assert_eq!(request_verdict(&call, "a", &caller_request(Method::Info, 4)), Verdict::OutOfOrder);
    assert!(matches!(
        request_verdict(&call, "a", &caller_request(Method::Info, 6)),
        Verdict::Pass(_)
    ));
}

/// The caller's last CSeq on `ours` once the gate passed `req`.
fn last_cseq_after(call: Call, req: SipRequest) -> Option<i64> {
    let Verdict::Pass(resolved) = request_verdict(&call, "a", &req) else {
        panic!("the gate passes {}", req.method());
    };
    let event = CallEvent::sip(SipMessage::Request(req), src());
    let call = note_in_order(call, "a", &resolved, &event);
    call.a_leg.dialogs.by_tags("ours", CALLER_TAG).expect("the dialog").peer_cseq
}

#[test]
fn a_request_the_gate_passes_sets_its_dialogs_last_cseq_and_an_ack_does_not() {
    let call = call_with_caller_dialog(true);
    assert_eq!(last_cseq_after(call.clone(), caller_request(Method::Info, 9)), Some(9));
    assert_eq!(last_cseq_after(call, caller_request(Method::Ack, 9)), Some(1));
}

#[test]
fn a_rejected_caller_dialog_refuses_her_bye_and_takes_her_prack() {
    let mut call = call_with_caller_dialog(true);
    call.a_leg.dialogs.by_tags_mut("ours", CALLER_TAG).expect("the dialog").state =
        DialogState::Ended(DialogEnd::Rejected(487));
    for state in [CallModelState::Terminating, CallModelState::Terminated] {
        call.state = state;
        let bye = request_verdict(&call, "a", &caller_request(Method::Bye, 2));
        assert_eq!(bye, Verdict::NoDialog, "{state:?}: the BYE names an ended dialog");
        let prack = request_verdict(&call, "a", &caller_request(Method::Prack, 2));
        assert!(matches!(prack, Verdict::Pass(_)), "{state:?}: the PRACK reaches its rules");
    }
}

#[test]
fn an_ending_dialog_answers_in_its_own_dialog_and_judges_the_order_first() {
    let mut call = call_with_caller_dialog(true);
    let d = call.a_leg.dialogs.by_tags_mut("ours", CALLER_TAG).expect("the dialog");
    d.state = DialogState::Ending(EndingBy::Bye);
    d.peer_cseq = Some(5);
    let named = |v: Verdict| match v {
        Verdict::Absorbed(r) | Verdict::Terminated(r) => r.dialog.map(|d| d.our_tag),
        other => panic!("answered by the gate, not {other:?}"),
    };
    let info = request_verdict(&call, "a", &caller_request(Method::Info, 6));
    assert!(matches!(info, Verdict::Absorbed(_)), "{info:?}");
    assert_eq!(named(info), Some("ours".to_string()));
    let update = request_verdict(&call, "a", &caller_request(Method::Update, 7));
    assert!(matches!(update, Verdict::Terminated(_)), "{update:?}");
    assert_eq!(named(update), Some("ours".to_string()));
    assert_eq!(request_verdict(&call, "a", &caller_request(Method::Info, 4)), Verdict::OutOfOrder);
}

/// A leg the record lacks — another owner dialled it — holds no dialog here:
/// its BYE draws a 481 and its ACK is dropped (RFC 3261 §12.2.2).
#[test]
fn a_request_on_a_leg_the_record_lacks_names_no_dialog() {
    let call = call_with_caller_dialog(true);
    assert_eq!(request_verdict(&call, "b-9", &caller_request(Method::Bye, 2)), Verdict::NoDialog);
    assert_eq!(request_verdict(&call, "b-9", &caller_request(Method::Ack, 1)), Verdict::Drop);
}
