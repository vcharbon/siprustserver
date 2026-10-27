//! A fork straggler's 2xx (RFC 3261 §13.2.2.4) through the real turn: the
//! dialog its release retires keeps the requests still open in it, whose
//! finals still reach their originators (§8.1.3.3), whichever rule sends the
//! release; a fork heard only after the winner's 2xx opens no dialog, so its
//! 2xx is a straggler's.

use b2bua_core::rules::{Match, RuleAction, RuleDefinition, RuleHandleResult, SERVICE_LAYER};
use call::helpers::records_of;
use call::{Call, Dialog, Direction};
use sip_message::Method;

use crate::turn_lane::drive::{dialled, run, Step};

/// A service rule tracking the early dialog of every 1xx to the b-leg INVITE
/// (`TrackEarlyDialog`), whatever the leg's state.
fn tracks_every_1xx() -> RuleDefinition {
    RuleDefinition::core(
        "test-track-every-1xx",
        SERVICE_LAYER,
        &[],
        Match::response().method("INVITE").status_class(1).direction(Direction::FromB),
        |ctx| {
            Some(RuleHandleResult::new(vec![RuleAction::TrackEarlyDialog {
                leg_id: ctx.source_leg_id.to_string(),
                b_tag: ctx.response()?.to().tag()?.to_string(),
            }]))
        },
    )
}

/// A service rule releasing every 2xx to the b-leg INVITE as a straggler's
/// (`ReleaseStraggler2xx`), whatever dialog it names.
fn releases_every_2xx() -> RuleDefinition {
    RuleDefinition::core(
        "test-release-every-2xx",
        SERVICE_LAYER,
        &[],
        Match::response().method("INVITE").status_class(2).direction(Direction::FromB),
        |ctx| {
            Some(RuleHandleResult::new(vec![RuleAction::ReleaseStraggler2xx {
                leg_id: ctx.source_leg_id.to_string(),
            }]))
        },
    )
}

/// `call`'s b-leg dialog under the callee tag `tag`.
fn fork(call: &Call, tag: &str) -> Option<Dialog> {
    let leg = call::helpers::find_leg(call, "b-1")?;
    call::helpers::callee_dialog(leg, tag).cloned()
}

/// The methods of the records `d` holds open.
fn open_methods(d: &Dialog) -> Vec<&str> {
    records_of(d).iter().map(|r| r.method.as_str()).collect()
}

/// `ReleaseStraggler2xx` sent on an early dialog whose PRACK awaits its final:
/// the retired dialog holds the PRACK among its open requests beside its BYE,
/// and bob's 200 settles it.
#[test]
fn a_straggler_released_from_its_early_dialog_keeps_its_open_prack() {
    let dialled = dialled().with_rule(releases_every_2xx());
    let script = [Step::BobAnswersReliably(183), Step::BobAnswers(200)];
    let released = run(&dialled, &script, usize::MAX, &[]);
    assert_eq!(released.sent, ["b-1 PRACK", "b-1 ACK", "b-1 BYE"], "PRACKed, then released");
    let d = fork(&released.call, "bobtag").expect("the fork's dialog");
    assert!(d.retired().is_some(), "the release retired the dialog");
    assert_eq!(open_methods(&d), ["PRACK", "BYE"], "the PRACK stays open beside the BYE");
    assert!(d.requests.is_empty(), "no record is left where a retired dialog is not read");

    let script = [script.as_slice(), &[Step::BobAnswersLast(Method::Prack, 200)]].concat();
    let settled = run(&dialled, &script, usize::MAX, &[]);
    let d = fork(&settled.call, "bobtag").expect("the fork's dialog");
    assert_eq!(open_methods(&d), ["BYE"], "bob's 200 settled the PRACK");
}

/// A fork that rings only after the winner's 2xx confirmed the leg: its
/// provisional, reliable or not, opens no dialog (no rule relays a 1xx on a
/// confirmed leg, and the INVITE it answers has its final), so its 2xx names
/// none and is released as a straggler's.
#[test]
fn a_fork_ringing_after_the_winner_answered_opens_no_dialog() {
    let mut dialled = dialled();
    dialled.call.sampled = Some(true);
    let answered = [Step::BobAnswers(180), Step::BobAnswers(200), Step::AliceAcks];
    for late in [Step::ForkAnswers("latefork", 180), Step::ForkAnswersReliably("latefork", 183)] {
        let script = [answered.as_slice(), std::slice::from_ref(&late)].concat();
        let rang = run(&dialled, &script, usize::MAX, &[]);
        assert_eq!(fork(&rang.call, "latefork"), None, "{late:?} opens no dialog");
        assert_eq!(rang.sent, ["b-1 ACK"], "{late:?} draws nothing");
    }

    let script = [
        answered.as_slice(),
        &[Step::ForkAnswers("latefork", 180), Step::ForkAnswers("latefork", 200)],
    ]
    .concat();
    let answered_late = run(&dialled, &script, usize::MAX, &[]);
    let last = answered_late.turns.last().expect("the late 2xx takes a turn");
    assert!(
        last.contains("release-fork-straggler-2xx"),
        "the late fork's 2xx is a straggler's: {last}"
    );
}

/// A fork a service tracked from a 1xx heard after the winner's 2xx holds an
/// early dialog on the confirmed leg: its 2xx is still a straggler's.
#[test]
fn a_late_fork_a_service_tracked_is_released_as_a_straggler() {
    let mut dialled = dialled().with_rule(tracks_every_1xx());
    dialled.call.sampled = Some(true);
    let script = [
        Step::BobAnswers(180),
        Step::BobAnswers(200),
        Step::AliceAcks,
        Step::ForkAnswers("latefork", 180),
        Step::ForkAnswers("latefork", 200),
    ];
    let tracked = run(&dialled, &script[..4], usize::MAX, &[]);
    let d = fork(&tracked.call, "latefork").expect("the service tracked the late fork");
    assert!(d.awaits_invite_final(), "an early dialog on the confirmed leg: {:?}", d.state);
    let answered_late = run(&dialled, &script, usize::MAX, &[]);
    let last = answered_late.turns.last().expect("the late 2xx takes a turn");
    assert!(
        last.contains("release-fork-straggler-2xx"),
        "the tracked fork's 2xx is a straggler's: {last}"
    );
}
