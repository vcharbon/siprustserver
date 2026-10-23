//! The drop turn on a wired core: a live copy of this node's own call is
//! dropped, with no CDR, on its backup's delete of that call alone; a delete of
//! another call on the ref leaves it, and a takeover copy is never dropped on
//! the notice.

use call::{Call, CallBodyCodec, MsgpackCodec};

use super::offer;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::materialise::{materialise, Materialised, Origin};
use crate::router::test_support::{invite, node, src, Node};
use crate::store::{CallStore, PartitionRole, PutOpts};

/// A ringing call `primary` owns and `backup` backs up.
fn ringing(primary: &str, backup: &str, cid: &str) -> Call {
    let config = B2buaConfig { self_ordinal: primary.into(), ..Default::default() };
    build_initial_call(
        &invite(primary, backup, cid),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    )
}

/// `call`, stored in `role` of `primary`'s partition and materialised from
/// `origin`: the live copy, its lock released.
async fn live(n: &Node, call: &Call, role: PartitionRole, primary: &str, origin: Origin) {
    let body = MsgpackCodec::new().encode(call).unwrap();
    let r = &call.call_ref;
    n.store.put_call(role, primary, r, body, &[], 60_000, 1, 0, &PutOpts::default()).await.unwrap();
    let ctx = n.core.router_ctx();
    let _guard = ctx.state.lock(r).await;
    let served = materialise(ctx, r, origin).await;
    assert!(matches!(served, Materialised::Served { .. }), "live: {served:?}");
}

/// The notice of the backup's applied delete of `call_ref` naming
/// `incarnation`, offered and its turn run.
async fn backup_deleted(n: &Node, call_ref: &str, incarnation: &str) {
    offer(n.core.router_ctx(), call_ref.to_string(), Some(incarnation.to_string())).await;
    sip_clock::testkit::settle().await;
}

#[tokio::test(start_paused = true)]
async fn a_live_own_call_is_dropped_on_its_backups_delete_of_it_alone() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let call = ringing("w0", "w1", "own");
    let r = call.call_ref.clone();
    live(&n, &call, PartitionRole::Primary, "w0", Origin::Reclaim(None)).await;

    let mut other = ringing("w0", "w1", "reborn");
    other.call_ref = r.clone();
    backup_deleted(&n, &r, other.incarnation()).await;
    assert!(ctx.state.peek(&r).is_some(), "a delete of another call on the ref leaves it");

    backup_deleted(&n, &r, call.incarnation()).await;
    assert!(ctx.state.peek(&r).is_none(), "the backup's discharge ends the live copy here");
    assert!(n.cdr.snapshot().is_empty(), "the dropped copy writes no CDR");
    assert_eq!(ctx.state.lock_count(), 0, "no lock entry is left behind");
}

#[tokio::test(start_paused = true)]
async fn a_takeover_copy_is_not_dropped_on_the_notice() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = ringing("w0", "w1", "copy");
    let r = call.call_ref.clone();
    live(&n, &call, PartitionRole::Backup, "w0", Origin::Takeover).await;

    backup_deleted(&n, &r, call.incarnation()).await;
    assert!(ctx.state.is_takeover(&r), "a takeover copy is ended by its own rules");
}

#[tokio::test(start_paused = true)]
async fn a_notice_for_a_call_not_live_here_leaves_nothing_behind() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let call = ringing("w0", "w1", "absent");
    backup_deleted(&n, &call.call_ref, call.incarnation()).await;
    assert!(ctx.state.peek(&call.call_ref).is_none());
    assert_eq!(ctx.state.lock_count(), 0, "the notice's queue and lock entry are released");
    assert!(n.cdr.snapshot().is_empty());
}
