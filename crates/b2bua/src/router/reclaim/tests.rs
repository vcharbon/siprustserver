//! The meeting of two copies of a call at a live copy ([`meet_refused_flush`],
//! [`reconcile_reverse_flush`], ADR-0055 §7): the copy that contains the
//! other is taken whole with the other's exempt fields merged in, the
//! primary's when each contains the other, and two copies that diverged end
//! the call cleanly; a meeting is not this node's write (ADR-0014). The
//! replica reap's release of deferred terminals.

use call::{Call, CallModelState, LegState, MsgpackCodec, TerminationCause};

use super::{meet_refused_flush, reap_expired_replicas, reconcile_reverse_flush, FlushDirection};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, node, src, Node};
use crate::store::PutOpts;
use crate::store::{
    envelope, CallStore, CallTopology, Enveloped, MaterialiseOrigin, PartitionRole,
};

/// A replicable call at `(p, b)`, owned by `w0` and backed up by `w1`,
/// stamped there.
fn base(p: i64, b: i64) -> Enveloped {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let call = build_initial_call(
        &invite("w0", "w1", "meet"),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    Enveloped::new(call, Some(CallTopology::new("w0", "w1", p, b)))
}

/// `copy` as a replicated body.
fn body(copy: &Enveloped) -> Vec<u8> {
    envelope::encode(&MsgpackCodec::new(), copy.topology.as_ref(), &copy.call).unwrap()
}

/// The caller is ringing: no final has left toward her yet.
fn unanswered(p: i64, b: i64, state: CallModelState) -> Enveloped {
    let mut copy = base(p, b);
    let call = &mut copy.call;
    call.state = state;
    *call.a_leg.invite_final_sent_mut() = (state != CallModelState::Active).then_some(480);
    let a_state =
        if state == CallModelState::Active { LegState::Early } else { LegState::Terminated };
    call::testkit::stage_leg(&mut call.a_leg, a_state);
    copy
}

/// The caller was answered — a durable fact of the body: the 2xx this stack
/// sent her is recorded on the leg and the pair is bridged, both of which
/// outlive the leg going `Terminated`.
fn answered(p: i64, b: i64, state: CallModelState) -> Enveloped {
    let Enveloped { mut call, topology } = base(p, b);
    call.state = state;
    *call.a_leg.invite_final_sent_mut() = Some(200);
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
    call.a_leg.own_tag = Some("staged".into());
    if state != CallModelState::Active {
        call::testkit::stage_leg(&mut call.a_leg, LegState::Terminated);
        call::helpers::record_termination(&mut call, 0, TerminationCause::RemoteBye, None);
    }
    Enveloped::new(call, topology)
}

/// `call` stamped at `(p, b)`: its last write that changed a compared field.
fn stamped(mut copy: Enveloped, p: i64, b: i64) -> Enveloped {
    let t = copy.topology.as_mut().expect("a replicable call");
    (t.stamp_gen, t.stamp_bak_gen) = (p, b);
    copy
}

/// `call` once this stack sent `n` more requests to the caller.
fn sent_to_the_caller(mut copy: Enveloped, n: i64) -> Enveloped {
    copy.call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog").our_cseq += n;
    copy
}

fn callers_sequence(call: &Call) -> i64 {
    call.a_leg.dialogs.confirmed().expect("the caller's dialog").our_cseq
}

/// The vector and the stamp of the live copy `n` holds for `call_ref`.
fn live_versions(n: &Node, call_ref: &str) -> ((i64, i64), (i64, i64)) {
    let state = &n.core.router_ctx().state;
    assert!(state.peek(call_ref).is_some(), "the copy is live");
    let t = state.topology(call_ref).expect("the copy is replicable");
    (t.vector(), t.stamp())
}

/// `n` serving `live` as `origin`, meeting `incoming` from `direction`.
async fn meet(
    n: &Node,
    live: &Enveloped,
    incoming: &Enveloped,
    direction: FlushDirection,
    origin: MaterialiseOrigin,
) {
    let ctx = n.core.router_ctx();
    assert!(ctx.state.materialize_if_absent(live.clone(), origin));
    meet_refused_flush(ctx, direction, &live.call.call_ref, &body(incoming), 0).await;
    sip_clock::testkit::settle().await;
}

/// **A copy the record contains brings its exempt fields, and nothing
/// else.** The backup sent a request to the caller since it read the
/// record's last compared write: the record keeps its own content and takes
/// the higher sequence and both counters, its stamp its own.
#[tokio::test(start_paused = true)]
async fn a_copy_the_record_contains_brings_its_exempt_fields_alone() {
    let n = node("w0").await;
    let mut live = answered(6, 1, CallModelState::Active);
    live.call.callback_context = Some("the record's".into());
    let copy = sent_to_the_caller(stamped(answered(4, 2, CallModelState::Active), 4, 1), 1);
    meet(&n, &live, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;

    let after = n.core.router_ctx().state.peek(&live.call.call_ref).expect("the call plays on");
    assert_eq!(callers_sequence(&after), callers_sequence(&copy.call), "the backup's request");
    assert_eq!(after.callback_context, live.call.callback_context, "the record's own content");
    assert_eq!(live_versions(&n, &live.call.call_ref), ((6, 2), (6, 1)));
    assert_eq!(n.metrics.repl_reverse_flush_refused_total(), 1, "the record kept its copy");
    assert_eq!(n.metrics.repl_copies_diverged_total(), 0);
}

/// **A copy that contains the record is taken whole.** The backup answered
/// the caller, and the record wrote nothing compared since the backup read
/// it — only a keepalive: the record takes the backup's copy, the vector at
/// the higher of each counter and no bump, the stamp the backup's.
#[tokio::test(start_paused = true)]
async fn a_copy_that_contains_the_record_is_taken_whole() {
    let n = node("w0").await;
    let live = stamped(unanswered(5, 1, CallModelState::Active), 4, 1);
    let copy = answered(4, 2, CallModelState::Active);
    meet(&n, &live, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;

    let after = n.core.router_ctx().state.peek(&live.call.call_ref).expect("the call plays on");
    assert!(call::helpers::caller_answered(&after), "the backup's answer");
    assert_eq!(live_versions(&n, &live.call.call_ref), ((5, 2), (4, 2)));
    assert!(n.cdr.snapshot().is_empty(), "nothing ends");
}

/// **Meetings log per peer, not per call** (ADR-0026): each meeting is a
/// `debug!` line of its call, and the burst one episode keyed by the other
/// owner, its outcomes counted.
#[tokio::test(start_paused = true)]
async fn meetings_log_one_episode_per_peer_and_no_info_line_per_call() {
    let (_capture, log) = observe_testkit::test_buffer();
    let n = node("w0").await;
    let live = answered(6, 1, CallModelState::Active);
    let copy = sent_to_the_caller(stamped(answered(4, 2, CallModelState::Active), 4, 1), 1);
    meet(&n, &live, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;

    let met = log.matching("copies met");
    assert!(met.iter().all(|e| e.level != "INFO" || e.fields.iter().any(|(k, _)| k == "edge")));
    let episode = met.iter().find(|e| e.level == "INFO").expect("the episode's rising edge");
    assert!(episode.fields.iter().any(|(k, v)| k == "peer" && v == "w1"), "{}", episode.line());
}

/// **Copies that contain each other keep the primary's.** Each wrote only
/// exempt fields since their common version: at the primary its own copy
/// stands; at the backup the primary's write stands the takeover copy down.
#[tokio::test(start_paused = true)]
async fn copies_that_contain_each_other_keep_the_primarys() {
    let mut primary = stamped(unanswered(5, 1, CallModelState::Active), 4, 1);
    primary.call.callback_context = Some("the primary's".into());
    let copy = stamped(unanswered(4, 2, CallModelState::Active), 4, 1);

    let n = node("w0").await;
    meet(&n, &primary, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;
    let after = n.core.router_ctx().state.peek(&primary.call.call_ref).expect("the call plays on");
    assert_eq!(after.callback_context, primary.call.callback_context);
    assert_eq!(live_versions(&n, &primary.call.call_ref), ((5, 2), (4, 1)));

    let n = node("w1").await;
    meet(&n, &copy, &primary, FlushDirection::Forward, MaterialiseOrigin::Takeover).await;
    assert!(n.core.router_ctx().state.peek(&copy.call.call_ref).is_none(), "the copy stood down");
    assert!(n.cdr.snapshot().is_empty(), "a copy that stands down writes no record");
}

/// Whether `n` no longer serves `call_ref` as a live call: ending, or gone
/// with its one CDR written.
fn ending(n: &Node, call_ref: &str) -> bool {
    match n.core.router_ctx().state.peek(call_ref) {
        Some(c) => c.state != CallModelState::Active,
        None => n.cdr.snapshot().len() == 1,
    }
}

/// **Copies that diverged end the call cleanly.** Each made a compared write
/// the other never read: the record ends the call from the union of both, by
/// a write of its own over both versions, its CDR closed on the divergence.
#[tokio::test(start_paused = true)]
async fn diverged_copies_end_the_call_cleanly() {
    let n = node("w0").await;
    let live = answered(6, 1, CallModelState::Active);
    let copy = sent_to_the_caller(answered(4, 2, CallModelState::Active), 3);
    meet(&n, &live, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;

    assert_eq!(n.metrics.repl_copies_diverged_total(), 1);
    assert!(ending(&n, &live.call.call_ref), "the call ends");
}

/// **The copy ranked higher is the base of the end.** The backup answered
/// the caller the record still holds ringing, and the record wrote a
/// compared field the backup never read: the call ends from the backup's
/// copy, the caller answered.
#[tokio::test(start_paused = true)]
async fn diverged_copies_end_from_the_one_that_answered_the_caller() {
    let n = node("w0").await;
    let mut live = unanswered(6, 1, CallModelState::Active);
    live.call.callback_context = Some("the record's".into());
    let copy = answered(4, 2, CallModelState::Active);
    meet(&n, &live, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;

    assert!(ending(&n, &live.call.call_ref), "the call ends");
    if let Some(after) = n.core.router_ctx().state.peek(&live.call.call_ref) {
        assert!(call::helpers::caller_answered(&after), "the backup's answer stands");
        assert_eq!(after.callback_context, None, "the backup's copy is the base");
    }
}

/// **A takeover copy ends the call on a primary it diverged from.** The
/// primary wrote behind a split while the copy relayed a request of its own:
/// the copy ends the call cleanly from the primary's copy, the base on a tie.
#[tokio::test(start_paused = true)]
async fn a_takeover_copy_ends_the_call_on_a_primary_it_diverged_from() {
    let n = node("w1").await;
    let copy = sent_to_the_caller(answered(4, 2, CallModelState::Active), 1);
    let mut primary = answered(6, 1, CallModelState::Active);
    primary.call.callback_context = Some("the primary's".into());
    meet(&n, &copy, &primary, FlushDirection::Forward, MaterialiseOrigin::Takeover).await;

    assert_eq!(n.metrics.repl_copies_diverged_total(), 1);
    assert!(ending(&n, &copy.call.call_ref), "the call ends");
}

/// **The backup's ending ends the call at the record**; its teardown still in
/// flight leaves the record as it is.
#[tokio::test(start_paused = true)]
async fn the_backups_ending_is_discharged_and_its_teardown_awaited() {
    let n = node("w0").await;
    let live = answered(6, 1, CallModelState::Active);
    let ending = answered(4, 2, CallModelState::Terminating);
    meet(&n, &live, &ending, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;
    let after = n.core.router_ctx().state.peek(&live.call.call_ref).expect("the call plays on");
    assert_eq!(after.state, CallModelState::Active, "a teardown in flight is awaited");
    assert_eq!(live_versions(&n, &live.call.call_ref).0, (6, 1), "nothing of it is read");

    let ended = answered(4, 3, CallModelState::Terminated);
    let body = body(&ended);
    meet_refused_flush(n.core.router_ctx(), FlushDirection::Reverse, &live.call.call_ref, &body, 0)
        .await;
    sip_clock::testkit::settle().await;
    assert!(n.core.router_ctx().state.peek(&live.call.call_ref).is_none(), "discharged");
    assert_eq!(n.cdr.snapshot().len(), 1, "one record");
}

/// **A copy that does not rebuild the caller's INVITE is never taken**
/// (ADR-0052 §11): the live copy, which does, keeps serving the call at its
/// own vector, and the refusal is counted.
#[tokio::test(start_paused = true)]
async fn a_copy_that_does_not_rebuild_the_callers_invite_is_never_taken() {
    let n = node("w0").await;
    let live = unanswered(4, 1, CallModelState::Active);
    let mut copy = answered(4, 2, CallModelState::Active);
    copy.call.a_leg_invite.headers.clear();
    meet(&n, &live, &copy, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;

    let after = n.core.router_ctx().state.peek(&live.call.call_ref).expect("the live copy serves");
    assert_eq!(after.a_leg_invite, live.call.a_leg_invite, "the caller's INVITE is the live one");
    assert!(!call::helpers::caller_answered(&after), "nothing compared of the copy landed");
    assert_eq!(n.metrics.unrecoverable_meetings_total(), 1);
    assert_eq!(live_versions(&n, &live.call.call_ref).0, (4, 1), "no vector without its body");
}

/// **A meeting is not this node's write.** The backup's write the store took
/// over our copy is taken whole, and the live copy holds the backup's version
/// exactly: our own axis does not move, so the backup's next write descends
/// from it and is taken in its turn.
#[tokio::test(start_paused = true)]
async fn a_taken_copy_holds_the_backups_version_so_its_next_write_is_taken() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let live = unanswered(4, 1, CallModelState::Active);
    let call_ref = live.call.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));
    let answered_there = answered(4, 2, CallModelState::Active);
    let first = body(&answered_there);
    let opts = PutOpts::default();
    let pri = PartitionRole::Primary;
    n.store.put_call(pri, "w0", &call_ref, first, &[], 60_000, 4, 2, &opts).await.unwrap();
    reconcile_reverse_flush(ctx, &call_ref).await;
    sip_clock::testkit::settle().await;
    assert_eq!(live_versions(&n, &call_ref), ((4, 2), (4, 2)), "the backup's version, no bump");

    let mut next = answered(4, 3, CallModelState::Active);
    next.call.callback_context = Some("the backup's next".into());
    let next_body = body(&next);
    meet_refused_flush(ctx, FlushDirection::Reverse, &call_ref, &next_body, 0).await;
    sip_clock::testkit::settle().await;
    let after = ctx.state.peek(&call_ref).expect("the call plays on");
    assert_eq!(after.callback_context, next.call.callback_context, "the next write is taken");
}

/// **Only the timers a taken copy brings are moved into this node's clock.**
/// The other owner's clock runs 5 s ahead of ours; the record contains the
/// backup's copy, and its own timer keeps the deadline this node armed it at.
#[tokio::test(start_paused = true)]
async fn a_kept_copy_leaves_its_own_timers_where_it_armed_them() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let now = ctx.clock.now_ms();
    let mut live = answered(6, 1, CallModelState::Active);
    let own = call::TimerEntry {
        key: call::TimerKey::call(call::TimerType::GlobalDuration),
        generation: 3,
        fire_at: now + 600_000,
    };
    live.call.timers.push(own.clone());
    let call_ref = live.call.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

    let copy = stamped(answered(4, 2, CallModelState::Active), 4, 1);
    let body = body(&copy);
    meet_refused_flush(ctx, FlushDirection::Reverse, &call_ref, &body, now + 5_000).await;
    sip_clock::testkit::settle().await;

    let after = ctx.state.peek(&call_ref).expect("the call plays on");
    let kept = after.timers.iter().find(|t| t.key == own.key).expect("the own timer stays");
    assert_eq!(kept.fire_at, own.fire_at, "not re-anchored by the other owner's skew");
}

/// The direction must match the role this node plays for the ref: a
/// Forward-labelled command for a call this node is PRIMARY of is a
/// misrouted frame and meets nothing.
#[tokio::test(start_paused = true)]
async fn a_direction_that_contradicts_this_nodes_role_meets_nothing() {
    let n = node("w0").await; // w0 is the ref's primary, so Forward is wrong.
    let ctx = n.core.router_ctx();
    let live = answered(1, 2, CallModelState::Active);
    let call_ref = live.call.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

    let replica = answered(5, 1, CallModelState::Terminated);
    let body = body(&replica);
    meet_refused_flush(ctx, FlushDirection::Forward, &call_ref, &body, 0).await;
    sip_clock::testkit::settle().await;

    assert_eq!(live_versions(&n, &call_ref).0, (1, 2), "nothing was read, nothing was written");
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
    terminal.call.limiter = call::CallLimiterState::admitted(
        "fold-key".into(),
        1,
        vec![
            call::LimiterEntry { id: "x".into(), limit: 10 },
            call::LimiterEntry { id: "x".into(), limit: 10 },
            call::LimiterEntry { id: "y".into(), limit: 10 },
        ],
    );
    let call_ref = terminal.call.call_ref.clone();
    let body = body(&terminal);
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
    terminal.call.limiter = call::CallLimiterState::admitted(
        "upkeep-key".into(),
        1,
        vec![
            call::LimiterEntry { id: "x".into(), limit: 10 },
            call::LimiterEntry { id: "y".into(), limit: 10 },
        ],
    );
    let call_ref = terminal.call.call_ref.clone();
    let body = body(&terminal);
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
fn as_call(mut copy: Enveloped, key: &str) -> Enveloped {
    copy.call.limiter = call::CallLimiterState::admitted(
        key.into(),
        1,
        vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
    );
    copy
}

/// **A refused flush of another call never meets the live one.** The
/// primary serves a retry on the identity of a call a backup took over and
/// ended; that call's terminal, refused by the store, reaches the meeting.
/// It is another call: meeting it would end the retry.
#[tokio::test(start_paused = true)]
async fn a_refused_flush_of_another_call_never_ends_the_live_one() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let live = as_call(answered(1, 0, CallModelState::Active), "retry-key");
    let call_ref = live.call.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

    let replica = as_call(answered(1, 3, CallModelState::Terminated), "first-key");
    let body = body(&replica);
    meet_refused_flush(ctx, FlushDirection::Reverse, &call_ref, &body, 0).await;
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
    let call_ref = live.call.call_ref.clone();
    assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));
    let stored = as_call(answered(2, 1, CallModelState::Terminated), "first-key");
    n.store
        .put_call(
            PartitionRole::Primary,
            "w0",
            &call_ref,
            body(&stored),
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
    let call_ref = terminal.call.call_ref.clone();
    for (call, (p, b), ttl_ms) in [(&terminal, (1, 2), 30_000), (&retry, (1, 0), 600_000)] {
        let opts = PutOpts {
            origin_now_ms: Some(0),
            incarnation: Some(call.call.incarnation().to_string()),
            ..PutOpts::default()
        };
        let body = body(call);
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

/// `call` with an INFO relayed toward the caller by worker `by`, awaiting its
/// final.
fn relaying(mut copy: Enveloped, by: call::RelayOwner) -> Enveloped {
    let d = copy.call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.our_cseq += 1;
    let mut record = call::RequestRecord::relayed("INFO", d.our_cseq, 9);
    record.relayed_by = by;
    d.requests.push(record);
    copy
}

/// **A contained copy that plays on does so from the primary's copy.** The
/// primary's write contains the takeover copy but lacks the INFO the copy
/// relays, whose final returns to the copy: the copy plays on from the
/// primary's copy taken whole, its own relay carried, and never claims the
/// primary's vector over its own body.
#[tokio::test(start_paused = true)]
async fn a_contained_copy_that_plays_on_takes_the_primarys_copy_with_its_own_records() {
    let n = node("w1").await;
    let by = n.core.router_ctx().relay_owner.clone();
    let copy = relaying(answered(4, 2, CallModelState::Active), by.clone());
    let mut primary = answered(6, 2, CallModelState::Active);
    primary.call.callback_context = Some("the primary's".into());
    meet(&n, &copy, &primary, FlushDirection::Forward, MaterialiseOrigin::Takeover).await;

    let after = n.core.router_ctx().state.peek(&copy.call.call_ref).expect("the copy plays on");
    assert_eq!(after.callback_context, primary.call.callback_context, "the primary's copy, whole");
    let d = after.a_leg.dialogs.confirmed().expect("the caller's dialog");
    assert!(d.requests.iter().any(|r| r.relayed_by == by), "the copy's own relay is kept");
    assert_eq!(live_versions(&n, &copy.call.call_ref), ((6, 2), (6, 2)), "the primary's version");
}

/// **Copies that contain each other play on from the primary's.** The two
/// copies made exempt writes alone since their common version, and the
/// takeover copy relays an INFO the primary's write lacks: the copy plays on
/// from the primary's copy taken whole, as the store keeps it, with its own
/// relay, leg-id space and keepalive (ADR-0055 §7).
#[tokio::test(start_paused = true)]
async fn copies_that_contain_each_other_play_on_from_the_primarys_copy() {
    let n = node("w1").await;
    let by = n.core.router_ctx().relay_owner.clone();
    let mut copy = relaying(stamped(answered(4, 2, CallModelState::Active), 4, 1), by.clone());
    copy.call.leg_space = call::LegSpace::Other;
    let keepalive = |fire_at| call::TimerEntry {
        key: call::TimerKey::call(call::TimerType::Keepalive),
        generation: 1,
        fire_at,
    };
    copy.call.timers.push(keepalive(300_000));
    let mut primary = stamped(answered(5, 1, CallModelState::Active), 4, 1);
    primary.call.callback_context = Some("the primary's".into());
    primary.call.timers.push(keepalive(200_000));
    meet(&n, &copy, &primary, FlushDirection::Forward, MaterialiseOrigin::Takeover).await;

    let after = n.core.router_ctx().state.peek(&copy.call.call_ref).expect("the copy plays on");
    assert_eq!(after.callback_context, primary.call.callback_context, "the primary's copy, whole");
    assert_eq!(after.leg_space, call::LegSpace::Other, "the copy mints from its own space");
    assert_eq!(after.timers, [keepalive(300_000)], "the copy runs its own keepalive");
    let d = after.a_leg.dialogs.confirmed().expect("the caller's dialog");
    assert!(d.requests.iter().any(|r| r.relayed_by == by), "the copy's own relay is kept");
    assert_eq!(live_versions(&n, &copy.call.call_ref), ((5, 2), (4, 1)), "the primary's stamp");
}

/// `call` with a confirmed callee leg `b-9` only this copy dialled.
fn with_a_leg_only_here(mut copy: Enveloped) -> Enveloped {
    let mut leg = call::testkit::dialled_leg("b-9", "only-here-tag");
    call::testkit::stage_leg(&mut leg, LegState::Confirmed);
    copy.call.b_legs.push(leg);
    copy
}

/// **An ending copy ranked higher still ends a dialog only the record
/// holds.** The backup ended the call behind the split while the record
/// confirmed a callee leg the backup never read: meeting the backup's
/// terminal, the record BYEs that leg before it discharges the call.
#[tokio::test(start_paused = true)]
async fn an_ending_ranked_higher_still_ends_a_leg_only_the_record_holds() {
    let n = node("w0").await;
    let live = with_a_leg_only_here(answered(6, 1, CallModelState::Active));
    let ended = answered(4, 2, CallModelState::Terminated);
    meet(&n, &live, &ended, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;

    assert_eq!(n.metrics.repl_copies_diverged_total(), 1, "the record ends the call");
    let ending = |c: &Call| {
        let leg = c.b_legs.iter().find(|l| l.leg_id == "b-9").expect("the leg only here");
        leg.state() == LegState::Terminated
    };
    match n.core.router_ctx().state.peek(&live.call.call_ref) {
        Some(after) => assert!(ending(&after), "the leg only here is ending"),
        None => {
            let cdrs = n.cdr.snapshot();
            assert_eq!(cdrs.len(), 1, "one record");
            assert!(cdrs[0].b_legs.iter().any(|l| l.leg_id == "b-9"), "it holds the leg only here");
            let reason = cdrs[0].events.iter().filter_map(|e| e.reason.as_deref()).next_back();
            assert_eq!(reason, Some("diverged"), "the record's teardown ended the call");
        }
    }
}

/// `copy` with a caller-facing dialog the answer retired, holding an INFO
/// worker `by` relayed into it, awaiting its final.
fn relaying_on_a_retired_dialog(mut copy: Enveloped, by: call::RelayOwner) -> Enveloped {
    let mut d = call::Dialog::new("retired", "retired-peer", call::DialogState::Retired, 1);
    let mut record = call::RequestRecord::relayed("INFO", 2, 9);
    record.relayed_by = by;
    d.requests.push(record);
    copy.call.a_leg.dialogs.insert(d);
    copy
}

/// **An ending copy ranked higher still ends a relay a retired dialog holds
/// here.** The record holds an INFO this worker relayed into a dialog the
/// answer retired, whose final returns here alone (RFC 3261 §18.2.2): as for
/// a live dialog's relay, the record's own teardown answers it.
#[tokio::test(start_paused = true)]
async fn an_ending_ranked_higher_still_ends_a_relay_a_retired_dialog_holds_here() {
    let n = node("w0").await;
    let by = n.core.router_ctx().relay_owner.clone();
    let live = relaying_on_a_retired_dialog(answered(6, 1, CallModelState::Active), by);
    let ended = answered(4, 2, CallModelState::Terminated);
    meet(&n, &live, &ended, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;

    assert_eq!(n.metrics.repl_copies_diverged_total(), 1, "the record ends the call");
}

/// `copy` ringing the caller from this node: the 180 it relayed opened the
/// caller's early dialog under its own tag, fronting callee leg `b-1`'s early
/// dialog.
fn ringing_here(mut copy: Enveloped) -> Enveloped {
    let mut leg = call::testkit::dialled_leg("b-1", "dialled-tag");
    leg.dialogs.insert(call::Dialog::new("dialled-tag", "callee-tag", call::DialogState::Early, 1));
    copy.call.b_legs.push(leg);
    copy.call.a_leg.own_tag = Some("ringing-tag".into());
    let d = call::Dialog::new("ringing-tag", "caller-tag", call::DialogState::Early, 0);
    copy.call.a_leg.dialogs.insert(d);
    copy
}

/// `copy` ended by the caller's CANCEL before any 1xx reached her from it:
/// she took a 487, and leg `b-1`'s INVITE was CANCELled and took its 487, no
/// dialog of either kept.
fn cancelled_before_any_ringing(mut copy: Enveloped) -> Enveloped {
    *copy.call.a_leg.invite_final_sent_mut() = Some(487);
    let mut leg = call::testkit::dialled_leg("b-1", "dialled-tag");
    let invite = leg.outbound_invite_mut().expect("the dialled INVITE");
    invite.cancel_sent = true;
    invite.record_final(487);
    copy.call.b_legs.push(leg);
    copy
}

/// **The end over two diverged copies keeps the model whole.** The record
/// rang the caller from the callee's early dialog; the backup, behind the
/// split, took her CANCEL and both INVITEs' 487s. The union keeps the
/// record's dialogs: the caller's under her leg's own tag, and every early
/// dialog of a leg whose INVITE took a final ended by it (RFC 3261 §12.1).
#[tokio::test(start_paused = true)]
async fn the_end_over_a_cancelled_copy_ends_the_early_dialogs_the_record_holds() {
    let mut n = node("w0").await;
    let live = ringing_here(base(6, 1));
    let ended = cancelled_before_any_ringing(unanswered(4, 2, CallModelState::Terminated));
    meet(&n, &live, &ended, FlushDirection::Reverse, MaterialiseOrigin::Reclaim).await;

    assert_eq!(n.metrics.repl_copies_diverged_total(), 1, "the record ends the call");
    let violated: Vec<String> =
        n.take_violations().iter().filter_map(|v| v.lines().next().map(String::from)).collect();
    assert_eq!(violated, Vec::<String>::new(), "the union states the model");
}
