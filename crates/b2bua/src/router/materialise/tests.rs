//! One test per disposition of [`materialise`], driven directly over a fully
//! wired core (real router, store, timer driver and transaction layer on a
//! simulated fabric) so the post-materialise list runs for real. Each test
//! holds the per-call lock across the call, as every caller does.

use call::{Call, CallBodyCodec, CallModelState, MsgpackCodec, TimerEntry, TimerType};
use sip_clock::Clock;

use super::{materialise, Materialised, Origin, Reason};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::repl::ReplicatingCallStore;
use crate::router::test_support::{invite, node, src};
use crate::store::{CallStore, PartitionRole, PutOpts};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

/// A call `pri` owns and `bak` backs up, in `state`, carrying one future-dated
/// keepalive so the timer re-arm does real work without firing.
fn call_in(pri: &str, bak: &str, cid: &str, state: CallModelState, clock: &Clock) -> Call {
    let config = B2buaConfig { self_ordinal: pri.into(), ..Default::default() };
    let mut call = build_initial_call(&invite(pri, bak, cid), src(), &config, 0);
    call.state = state;
    if state != CallModelState::Active {
        // A synthetic terminal states its cause as every live path does.
        call = call::helpers::record_termination(call, 0, call::TerminationCause::Supervisor, None);
    }
    call.timers.push(TimerEntry {
        id: format!("keepalive-{cid}"),
        timer_type: TimerType::Keepalive,
        fire_at: clock.now_ms() + 300_000,
        leg_id: None,
    });
    call
}

/// Seed `call` into `(role, primary)` of the replicating store.
async fn put(store: &ReplicatingCallStore, role: PartitionRole, primary: &str, call: &Call) {
    let body = MsgpackCodec::new().encode(call);
    let gen = call.topology.as_ref().map(|t| t.gen).unwrap_or(1);
    store
        .put_call(role, primary, &call.call_ref, body, &[], 60_000, gen, 0, &PutOpts::default())
        .await
        .unwrap();
}

/// The `Served` call of a disposition, or the disposition it was instead.
fn served(d: Materialised) -> Call {
    match d {
        Materialised::Served(c) => c,
        other => panic!("expected Served, got {other:?}"),
    }
}

/// The `Resident` call of a disposition, or the disposition it was instead.
fn resident(d: Materialised) -> Call {
    match d {
        Materialised::Resident(c) => c,
        other => panic!("expected Resident, got {other:?}"),
    }
}

/// The value `e`'s `totals` field carries for `counter` (`0` when absent): the
/// field is the `name=n` run a `Tally` renders.
fn tallied(e: &observe::CapturedEvent, counter: &str) -> u64 {
    e.fields
        .iter()
        .find(|(n, _)| n == "totals")
        .map(|(_, v)| v.as_str())
        .unwrap_or_default()
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix(counter)?.strip_prefix('=')?.parse().ok())
        .unwrap_or(0)
}

/// The rising edge of the one takeover episode the log carries, keyed by `peer`.
fn takeover_rising(log: &observe::TestLogHandle, peer: &str) -> observe::CapturedEvent {
    let events = log.matching("acting-backup takeover");
    let rising: Vec<_> = events
        .iter()
        .filter(|e| e.fields.iter().any(|(n, v)| n == "edge" && v == "rising"))
        .cloned()
        .collect();
    assert_eq!(rising.len(), 1, "one takeover episode opens: {:?}", log.lines());
    assert!(
        rising[0].fields.iter().any(|(n, v)| n == "peer" && v == peer),
        "the episode is keyed by the dead peer: {}",
        rising[0].line()
    );
    rising[0].clone()
}

// ---------------------------------------------------------------------------
// Takeover
// ---------------------------------------------------------------------------

/// A `bak:` replica is served once — inserted, timers re-armed, marked,
/// counted and logged — and found resident on the next call, which re-arms
/// nothing.
#[tokio::test(start_paused = true)]
async fn takeover_serves_a_backup_replica_once_then_finds_it_resident() {
    let (_capture, log) = observe::test_buffer();
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = call_in("w0", "w1", "cid-served", CallModelState::Active, &n.clock);
    let r = call.call_ref.clone();
    put(&n.store, BAK, "w0", &call).await;

    let _guard = ctx.state.lock(&r).await;
    let live = served(materialise(ctx, &r, Origin::Takeover).await);
    assert_eq!(live.call_ref, r);
    assert!(ctx.state.is_takeover(&r), "the copy is marked for self-release");
    assert!(ctx.state.peek(&r).is_some(), "the copy is live");
    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 1);
    assert_eq!(n.metrics.repl_takeover_refused_terminated_total(), 0);
    sip_clock::testkit::settle().await;
    assert_eq!(n.metrics.timer_queue_len(), 1, "the replicated keepalive is re-armed here");
    assert_eq!(tallied(&takeover_rising(&log, "w0"), "hydrated"), 1);

    let again = resident(materialise(ctx, &r, Origin::Takeover).await);
    assert_eq!(again.call_ref, r);
    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 1, "a resident copy is no hydration");
    sip_clock::testkit::settle().await;
    assert_eq!(n.metrics.timer_queue_len(), 1, "a resident copy re-arms nothing");
}

/// A `Terminated` replica — the image of a call that already ended — is
/// refused, counted, logged and left in place for its primary; a `Terminating`
/// one still owes the teardown's end and is served (D1).
#[tokio::test(start_paused = true)]
async fn takeover_refuses_a_terminated_replica_and_serves_a_terminating_one() {
    let (_capture, log) = observe::test_buffer();
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let ended = call_in("w0", "w1", "cid-ended", CallModelState::Terminated, &n.clock);
    let r_ended = ended.call_ref.clone();
    put(&n.store, BAK, "w0", &ended).await;

    let _guard = ctx.state.lock(&r_ended).await;
    assert!(matches!(
        materialise(ctx, &r_ended, Origin::Takeover).await,
        Materialised::Refused(Reason::Terminated)
    ));
    assert_eq!(n.metrics.repl_takeover_refused_terminated_total(), 1);
    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 0);
    assert!(ctx.state.peek(&r_ended).is_none(), "nothing went live");
    assert!(!ctx.state.is_takeover(&r_ended), "nothing was marked");
    assert!(
        n.store.get_call(BAK, "w0", &r_ended).await.unwrap().is_some(),
        "the replica stays for the primary to fold or reclaim"
    );
    assert_eq!(tallied(&takeover_rising(&log, "w0"), "refused_terminated"), 1);
    drop(_guard);

    let dying = call_in("w0", "w1", "cid-dying", CallModelState::Terminating, &n.clock);
    let r_dying = dying.call_ref.clone();
    put(&n.store, BAK, "w0", &dying).await;
    let _guard = ctx.state.lock(&r_dying).await;
    let live = served(materialise(ctx, &r_dying, Origin::Takeover).await);
    assert_eq!(live.state, CallModelState::Terminating);
    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 1);
    assert_eq!(n.metrics.repl_takeover_refused_terminated_total(), 1, "no second refusal");
}

/// Every takeover miss is typed: a ref this node is primary for, a ref with no
/// replica, a body that does not decode. None counts as anything.
#[tokio::test(start_paused = true)]
async fn takeover_misses_are_typed() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();

    let own = call_in("w1", "w0", "cid-own", CallModelState::Active, &n.clock);
    put(&n.store, PRI, "w1", &own).await;
    assert!(matches!(
        materialise(ctx, &own.call_ref, Origin::Takeover).await,
        Materialised::Refused(Reason::NotBackupRole)
    ));

    assert!(matches!(
        materialise(ctx, "w0|nobody|t", Origin::Takeover).await,
        Materialised::Refused(Reason::NoReplica)
    ));

    n.store
        .put_call(
            BAK,
            "w0",
            "w0|garbage|t",
            vec![0xAB, 0xCD],
            &[],
            60_000,
            1,
            0,
            &PutOpts::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        materialise(ctx, "w0|garbage|t", Origin::Takeover).await,
        Materialised::Refused(Reason::Decode)
    ));

    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 0);
    assert_eq!(n.metrics.repl_takeover_refused_terminated_total(), 0);
    assert_eq!(ctx.state.active_count(), 0);
}

// ---------------------------------------------------------------------------
// Reclaim
// ---------------------------------------------------------------------------

/// A `pri:` body is served once — inserted, timers re-armed, counted, its
/// straggler line written — with no takeover mark, and found resident next.
#[tokio::test(start_paused = true)]
async fn reclaim_serves_an_own_replica_once_then_finds_it_resident() {
    let (_capture, log) = observe::test_buffer();
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let call = call_in("w1", "w0", "cid-reclaim", CallModelState::Active, &n.clock);
    let r = call.call_ref.clone();
    put(&n.store, PRI, "w1", &call).await;

    let _guard = ctx.state.lock(&r).await;
    let live = served(materialise(ctx, &r, Origin::Reclaim(None)).await);
    assert_eq!(live.call_ref, r);
    assert!(!ctx.state.is_takeover(&r), "an own call is no takeover copy");
    assert!(ctx.state.peek(&r).is_some(), "the call is live");
    assert_eq!(n.metrics.repl_reclaimed_total(), 1);
    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 0);
    sip_clock::testkit::settle().await;
    assert_eq!(n.metrics.timer_queue_len(), 1, "the replicated keepalive is re-armed here");
    assert_eq!(log.matching("straggler reclaim").len(), 1, "{:?}", log.lines());

    let again = resident(materialise(ctx, &r, Origin::Reclaim(None)).await);
    assert_eq!(again.call_ref, r);
    assert_eq!(n.metrics.repl_reclaimed_total(), 1, "a resident call is no reclaim");
    sip_clock::testkit::settle().await;
    assert_eq!(n.metrics.timer_queue_len(), 1, "a resident call re-arms nothing");
}

/// A terminal `pri:` body is a backup's deferred terminal (ADR-0020 X3): it is
/// discharged through the funnel — one CDR, the body gone from the live map —
/// never re-served. `Terminating` is forced terminal and discharged too.
#[tokio::test(start_paused = true)]
async fn reclaim_discharges_a_terminal_body_instead_of_serving_it() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();
    let ended = call_in("w1", "w0", "cid-ended", CallModelState::Terminated, &n.clock);
    let r_ended = ended.call_ref.clone();
    put(&n.store, PRI, "w1", &ended).await;

    let _guard = ctx.state.lock(&r_ended).await;
    assert!(matches!(
        materialise(ctx, &r_ended, Origin::Reclaim(None)).await,
        Materialised::Refused(Reason::Terminated)
    ));
    sip_clock::testkit::settle().await;
    assert!(ctx.state.peek(&r_ended).is_none(), "the discharged body is not live");
    assert_eq!(n.cdr.snapshot().len(), 1, "the deferred terminal's CDR is written here");
    assert_eq!(n.metrics.repl_reclaimed_total(), 0, "a discharge is no re-serve");
    drop(_guard);

    let dying = call_in("w1", "w0", "cid-dying", CallModelState::Terminating, &n.clock);
    let r_dying = dying.call_ref.clone();
    put(&n.store, PRI, "w1", &dying).await;
    let _guard = ctx.state.lock(&r_dying).await;
    assert!(matches!(
        materialise(ctx, &r_dying, Origin::Reclaim(None)).await,
        Materialised::Refused(Reason::Terminated)
    ));
    sip_clock::testkit::settle().await;
    assert!(ctx.state.peek(&r_dying).is_none(), "the forced-terminal body is not live");
    assert_eq!(n.cdr.snapshot().len(), 2, "a Terminating deferral is completed and billed");
    assert_eq!(ctx.state.active_count(), 0);
}

/// Every reclaim miss is typed: a ref this node backs up, a ref with no body.
#[tokio::test(start_paused = true)]
async fn reclaim_misses_are_typed() {
    let n = node("w1").await;
    let ctx = n.core.router_ctx();

    let theirs = call_in("w0", "w1", "cid-theirs", CallModelState::Active, &n.clock);
    put(&n.store, BAK, "w0", &theirs).await;
    assert!(matches!(
        materialise(ctx, &theirs.call_ref, Origin::Reclaim(None)).await,
        Materialised::Refused(Reason::NotPrimaryRole)
    ));

    assert!(matches!(
        materialise(ctx, "w1|nobody|t", Origin::Reclaim(None)).await,
        Materialised::Refused(Reason::NoReplica)
    ));

    assert_eq!(n.metrics.repl_reclaimed_total(), 0);
    assert_eq!(ctx.state.active_count(), 0);
}
