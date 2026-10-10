//! The dialog gate's table (ADR-0041): which dialog state takes which request,
//! and what a caller-facing dialog the caller was never shown names.

use b2bua_sdk::event::CallEvent;
use call::{Call, CallModelState, Dialog, DialogEnd, DialogState, RetiredDialog, StragglerRelease};
use sip_message::{Method, SipMessage, SipRequest};

use super::dialog_gate::{admits, note_in_order, request_verdict, Verdict};
use super::test_support::{invite, src};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;

/// The caller's tag for the in-dialog requests below: the From-tag of `invite`.
const CALLER_TAG: &str = "alicetag";

fn dialog(state: DialogState) -> Dialog {
    Dialog::new("ours", CALLER_TAG, state, 1)
}

fn retired(release: bool) -> DialogState {
    let release = release.then(StragglerRelease::default);
    DialogState::Retired(RetiredDialog { open_relays: vec![], release })
}

const METHODS: [Method; 7] = [
    Method::Invite,
    Method::Bye,
    Method::Options,
    Method::Prack,
    Method::Info,
    Method::Update,
    Method::Ack,
];

/// The methods `state` takes on a call that lives, and on one being torn down.
fn taken(state: DialogState, call_ending: bool) -> Vec<Method> {
    let d = dialog(state);
    METHODS.into_iter().filter(|m| admits(Some(&d), m, call_ending)).collect()
}

#[test]
fn each_dialog_state_takes_the_requests_its_table_row_names() {
    let all = METHODS.to_vec();
    let ending = vec![Method::Bye, Method::Options, Method::Prack, Method::Ack];
    let by_bye = DialogEnd::OurBye { final_status: Some(200) };
    assert_eq!(taken(DialogState::Early, false), all, "early");
    assert_eq!(taken(DialogState::Confirmed, false), all, "confirmed");
    assert_eq!(taken(DialogState::Ending, false), ending, "ending");
    assert_eq!(
        taken(DialogState::Ended(DialogEnd::Rejected(487)), false),
        vec![Method::Prack],
        "a dialog its INVITE's non-2xx ended still takes the PRACK of what it showed"
    );
    assert_eq!(
        taken(DialogState::Ended(DialogEnd::PeerBye), false),
        vec![Method::Ack],
        "after the peer's BYE only the 2xx's ACK (RFC 5407 §2)"
    );
    assert_eq!(taken(DialogState::Ended(by_bye), false), vec![Method::Ack], "after our BYE");
    assert_eq!(taken(retired(false), false), Vec::<Method>::new(), "retired");
    assert_eq!(
        taken(retired(true), false),
        vec![Method::Bye],
        "a retired fork this stack released takes the BYE crossing the release"
    );
    assert!(!admits(None, &Method::Bye, false), "no dialog takes nothing");
}

#[test]
fn a_live_dialog_of_a_call_being_torn_down_reads_as_ending() {
    let ending = vec![Method::Bye, Method::Options, Method::Prack, Method::Ack];
    assert_eq!(taken(DialogState::Early, true), ending, "early");
    assert_eq!(taken(DialogState::Confirmed, true), ending, "confirmed");
}

/// A call whose caller-facing dialog `ours` is early, `shown` to the caller
/// or not.
fn call_with_caller_dialog(shown: bool) -> Call {
    let mut call = build_initial_call(
        &invite("w0", "w1", "gate"),
        src(),
        &B2buaConfig::default(),
        &sip_txn::IdGen::seeded(1),
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
    let inv = invite("w0", "w1", "gate");
    inv.thaw()
        .with_method(method.clone())
        .set(inv.to().clone().with_tag("ours"))
        .set(sip_message::header::CSeq::new(cseq, method))
        .freeze()
        .expect("an in-dialog request of the same dialog is complete")
}

#[test]
fn a_caller_facing_dialog_the_caller_was_never_shown_names_nothing() {
    let unshown = call_with_caller_dialog(false);
    assert_eq!(request_verdict(&unshown, "a", &caller_request(Method::Info, 2)), Verdict::NoDialog);
    assert_eq!(request_verdict(&unshown, "a", &caller_request(Method::Ack, 1)), Verdict::Drop);

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
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Request(req)),
        src: src(),
        matched_client_txn: false,
    };
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
