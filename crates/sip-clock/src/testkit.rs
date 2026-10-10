use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

/// Advance paused tokio time by `total`, running the work in flight at every
/// instant a timer falls due inside the span: the caller sleeps, so the
/// runtime polls every ready task until none is left and only then
/// auto-advances, to the next due timer. A request a detached task sends
/// inside the span is answered at its wire instant, within its budget. The
/// trailing [`settle`] runs the work due at the span's last instant before
/// the caller resumes. Panics outside a paused runtime; a task that never
/// goes idle (a `yield_now` loop) holds the clock and the advance with it.
pub async fn advance_settled(total: Duration) {
    // tokio's own check that the clock is paused; moves no time.
    tokio::time::advance(Duration::ZERO).await;
    tokio::time::sleep(total).await;
    settle().await;
}

/// Real time [`run_paused_within`] gives a runtime to take its abort.
pub const ABORT_GRACE: Duration = Duration::from_secs(1);

/// Run `scenario` on a paused current-thread runtime of its own thread and
/// return its output, or `None` when it has not finished after `wall` of real
/// time. A task that keeps yielding but never goes idle holds the paused
/// clock, so a scenario that sleeps behind it would hang: this bounds such a
/// test in real time, and drops the runtime and every task on it before it
/// returns. A panic in the scenario is re-raised. A task stuck in a
/// synchronous loop never lets the runtime see the abort: after
/// [`ABORT_GRACE`] this panics and leaves that thread running.
pub fn run_paused_within<F, Fut, T>(wall: Duration, scenario: F) -> Option<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
{
    let (abort, aborted) = tokio::sync::oneshot::channel::<()>();
    let (done, finished) = std::sync::mpsc::channel();
    let runner = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("a paused current-thread runtime");
        let out = rt.block_on(async move {
            tokio::select! {
                biased;
                _ = aborted => None,
                out = scenario() => Some(out),
            }
        });
        drop(rt);
        let _ = done.send(out);
    });
    let out = match finished.recv_timeout(wall) {
        Ok(out) => out,
        // The runner panicked; the join below re-raises it.
        Err(RecvTimeoutError::Disconnected) => None,
        Err(RecvTimeoutError::Timeout) => {
            let _ = abort.send(());
            match finished.recv_timeout(ABORT_GRACE) {
                Ok(out) => out,
                Err(RecvTimeoutError::Disconnected) => None,
                Err(RecvTimeoutError::Timeout) => panic!(
                    "the scenario's runtime did not take the abort within {ABORT_GRACE:?}: \
                         a task is stuck in a synchronous loop (its thread is left running)"
                ),
            }
        }
    };
    if let Err(panic) = runner.join() {
        std::panic::resume_unwind(panic);
    }
    out
}

/// Generous settle count, sized for the deepest test pipeline (the failover
/// harness drives the SIP *and* replication planes together — notify →
/// server drain → send → transit-delivery actor → puller recv → store apply
/// → status publish, several task hops). One `yield_now` advances exactly
/// one hop, so we yield generously.
const SETTLE_YIELDS: usize = 96;

/// Let every spawned task in the sim pipeline hop forward without advancing
/// time. One `yield_now` only advances one task hop and the pipeline is many
/// hops deep, so this yields `SETTLE_YIELDS` times. The single home for the
/// "settle generously" idiom the repl tests and the ha / failover harnesses
/// share.
pub async fn settle() {
    for _ in 0..SETTLE_YIELDS {
        tokio::task::yield_now().await;
    }
}

/// Advance paused tokio time by `total` in settled 100 ms chunks: each chunk
/// is an [`advance_settled`], so work falling due inside a chunk runs at its
/// own instant. The body behind the replication and failover harnesses'
/// `advance()` and the repl tests' `tick()`.
///
/// `pump(d)` advances `ceil(d/100ms) + 1` chunks of virtual time: the
/// trailing chunk delivers the frames produced at the span's last instant.
pub async fn pump(total: Duration) {
    pump_sampled(total, || {}).await;
}

/// [`pump`] with `on_chunk` run after every chunk — the sampling seam for a
/// harness that observes component state as time moves (it sees the
/// pipeline at 100 ms granularity, never between two chunks). Timing is
/// identical to [`pump`]; `on_chunk` is synchronous so it cannot perturb the
/// pipeline it observes.
pub async fn pump_sampled(total: Duration, mut on_chunk: impl FnMut()) {
    const CHUNK: Duration = Duration::from_millis(100);
    let chunks = (total.as_millis() as u64).div_ceil(100).max(1);
    for _ in 0..=chunks {
        advance_settled(CHUNK).await;
        on_chunk();
    }
}
