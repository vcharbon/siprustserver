//! Pins the fold predicate ([`reverse_flush_dominates`]) directly — which
//! bodies carry lifecycle progress a live copy lacks, and which are a branch
//! off a stale view — the direction rule of [`fold_refused_flush`] itself:
//! who may end a call on a folded body (ADR-0031 D3), and that a fold is not
//! this node's write (ADR-0014).

use call::testkit::{stage_reinvite_round, RoundStage, STAGED_ROUND_CSEQ};
use call::{Call, CallBodyCodec, CallModelState, CallTopology, LegState, MsgpackCodec};

use super::{
    fold_refused_flush, reap_expired_replicas, reconcile_reverse_flush, reverse_flush_dominates,
    FlushDirection,
};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, node, src};
use crate::store::{CallStore, MaterialiseOrigin, PartitionRole, PutOpts};

/// A replicable call at `(p, b)`, owned by `w0` and backed up by `w1`.
fn base(p: i64, b: i64) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", "fold"),
        src(),
        &config,
        &sip_txn::IdGen::seeded(1),
        0,
    );
    call.topology = Some(CallTopology { pri: "w0".into(), bak: "w1".into(), gen: p, bak_gen: b });
    call
}

/// The caller is ringing: no final has left toward her yet.
fn unanswered(p: i64, b: i64, state: CallModelState) -> Call {
    let mut call = base(p, b);
    call.state = state;
    call.a_leg.invite_final_sent = (state != CallModelState::Active).then_some(480);
    let a_state =
        if state == CallModelState::Active { LegState::Early } else { LegState::Terminated };
    call::testkit::stage_leg(&mut call.a_leg, a_state);
    call
}

/// The caller was answered — a durable fact of the body: the 2xx this stack
/// sent her is recorded on the leg and the pair is bridged, both of which
/// outlive the leg going `Terminated`.
fn answered(p: i64, b: i64, state: CallModelState) -> Call {
    let mut call = base(p, b);
    call.state = state;
    call.a_leg.invite_final_sent = Some(200);
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
    if state != CallModelState::Active {
        call::testkit::stage_leg(&mut call.a_leg, LegState::Terminated);
    }
    call
}

/// Lifecycle progress is a CHAIN: the replica folds only when it made every
/// step the live copy made and at least one more. A teardown of a caller who
/// was never answered, arriving at a live copy whose caller IS answered, is a
/// ring-deadline teardown on a stale view — a branch, and refused (ADR-0031
/// D3). Folding it ends a call the other owner is serving.
#[test]
fn a_terminal_body_that_never_answered_does_not_fold_into_an_answered_live_copy() {
    let replica = unanswered(1, 0, CallModelState::Terminated);
    let live = answered(3, 1, CallModelState::Active);
    assert!(
        !reverse_flush_dominates(&replica, &live),
        "an unanswered teardown is a branch off a stale view, not progress",
    );
}

/// The same shape one step along the chain: a teardown of a call the replica
/// ALSO reads as answered is genuine progress and folds.
#[test]
fn a_terminal_body_that_answered_folds_into_an_answered_live_copy() {
    let replica = answered(1, 1, CallModelState::Terminated);
    let live = answered(3, 0, CallModelState::Active);
    assert!(
        reverse_flush_dominates(&replica, &live),
        "ending is the second axis: terminated over active, both answered",
    );
}

/// The answer itself: the replica answered the caller our live copy still
/// reads as ringing.
#[test]
fn an_answer_folds_into_an_unanswered_live_copy() {
    let replica = answered(1, 1, CallModelState::Active);
    let live = unanswered(3, 0, CallModelState::Active);
    assert!(reverse_flush_dominates(&replica, &live), "answered over unanswered");
}

/// And the reverse of that is not progress.
#[test]
fn an_unanswered_body_does_not_fold_into_an_answered_live_copy() {
    let replica = unanswered(1, 1, CallModelState::Active);
    let live = answered(2, 1, CallModelState::Active);
    assert!(
        !reverse_flush_dominates(&replica, &live),
        "a ringing view never outranks an answered one; the vector says nothing here",
    );
}

/// The vector clause: `p` unchanged since the backup branched and `b`
/// advanced folds a body that stands behind the live copy on neither axis.
#[test]
fn the_vector_clause_still_folds_an_advanced_backup_counter() {
    let replica = answered(1, 2, CallModelState::Active);
    let live = answered(1, 1, CallModelState::Active);
    assert!(reverse_flush_dominates(&replica, &live), "p unchanged, b advanced");
}

/// A backup that adopted our `p` without the body that carried it (a refused
/// forward flush, ADR-0031 D3) and then ended the call it saw ringing flushes
/// at our `p` with its `b` advanced: the vector clause alone would end the
/// call we answered. The body stands behind ours on the answer axis, so it is
/// a branch and never folds.
#[test]
fn the_vector_clause_never_folds_a_teardown_of_a_call_it_never_saw_answered() {
    let replica = unanswered(6, 3, CallModelState::Terminated);
    let live = answered(6, 1, CallModelState::Active);
    assert!(
        !reverse_flush_dominates(&replica, &live),
        "an adopted p does not make an unanswered teardown our call's end",
    );
}

/// The mirror: a backup that answered the caller while our live copy was
/// ending the call it never saw answered flushes at our `p` with its `b`
/// advanced; the answer ranks first, so the vector clause folds it.
#[test]
fn the_vector_clause_folds_an_answer_into_a_live_copy_ending_unanswered() {
    let replica = answered(6, 2, CallModelState::Active);
    let live = unanswered(6, 1, CallModelState::Terminating);
    assert!(reverse_flush_dominates(&replica, &live), "an answered call is never behind");
}

/// The answered call at `(p, b)`, a re-INVITE round on the caller's dialog
/// standing at `stage`.
fn in_round(p: i64, b: i64, stage: RoundStage) -> Call {
    let mut call = answered(p, b, CallModelState::Active);
    stage_reinvite_round(&mut call.a_leg, "z9hG4bK-round", stage);
    call
}

/// The `(p,b)` of the live copy `n` holds for `call_ref`.
fn live_pb(n: &crate::router::test_support::Node, call_ref: &str) -> (i64, i64) {
    let t = n.core.router_ctx().state.peek(call_ref).expect("the copy is live").topology;
    let t = t.expect("the copy is replicable");
    (t.gen, t.bak_gen)
}

/// **Only a primary discharges.** A primary's terminal body reaching the
/// BACKUP that took its call over carries real progress — but the live
/// takeover copy is the call's record and only its own signals end it
/// (ADR-0014 §2 / ADR-0020 X3). The fold reads the body for its counter
/// alone: no CDR, no release, the call plays on.
#[tokio::test(start_paused = true)]
async fn a_forward_terminal_ends_no_call_at_the_backup_and_its_counter_is_adopted() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let live = answered(1, 2, CallModelState::Active);
    let call_ref = live.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

    // The partitioned primary tore its own copy down and flushed it forward.
    let replica = answered(3, 1, CallModelState::Terminated);
    let body = MsgpackCodec::new().encode(&replica);
    fold_refused_flush(ctx, FlushDirection::Forward, &call_ref, &body, 0).await;
    sip_clock::testkit::settle().await;

    let after = ctx.state.peek(&call_ref).expect("the takeover copy still serves the call");
    assert_eq!(after.state, CallModelState::Active, "no discharge in the Forward direction");
    assert!(n.cdr.snapshot().is_empty(), "an acting backup writes no CDR for a folded terminal");
    // `p` is taken from the flush; `b` is untouched — adopting a version is a
    // read this node records, not a mutation of the call.
    assert_eq!(live_pb(&n, &call_ref), (3, 2), "the seen `p` is adopted, and only it");
}

/// **The answer crosses the split toward a backup too.** A forward `Put`
/// refused on the counters alone (`sb > b_in`) can still carry the authority's
/// own answer for a call this node's takeover copy reads as ringing. That is
/// progress the copy lacks, so it folds: the copy takes the answer, both
/// counters are adopted, and nothing is discharged — the call plays on.
#[tokio::test(start_paused = true)]
async fn a_forward_answer_folds_into_a_ringing_takeover_copy_without_ending_it() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let live = unanswered(1, 2, CallModelState::Active);
    let call_ref = live.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

    // The authority answered the caller and flushed; its `b` is behind the
    // Element's, so the store refused the frame and handed the body up.
    let replica = answered(3, 1, CallModelState::Active);
    let body = MsgpackCodec::new().encode(&replica);
    fold_refused_flush(ctx, FlushDirection::Forward, &call_ref, &body, 0).await;
    sip_clock::testkit::settle().await;

    let after = ctx.state.peek(&call_ref).expect("the takeover copy still serves the call");
    assert_eq!(after.state, CallModelState::Active, "the fold ends no call");
    assert!(call::helpers::caller_answered(&after), "the copy took the authority's answer");
    assert!(n.cdr.snapshot().is_empty(), "a fold that ends nothing writes no record");
    // A fold adopts `max` on BOTH axes — (3,2) — and is not this node's
    // write: its own axis does not move.
    assert_eq!(live_pb(&n, &call_ref), (3, 2), "both counters adopted, ours not bumped");
}

/// A `Terminating` body ends no call in either direction — it is a teardown
/// still in flight — but the version it names is still seen, so the counter
/// is adopted and this node's next flush is not refused in its turn.
#[tokio::test(start_paused = true)]
async fn a_forward_terminating_body_only_moves_the_counter() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let live = answered(1, 2, CallModelState::Active);
    let call_ref = live.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

    let replica = answered(4, 1, CallModelState::Terminating);
    let body = MsgpackCodec::new().encode(&replica);
    fold_refused_flush(ctx, FlushDirection::Forward, &call_ref, &body, 0).await;
    sip_clock::testkit::settle().await;

    let after = ctx.state.peek(&call_ref).expect("the copy still serves the call");
    assert_eq!(after.state, CallModelState::Active, "a teardown in flight ends nothing");
    assert_eq!(live_pb(&n, &call_ref), (4, 2), "the seen `p` is adopted, and only it");
}

/// The Element `w1` holds for `call_ref`, decoded.
async fn element(n: &crate::router::test_support::Node, call_ref: &str) -> Call {
    let body = n.store.get_call(PartitionRole::Backup, "w0", call_ref).await.unwrap();
    MsgpackCodec::new().decode(&body.expect("the Element")).expect("it decodes")
}

/// **An adoption never writes over the version it adopted.** The takeover
/// copy ends a call it never saw answered at `(3,2)`; the primary's answered
/// `(6,1)` is refused here (`b` behind) and its fold is queued for the copy's
/// turn. Meanwhile the primary refuses the copy's ending, adopts `(6,2)` and
/// republishes its answered call there, which the Element takes. When the
/// copy's turn then adopts `(6,2)` from the queued refusal, the Element
/// already holds that version — the primary's — and keeps it.
#[tokio::test(start_paused = true)]
async fn a_backups_adoption_never_overwrites_the_primarys_body_at_the_adopted_version() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let copy = unanswered(3, 2, CallModelState::Terminating);
    let call_ref = copy.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(copy, MaterialiseOrigin::Takeover));
    let healed = answered(6, 2, CallModelState::Active);
    let body = MsgpackCodec::new().encode(&healed);
    let opts = PutOpts::default();
    let bak = PartitionRole::Backup;
    n.store.put_call(bak, "w0", &call_ref, body, &[], 60_000, 6, 2, &opts).await.unwrap();

    let refused = MsgpackCodec::new().encode(&answered(6, 1, CallModelState::Active));
    fold_refused_flush(ctx, FlushDirection::Forward, &call_ref, &refused, 0).await;
    sip_clock::testkit::settle().await;

    assert_eq!(live_pb(&n, &call_ref), (6, 2), "the copy adopts the primary's `p`");
    let held = element(&n, &call_ref).await;
    assert!(call::helpers::caller_answered(&held), "the Element keeps the primary's answer");
    assert_eq!(held.state, CallModelState::Active);
}

/// The direction must match the role this node plays for the ref: a
/// Forward-labelled command for a call this node is PRIMARY of is a
/// misrouted frame and folds nothing.
#[tokio::test(start_paused = true)]
async fn a_direction_that_contradicts_this_nodes_role_folds_nothing() {
    let n = node("w0").await; // w0 is the ref's primary, so Forward is wrong.
    let ctx = n.core.router_ctx();
    let live = answered(1, 2, CallModelState::Active);
    let call_ref = live.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

    let replica = answered(5, 1, CallModelState::Terminated);
    let body = MsgpackCodec::new().encode(&replica);
    fold_refused_flush(ctx, FlushDirection::Forward, &call_ref, &body, 0).await;
    sip_clock::testkit::settle().await;

    assert_eq!(live_pb(&n, &call_ref), (1, 2), "nothing was read, nothing was written");
    assert!(n.cdr.snapshot().is_empty(), "and nothing was discharged");
}

/// **A read never takes a deferred terminal's release.** A backup holds a
/// terminal it deferred to its dead primary; the body's TTL runs out and a
/// request for the call reaches the backup before the periodic reap does.
/// The takeover read finds nothing to serve, and the reap that follows
/// still releases the call's key and counts its CDR lost, once; a second
/// pass finds nothing.
#[tokio::test(start_paused = true)]
async fn an_expired_deferred_terminal_read_before_the_reap_is_still_released_once() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    ctx.limiter.hold_releases();
    let mut terminal = answered(1, 2, CallModelState::Terminated);
    terminal.limiter = call::CallLimiterState::admitted(
        "fold-key".into(),
        1,
        vec![
            call::LimiterEntry { id: "x".into(), limit: 10 },
            call::LimiterEntry { id: "x".into(), limit: 10 },
            call::LimiterEntry { id: "y".into(), limit: 10 },
        ],
    );
    let call_ref = terminal.call_ref.clone();
    let body = MsgpackCodec::new().encode(&terminal);
    n.store
        .put_call(
            PartitionRole::Backup,
            "w0",
            &call_ref,
            body,
            &[],
            1_000,
            1,
            2,
            &PutOpts::default(),
        )
        .await
        .unwrap();

    // Past the body's TTL, before the core's first reap tick (30 s).
    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    assert!(ctx.state.peek_replica(&call_ref).await.is_err(), "nothing left to serve");

    reap_expired_replicas(ctx, n.clock.now_ms()).await;
    assert_eq!(
        ctx.limiter.waiting_keys(),
        vec!["fold-key".to_string()],
        "the reap released the call by its key"
    );
    assert_eq!(n.metrics.repl_terminal_lost_total(), 1, "its CDR counted lost, once");
    assert!(
        n.store.peek_body_raw(PartitionRole::Backup, "w0", &call_ref).await.is_none(),
        "the reap evicted the body"
    );

    reap_expired_replicas(ctx, n.clock.now_ms()).await;
    assert_eq!(ctx.limiter.waiting(), 1, "a second pass releases nothing");
    assert_eq!(n.metrics.repl_terminal_lost_total(), 1, "and counts nothing");
}

/// **A panic in the store's other upkeep never takes the evicted bodies.**
/// With the replica changelog's lock poisoned, the reap still releases an
/// expired deferred terminal's key and counts its CDR lost, once.
#[tokio::test(start_paused = true)]
async fn a_poisoned_changelog_lock_does_not_cost_an_expired_terminal_its_release() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    ctx.limiter.hold_releases();
    let mut terminal = answered(1, 2, CallModelState::Terminated);
    terminal.limiter = call::CallLimiterState::admitted(
        "upkeep-key".into(),
        1,
        vec![
            call::LimiterEntry { id: "x".into(), limit: 10 },
            call::LimiterEntry { id: "y".into(), limit: 10 },
        ],
    );
    let call_ref = terminal.call_ref.clone();
    let body = MsgpackCodec::new().encode(&terminal);
    n.store
        .put_call(
            PartitionRole::Backup,
            "w0",
            &call_ref,
            body,
            &[],
            1_000,
            1,
            2,
            &PutOpts::default(),
        )
        .await
        .unwrap();
    n.store.changelog().poison_for_test();

    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    let pass = tokio::spawn({
        let ctx = ctx.clone();
        async move { reap_expired_replicas(&ctx, ctx.clock.now_ms()).await }
    });
    assert!(pass.await.is_ok(), "the reap pass completes");
    assert_eq!(ctx.limiter.waiting_keys(), vec!["upkeep-key".to_string()]);
    assert_eq!(n.metrics.repl_terminal_lost_total(), 1);
    reap_expired_replicas(ctx, n.clock.now_ms()).await;
    assert_eq!(ctx.limiter.waiting(), 1, "released once");
    assert_eq!(n.metrics.repl_terminal_lost_total(), 1, "counted once");
}

/// `call` as another call on the same ref: its own incarnation, holding
/// `x` under it.
fn as_call(mut call: Call, key: &str) -> Call {
    call.limiter = call::CallLimiterState::admitted(
        key.into(),
        1,
        vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
    );
    call
}

/// **A refused flush of another call never folds into the live one.** The
/// primary serves a retry on the identity of a call a backup took over and
/// ended; that call's terminal, refused by the store, reaches the fold. It
/// carries lifecycle progress over the retry's live copy, but it is another
/// call: folding it would end the retry.
#[tokio::test(start_paused = true)]
async fn a_refused_flush_of_another_call_never_ends_the_live_one() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let live = as_call(answered(1, 0, CallModelState::Active), "retry-key");
    let call_ref = live.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

    let replica = as_call(answered(1, 3, CallModelState::Terminated), "first-key");
    let body = MsgpackCodec::new().encode(&replica);
    fold_refused_flush(ctx, FlushDirection::Reverse, &call_ref, &body, 0).await;
    sip_clock::testkit::settle().await;

    let after = ctx.state.peek(&call_ref).expect("the retry is still served");
    assert_eq!(after.incarnation(), "retry-key");
    assert_eq!(after.state, CallModelState::Active, "the retry plays on");
    assert!(n.cdr.snapshot().is_empty(), "nothing was discharged");
}

/// **The reverse reconcile reads only its own call.** The primary's store
/// still holds a call its release is about to delete (the write is queued)
/// when the retry on the same identity is already live; a reconcile of the
/// ref reads that stored call, which is not the live one, and leaves the
/// retry alone.
#[tokio::test(start_paused = true)]
async fn a_reverse_reconcile_of_another_stored_call_never_ends_the_live_one() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let live = as_call(answered(1, 0, CallModelState::Active), "retry-key");
    let call_ref = live.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));
    let stored = as_call(answered(2, 1, CallModelState::Terminated), "first-key");
    n.store
        .put_call(
            PartitionRole::Primary,
            "w0",
            &call_ref,
            MsgpackCodec::new().encode(&stored),
            &[],
            60_000,
            2,
            1,
            &PutOpts::default(),
        )
        .await
        .unwrap();

    reconcile_reverse_flush(ctx, &call_ref).await;
    sip_clock::testkit::settle().await;

    let after = ctx.state.peek(&call_ref).expect("the retry is still served");
    assert_eq!(after.incarnation(), "retry-key");
    assert_eq!(after.state, CallModelState::Active, "the retry plays on");
    assert!(n.cdr.snapshot().is_empty(), "nothing was discharged");
}

/// **A deferred terminal another call replaced is released by its own
/// key.** A backup holds the terminal its takeover copy deferred to the
/// primary; the primary's next write on the ref is a retry, which replaces
/// it. The reap releases the replaced call's own key and counts its CDR
/// lost, once; the retry's key is never touched.
#[tokio::test(start_paused = true)]
async fn a_deferred_terminal_another_call_replaced_is_released_by_its_own_key() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    ctx.limiter.hold_releases();
    let terminal = as_call(answered(1, 2, CallModelState::Terminated), "first-key");
    let retry = as_call(unanswered(1, 0, CallModelState::Active), "retry-key");
    let call_ref = terminal.call_ref.clone();
    for (call, (p, b), ttl_ms) in [(&terminal, (1, 2), 30_000), (&retry, (1, 0), 600_000)] {
        let opts = PutOpts {
            origin_now_ms: Some(0),
            incarnation: Some(call.incarnation().to_string()),
            ..PutOpts::default()
        };
        let body = MsgpackCodec::new().encode(call);
        n.store
            .put_call(PartitionRole::Backup, "w0", &call_ref, body, &[], ttl_ms, p, b, &opts)
            .await
            .unwrap();
    }

    reap_expired_replicas(ctx, n.clock.now_ms()).await;
    assert_eq!(ctx.limiter.waiting_keys(), vec!["first-key".to_string()]);
    assert_eq!(n.metrics.repl_terminal_lost_total(), 1, "its CDR counted lost");
    reap_expired_replicas(ctx, n.clock.now_ms()).await;
    assert_eq!(ctx.limiter.waiting(), 1, "released once");
    assert_eq!(n.metrics.repl_terminal_lost_total(), 1, "counted once");
}

/// **A fold is not this node's write.** The backup's flush at our `p` with
/// its `b` advanced folds, and the live copy then holds the backup's version
/// exactly: our own axis does not move. The backup's next write — the same
/// `p`, its `b` once more, still no lifecycle progress — is then the next
/// version and folds in its turn, where a bumped `p` would refuse it.
#[tokio::test(start_paused = true)]
async fn a_reverse_fold_holds_the_backups_version_so_its_next_write_folds() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let live = answered(4, 1, CallModelState::Active);
    let call_ref = live.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));
    let relayed = in_round(4, 2, RoundStage::Open);
    let body = MsgpackCodec::new().encode(&relayed);
    let opts = PutOpts::default();
    let pri = PartitionRole::Primary;
    n.store.put_call(pri, "w0", &call_ref, body, &[], 60_000, 4, 2, &opts).await.unwrap();

    reconcile_reverse_flush(ctx, &call_ref).await;
    sip_clock::testkit::settle().await;

    assert_eq!(live_pb(&n, &call_ref), (4, 2), "the fold takes the backup's version, no bump");
    let after = ctx.state.peek(&call_ref).expect("the call plays on");
    let next = in_round(4, 3, RoundStage::Open);
    assert!(reverse_flush_dominates(&next, &after), "the backup's next write is the next version");
}

/// **An answered re-INVITE round folds over the vector.** The primary wrote
/// the call since the backup's copy branched, so the store refused the copy's
/// write; that write answers the round the live copy holds open, which only
/// the copy holding the round's client transaction could see. It folds, and
/// quietly: the live copy awaits the caller's ACK for the 2xx, its own axis
/// unmoved. The same write with the round still open is refused, its `b`
/// adopted alone.
#[tokio::test(start_paused = true)]
async fn a_refused_flush_answering_the_round_the_live_copy_holds_open_folds() {
    for (stage, folds) in [(RoundStage::Answered, true), (RoundStage::Open, false)] {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        let live = in_round(6, 1, RoundStage::Open);
        let call_ref = live.call_ref.clone();
        assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

        let replica = in_round(4, 2, stage);
        let body = MsgpackCodec::new().encode(&replica);
        fold_refused_flush(ctx, FlushDirection::Reverse, &call_ref, &body, 0).await;
        sip_clock::testkit::settle().await;

        let after = ctx.state.peek(&call_ref).expect("the call plays on");
        let awaited = after.a_leg.dialogs.confirmed().and_then(|d| d.awaited_ack_cseq);
        let expected = folds.then_some(STAGED_ROUND_CSEQ);
        assert_eq!(awaited, expected, "{stage:?}: the round's ACK is awaited iff it folded");
        assert_eq!(live_pb(&n, &call_ref), (6, 2), "{stage:?}: the backup's `b`, our `p`");
    }
}
