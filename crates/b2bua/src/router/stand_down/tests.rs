//! The stand-down rule, as a pure decision over the two bodies and as the
//! turn a wired core runs on the notice: a takeover copy that authored nothing
//! stands down on any write of its primary (i), one that authored stands down
//! only on its primary's answer of the ringing call it serves (ii), and every
//! other copy plays on.

use call::{Call, CallBodyCodec, CallModelState, MsgpackCodec};

use super::{offer, stands_down};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::materialise::{materialise, Materialised, Origin};
use crate::router::test_support::{invite, node, src, Node};
use crate::store::{CallStore, PartitionRole, PutOpts};

/// A ringing call `w0` owns and `w1` backs up: live, its caller unanswered.
fn ringing(cid: &str) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    build_initial_call(&invite("w0", "w1", cid), src(), &config, &sip_txn::IdGen::seeded(1), 0)
}

/// `call` with its caller answered 200.
fn answered(mut call: Call) -> Call {
    call.a_leg.invite_final_sent = Some(200);
    call
}

/// `call` ending: its teardown under way, its caller unanswered.
fn ending(mut call: Call) -> Call {
    call.state = CallModelState::Terminating;
    call
}

/// `call` as another call on the same ref: a new incarnation.
fn reborn(call: &Call) -> Call {
    let mut other = ringing("reborn");
    other.call_ref = call.call_ref.clone();
    other
}

#[test]
fn a_copy_that_authored_nothing_stands_down_on_any_write_of_its_primary() {
    let copy = ringing("i");
    assert!(stands_down(&copy, &copy, false), "on the reclaim's write of the ringing call");
    assert!(stands_down(&answered(copy.clone()), &copy, false), "on the primary's answer");
}

#[test]
fn a_copy_that_authored_stands_down_on_its_primarys_answer_of_the_ringing_call() {
    let copy = ringing("ii");
    assert!(stands_down(&answered(copy.clone()), &copy, true));
}

#[test]
fn a_copy_that_authored_plays_on_unless_its_ringing_call_was_answered_by_the_primary() {
    let copy = ringing("plays-on");
    assert!(!stands_down(&copy, &copy, true), "the primary's write of the ringing call");
    let both = answered(copy.clone());
    assert!(!stands_down(&both, &both, true), "a copy that answered the caller itself");
    assert!(
        !stands_down(&answered(copy.clone()), &ending(copy.clone()), true),
        "a copy holding a teardown",
    );
}

#[test]
fn a_write_of_another_call_on_the_ref_never_stands_a_copy_down() {
    let copy = ringing("incarnation");
    let other = reborn(&copy);
    assert_ne!(other.incarnation(), copy.incarnation());
    assert!(!stands_down(&other, &copy, false));
    assert!(!stands_down(&answered(other), &copy, true));
}

/// `call`, stored as `w1`'s Element of `w0`'s call and taken over: the live
/// takeover copy, its lock released.
async fn taken_over(n: &Node, call: &Call) {
    let body = MsgpackCodec::new().encode(call);
    n.store
        .put_call(
            PartitionRole::Backup,
            "w0",
            &call.call_ref,
            body,
            &[],
            60_000,
            1,
            0,
            &PutOpts::default(),
        )
        .await
        .unwrap();
    let ctx = n.core.router_ctx();
    let _guard = ctx.state.lock(&call.call_ref).await;
    let served = materialise(ctx, &call.call_ref, Origin::Takeover).await;
    assert!(matches!(served, Materialised::Served { .. }), "taken over: {served:?}");
}

/// The copy of `call_ref` writes once, as a turn of its own would.
fn authors(n: &Node, call_ref: &str) {
    let ctx = n.core.router_ctx();
    let copy = ctx.state.peek(call_ref).expect("the live copy");
    ctx.state.update(copy);
    assert_eq!(ctx.state.takeover_authored(call_ref), Some(true));
}

/// The notice of `primary`'s applied write, offered and its turn run.
async fn primary_wrote(n: &Node, primary: &Call) {
    let body = MsgpackCodec::new().encode(primary);
    offer(n.core.router_ctx(), primary.call_ref.clone(), &body).await;
    sip_clock::testkit::settle().await;
}

#[tokio::test(start_paused = true)]
async fn the_turn_releases_a_copy_that_authored_nothing_off_the_store() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("turn-i");
    let r = call.call_ref.clone();
    taken_over(&n, &call).await;
    assert_eq!(ctx.state.takeover_authored(&r), Some(false));

    primary_wrote(&n, &call).await;

    assert!(ctx.state.peek(&r).is_none(), "the copy stood down");
    assert!(!ctx.state.is_takeover(&r));
    assert!(
        n.store.get_call(PartitionRole::Backup, "w0", &r).await.unwrap().is_some(),
        "the Element stays for its primary",
    );
    assert!(n.cdr.snapshot().is_empty(), "the copy writes no CDR");
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
}

#[tokio::test(start_paused = true)]
async fn the_turn_keeps_a_copy_that_authored_until_its_primary_answers() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("turn-ii");
    let r = call.call_ref.clone();
    taken_over(&n, &call).await;
    authors(&n, &r);

    primary_wrote(&n, &call).await;
    assert!(ctx.state.is_takeover(&r), "the primary's ringing write leaves the copy live");

    primary_wrote(&n, &answered(call)).await;
    assert!(ctx.state.peek(&r).is_none(), "the primary's answer stands the copy down");
    assert!(n.cdr.snapshot().is_empty(), "the copy writes no CDR");
}

#[tokio::test(start_paused = true)]
async fn a_notice_for_a_call_with_no_takeover_copy_here_queues_nothing() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("no-copy");

    primary_wrote(&n, &call).await;

    assert!(!ctx.dispatcher.has_queue(&call.call_ref), "no queue opened for the call");
    assert_eq!(ctx.state.lock_count(), 0);
}
