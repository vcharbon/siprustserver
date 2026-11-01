//! Waiting for a SUT's spawned tasks to land.

/// Poll `cond` until it holds, yielding so spawned teardown / CDR-writer tasks
/// drain. The single home for the "wait for the async teardown to land" loop.
///
/// Works under both wall-clock (`#[tokio::test]`) and `start_paused` tests:
/// under a paused runtime the `sleep` auto-advances virtual time, so the
/// awaited spawned tasks still get to run. Bounded (200 × 5 ms) so a real
/// regression — `cond` never holds — falls through to the test's assertions
/// instead of hanging.
///
/// For tests that must *advance the clock to trip a timer* (not merely drain
/// already-due tasks), advance with `Harness::advance` first, then settle here.
pub async fn settle_until(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}
