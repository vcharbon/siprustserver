//! A 481 ends the dialog it answers in (RFC 3261 §12.2.1.2): `handle-481`
//! ends the call only on a 481 from a leg not terminated, in a dialog early
//! or confirmed. In a dialog our BYE is ending or has ended, it denies
//! nothing and no rule tears the session down.

use call::helpers::Resolved;
use call::{DialogEnd, DialogId};

use super::fixtures::*;
use super::*;

/// An Active call whose callee leg `b-1` holds `dialogs`, each `(peer tag,
/// state)` under this stack's tag `svc`.
fn call_with_callee_dialogs(dialogs: &[(&str, DialogState)]) -> call::Call {
    let mut call = test_call();
    confirm_a(&mut call);
    let mut leg = b_leg_pending();
    for (peer, state) in dialogs {
        leg.dialogs.insert(Dialog::new("svc", *peer, state.clone(), 1));
    }
    if let Some(invite) = leg.outbound_invite_mut() {
        invite.record_final(200);
    }
    call::helpers::add_b_leg(&mut call, leg);
    assert_eq!(call.state, CallModelState::Active);
    call
}

/// The rules ranked for bob's 481 to an INFO in his dialog `peer_tag` of
/// `call`, and whether any of them ends the session.
fn info_481_in(call: &call::Call, peer_tag: &str) -> (Vec<&'static str>, bool) {
    let raw = format!(
        "SIP/2.0 481 Call/Transaction Does Not Exist\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKinfo\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag={peer_tag}\r\n\
Call-ID: bcid@x\r\n\
CSeq: 2 INFO\r\n\
Content-Length: 0\r\n\r\n"
    );
    let resp = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    };
    let event = CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let resolved = Resolved {
        dialog: Some(DialogId { our_tag: "svc".into(), peer_tag: peer_tag.into() }),
        record: None,
    };
    let ctx = context(call, &event, "b-1", &config).resolving(&resolved);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let ends = ranked.iter().filter_map(|r| (r.handle)(&ctx)).any(|outcome| {
        outcome.actions.iter().any(|a| {
            matches!(a, RuleAction::BeginTermination { .. } | RuleAction::TerminateCall { .. })
        })
    });
    (ranked.iter().map(|r| r.id).collect(), ends)
}

/// Bob's 481 in the confirmed dialog of a live leg denies it: `handle-481`.
#[test]
fn a_481_in_a_confirmed_dialog_ends_the_call() {
    let call = call_with_callee_dialogs(&[("bobtag", DialogState::Confirmed)]);
    let (ranked, ends) = info_481_in(&call, "bobtag");
    assert_eq!(ranked.first().copied(), Some("handle-481"));
    assert!(ends, "the denied dialog ends the session");
}

/// Bob's 481 in the dialog our BYE is ending: his dialog is gone, ours is
/// going, and the call stays up.
#[test]
fn a_481_in_a_dialog_our_bye_is_ending_tears_nothing_down() {
    let call = call_with_callee_dialogs(&[("bobtag", DialogState::Ending(EndingBy::Bye))]);
    let (ranked, ends) = info_481_in(&call, "bobtag");
    assert!(!ranked.contains(&"handle-481"), "got {ranked:?}");
    assert!(!ends, "no rule ends the session on {ranked:?}");
}

/// Bob's 481 in a dialog of his our BYE ended, while another of his dialogs
/// is confirmed: the leg stands, the 481 names the ended dialog, and the
/// call stays up.
#[test]
fn a_481_in_an_ended_dialog_of_a_live_leg_tears_nothing_down() {
    let call = call_with_callee_dialogs(&[
        ("bobtag", DialogState::Confirmed),
        ("oldtag", DialogState::Ended(DialogEnd::OurBye { final_status: Some(200) })),
    ]);
    assert_eq!(call.b_legs[0].state(), LegState::Confirmed);
    let (ranked, ends) = info_481_in(&call, "oldtag");
    assert!(!ranked.contains(&"handle-481"), "got {ranked:?}");
    assert!(!ends, "no rule ends the session on {ranked:?}");
}
