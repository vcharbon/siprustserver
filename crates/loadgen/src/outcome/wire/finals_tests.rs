//! Which finals fail a side's in-dialog requests, read off the call's wire facts.

use crate::mux::{Datagram, Dir, Side};
use crate::outcome::wire::tests::{feed, msg};
use crate::outcome::wire::{CallWire, WireFacts, WireFailure};
use sip_clock::Clock;

/// A 481 to an INFO the caller sent before the peer's BYE reached her answers
/// a request in flight when the dialog ended (RFC 3261 §15.1.2): no failure,
/// and the dialog is not read as gone. The same 481 with no BYE is one.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_481_crossing_a_bye_is_no_failure() {
    let setup = |cid: &str| {
        vec![
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", cid, "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", cid, "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("ACK sip:b@h SIP/2.0", cid, "1 ACK", "t1", "")),
            (Side::Caller, Dir::Out, msg("INFO sip:b@h SIP/2.0", cid, "2 INFO", "t1", "")),
        ]
    };
    let mut crossed = setup("c9");
    crossed.extend([
        (Side::Caller, Dir::In, msg("BYE sip:a@h SIP/2.0", "c9", "7 BYE", "t1", "")),
        (Side::Caller, Dir::In, msg("SIP/2.0 481 No Dialog", "c9", "2 INFO", "t1", "")),
    ]);
    let f = feed(&CallWire::new(Clock::test_at(0)), &crossed);
    assert!(f.failures.is_empty(), "{:?}", f.failures);
    assert!(!f.saw_481);
    let mut alone = setup("c10");
    alone.push((Side::Caller, Dir::In, msg("SIP/2.0 481 No Dialog", "c10", "2 INFO", "t1", "")));
    let f = feed(&CallWire::new(Clock::test_at(0)), &alone);
    assert_eq!(f.failures, vec![WireFailure { at_ms: 0, method: "INFO".into(), code: Some(481) }]);
}

/// A 491 to a re-INVITE the side then re-sent is no failure: the retry's own
/// final judges it (RFC 3261 §14.1). A refusal never retried stays one.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_491_the_side_retried_is_no_failure() {
    let reinvite = |cseq: &str| msg("INVITE sip:b@h SIP/2.0", "c11", cseq, "t1", "");
    let f = feed(
        &CallWire::new(Clock::test_at(0)),
        &[
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c11", "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c11", "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, reinvite("2 INVITE")),
            (
                Side::Caller,
                Dir::In,
                msg("SIP/2.0 491 Request Pending", "c11", "2 INVITE", "t1", ""),
            ),
            (Side::Caller, Dir::Out, reinvite("3 INVITE")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c11", "3 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, reinvite("4 INVITE")),
            (Side::Caller, Dir::In, msg("SIP/2.0 488 Not Here", "c11", "4 INVITE", "t1", "")),
        ],
    );
    assert_eq!(
        f.failures,
        vec![WireFailure { at_ms: 0, method: "INVITE".into(), code: Some(488) }]
    );
}

/// A retry of a 491'd re-INVITE takes a CSeq above the refused one (RFC 3261
/// §12.2.1.1): a later re-INVITE numbered below it retries nothing, and the
/// 491 stays a failure.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_request_numbered_below_a_491_retries_nothing() {
    let reinvite = |cseq: &str| msg("INVITE sip:b@h SIP/2.0", "c21", cseq, "t1", "");
    let f = feed(
        &CallWire::new(Clock::test_at(0)),
        &[
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c21", "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c21", "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, reinvite("4 INVITE")),
            (Side::Caller, Dir::In, msg("SIP/2.0 491 Pending", "c21", "4 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, reinvite("3 INVITE")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c21", "3 INVITE", "t1", "")),
        ],
    );
    assert_eq!(
        f.failures,
        vec![WireFailure { at_ms: 0, method: "INVITE".into(), code: Some(491) }]
    );
}

/// A refusal a UA gave a request the loss model's drops explain, relayed to
/// the other side, is excused there: the caller's OPTIONS 500 goes, her INFO
/// 488 stays, and the dialog is no longer read as gone once its 481 went.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn an_excused_refusal_is_no_failure() {
    let wire = CallWire::new(Clock::test_at(0));
    let f = feed(
        &wire,
        &[
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c12", "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c12", "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("OPTIONS sip:b@h SIP/2.0", "c12", "2 OPTIONS", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 500 Out Of Order", "c12", "2 OPTIONS", "t1", "")),
            (Side::Caller, Dir::Out, msg("INFO sip:b@h SIP/2.0", "c12", "3 INFO", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 488 Not Here", "c12", "3 INFO", "t1", "")),
            (Side::Caller, Dir::Out, msg("PRACK sip:b@h SIP/2.0", "c12", "4 PRACK", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 481 No Dialog", "c12", "4 PRACK", "t1", "")),
        ],
    );
    assert!(f.saw_481);
    let callee = Some("callee".to_string());
    wire.excuse(&[
        (callee.clone(), "OPTIONS".to_string(), 500),
        (callee, "PRACK".to_string(), 481),
    ]);
    let f = wire.facts();
    assert_eq!(f.failures, vec![WireFailure { at_ms: 0, method: "INFO".into(), code: Some(488) }]);
    assert!(!f.saw_481);
}

/// A 500 to a request whose first copy the loss model dropped on its way
/// out, refused once a higher CSeq of the side had reached the peer, is the
/// peer's refusal of a CSeq going backwards (RFC 3261 §12.2.2): no failure.
/// With no drop, the same 500 is one.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_500_to_a_request_the_loss_reordered_is_no_failure() {
    let run = |lost: bool| {
        let wire = CallWire::new(Clock::test_at(0));
        let update = msg("UPDATE sip:b@h SIP/2.0", "c13", "3 UPDATE", "t1", "");
        let tap = wire.tap();
        let loss = wire.loss_tap();
        for (dir, raw) in [
            (Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c13", "1 INVITE", "", "")),
            (Dir::In, msg("SIP/2.0 200 OK", "c13", "1 INVITE", "t1", "")),
            (Dir::Out, update.clone()),
        ] {
            tap(Side::Caller, dir, &Datagram::sniff(&raw));
        }
        if lost {
            loss(Side::Caller, Dir::Out, &Datagram::sniff(&update), &[]);
        }
        for (dir, raw) in [
            (Dir::Out, msg("INFO sip:b@h SIP/2.0", "c13", "4 INFO", "t1", "")),
            (Dir::In, msg("SIP/2.0 200 OK", "c13", "4 INFO", "t1", "")),
            (Dir::In, msg("SIP/2.0 500 Out Of Order", "c13", "3 UPDATE", "t1", "")),
        ] {
            tap(Side::Caller, dir, &Datagram::sniff(&raw));
        }
        wire.facts().failures
    };
    assert!(run(true).is_empty(), "{:?}", run(true));
    assert_eq!(
        run(false),
        vec![WireFailure { at_ms: 0, method: "UPDATE".into(), code: Some(500) }]
    );
}

/// A 487 to an INFO the caller sent, answered just before the BYE that ended
/// her dialog reached her, answers a request pending as the dialog ended
/// (RFC 3261 §15.1.2): excused once the call is over. With no BYE it stays.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_487_before_the_bye_that_ended_the_dialog_is_excused() {
    let run = |bye: bool| {
        let wire = CallWire::new(Clock::test_at(0));
        let mut events = vec![
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c14", "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c14", "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("INFO sip:b@h SIP/2.0", "c14", "2 INFO", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 487 Terminated", "c14", "2 INFO", "t1", "")),
        ];
        if bye {
            events.push((
                Side::Caller,
                Dir::In,
                msg("BYE sip:a@h SIP/2.0", "c14", "9 BYE", "t1", ""),
            ));
        }
        feed(&wire, &events);
        wire.excuse(&[]);
        wire.facts().failures
    };
    assert!(run(true).is_empty(), "{:?}", run(true));
    assert_eq!(run(false), vec![WireFailure { at_ms: 0, method: "INFO".into(), code: Some(487) }]);
}

/// A 481 to a side's own BYE, with no other BYE on its dialog, says the
/// dialog was gone before the BYE: a failure, the caller's and a leg's.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_481_to_the_side_s_own_bye_stays_a_failure() {
    let wire = CallWire::new(Clock::test_at(0));
    let f = feed(
        &wire,
        &[
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c15", "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c15", "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("ACK sip:b@h SIP/2.0", "c15", "1 ACK", "t1", "")),
            (
                Side::Leg("callee"),
                Dir::In,
                msg("INVITE sip:q@h SIP/2.0", "b15", "1 INVITE", "", ""),
            ),
            (Side::Leg("callee"), Dir::Out, msg("SIP/2.0 200 OK", "b15", "1 INVITE", "q1", "")),
            (Side::Caller, Dir::Out, msg("BYE sip:b@h SIP/2.0", "c15", "2 BYE", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 481 No Dialog", "c15", "2 BYE", "t1", "")),
            (Side::Leg("callee"), Dir::Out, msg("BYE sip:a@h SIP/2.0", "b15", "5 BYE", "q1", "")),
            (Side::Leg("callee"), Dir::In, msg("SIP/2.0 481 No Dialog", "b15", "5 BYE", "q1", "")),
        ],
    );
    let failed = WireFailure { at_ms: 0, method: "BYE".into(), code: Some(481) };
    assert_eq!(f.failures, vec![failed.clone()]);
    assert!(f.saw_481);
    assert_eq!(f.legs[0].failures, vec![failed]);
}

/// A 487 to an INFO, the BYE that ends the dialog coming 10 s later, answers
/// no request pending as the dialog ended: it stays a failure. The same 487
/// with the BYE in the same instant is excused.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_487_far_from_any_bye_stays_a_failure() {
    let wire = CallWire::new(Clock::test_at(0));
    feed(
        &wire,
        &[
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c16", "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c16", "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("INFO sip:b@h SIP/2.0", "c16", "2 INFO", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 487 Terminated", "c16", "2 INFO", "t1", "")),
            (Side::Caller, Dir::Out, msg("INFO sip:b@h SIP/2.0", "c16", "3 INFO", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c16", "3 INFO", "t1", "")),
        ],
    );
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    feed(
        &wire,
        &[
            (Side::Caller, Dir::Out, msg("BYE sip:b@h SIP/2.0", "c16", "4 BYE", "t1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c16", "4 BYE", "t1", "")),
        ],
    );
    wire.excuse(&[]);
    assert_eq!(
        wire.facts().failures,
        vec![WireFailure { at_ms: 0, method: "INFO".into(), code: Some(487) }]
    );
}

/// The INFO relayed to the callee leg, delivered (`Some(false)`), dropped on
/// its way to it (`Some(true)`), or never sent (`None`).
fn relayed_info(wire: &CallWire, lost: Option<bool>) {
    let info = msg("INFO sip:q@h SIP/2.0", "b18", "3 INFO", "q1", "");
    match lost {
        Some(true) => wire.loss_tap()(Side::Leg("callee"), Dir::In, &Datagram::sniff(&info), &[]),
        Some(false) => {
            feed(wire, &[(Side::Leg("callee"), Dir::In, info)]);
        }
        None => {}
    }
}

/// The caller's INFO answered 487 as the callee leg ends by a BYE it
/// received in the same instant, `relayed` as [`relayed_info`] says.
async fn a_487_as_the_callee_leg_ends(relayed: Option<bool>) -> Vec<WireFailure> {
    let wire = CallWire::new(Clock::test_at(0));
    feed(
        &wire,
        &[
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c18", "1 INVITE", "", "")),
            (
                Side::Leg("callee"),
                Dir::In,
                msg("INVITE sip:q@h SIP/2.0", "b18", "1 INVITE", "", ""),
            ),
            (Side::Leg("callee"), Dir::Out, msg("SIP/2.0 200 OK", "b18", "1 INVITE", "q1", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c18", "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("INFO sip:b@h SIP/2.0", "c18", "2 INFO", "t1", "")),
        ],
    );
    relayed_info(&wire, relayed);
    feed(
        &wire,
        &[
            (Side::Caller, Dir::In, msg("SIP/2.0 487 Terminated", "c18", "2 INFO", "t1", "")),
            (Side::Leg("callee"), Dir::In, msg("BYE sip:q@h SIP/2.0", "b18", "2 BYE", "q1", "")),
        ],
    );
    wire.excuse(&[]);
    wire.facts().failures
}

/// A 487 is excused by another leg's BYE only when that leg was sent a
/// request of its method after the refused one left: delivered or dropped on
/// its way. A leg that was sent no INFO had no INFO of the caller pending.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_487_is_excused_only_by_a_leg_the_request_was_relayed_to() {
    assert!(a_487_as_the_callee_leg_ends(Some(false)).await.is_empty(), "delivered");
    assert!(a_487_as_the_callee_leg_ends(Some(true)).await.is_empty(), "dropped on its way");
    assert_eq!(
        a_487_as_the_callee_leg_ends(None).await,
        vec![WireFailure { at_ms: 0, method: "INFO".into(), code: Some(487) }],
        "the leg was sent no INFO"
    );
}

/// A 487 to the caller's INFO answers a request relayed toward a callee leg
/// a BYE ended (sent or received there) in the same instant: excused, while
/// the caller's own dialog lives on. The same BYE 10 s later ended no leg the
/// request was pending toward: the 487 stays a failure.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_487_as_another_leg_ends_is_excused_only_within_t2() {
    async fn run(leg_dir: Dir, later: bool) -> Vec<WireFailure> {
        let wire = CallWire::new(Clock::test_at(0));
        feed(
            &wire,
            &[
                (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c18", "1 INVITE", "", "")),
                (
                    Side::Leg("callee"),
                    Dir::In,
                    msg("INVITE sip:q@h SIP/2.0", "b18", "1 INVITE", "", ""),
                ),
                (Side::Leg("callee"), Dir::Out, msg("SIP/2.0 200 OK", "b18", "1 INVITE", "q1", "")),
                (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c18", "1 INVITE", "t1", "")),
                (Side::Caller, Dir::Out, msg("ACK sip:b@h SIP/2.0", "c18", "1 ACK", "t1", "")),
                (Side::Caller, Dir::Out, msg("INFO sip:b@h SIP/2.0", "c18", "2 INFO", "t1", "")),
                (
                    Side::Leg("callee"),
                    Dir::In,
                    msg("INFO sip:q@h SIP/2.0", "b18", "3 INFO", "q1", ""),
                ),
                (Side::Caller, Dir::In, msg("SIP/2.0 487 Terminated", "c18", "2 INFO", "t1", "")),
            ],
        );
        if later {
            tokio::time::advance(std::time::Duration::from_secs(10)).await;
        }
        let bye = msg("BYE sip:q@h SIP/2.0", "b18", "2 BYE", "q1", "");
        feed(&wire, &[(Side::Leg("callee"), leg_dir, bye)]);
        wire.excuse(&[]);
        wire.facts().failures
    }
    for dir in [Dir::In, Dir::Out] {
        assert!(run(dir, false).await.is_empty(), "{dir:?}: the leg's BYE in the same instant");
        assert_eq!(
            run(dir, true).await,
            vec![WireFailure { at_ms: 0, method: "INFO".into(), code: Some(487) }],
            "{dir:?}: the leg's BYE 10 s later"
        );
    }
}

/// A 481 to the caller's INFO that arrives before the BYE ending her dialog,
/// that BYE's first copy dropped on its way to her and its retransmission
/// arriving within T2 after the 481, refuses a request the peer received
/// once it had ended the dialog (RFC 3261 §12.2.2): excused, and the dialog
/// is not read as gone. The same order with no copy of the BYE dropped, or
/// the retransmission 10 s later, leaves the 481 a failure.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_481_before_a_bye_whose_first_copy_was_lost_is_excused() {
    async fn run(lost: bool, later: bool) -> WireFacts {
        let wire = CallWire::new(Clock::test_at(0));
        let bye = msg("BYE sip:a@h SIP/2.0", "c20", "7 BYE", "t1", "");
        feed(
            &wire,
            &[
                (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c20", "1 INVITE", "", "")),
                (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c20", "1 INVITE", "t1", "")),
                (Side::Caller, Dir::Out, msg("ACK sip:b@h SIP/2.0", "c20", "1 ACK", "t1", "")),
            ],
        );
        if lost {
            wire.loss_tap()(Side::Caller, Dir::In, &Datagram::sniff(&bye), &[]);
        }
        feed(
            &wire,
            &[
                (Side::Caller, Dir::Out, msg("INFO sip:b@h SIP/2.0", "c20", "2 INFO", "t1", "")),
                (Side::Caller, Dir::In, msg("SIP/2.0 481 No Dialog", "c20", "2 INFO", "t1", "")),
            ],
        );
        if later {
            tokio::time::advance(std::time::Duration::from_secs(10)).await;
        }
        feed(&wire, &[(Side::Caller, Dir::In, bye)])
    }
    let f = run(true, false).await;
    assert!(f.failures.is_empty(), "{:?}", f.failures);
    assert!(!f.saw_481);
    let failed = vec![WireFailure { at_ms: 0, method: "INFO".into(), code: Some(481) }];
    let f = run(false, false).await;
    assert_eq!(f.failures, failed, "no copy of the BYE dropped");
    assert!(f.saw_481);
    let f = run(true, true).await;
    assert_eq!(f.failures, failed, "the BYE 10 s after the 481");
}

/// A 500 to a re-INVITE without a Retry-After asks for no retry (RFC 3261
/// §14.1): the side's next re-INVITE leaves it a failure. One carrying a
/// Retry-After, then retried, is no failure.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_500_without_retry_after_stays_a_failure_when_retried() {
    let run = |extra: &str| {
        let reinvite = |cseq: &str| msg("INVITE sip:b@h SIP/2.0", "c17", cseq, "t1", "");
        feed(
            &CallWire::new(Clock::test_at(0)),
            &[
                (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c17", "1 INVITE", "", "")),
                (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c17", "1 INVITE", "t1", "")),
                (Side::Caller, Dir::Out, reinvite("2 INVITE")),
                (Side::Caller, Dir::In, msg("SIP/2.0 500 Error", "c17", "2 INVITE", "t1", extra)),
                (Side::Caller, Dir::Out, reinvite("3 INVITE")),
                (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c17", "3 INVITE", "t1", "")),
            ],
        )
        .failures
    };
    assert_eq!(run(""), vec![WireFailure { at_ms: 0, method: "INVITE".into(), code: Some(500) }]);
    assert_eq!(run("Retry-After: 2\r\n"), vec![]);
}

/// A 481 to an INFO the seal left pending is that request's failure only:
/// the excuse never reads the dialog gone from it.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_late_481_after_the_seal_never_reads_the_dialog_gone() {
    let wire = CallWire::new(Clock::test_at(0));
    feed(
        &wire,
        &[
            (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c18", "1 INVITE", "", "")),
            (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c18", "1 INVITE", "t1", "")),
            (Side::Caller, Dir::Out, msg("INFO sip:b@h SIP/2.0", "c18", "2 INFO", "t1", "")),
        ],
    );
    wire.seal();
    feed(&wire, &[(Side::Caller, Dir::In, msg("SIP/2.0 481 No", "c18", "2 INFO", "t1", ""))]);
    wire.excuse(&[]);
    let f = wire.facts();
    assert_eq!(f.failures, vec![WireFailure { at_ms: 0, method: "INFO".into(), code: Some(481) }]);
    assert!(!f.saw_481, "the seal's ending stands, never cut");
}

/// One refusal a UA gave excuses one failure of its method and status on
/// each other side, never one on its own side: the callee's OPTIONS 500
/// excuses one of the caller's two, none of its own leg's; the caller's
/// excuses the leg's.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_refusal_excuses_one_failure_on_each_other_side() {
    let options = |side, cid: &str, cseq: &str, tag: &str| {
        [
            (side, Dir::Out, msg("OPTIONS sip:b@h SIP/2.0", cid, cseq, tag, "")),
            (side, Dir::In, msg("SIP/2.0 500 Out Of Order", cid, cseq, tag, "")),
        ]
    };
    let callee = Side::Leg("callee");
    let mut events = vec![
        (Side::Caller, Dir::Out, msg("INVITE sip:b@h SIP/2.0", "c19", "1 INVITE", "", "")),
        (Side::Caller, Dir::In, msg("SIP/2.0 200 OK", "c19", "1 INVITE", "t1", "")),
        (callee, Dir::In, msg("INVITE sip:q@h SIP/2.0", "b19", "1 INVITE", "", "")),
        (callee, Dir::Out, msg("SIP/2.0 200 OK", "b19", "1 INVITE", "q1", "")),
    ];
    events.extend(options(Side::Caller, "c19", "2 OPTIONS", "t1"));
    events.extend(options(Side::Caller, "c19", "3 OPTIONS", "t1"));
    events.extend(options(callee, "b19", "2 OPTIONS", "q1"));
    let run = |by: Option<&str>| {
        let wire = CallWire::new(Clock::test_at(0));
        feed(&wire, &events);
        wire.excuse(&[(by.map(str::to_string), "OPTIONS".to_string(), 500)]);
        let f = wire.facts();
        (f.failures.len(), f.legs[0].failures.len())
    };
    assert_eq!(run(Some("callee")), (1, 1));
    assert_eq!(run(None), (2, 0));
}
