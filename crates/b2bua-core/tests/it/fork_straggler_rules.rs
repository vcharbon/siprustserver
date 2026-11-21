//! A fork straggler's 2xx (RFC 3261 §13.2.2.4) through the real turn: the
//! dialog its release retires keeps the requests still open in it, whose
//! finals still reach their originators (§8.1.3.3), whichever rule sends the
//! release; a fork heard only after the winner's 2xx opens no dialog, or,
//! when that is a reliable provisional, one retired at once behind its PRACK,
//! so its 2xx is a straggler's.

use b2bua_core::rules::{Match, RuleAction, RuleDefinition, RuleHandleResult, SERVICE_LAYER};
use b2bua_core::turn_driver::{Driver, Step};
use call::{Call, Dialog, Direction};
use sip_message::Method;

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
    d.requests.iter().map(|r| r.method.as_str()).collect()
}

/// `ReleaseStraggler2xx` sent on an early dialog whose PRACK awaits its final:
/// the retired dialog holds the PRACK among its open requests beside its BYE,
/// and bob's 200 settles it.
#[test]
fn a_straggler_released_from_its_early_dialog_keeps_its_open_prack() {
    let driver = Driver::new().with_rule(releases_every_2xx());
    let mut call = driver.dial();
    call.run([Step::AnswersReliably("b-1", 183), Step::Answers("b-1", 200)]);
    assert_eq!(call.sent(), ["b-1 PRACK", "b-1 ACK", "b-1 BYE"], "PRACKed, then released");
    let d = fork(call.call(), "bobtag").expect("the fork's dialog");
    assert!(d.is_retired(), "the release retired the dialog");
    assert_eq!(open_methods(&d), ["PRACK", "BYE"], "the PRACK stays open beside the BYE");

    call.turned(Step::AnswersLast("b-1", Method::Prack, 200));
    let d = fork(call.call(), "bobtag").expect("the fork's dialog");
    assert_eq!(open_methods(&d), ["BYE"], "bob's 200 settled the PRACK");
}

/// The callee rings, answers 200, and the caller ACKs.
const ANSWERED: [Step; 3] = [Step::Answers("b-1", 180), Step::Answers("b-1", 200), Step::AliceAcks];

/// A fork that rings unreliably only after the winner's 2xx confirmed the
/// leg opens no dialog (no rule relays a 1xx on a confirmed leg, and the
/// INVITE it answers has its final), so its 2xx names none and is released as
/// a straggler's.
#[test]
fn a_fork_ringing_unreliably_after_the_winner_answered_opens_no_dialog() {
    let driver = Driver::new();
    let mut call = driver.dial();
    call.run(ANSWERED.clone());
    call.turned(Step::ForkAnswers("latefork", 180));
    assert_eq!(fork(call.call(), "latefork"), None, "the 180 opens no dialog");
    assert_eq!(call.sent(), ["b-1 ACK"], "and draws nothing");

    let late = call.turned(Step::ForkAnswers("latefork", 200));
    assert_eq!(
        late.rule,
        Some("release-fork-straggler-2xx"),
        "the late fork's 2xx is a straggler's"
    );
}

/// A fork whose first reliable provisional reaches this stack only after the
/// winner's 2xx confirmed the leg is PRACKed on its own early dialog, which
/// is retired at once as the 2xx retired its siblings' (RFC 3262 §4, RFC 3261
/// §13.2.2.4); no rule reads it, and its 2xx is a straggler's.
#[test]
fn a_fork_ringing_reliably_after_the_winner_answered_is_pracked_on_a_retired_dialog() {
    let driver = Driver::new();
    let mut call = driver.dial();
    call.run(ANSWERED.clone());
    let ring = call.turned(Step::ForkAnswersReliably("latefork", 183));
    assert_eq!(ring.rule, None, "no rule reads it");
    let d = fork(call.call(), "latefork").expect("the late fork's early dialog");
    assert!(d.is_retired(), "retired at once");
    assert_eq!(open_methods(&d), ["PRACK"], "its PRACK open in it");
    assert_eq!(call.sent(), ["b-1 ACK", "b-1 PRACK"], "PRACKed here");

    let late = call.turned(Step::ForkAnswers("latefork", 200));
    assert_eq!(
        late.rule,
        Some("release-fork-straggler-2xx"),
        "the late fork's 2xx is a straggler's"
    );
}

/// A fork a service tracked from a 1xx heard after the winner's 2xx holds an
/// early dialog on the confirmed leg: its 2xx is still a straggler's.
#[test]
fn a_late_fork_a_service_tracked_is_released_as_a_straggler() {
    let driver = Driver::new().with_rule(tracks_every_1xx());
    let mut call = driver.dial();
    call.run([
        Step::Answers("b-1", 180),
        Step::Answers("b-1", 200),
        Step::AliceAcks,
        Step::ForkAnswers("latefork", 180),
    ]);
    let d = fork(call.call(), "latefork").expect("the service tracked the late fork");
    assert!(d.awaits_invite_final(), "an early dialog on the confirmed leg: {:?}", d.state);
    let late = call.turned(Step::ForkAnswers("latefork", 200));
    assert_eq!(
        late.rule,
        Some("release-fork-straggler-2xx"),
        "the tracked fork's 2xx is a straggler's"
    );
}

/// A fork a service tracked early on the confirmed leg then rings reliably:
/// this stack PRACKs it on that dialog (RFC 3262 §4), which stays the
/// service's, early; no rule reads the provisional.
#[test]
fn a_late_fork_a_service_tracked_is_pracked_when_it_rings_reliably() {
    let driver = Driver::new().with_rule(tracks_every_1xx());
    let mut call = driver.dial();
    call.run(ANSWERED.clone()).run([Step::ForkAnswers("latefork", 180)]);
    let ring = call.turned(Step::ForkAnswersReliably("latefork", 183));
    assert_eq!(ring.rule, None, "no rule reads it");
    assert_eq!(call.sent(), ["b-1 ACK", "b-1 PRACK"], "PRACKed here");
    let d = fork(call.call(), "latefork").expect("the tracked fork");
    assert!(d.awaits_invite_final(), "still early: {:?}", d.state);
    assert_eq!(open_methods(&d), ["PRACK"], "its PRACK open in it");
}

/// Past 64·T1 (32 s): the bound of a late fork's early dialog, counted from
/// the 2xx that answered the INVITE (RFC 3261 §13.2.2.4).
const PAST_64_T1_MS: i64 = 33_000;

/// The record lost the turns of bob's 2xx and of a re-INVITE relayed to him;
/// his 2xx to that re-INVITE proves the dialled INVITE answered (ADR-0055
/// §4). The late forks' early dialogs are bounded by 64·T1 from that proof: a
/// fork ringing reliably inside it is PRACKed, one past it is not.
#[test]
fn the_answer_the_evidence_proves_bounds_the_late_forks_early_dialogs() {
    let driver = Driver::new();
    let mut call = crate::calls::ringing(&driver);
    call.lose([Step::Answers("b-1", 200), Step::AliceAcks, Step::Sends("a", Method::Invite)]);
    let evidence = call.turned(Step::AnswersLast("b-1", Method::Invite, 200));
    assert_eq!(evidence.rule, Some("evidence-final"), "the 2xx is evidence");

    let inside = call.turned(Step::ForkAnswersReliably("inside", 183));
    let pracked = |turn: &b2bua_core::turn_driver::Turn| {
        turn.requests_to("b-1").iter().filter(|r| r.method() == "PRACK").count()
    };
    assert_eq!(pracked(inside), 1, "a fork inside the bound is PRACKed");

    call.run([Step::Advance(PAST_64_T1_MS)]);
    let past = call.try_step(Step::ForkAnswersReliably("past", 183));
    let past = past.turn().map_or(0, pracked);
    assert_eq!(past, 0, "a fork past the bound is not");
}
