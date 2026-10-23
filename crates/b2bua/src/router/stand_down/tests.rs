//! The stand-down rule, as a pure decision over the two bodies and as the
//! turn a wired core runs on the notice: a takeover copy stands down on a
//! write of its primary that covers every write the copy made and holds every
//! re-INVITE round the copy holds, as far along, and on its primary's delete
//! of the call; every other copy plays on.

use std::sync::Arc;

use call::testkit::{stage_leg, stage_reinvite_round, RoundStage};
use call::{Call, CallBodyCodec, Direction, LegState, MsgpackCodec, PendingRequest};

use super::{covers as covers_on, finishes_nothing_alone, offer};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::materialise::{materialise, Materialised, Origin};
use crate::router::test_support::{invite, node, src, Node};
use crate::router::PrimaryWrite;
use crate::store::{CallStore, PartitionRole, PutOpts};

/// The node serving the takeover copy.
const COPY: &str = "w1";

/// [`covers_on`] the copy's node.
fn covers(primary: &Call, copy: &Call) -> bool {
    covers_on(primary, copy, COPY)
}

/// A ringing call `w0` owns and `w1` backs up: live, its caller unanswered.
fn ringing(cid: &str) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    build_initial_call(&invite("w0", "w1", cid), src(), &config, &mut sip_ids::TurnIds::fixed(1), 0)
}

/// `call` at backup counter `b`.
fn at_b(mut call: Call, b: i64) -> Call {
    call.topology.as_mut().expect("a proxied call has a topology").bak_gen = b;
    call
}

/// `call` as another call on the same ref: a new incarnation.
fn reborn(call: &Call) -> Call {
    let mut other = ringing("reborn");
    other.call_ref = call.call_ref.clone();
    other
}

#[test]
fn a_primary_at_or_past_the_copys_backup_counter_covers_it() {
    let copy = ringing("covered");
    assert!(covers(&copy, &copy), "a copy that wrote nothing: any write of its primary");
    let wrote = at_b(copy.clone(), 2);
    assert!(covers(&at_b(copy.clone(), 2), &wrote), "the primary read the copy's last write");
    assert!(covers(&at_b(copy.clone(), 3), &wrote));
}

#[test]
fn a_primary_behind_the_copys_backup_counter_does_not_cover_it() {
    let copy = ringing("uncovered");
    assert!(!covers(&at_b(copy.clone(), 1), &at_b(copy, 2)), "one write the primary never read");
}

#[test]
fn a_write_of_another_call_on_the_ref_never_covers_a_copy() {
    let copy = ringing("incarnation");
    let other = at_b(reborn(&copy), 5);
    assert_ne!(other.incarnation(), copy.incarnation());
    assert!(!covers(&other, &copy));
}

/// The answered call at backup counter `b`, a re-INVITE round on the caller's
/// dialog standing at `stage`, or none.
fn in_round(b: i64, stage: Option<RoundStage>) -> Call {
    let mut call = at_b(ringing("round"), b);
    call.a_leg.invite_final_sent = Some(200);
    stage_leg(&mut call.a_leg, LegState::Confirmed);
    if let Some(stage) = stage {
        stage_reinvite_round(&mut call.a_leg, "z9hG4bK-round", stage);
    }
    call
}

#[test]
fn a_primary_that_never_reached_the_copys_round_does_not_cover_it() {
    for stage in [RoundStage::Open, RoundStage::Answered, RoundStage::Acked] {
        let copy = in_round(2, Some(stage));
        let primary = in_round(2, None);
        assert!(!covers(&primary, &copy), "{stage:?}: the round is the copy's alone");
    }
}

#[test]
fn a_primary_holding_the_copys_round_at_its_stage_or_past_covers_it() {
    let copy = in_round(2, Some(RoundStage::Open));
    assert!(covers(&in_round(2, Some(RoundStage::Open)), &copy), "the primary holds it");
    assert!(covers(&in_round(2, Some(RoundStage::Answered)), &copy), "the primary saw it on");
    assert!(covers(&in_round(2, Some(RoundStage::Refused)), &copy), "the primary saw it end");
}

/// `call` with one more INFO relayed toward the caller by node `by`, awaiting
/// its final.
fn relaying(mut call: Call, by: &str) -> Call {
    let d = call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.our_cseq += 1;
    d.inbound_pending_requests.push(PendingRequest {
        method: "INFO".into(),
        outbound_cseq: d.our_cseq,
        inbound_cseq: 9,
        source_vias: Vec::new(),
        source_call_id: String::new(),
        source_from: String::new(),
        source_to: String::new(),
        direction: Direction::FromB,
        cancelled: false,
        offered_100rel: false,
        offered: false,
        source_timestamp: None,
        relayed_by: by.into(),
    });
    call
}

#[test]
fn a_primary_lacking_a_request_the_copy_relays_does_not_cover_it() {
    let primary = in_round(2, None);
    let copy = relaying(primary.clone(), COPY);
    assert!(!covers(&primary, &copy), "the INFO's final follows the copy's Via");
    assert!(covers(&copy, &copy), "a primary holding the relay covers it");
}

/// A relay the copy holds as the primary's own — read with the primary's
/// body, its final returning to the primary — pins nothing, whatever the
/// primary's record now holds of it.
#[test]
fn a_relay_another_node_made_never_pins_the_copy() {
    let primary = in_round(2, None);
    let copy = relaying(primary.clone(), "w0");
    assert!(covers(&primary, &copy));
    assert!(finishes_nothing_alone(None, &copy, COPY), "nor on the primary's delete");
}

/// The primary's delete releases a copy that holds nothing only it can
/// finish; a copy holding its own relay, or a round it relayed, plays on.
#[test]
fn a_deleted_record_releases_only_a_copy_that_finishes_nothing_alone() {
    let call = in_round(2, None);
    assert!(finishes_nothing_alone(None, &call, COPY));
    assert!(!finishes_nothing_alone(None, &relaying(call.clone(), COPY), COPY));
    let mut round = in_round(2, Some(RoundStage::Open));
    let d = round.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.inbound_pending_requests.iter_mut().for_each(|p| p.relayed_by = COPY.into());
    assert!(!finishes_nothing_alone(None, &round, COPY), "the round's final is the copy's");
    let answered = in_round(2, Some(RoundStage::Answered));
    assert!(finishes_nothing_alone(None, &answered, COPY), "its final is taken, its ACK routed");
}

/// A primary that refused the copy's round raised its sequence past the
/// round's CSeq: that is no reach of the round, read from its transaction.
#[test]
fn a_raised_sequence_is_no_round_the_primary_holds() {
    let mut copy = in_round(2, Some(RoundStage::Open));
    let d = copy.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.inbound_pending_requests.iter_mut().for_each(|p| p.relayed_by = COPY.into());
    let mut primary = in_round(2, None);
    primary.a_leg.dialogs.confirmed_mut().expect("the caller's dialog").our_cseq += 5;
    assert!(!covers(&primary, &copy), "the copy alone holds the round's transaction");
}

#[test]
fn a_primary_behind_the_copy_on_its_round_does_not_cover_it() {
    let copy = in_round(2, Some(RoundStage::Answered));
    let primary = in_round(2, Some(RoundStage::Open));
    assert!(!covers(&primary, &copy), "only the copy took the round's 2xx");
}

/// `call`, stored as `w1`'s Element of `w0`'s call and taken over: the live
/// takeover copy, its lock released.
async fn taken_over(n: &Node, call: &Call) {
    let body = MsgpackCodec::new().encode(call).unwrap();
    let ttl = 60_000;
    let opts = PutOpts::default();
    let r = &call.call_ref;
    n.store.put_call(PartitionRole::Backup, "w0", r, body, &[], ttl, 1, 0, &opts).await.unwrap();
    let ctx = n.core.router_ctx();
    let _guard = ctx.state.lock(r).await;
    let served = materialise(ctx, r, Origin::Takeover).await;
    assert!(matches!(served, Materialised::Served { .. }), "taken over: {served:?}");
}

/// The copy of `call_ref` writes once, as a turn of its own would.
fn authors(n: &Node, call_ref: &str) {
    let ctx = n.core.router_ctx();
    ctx.state.update(ctx.state.peek(call_ref).expect("the live copy"));
}

/// The notice of the primary's applied `write` of `call_ref`, offered and its
/// turn run.
async fn primary_wrote(n: &Node, call_ref: &str, write: PrimaryWrite) {
    offer(n.core.router_ctx(), call_ref.to_string(), write).await;
    sip_clock::testkit::settle().await;
}

fn put(call: &Call) -> PrimaryWrite {
    PrimaryWrite::Put(Arc::from(MsgpackCodec::new().encode(call).unwrap()))
}

#[tokio::test(start_paused = true)]
async fn the_turn_stands_a_covered_copy_down_off_the_store() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("turn-covered");
    let r = call.call_ref.clone();
    taken_over(&n, &call).await;

    primary_wrote(&n, &r, put(&call)).await;

    assert!(ctx.state.peek(&r).is_none(), "the copy stood down");
    assert!(!ctx.state.is_takeover(&r));
    assert!(
        n.store.get_call(PartitionRole::Backup, "w0", &r).await.unwrap().is_some(),
        "the Element stays for its primary",
    );
    assert!(n.cdr.snapshot().is_empty(), "the copy writes no CDR");
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
    assert_eq!(n.metrics.repl_stand_down_total(), 1, "counted as a stand-down");
    assert_eq!(n.metrics.repl_self_release_total(), 0, "not as a self-release");
    let removals = n.metrics.removals_of_total(crate::metrics::RemovalClass::StandDown);
    assert_eq!(removals, 1, "its queue is removed as a stand-down");
}

#[tokio::test(start_paused = true)]
async fn the_turn_keeps_a_copy_until_its_primary_read_its_every_write() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("turn-uncovered");
    let r = call.call_ref.clone();
    taken_over(&n, &call).await;
    authors(&n, &r);
    authors(&n, &r);

    primary_wrote(&n, &r, put(&at_b(call.clone(), 1))).await;
    assert!(ctx.state.is_takeover(&r), "a primary one write behind leaves the copy live");

    primary_wrote(&n, &r, put(&at_b(call, 2))).await;
    assert!(ctx.state.peek(&r).is_none(), "a primary at the copy's last write stands it down");
    assert!(n.cdr.snapshot().is_empty(), "the copy writes no CDR");
}

#[tokio::test(start_paused = true)]
async fn the_turn_stands_a_copy_down_on_its_primarys_delete_of_the_call_alone() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("turn-deleted");
    let r = call.call_ref.clone();
    taken_over(&n, &call).await;
    authors(&n, &r);

    let another = reborn(&call).incarnation().to_string();
    primary_wrote(&n, &r, PrimaryWrite::Delete(Some(another))).await;
    assert!(ctx.state.is_takeover(&r), "the delete of another call on the ref");

    let own = call.incarnation().to_string();
    primary_wrote(&n, &r, PrimaryWrite::Delete(Some(own))).await;
    assert!(ctx.state.peek(&r).is_none(), "the primary ended and deleted the call");
    assert!(n.cdr.snapshot().is_empty(), "the copy writes no CDR");
}

#[tokio::test(start_paused = true)]
async fn a_notice_for_a_call_with_no_takeover_copy_here_queues_nothing() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("no-copy");

    primary_wrote(&n, &call.call_ref, put(&call)).await;

    assert!(!ctx.dispatcher.has_queue(&call.call_ref), "no queue opened for the call");
    assert_eq!(ctx.state.lock_count(), 0);
}
