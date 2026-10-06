//! The worker's paced sweep: one task that runs its steps once per interval
//! (the reaper sweep, then the replica reap, `b2bua_core`). Each step runs
//! behind its own panic boundary, so a step that panics costs that pass of
//! that step only: the replica reap, the only eviction site of expired
//! replica bodies, runs whatever the reaper sweep does.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

type StepFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// One step of a sweep pass: `pass` runs it, `on_panic` counts a pass of it
/// that panicked.
pub(crate) struct SweepStep {
    name: &'static str,
    pass: Arc<dyn Fn() -> StepFuture + Send + Sync>,
    on_panic: Box<dyn Fn() + Send + Sync>,
}

impl SweepStep {
    /// A step named `name` (the log field of a panic) that runs `pass` and
    /// calls `on_panic` when a pass of it panics.
    pub(crate) fn new<F, Fut>(
        name: &'static str,
        pass: F,
        on_panic: impl Fn() + Send + Sync + 'static,
    ) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self { name, pass: Arc::new(move || Box::pin(pass())), on_panic: Box::new(on_panic) }
    }
}

/// Aborts the task it holds when dropped, so aborting the sweep aborts the
/// step it is running.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run every step in order once per `interval`, the first pass `interval`
/// after the start, until the task is aborted with the worker. A step whose
/// pass panics is logged and counted by its `on_panic`, and the pass goes on
/// with the next step; the panicking step runs again one interval later,
/// never sooner.
pub(crate) async fn run(interval: Duration, steps: Vec<SweepStep>) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // skip the immediate first tick
    loop {
        tick.tick().await;
        for step in &steps {
            let pass = step.pass.clone();
            let mut running = AbortOnDrop(tokio::spawn(async move { pass().await }));
            if let Err(e) = (&mut running.0).await {
                if e.is_panic() {
                    (step.on_panic)();
                    tracing::error!(
                        step = step.name,
                        "sweep step panicked; it runs again next pass"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use call::{CallBodyCodec, CallLimiterState, CallModelState, MsgpackCodec};

    use crate::config::B2buaConfig;
    use crate::initial_invite::build_initial_call;
    use crate::router::test_support::{invite, node_with, src};
    use crate::store::{CallStore, PartitionRole, PutOpts};

    use super::SweepStep;

    /// The replica reap step of `ctx`'s node, its panics counted on `metrics`.
    fn replica_step(
        ctx: Arc<crate::router::RouterCtx>,
        metrics: crate::metrics::B2buaMetrics,
    ) -> SweepStep {
        SweepStep::new(
            "replica_reap",
            move || {
                let ctx = ctx.clone();
                async move { crate::router::reap_expired_replicas(&ctx, ctx.clock.now_ms()).await }
            },
            move || metrics.bump_replica_reap_panic(),
        )
    }

    /// A step that panics does not end the sweep: its next pass evicts the
    /// expired deferred terminal and releases its key, and the panic is counted.
    #[tokio::test(start_paused = true)]
    async fn a_sweep_pass_that_panics_once_is_followed_by_one_that_releases() {
        // The core's own sweep stays out of the way: this test drives its own.
        let n = node_with("w1", |c| c.reaper_sweep_interval_sec = 3_600).await;
        let ctx = n.core.router_ctx().clone();
        ctx.limiter.hold_releases();
        let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let mut terminal = build_initial_call(
            &invite("w0", "w1", "sweep"),
            src(),
            &config,
            &sip_txn::IdGen::seeded(1),
            0,
        );
        terminal.state = CallModelState::Terminated;
        terminal.limiter = CallLimiterState::admitted(
            "sweep-key".into(),
            1,
            vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
        );
        let call_ref = terminal.call_ref.clone();
        n.store
            .put_call(
                PartitionRole::Backup,
                "w0",
                &call_ref,
                MsgpackCodec::new().encode(&terminal),
                &[],
                500,
                1,
                1,
                &PutOpts::default(),
            )
            .await
            .unwrap();

        let panics = Arc::new(AtomicBool::new(true));
        let pass_ctx = ctx.clone();
        let metrics = n.metrics.clone();
        let step = SweepStep::new(
            "replica_reap",
            move || {
                let (ctx, panics) = (pass_ctx.clone(), panics.clone());
                async move {
                    assert!(!panics.swap(false, Ordering::SeqCst), "the first pass panics");
                    crate::router::reap_expired_replicas(&ctx, ctx.clock.now_ms()).await;
                }
            },
            move || metrics.bump_replica_reap_panic(),
        );
        let task = tokio::spawn(super::run(Duration::from_secs(1), vec![step]));
        sip_clock::testkit::settle().await; // the sweep's interval starts now

        tokio::time::advance(Duration::from_millis(1_100)).await;
        sip_clock::testkit::settle().await;
        assert!(ctx.limiter.waiting_keys().is_empty(), "the first pass panicked");

        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(1)).await;
            sip_clock::testkit::settle().await;
        }
        assert_eq!(
            ctx.limiter.waiting_keys(),
            vec!["sweep-key".to_string()],
            "a later pass released the call"
        );
        assert_eq!(n.metrics.repl_terminal_lost_total(), 1);
        assert_eq!(n.metrics.replica_reap_panics_total(), 1, "the panic is counted");
        task.abort();
    }

    /// A step that panics every time panics once per interval: no hot spin.
    #[tokio::test(start_paused = true)]
    async fn a_step_that_keeps_panicking_runs_once_per_interval() {
        let metrics = crate::metrics::B2buaMetrics::new();
        let counted = metrics.clone();
        let step = SweepStep::new(
            "replica_reap",
            || async { panic!("every pass panics") },
            move || counted.bump_replica_reap_panic(),
        );
        let task = tokio::spawn(super::run(Duration::from_secs(1), vec![step]));
        for _ in 0..100 {
            tokio::time::advance(Duration::from_millis(100)).await;
            sip_clock::testkit::settle().await;
        }
        let panics = metrics.replica_reap_panics_total();
        assert!((9..=10).contains(&panics), "{panics} panics in 10 s at a 1 s interval");
        task.abort();
    }

    /// A reaper step that panics on every pass never stops the replica reap
    /// that runs after it: each pass still evicts and releases what expired.
    #[tokio::test(start_paused = true)]
    async fn a_reaper_step_that_keeps_panicking_never_stops_the_replica_reap() {
        let n = node_with("w1", |c| c.reaper_sweep_interval_sec = 3_600).await;
        let ctx = n.core.router_ctx().clone();
        ctx.limiter.hold_releases();
        seed_expiring_terminal(&n, "early", "early-key", 500).await;
        seed_expiring_terminal(&n, "late", "late-key", 1_500).await;

        let counted = n.metrics.clone();
        let reaper = SweepStep::new(
            "reaper",
            || async { reaper_step_that_panics() },
            move || counted.bump_reaper_sweep_panic(),
        );
        let replica = replica_step(ctx.clone(), n.metrics.clone());
        let task = tokio::spawn(super::run(Duration::from_secs(1), vec![reaper, replica]));
        sip_clock::testkit::settle().await; // the sweep's interval starts now

        tokio::time::advance(Duration::from_millis(1_100)).await;
        sip_clock::testkit::settle().await;
        assert_eq!(ctx.limiter.waiting_keys(), vec!["early-key".to_string()]);
        tokio::time::advance(Duration::from_secs(1)).await;
        sip_clock::testkit::settle().await;
        assert_eq!(
            ctx.limiter.waiting_keys(),
            vec!["early-key".to_string(), "late-key".to_string()],
            "the second pass released the later terminal"
        );
        assert_eq!(n.metrics.repl_terminal_lost_total(), 2);
        assert_eq!(n.metrics.reaper_sweep_panics_total(), 2, "each reaper pass panicked");
        assert_eq!(n.metrics.replica_reap_panics_total(), 0);
        task.abort();
    }

    fn reaper_step_that_panics() {
        panic!("the reaper sweep panics");
    }

    /// Seed a deferred terminal of `w0`'s call `cid` in `w1`'s backup
    /// partition, owing the release of `key`, expiring `ttl_ms` from now.
    async fn seed_expiring_terminal(
        n: &crate::router::test_support::Node,
        cid: &str,
        key: &str,
        ttl_ms: i64,
    ) {
        let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let mut terminal = build_initial_call(
            &invite("w0", "w1", cid),
            src(),
            &config,
            &sip_txn::IdGen::seeded(1),
            0,
        );
        terminal.state = CallModelState::Terminated;
        terminal.limiter = CallLimiterState::admitted(
            key.into(),
            1,
            vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
        );
        n.store
            .put_call(
                PartitionRole::Backup,
                "w0",
                &terminal.call_ref,
                MsgpackCodec::new().encode(&terminal),
                &[],
                ttl_ms,
                1,
                1,
                &PutOpts::default(),
            )
            .await
            .unwrap();
    }
}
