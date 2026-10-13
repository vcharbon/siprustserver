use super::*;
use crate::metrics::B2buaMetrics;
use crate::repl::ReplicatingCallStore;
use crate::store::{CallStore, PartitionRole};
use async_trait::async_trait;
use repl_net::frame::{Frame, Op, Partition, Watermark};
use repl_net::transport::{
    ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
    SendError,
};
use sip_clock::Clock;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Mutex;
use tokio::sync::{mpsc, watch};

/// A scripted client connection that delivers each pre-arranged bootstrap
/// frame after a fixed clock-time `gap`, then parks (a quiet tail). Faithful
/// to the real server, EVERY bootstrap `Data` carries the SAME scan-start
/// head `at`, which a watermark apply-gate would drop.
struct PacedConn {
    frames: Mutex<VecDeque<Frame>>,
    gap_ms: u64,
}

#[async_trait]
impl ReplicationConnection for PacedConn {
    async fn send(&self, _frame: Frame) -> Result<(), SendError> {
        Ok(())
    }
    async fn recv(&self) -> Option<Frame> {
        tokio::time::sleep(Duration::from_millis(self.gap_ms)).await;
        let next = self.frames.lock().unwrap().pop_front();
        match next {
            Some(f) => Some(f),
            // Script exhausted: the tail is quiet — park forever.
            None => std::future::pending().await,
        }
    }
    fn peer_addr(&self) -> SocketAddr {
        "127.0.0.1:9".parse().unwrap()
    }
    fn local_addr(&self) -> SocketAddr {
        "127.0.0.1:8".parse().unwrap()
    }
}

/// A network whose every `connect` hands back a fresh paced bootstrap script:
/// `n` `Data{Pri,Put}` frames (all stamped the same `w_scan`) then the
/// first catch-up `Noop{w_scan}`.
struct PacedNet {
    n: usize,
    gap_ms: u64,
    w_scan: Watermark,
}

#[async_trait]
impl ReplicationNetwork for PacedNet {
    async fn connect(
        &self,
        _dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        let mut q = VecDeque::new();
        for i in 0..self.n {
            q.push_back(Frame::Data {
                at: self.w_scan, // SAME head for every pre-seed frame
                op: Op::Put,
                partition: Partition::Pri,
                call_ref: format!("w1|{i}|t"),
                call_gen: 1,
                call_bgen: 0,
                body_ttl_ms: 0,
                origin_now_ms: 0,
                indexes: Vec::new(),
                body: Some(Arc::from(format!("body{i}").into_bytes().into_boxed_slice())),
                answered: false,
                incarnation: None,
            });
        }
        q.push_back(Frame::Noop { at: self.w_scan });
        Ok(Box::new(PacedConn { frames: Mutex::new(q), gap_ms: self.gap_ms }))
    }
    async fn listen(
        &self,
        _local: SocketAddr,
    ) -> Result<Box<dyn ReplicationListener>, ListenError> {
        unreachable!("the paced net is pull-only")
    }
}

/// A bootstrap whose `n` frames all share `at = W` re-hydrates IN FULL:
/// `(p,b)` dominance is the only idempotency, so every frame applies. The
/// whole stream also outlasts the hard-timeout window, which does not abandon
/// the connection.
#[tokio::test(start_paused = true)]
async fn bootstrap_frames_sharing_watermark_all_rehydrate() {
    let clock = Clock::test_at(0);
    let n = 10usize;
    let store = ReplicatingCallStore::new(1, clock.clone());
    let net: Arc<dyn ReplicationNetwork> =
        Arc::new(PacedNet { n, gap_ms: 100, w_scan: Watermark::new(1, 100) });
    let config = PullerConfig {
        backoff_init_ms: 50,
        backoff_max_ms: 1_000,
        bootstrap_hard_timeout_ms: 500, // stream runs 1000ms > this, on purpose
        ..PullerConfig::default()
    };
    let (puller, status) = Puller::new_at(
        "w0",
        "w1",
        Partition::Pri,
        "127.0.0.1:9".parse().unwrap(),
        net,
        store.clone(),
        config,
        Watermark::new(0, 0),
        B2buaMetrics::new(),
    );
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    tokio::spawn(async move { puller.run(cancel_rx).await });

    // Drive the paused clock well past the 1000ms stream (+ tail settle).
    for _ in 0..40 {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
    }

    // ALL n pre-seed calls landed in pri:{w1} — none lost to an apply-gate
    // collision.
    for i in 0..n {
        let cr = format!("w1|{i}|t");
        assert!(
            store.get_call(PartitionRole::Primary, "w1", &cr).await.unwrap().is_some(),
            "call {i} must be re-hydrated into pri:{{w1}} (no client-side apply-gate to drop it)",
        );
    }
    assert!(
        status.borrow().bootstrap_complete,
        "first catch-up Noop observed ⇒ bootstrap-complete",
    );
}

// ---- backup ceilings (ADR-0037) ----

/// A Backup-flow puller on `w1` pulling `w0`, its gate at `capacity`.
fn backup_puller(
    capacity: crate::config::CapacityConfig,
) -> (
    Puller,
    ReplicatingCallStore,
    crate::capacity::CapacityGate,
    crate::capacity::SimulatedSystemControl,
) {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    let (probe, control) = crate::capacity::simulated();
    let gate = crate::capacity::CapacityGate::new(Arc::new(probe));
    gate.configure(&capacity);
    let net: Arc<dyn ReplicationNetwork> =
        Arc::new(PacedNet { n: 0, gap_ms: 100, w_scan: Watermark::new(1, 1) });
    let (puller, _status) = Puller::new_at(
        "w0",
        "w1",
        Partition::Bak,
        "127.0.0.1:9".parse().unwrap(),
        net,
        store.clone(),
        PullerConfig::fast_test(),
        Watermark::new(0, 0),
        B2buaMetrics::new(),
    );
    (puller.with_capacity(gate.clone()), store, gate, control)
}

async fn put(p: &Puller, partition: Partition, call_ref: &str, gen: i64) -> ApplyOutcome {
    let body: Arc<[u8]> = Arc::from(b"body".to_vec().into_boxed_slice());
    p.apply_to_store(
        Op::Put,
        partition,
        call_ref,
        gen,
        0,
        0,
        0,
        &[],
        Some(body),
        false,
        None,
        ApplyMode::Forward,
    )
    .await
}

async fn held(store: &ReplicatingCallStore, call_ref: &str) -> bool {
    store.get_call(PartitionRole::Backup, "w0", call_ref).await.unwrap().is_some()
}

/// At the count ceiling a replica of a new call is not stored; replicas
/// already held keep taking updates, and a freed slot lets the next `Put`
/// of the shed call in.
#[tokio::test]
async fn the_backup_count_ceiling_sheds_new_replicas_only() {
    let (p, store, gate, _) = backup_puller(crate::config::CapacityConfig {
        backup_calls: Some(2),
        ..Default::default()
    });
    assert_eq!(put(&p, Partition::Bak, "w0|a|t", 1).await, ApplyOutcome::Applied);
    assert_eq!(put(&p, Partition::Bak, "w0|b|t", 1).await, ApplyOutcome::Applied);
    assert_eq!(store.backup_held(), 2);

    assert_eq!(put(&p, Partition::Bak, "w0|c|t", 1).await, ApplyOutcome::Shed);
    assert!(!held(&store, "w0|c|t").await, "a shed replica is not stored");
    assert_eq!(gate.backup_shed_total(crate::capacity::BackupBound::Calls), 1);

    assert_eq!(
        put(&p, Partition::Bak, "w0|a|t", 2).await,
        ApplyOutcome::Applied,
        "a held replica keeps taking updates at the ceiling"
    );

    let delete = p
        .apply_to_store(
            Op::Delete,
            Partition::Bak,
            "w0|b|t",
            2,
            0,
            0,
            0,
            &[],
            None,
            false,
            None,
            ApplyMode::Forward,
        )
        .await;
    assert_eq!(delete, ApplyOutcome::Applied);
    assert_eq!(store.backup_held(), 1);
    assert_eq!(put(&p, Partition::Bak, "w0|c|t", 2).await, ApplyOutcome::Applied);
    assert!(held(&store, "w0|c|t").await, "the next Put after a free slot stores it");
}

/// At the backup RSS ceiling a replica of a new call is not stored.
#[tokio::test]
async fn the_backup_rss_ceiling_sheds_new_replicas() {
    let (p, store, gate, control) = backup_puller(crate::config::CapacityConfig {
        backup_rss_bytes: Some(1000),
        ..Default::default()
    });
    assert_eq!(put(&p, Partition::Bak, "w0|a|t", 1).await, ApplyOutcome::Applied);
    control.set_rss_bytes(Some(1000));
    gate.sample(crate::capacity::Occupancy::default());
    assert_eq!(put(&p, Partition::Bak, "w0|b|t", 1).await, ApplyOutcome::Shed);
    assert_eq!(put(&p, Partition::Bak, "w0|a|t", 2).await, ApplyOutcome::Applied);
    assert_eq!(gate.backup_shed_total(crate::capacity::BackupBound::Rss), 1);
    assert_eq!(store.backup_held(), 1);
}

/// Our own calls coming home (the `Pri` partition) are never shed: they
/// are live calls, not backups.
#[tokio::test]
async fn a_reclaimed_own_call_is_never_shed() {
    let (p, store, _, _) = backup_puller(crate::config::CapacityConfig {
        backup_calls: Some(1),
        ..Default::default()
    });
    assert_eq!(put(&p, Partition::Bak, "w0|a|t", 1).await, ApplyOutcome::Applied);
    assert_eq!(put(&p, Partition::Pri, "w1|mine|t", 1).await, ApplyOutcome::Applied);
    assert_eq!(store.backup_held(), 1);
}

/// A shed replica is not a split: nothing rides up to the router.
#[tokio::test]
async fn a_shed_replica_signals_nothing() {
    let (p, _, _, _) = backup_puller(crate::config::CapacityConfig::default());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let p = p.with_repl_sink(tx);
    p.signal(ApplyMode::Forward, ApplyOutcome::Shed, true, "w0|a|t", bodyless(), 0);
    p.signal(ApplyMode::Reverse, ApplyOutcome::Shed, true, "w0|a|t", bodyless(), 0);
    assert!(rx.try_recv().is_err());
}

/// A `Put` with no body, as a refused flush's signal carries it.
fn bodyless() -> crate::router::PrimaryWrite {
    crate::router::PrimaryWrite::Put(Arc::from(Vec::new()))
}

/// An applied forward write rides up for a call the router serves as a live
/// takeover copy, in a scan as on the tail — a `Put` with its body, a
/// `Delete` with the incarnation it names; a write of any other call, a
/// dominated re-delivery, a refused `Delete`, and a puller with no probe hand
/// nothing up.
#[tokio::test]
async fn an_applied_forward_write_rides_up_for_a_live_takeover_copy_alone() {
    use crate::router::PrimaryWrite::{Delete, Put};
    let (copy, other) = ("w0|copy|t", "w0|other|t");
    let body: Arc<[u8]> = Arc::from(b"primary".to_vec().into_boxed_slice());
    let (p, _, _, _) = backup_puller(crate::config::CapacityConfig::default());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let probe: crate::store::TakeoverProbe = Arc::new(move |r: &str| r == copy);
    let p = p.with_repl_sink(RouterSink::new(tx, probe));
    let (forward, applied) = (ApplyMode::Forward, ApplyOutcome::Applied);
    for bootstrapped in [false, true] {
        p.signal(forward, applied, bootstrapped, copy, Put(body.clone()), 0);
        match rx.try_recv() {
            Ok(ReplCommand::ForwardFlushApplied { call_ref, write: Put(sent) }) => {
                assert_eq!(call_ref, copy);
                assert_eq!(&*sent, &*body, "the primary's body rides up");
            }
            other => panic!("expected the applied write (bootstrapped: {bootstrapped}): {other:?}"),
        }
    }
    p.signal(forward, applied, true, copy, Delete(Some("w0|copy|t#1".into())), 0);
    match rx.try_recv() {
        Ok(ReplCommand::ForwardFlushApplied { call_ref, write: Delete(Some(deleted)) }) => {
            assert_eq!((call_ref.as_str(), deleted.as_str()), (copy, "w0|copy|t#1"));
        }
        other => panic!("expected the applied delete: {other:?}"),
    }
    p.signal(forward, applied, true, other, Put(body.clone()), 0);
    p.signal(forward, applied, true, other, Delete(None), 0);
    p.signal(forward, ApplyOutcome::Dominated, true, copy, Put(body.clone()), 0);
    p.signal(forward, ApplyOutcome::Refused, true, copy, Delete(None), 0);
    assert!(rx.try_recv().is_err(), "nothing else rides up");

    let (unprobed, _, _, _) = backup_puller(crate::config::CapacityConfig::default());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let unprobed = unprobed.with_repl_sink(tx);
    unprobed.signal(forward, applied, true, copy, Put(body), 0);
    assert!(rx.try_recv().is_err(), "no probe, no signal");
}

/// A connection that plays a fixed script, one frame per `gap_ms`, and
/// records every frame the puller sends back.
struct RecordingConn {
    frames: Mutex<VecDeque<Frame>>,
    sent: Arc<Mutex<Vec<Frame>>>,
}

#[async_trait]
impl ReplicationConnection for RecordingConn {
    async fn send(&self, frame: Frame) -> Result<(), SendError> {
        self.sent.lock().unwrap().push(frame);
        Ok(())
    }
    async fn recv(&self) -> Option<Frame> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let next = self.frames.lock().unwrap().pop_front();
        match next {
            Some(f) => Some(f),
            None => std::future::pending().await,
        }
    }
    fn peer_addr(&self) -> SocketAddr {
        "127.0.0.1:9".parse().unwrap()
    }
    fn local_addr(&self) -> SocketAddr {
        "127.0.0.1:8".parse().unwrap()
    }
}

struct RecordingNet {
    script: Mutex<Option<VecDeque<Frame>>>,
    sent: Arc<Mutex<Vec<Frame>>>,
}

#[async_trait]
impl ReplicationNetwork for RecordingNet {
    async fn connect(
        &self,
        _dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        let frames = self.script.lock().unwrap().take().unwrap_or_default();
        Ok(Box::new(RecordingConn { frames: Mutex::new(frames), sent: self.sent.clone() }))
    }
    async fn listen(
        &self,
        _local: SocketAddr,
    ) -> Result<Box<dyn ReplicationListener>, ListenError> {
        unreachable!("the recording net is pull-only")
    }
}

fn data(op: Op, call_ref: &str, gen: i64, counter: u64) -> Frame {
    Frame::Data {
        at: Watermark::new(1, counter),
        op,
        partition: Partition::Bak,
        call_ref: call_ref.to_string(),
        call_gen: gen,
        call_bgen: 0,
        body_ttl_ms: 0,
        origin_now_ms: 0,
        indexes: Vec::new(),
        body: (op == Op::Put).then(|| Arc::from(b"body".to_vec().into_boxed_slice())),
        answered: false,
        incarnation: None,
    }
}

/// Run a puller of `partition` from `w0` on `w1`, resuming from `start`,
/// through `script`, its gate at `capacity`; the positions it reported.
async fn reported_positions(
    start: Watermark,
    partition: Partition,
    store: &ReplicatingCallStore,
    capacity: crate::config::CapacityConfig,
    script: Vec<Frame>,
) -> Vec<Watermark> {
    let (probe, _control) = crate::capacity::simulated();
    let gate = crate::capacity::CapacityGate::new(Arc::new(probe));
    gate.configure(&capacity);
    let sent = Arc::new(Mutex::new(Vec::new()));
    let net: Arc<dyn ReplicationNetwork> = Arc::new(RecordingNet {
        script: Mutex::new(Some(VecDeque::from(script))),
        sent: sent.clone(),
    });
    let (puller, _status) = Puller::new_at(
        "w0",
        "w1",
        partition,
        "127.0.0.1:9".parse().unwrap(),
        net,
        store.clone(),
        PullerConfig::fast_test(),
        start,
        B2buaMetrics::new(),
    );
    let puller = puller.with_capacity(gate);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let task = tokio::spawn(async move { puller.run(cancel_rx).await });
    for _ in 0..40 {
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
    }
    let _ = cancel_tx.send(true);
    task.abort();
    let reported = sent
        .lock()
        .unwrap()
        .iter()
        .filter_map(|f| match f {
            Frame::Position { at } => Some(*at),
            _ => None,
        })
        .collect();
    reported
}

fn at(counter: u64) -> Watermark {
    Watermark::new(1, counter)
}

/// While a shed replica stands, the flow reports no position past the last
/// one it held everything up to, whatever else it applies; once the
/// replica is stored, the report catches up (ADR-0031 D2 x ADR-0037).
#[tokio::test(start_paused = true)]
async fn a_shed_replica_holds_the_reported_position_until_it_is_stored() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    let reported = reported_positions(
        Watermark::new(0, 0),
        Partition::Bak,
        &store,
        crate::config::CapacityConfig { backup_calls: Some(1), ..Default::default() },
        vec![
            Frame::Noop { at: at(1) },
            data(Op::Put, "w0|a|t", 1, 2),
            data(Op::Put, "w0|b|t", 1, 3),
            data(Op::Put, "w0|a|t", 2, 4),
            Frame::Noop { at: at(4) },
            data(Op::Delete, "w0|a|t", 3, 5),
            data(Op::Put, "w0|b|t", 2, 6),
            Frame::Noop { at: at(6) },
        ],
    )
    .await;
    let want: Vec<Watermark> = [1, 2, 2, 2, 2, 2, 6, 6].into_iter().map(at).collect();
    assert_eq!(reported, want, "positions reported per frame");
    assert_eq!(store.shed_count(), 0, "the stored replica cleared its mark");
}

/// A Delete of a shed replica's call releases the position it held.
#[tokio::test(start_paused = true)]
async fn a_deleted_shed_replica_releases_the_position() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    let reported = reported_positions(
        Watermark::new(0, 0),
        Partition::Bak,
        &store,
        crate::config::CapacityConfig { backup_calls: Some(1), ..Default::default() },
        vec![
            Frame::Noop { at: at(1) },
            data(Op::Put, "w0|a|t", 1, 2),
            data(Op::Put, "w0|b|t", 1, 3),
            data(Op::Delete, "w0|b|t", 2, 4),
        ],
    )
    .await;
    let want: Vec<Watermark> = [1, 2, 2, 4].into_iter().map(at).collect();
    assert_eq!(reported, want);
}

/// A Backup scan drops the marks an earlier stream left (an old
/// incarnation of the primary): the scan re-sends every live body.
#[tokio::test(start_paused = true)]
async fn a_backup_scan_drops_the_marks_of_an_earlier_stream() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    store.note_shed("w0|x|t", "w0", 1, at(3), 0);
    let reported = reported_positions(
        Watermark::new(0, 0),
        Partition::Bak,
        &store,
        crate::config::CapacityConfig::default(),
        vec![Frame::Noop { at: Watermark::new(2, 1) }],
    )
    .await;
    assert_eq!(reported, vec![Watermark::new(2, 1)]);
    assert_eq!(store.shed_count(), 0);
}

/// `frame` as the primary's next incarnation (gen 2) sends it.
fn reborn(mut frame: Frame) -> Frame {
    if let Frame::Data { at, .. } = &mut frame {
        at.gen = 2;
    }
    frame
}

/// A rebooted primary is tailed warm, with no scan: the marks its old
/// incarnation's writes left go at the first position of the new one.
#[tokio::test(start_paused = true)]
async fn a_warm_resume_past_a_reboot_drops_the_old_incarnations_marks() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    store.note_shed("w0|x|t", "w0", 1, at(3), 0);
    let reported = reported_positions(
        at(5),
        Partition::Bak,
        &store,
        crate::config::CapacityConfig::default(),
        vec![reborn(data(Op::Put, "w0|y|t", 1, 1)), Frame::Noop { at: Watermark::new(2, 1) }],
    )
    .await;
    assert_eq!(reported, vec![Watermark::new(2, 1), Watermark::new(2, 1)]);
    assert_eq!(store.shed_count(), 0);
}

/// A Reclaim flow sheds nothing, so a Backup flow's marks of the same
/// peer never cap it.
#[tokio::test(start_paused = true)]
async fn a_reclaim_flow_is_never_capped_by_backup_marks() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    store.note_shed("w0|x|t", "w0", 1, at(1), 0);
    let reported = reported_positions(
        Watermark::new(0, 0),
        Partition::Pri,
        &store,
        crate::config::CapacityConfig::default(),
        vec![Frame::Noop { at: at(9) }],
    )
    .await;
    assert_eq!(reported, vec![at(9)]);
    assert_eq!(store.shed_count(), 1, "the Backup flow's mark stands");
}

/// A re-delivered Put of a call deleted inside the tombstone window is
/// ignored by the store, never shed: it marks nothing.
#[tokio::test]
async fn a_tombstoned_put_is_not_shed() {
    let (p, _store, gate, _) = backup_puller(crate::config::CapacityConfig {
        backup_calls: Some(1),
        ..Default::default()
    });
    assert_eq!(put(&p, Partition::Bak, "w0|a|t", 1).await, ApplyOutcome::Applied);
    let delete = p
        .apply_to_store(
            Op::Delete,
            Partition::Bak,
            "w0|a|t",
            2,
            0,
            0,
            0,
            &[],
            None,
            false,
            None,
            ApplyMode::Forward,
        )
        .await;
    assert_eq!(delete, ApplyOutcome::Applied);
    assert_eq!(put(&p, Partition::Bak, "w0|c|t", 1).await, ApplyOutcome::Applied);
    assert_ne!(put(&p, Partition::Bak, "w0|a|t", 1).await, ApplyOutcome::Shed);
    assert_eq!(gate.backup_shed_total(crate::capacity::BackupBound::Calls), 0);
}

// ---- call incarnations on one callRef ----

/// A `Put` of `call_ref` at `(p, b)`, naming `incarnation`, applied in `mode`.
async fn put_of(
    p: &Puller,
    partition: Partition,
    call_ref: &str,
    (gen, bgen): (i64, i64),
    incarnation: &str,
    mode: ApplyMode,
) -> ApplyOutcome {
    let body: Arc<[u8]> = Arc::from(incarnation.as_bytes().to_vec().into_boxed_slice());
    p.apply_to_store(
        Op::Put,
        partition,
        call_ref,
        gen,
        bgen,
        60_000,
        0,
        &[],
        Some(body),
        false,
        Some(incarnation.to_string()),
        mode,
    )
    .await
}

/// The body stored for `call_ref` in `role` of `w0`.
async fn body_of(store: &ReplicatingCallStore, role: PartitionRole, call_ref: &str) -> Vec<u8> {
    store.get_call(role, "w0", call_ref).await.unwrap().expect("a body is held").to_vec()
}

/// The `(p,b)` vector orders versions of one call. A new call on the ref
/// restarts at `p = 1`, so a backup that missed the old call's delete holds
/// it at a version the new call's first `Put` does not dominate; the new
/// call replaces it all the same, in the tail and in a bootstrap.
#[tokio::test]
async fn a_new_call_on_the_ref_replaces_an_element_its_version_does_not_dominate() {
    for mode in [ApplyMode::Forward, ApplyMode::Bootstrap] {
        let (p, store, _, _) = backup_puller(crate::config::CapacityConfig::default());
        let r = "w0|a|t";
        assert_eq!(
            put_of(&p, Partition::Bak, r, (5, 0), "w0|a|t#1", mode).await,
            ApplyOutcome::Applied
        );
        assert_eq!(
            put_of(&p, Partition::Bak, r, (5, 0), "w0|a|t#1", mode).await,
            ApplyOutcome::Dominated,
            "a re-delivery of the same call is still dominated ({mode:?})"
        );
        assert_eq!(
            put_of(&p, Partition::Bak, r, (1, 0), "w0|a|t#2", mode).await,
            ApplyOutcome::Applied,
            "{mode:?}"
        );
        assert_eq!(body_of(&store, PartitionRole::Backup, r).await, b"w0|a|t#2");
        assert_eq!(store.current_cv(PartitionRole::Backup, "w0", r), Some((1, 0)));
        assert_eq!(store.incarnation(r).as_deref(), Some("w0|a|t#2"));
        assert_eq!(store.backup_held(), 1);
    }
}

/// A reverse flush of a call the Element no longer holds — a takeover
/// copy's view of the call the authority replaced on the ref — is refused
/// whatever its version, and nothing is handed to the fold: folding it
/// would end or rewrite the new call.
#[tokio::test]
async fn a_reverse_flush_of_a_replaced_call_folds_into_nothing() {
    let (p, store, _, _) = backup_puller(crate::config::CapacityConfig::default());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let p = p.with_repl_sink(tx);
    let r = "w0|a|t";
    let reverse = ApplyMode::Reverse;
    assert_eq!(
        put_of(&p, Partition::Pri, r, (1, 0), "w0|a|t#2", ApplyMode::Bootstrap).await,
        ApplyOutcome::Applied
    );
    let outcome = put_of(&p, Partition::Pri, r, (1, 7), "w0|a|t#1", reverse).await;
    assert_eq!(outcome, ApplyOutcome::Superseded);
    p.signal(reverse, outcome, true, r, bodyless(), 0);
    assert!(rx.try_recv().is_err(), "nothing rides up to the router");
    assert_eq!(body_of(&store, PartitionRole::Primary, r).await, b"w0|a|t#2");

    let outcome = put_of(&p, Partition::Pri, r, (1, 1), "w0|a|t#2", reverse).await;
    assert_eq!(outcome, ApplyOutcome::Applied, "the held call's own flush still applies");
}

/// A deleted call stays buried on the apply path: its late `Put` is not
/// stored, while a new call on the same ref is.
#[tokio::test]
async fn a_deleted_call_is_not_resurrected_and_a_new_one_on_its_ref_is_stored() {
    let (p, store, _, _) = backup_puller(crate::config::CapacityConfig::default());
    let r = "w0|a|t";
    let forward = ApplyMode::Forward;
    put_of(&p, Partition::Bak, r, (3, 0), "w0|a|t#1", forward).await;
    let delete = p
        .apply_to_store(Op::Delete, Partition::Bak, r, 0, 0, 0, 0, &[], None, false, None, forward)
        .await;
    assert_eq!(delete, ApplyOutcome::Applied);

    put_of(&p, Partition::Bak, r, (4, 0), "w0|a|t#1", forward).await;
    assert!(store.get_call(PartitionRole::Backup, "w0", r).await.unwrap().is_none());
    put_of(&p, Partition::Bak, r, (1, 0), "w0|a|t#2", forward).await;
    assert_eq!(body_of(&store, PartitionRole::Backup, r).await, b"w0|a|t#2");
}

// ---- the lifecycle order of a split Element (ADR-0031 D3, ADR-0047 D7) ----

/// A call `w0` owns and `w1` backs up, its caller answered `final_sent` and
/// in `state`.
fn body_at(final_sent: Option<u16>, state: call::CallModelState) -> Arc<[u8]> {
    use call::CallBodyCodec;
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let invite = crate::router::test_support::invite("w0", "w1", "split");
    let id_gen = sip_txn::IdGen::seeded(1);
    let src = crate::router::test_support::src();
    let mut call = crate::initial_invite::build_initial_call(&invite, src, &config, &id_gen, 0);
    call.a_leg.invite_final_sent = final_sent;
    call.state = state;
    Arc::from(call::MsgpackCodec::new().encode(&call))
}

/// A forward `Put` of `body` at `(p, b)`.
async fn forward(p: &Puller, (gen, bgen): (i64, i64), body: Arc<[u8]>) -> ApplyOutcome {
    p.apply_to_store(
        Op::Put,
        Partition::Bak,
        "w0|split|t",
        gen,
        bgen,
        60_000,
        0,
        &[],
        Some(body),
        false,
        None,
        ApplyMode::Forward,
    )
    .await
}

/// An Element an acting backup authored holding its ending of a call it
/// never saw answered: the authority's answered call replaces it, at a newer
/// version and at the Element's own `(p,b)` (the authority adopted the copy's
/// `b` and republished its call there). An unanswered ending never replaces
/// an answered Element, and an answered Element a backup ended keeps its
/// ending against the authority's earlier answer.
#[tokio::test]
async fn an_unanswered_ending_never_outranks_an_answered_call_in_a_split_element() {
    use call::CallModelState::{Active, Terminated};
    let (answered, ended) = (Some(200), Some(480));
    for at in [(7, 3), (6, 3)] {
        let (p, store, _, _) = backup_puller(crate::config::CapacityConfig::default());
        let r = "w0|split|t";
        assert_eq!(forward(&p, (6, 3), body_at(ended, Terminated)).await, ApplyOutcome::Applied);
        let outcome = forward(&p, at, body_at(answered, Active)).await;
        assert_eq!(outcome, ApplyOutcome::Applied, "the answered call at {at:?}");
        assert_eq!(store.current_cv(PartitionRole::Backup, "w0", r), Some(at));
        let again = forward(&p, at, body_at(answered, Active)).await;
        assert_eq!(again, ApplyOutcome::Dominated, "and a re-delivery of it is one");
    }

    let (p, _, _, _) = backup_puller(crate::config::CapacityConfig::default());
    assert_eq!(forward(&p, (6, 3), body_at(answered, Active)).await, ApplyOutcome::Applied);
    let outcome = forward(&p, (7, 3), body_at(ended, Terminated)).await;
    assert_eq!(outcome, ApplyOutcome::Refused, "an unanswered ending is a branch");
    let outcome = forward(&p, (6, 3), body_at(ended, Terminated)).await;
    assert_eq!(outcome, ApplyOutcome::Dominated, "at the Element's version too");

    let (p, _, _, _) = backup_puller(crate::config::CapacityConfig::default());
    assert_eq!(forward(&p, (6, 3), body_at(answered, Terminated)).await, ApplyOutcome::Applied);
    let outcome = forward(&p, (7, 3), body_at(answered, Active)).await;
    assert_eq!(outcome, ApplyOutcome::Refused, "the backup's end of the answered call stays");
}
