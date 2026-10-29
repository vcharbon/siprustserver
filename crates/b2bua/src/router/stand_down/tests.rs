//! The stand-down rule, as a pure decision over the two bodies and as the
//! turn a wired core runs on the notice: a takeover copy stands down on a
//! write of its primary that contains it (ADR-0055 §7) and holds every
//! request record the copy keeps open, and on its primary's delete of the
//! call when it holds none; every other copy plays on.

use std::sync::Arc;

use call::testkit::{stage_leg, stage_reinvite_round, RoundStage};
use call::{LegState, MsgpackCodec, RelayOwner, RequestRecord};

use super::{covers as covers_on, finishes_nothing_alone, offer};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::materialise::{materialise, Materialised, Origin};
use crate::router::test_support::{invite, node, src, Node};
use crate::router::PrimaryWrite;
use crate::store::{envelope, CallStore, Enveloped, PartitionRole, PutOpts};

/// The worker serving the takeover copy.
fn copy_worker() -> RelayOwner {
    RelayOwner::new("w1", "w1-boot-2")
}

/// [`covers_on`] the copy's worker.
fn covers(primary: &Enveloped, copy: &Enveloped) -> bool {
    covers_on(primary, copy, &copy_worker())
}

/// A ringing call `w0` owns and `w1` backs up: live, its caller unanswered.
fn ringing(cid: &str) -> Enveloped {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let invite = invite("w0", "w1", cid);
    let call = build_initial_call(&invite, src(), &config, &mut sip_ids::TurnIds::fixed(1), 0);
    let topology = crate::router::topology_cookie::placement(&call);
    Enveloped::new(call, topology)
}

/// `copy` as a replicated body.
fn body(copy: &Enveloped) -> Vec<u8> {
    envelope::encode(&MsgpackCodec::new(), copy.topology.as_ref(), &copy.call).unwrap()
}

/// `call` at backup counter `b`, its last compared write made there.
fn at_b(mut copy: Enveloped, b: i64) -> Enveloped {
    let t = copy.topology.as_mut().expect("a proxied call has a topology");
    t.bak_gen = b;
    t.stamp_vector();
    copy
}

/// `call` as another call on the same ref: a new incarnation.
fn reborn(copy: &Enveloped) -> Enveloped {
    let mut other = ringing("reborn");
    other.call.call_ref = copy.call.call_ref.clone();
    other
}

#[test]
fn a_primary_that_read_the_copys_last_compared_write_covers_it() {
    let copy = ringing("covered");
    assert!(covers(&copy, &copy), "a copy that wrote nothing: any write of its primary");
    let wrote = at_b(copy.clone(), 2);
    assert!(covers(&at_b(copy.clone(), 2), &wrote), "the primary read the copy's last write");
    assert!(covers(&at_b(copy.clone(), 3), &wrote));
}

#[test]
fn a_primary_that_never_read_the_copys_last_compared_write_does_not_cover_it() {
    let copy = ringing("uncovered");
    assert!(!covers(&at_b(copy.clone(), 1), &at_b(copy, 2)), "one write the primary never read");
}

#[test]
fn a_write_of_another_call_on_the_ref_never_covers_a_copy() {
    let copy = ringing("incarnation");
    let other = at_b(reborn(&copy), 5);
    assert_ne!(other.call.incarnation(), copy.call.incarnation());
    assert!(!covers(&other, &copy));
}

/// The answered call at backup counter `b`, a re-INVITE round on the caller's
/// dialog standing at `stage`, or none.
fn in_round(b: i64, stage: Option<RoundStage>) -> Enveloped {
    let mut copy = at_b(ringing("round"), b);
    let call = &mut copy.call;
    *call.a_leg.invite_final_sent_mut() = Some(200);
    stage_leg(&mut call.a_leg, LegState::Confirmed);
    if let Some(stage) = stage {
        stage_reinvite_round(&mut call.a_leg, "z9hG4bK-round", stage);
    }
    copy
}

/// At the copy's own version the primary's record lacks the round the copy
/// relayed, whose final returns to the copy: the primary does not cover it,
/// however much of the copy it read.
#[test]
fn a_primary_lacking_the_round_the_copy_relays_does_not_cover_it() {
    let mut copy = in_round(2, Some(RoundStage::Open));
    let d = copy.call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.sent_invite.iter_mut().for_each(|p| p.relayed_by = copy_worker());
    let primary = in_round(2, None);
    assert!(!covers(&primary, &copy), "the round's final is the copy's alone");
}

#[test]
fn a_primary_that_never_read_the_copys_round_does_not_cover_it() {
    for stage in [RoundStage::Open, RoundStage::Answered, RoundStage::Acked] {
        let copy = in_round(2, Some(stage));
        let primary = in_round(1, None);
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

/// `call` with one more INFO relayed toward the caller by worker `by`,
/// awaiting its final.
fn relaying(mut copy: Enveloped, by: RelayOwner) -> Enveloped {
    let d = copy.call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.our_cseq += 1;
    d.requests.push(RequestRecord {
        method: "INFO".into(),
        outbound_cseq: d.our_cseq,
        inbound_cseq: 9,
        source_vias: Vec::new(),
        source_call_id: String::new(),
        source_from: String::new(),
        source_to: String::new(),
        origin: Default::default(),
        usage: Default::default(),
        sent: None,
        deadline_ms: None,
        cancelled: false,
        offered_100rel: false,
        offered: false,
        source_timestamp: None,
        relayed_by: by,
        author: None,
        invite: None,
    });
    copy
}

#[test]
fn a_primary_lacking_a_request_the_copy_relays_does_not_cover_it() {
    let primary = in_round(2, None);
    let copy = relaying(primary.clone(), copy_worker());
    assert!(!covers(&primary, &copy), "the INFO's final follows the copy's Via");
    assert!(covers(&copy, &copy), "a primary holding the relay covers it");
}

/// A relay the copy holds as the primary's own — read with the primary's
/// body, its final returning to the primary — pins nothing, whatever the
/// primary's record now holds of it.
#[test]
fn a_relay_another_node_made_never_pins_the_copy() {
    let primary = in_round(2, None);
    let copy = relaying(primary.clone(), RelayOwner::new("w0", "w0-boot-1"));
    assert!(covers(&primary, &copy));
    assert!(
        finishes_nothing_alone(None, &copy.call, &copy_worker()),
        "nor on the primary's delete"
    );
}

/// A relay the copy's ordinal made in a former boot returns to that boot: it
/// pins nothing on the restarted worker (ADR-0055 §6).
#[test]
fn a_relay_a_former_boot_of_the_copys_ordinal_made_never_pins_the_copy() {
    let primary = in_round(2, None);
    let copy = relaying(primary.clone(), RelayOwner::new("w1", "w1-boot-1"));
    assert!(covers(&primary, &copy));
    assert!(
        finishes_nothing_alone(None, &copy.call, &copy_worker()),
        "nor on the primary's delete"
    );
}

/// `call` with a callee leg whose dialling INVITE worker `by` sent, final
/// `final_received`.
fn dialling(mut copy: Enveloped, by: RelayOwner, final_received: Option<u16>) -> Enveloped {
    let mut leg = call::testkit::dialled_leg("c-1", "dialled-tag");
    if let Some(record) = leg.invite.as_mut() {
        record.relayed_by = by;
    }
    if let Some(status) = final_received {
        leg.outbound_invite_mut().expect("the dialled INVITE").record_final(status);
    }
    copy.call.b_legs.push(leg);
    copy
}

/// An INVITE the copy dialled and that awaits its final pins the copy, even
/// when the primary's record holds it: the proxy's cookie on that INVITE
/// names the copy, so its responses come back to the copy alone.
#[test]
fn an_invite_the_copy_dialled_awaiting_its_final_pins_the_copy() {
    let copy = dialling(in_round(2, None), copy_worker(), None);
    assert!(!covers(&copy, &copy), "the callee's final comes back to the copy");
}

/// The dialled INVITE pins nothing once it took its final, nor when another
/// node sent it.
#[test]
fn an_answered_dialled_invite_or_one_another_node_sent_never_pins_the_copy() {
    let answered = dialling(in_round(2, None), copy_worker(), Some(200));
    assert!(covers(&answered, &answered), "its final is in");
    let theirs = dialling(in_round(2, None), RelayOwner::new("w0", "w0-boot-1"), None);
    assert!(covers(&theirs, &theirs), "the primary sent it");
}

/// The primary's delete releases a copy that holds nothing only it can
/// finish; a copy holding its own relay, or a round it relayed, plays on.
#[test]
fn a_deleted_record_releases_only_a_copy_that_finishes_nothing_alone() {
    let call = in_round(2, None);
    assert!(finishes_nothing_alone(None, &call.call, &copy_worker()));
    assert!(!finishes_nothing_alone(
        None,
        &relaying(call.clone(), copy_worker()).call,
        &copy_worker()
    ));
    let mut round = in_round(2, Some(RoundStage::Open));
    let d = round.call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.sent_invite.iter_mut().for_each(|p| p.relayed_by = copy_worker());
    assert!(
        !finishes_nothing_alone(None, &round.call, &copy_worker()),
        "the round's final is the copy's"
    );
    let answered = in_round(2, Some(RoundStage::Answered));
    assert!(
        finishes_nothing_alone(None, &answered.call, &copy_worker()),
        "its final is taken, its ACK routed"
    );
}

/// A primary that refused the copy's round raised its sequence past the
/// round's CSeq: that is no reach of the round, read from its transaction.
#[test]
fn a_raised_sequence_is_no_round_the_primary_holds() {
    let mut copy = in_round(2, Some(RoundStage::Open));
    let d = copy.call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.sent_invite.iter_mut().for_each(|p| p.relayed_by = copy_worker());
    let mut primary = in_round(2, None);
    primary.call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog").our_cseq += 5;
    assert!(!covers(&primary, &copy), "the copy alone holds the round's transaction");
}

#[test]
fn a_primary_that_never_read_the_copys_answer_to_its_round_does_not_cover_it() {
    let copy = in_round(2, Some(RoundStage::Answered));
    let primary = in_round(1, Some(RoundStage::Open));
    assert!(!covers(&primary, &copy), "only the copy took the round's 2xx");
}

/// `call`, stored as `w1`'s Element of `w0`'s call and taken over: the live
/// takeover copy, its lock released.
async fn taken_over(n: &Node, copy: &Enveloped) {
    let body = body(copy);
    let ttl = 60_000;
    let opts = PutOpts::default();
    let r = &copy.call.call_ref;
    n.store.put_call(PartitionRole::Backup, "w0", r, body, &[], ttl, 1, 0, &opts).await.unwrap();
    let ctx = n.core.router_ctx();
    let _guard = ctx.state.lock(r).await;
    let served = materialise(ctx, r, Origin::Takeover).await;
    assert!(matches!(served, Materialised::Served { .. }), "taken over: {served:?}");
}

/// The copy of `call_ref` writes once, changing a compared field, as a turn
/// of its own would.
fn authors(n: &Node, call_ref: &str) {
    let ctx = n.core.router_ctx();
    let mut call = ctx.state.peek(call_ref).expect("the live copy");
    let written = call.callback_context.as_ref().map_or(0, String::len);
    call.callback_context = Some("w".repeat(written + 1));
    ctx.state.update(call);
}

/// The notice of the primary's applied `write` of `call_ref`, offered and its
/// turn run.
async fn primary_wrote(n: &Node, call_ref: &str, write: PrimaryWrite) {
    offer(n.core.router_ctx(), call_ref.to_string(), write).await;
    sip_clock::testkit::settle().await;
}

fn put(copy: &Enveloped) -> PrimaryWrite {
    PrimaryWrite::Put(Arc::from(body(copy)))
}

#[tokio::test(start_paused = true)]
async fn the_turn_stands_a_covered_copy_down_off_the_store() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("turn-covered");
    let r = call.call.call_ref.clone();
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
    let r = call.call.call_ref.clone();
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
    let r = call.call.call_ref.clone();
    taken_over(&n, &call).await;
    authors(&n, &r);

    let another = reborn(&call).call.incarnation().to_string();
    primary_wrote(&n, &r, PrimaryWrite::Delete(Some(another))).await;
    assert!(ctx.state.is_takeover(&r), "the delete of another call on the ref");

    let own = call.call.incarnation().to_string();
    primary_wrote(&n, &r, PrimaryWrite::Delete(Some(own))).await;
    assert!(ctx.state.peek(&r).is_none(), "the primary ended and deleted the call");
    assert!(n.cdr.snapshot().is_empty(), "the copy writes no CDR");
}

#[tokio::test(start_paused = true)]
async fn a_notice_for_a_call_with_no_takeover_copy_here_queues_nothing() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("no-copy");

    primary_wrote(&n, &call.call.call_ref, put(&call)).await;

    assert!(!ctx.dispatcher.has_queue(&call.call.call_ref), "no queue opened for the call");
    assert_eq!(ctx.state.lock_count(), 0);
}
