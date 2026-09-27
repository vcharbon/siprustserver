//! The worker's paced sweep: one task that runs one pass per interval (the
//! reaper sweep, then the replica reap, `b2bua_core`).

use std::future::Future;
use std::time::Duration;

use crate::metrics::B2buaMetrics;

/// Run `pass` once per `interval`, the first one `interval` after the start,
/// until the task is aborted with the worker.
pub(crate) async fn run<F, Fut>(interval: Duration, _metrics: B2buaMetrics, pass: F)
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // skip the immediate first tick
    loop {
        tick.tick().await;
        pass().await;
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

    /// A pass that panics does not end the sweep: the next pass evicts the
    /// expired deferred terminal and releases its key, and the panic is counted.
    #[tokio::test(start_paused = true)]
    async fn a_sweep_pass_that_panics_once_is_followed_by_one_that_releases() {
        // The core's own sweep stays out of the way: this test drives its own.
        let n = node_with("w1", |c| c.reaper_sweep_interval_sec = 3_600).await;
        let ctx = n.core.router_ctx().clone();
        ctx.limiter_releases.hold();
        let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let mut terminal = build_initial_call(
            &invite("w0", "w1", "sweep"),
            src(),
            &config,
            &sip_txn::IdGen::seeded(1),
            0,
        );
        terminal.state = CallModelState::Terminated;
        terminal.limiter = CallLimiterState::admitted("sweep-key".into(), vec!["x".into()]);
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
        let task = tokio::spawn(super::run(Duration::from_secs(1), n.metrics.clone(), move || {
            let (ctx, panics) = (pass_ctx.clone(), panics.clone());
            async move {
                assert!(!panics.swap(false, Ordering::SeqCst), "the first pass panics");
                crate::router::reap_expired_replicas(&ctx, ctx.clock.now_ms()).await;
            }
        }));

        tokio::time::advance(Duration::from_millis(1_100)).await;
        sip_clock::testkit::settle().await;
        assert!(ctx.limiter_releases.waiting_keys().is_empty(), "the first pass panicked");

        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(1)).await;
            sip_clock::testkit::settle().await;
        }
        assert_eq!(
            ctx.limiter_releases.waiting_keys(),
            vec!["sweep-key".to_string()],
            "a later pass released the call"
        );
        assert_eq!(n.metrics.repl_terminal_lost_total(), 1);
        assert_eq!(n.metrics.sweep_restarts_total(), 1, "the panic is counted");
        task.abort();
    }
}
