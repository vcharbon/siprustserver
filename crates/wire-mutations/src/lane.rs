//! What every no-panic lane runs under: a small stack, so a recursion the
//! input drives overflows in the lane first, and one fixed proptest seed, so
//! a run draws the same cases and a red run repeats.

use proptest::test_runner::{Config, FileFailurePersistence, RngSeed, TestRunner};

/// The stack a lane runs on: well under a runtime worker's.
pub const LANE_STACK: usize = 512 * 1024;

/// The seed every lane property draws its cases from.
pub const LANE_SEED: u64 = 0x5eed_1a5e;

/// Runs `f` on a thread with the lane's stack, its panic re-raised here.
pub fn on_lane_stack(f: impl FnOnce() + Send + 'static) {
    let worker = std::thread::Builder::new()
        .name("no-panic-lane".into())
        .stack_size(LANE_STACK)
        .spawn(f)
        .expect("spawn the lane thread");
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

/// A lane runner drawing `cases` cases from [`LANE_SEED`]; a failure is
/// persisted at `persisted`, a path relative to the crate under test, and
/// replayed first on the next run.
pub fn runner(persisted: &'static str, cases: u32) -> TestRunner {
    TestRunner::new(Config {
        cases,
        max_shrink_time: 60_000,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(persisted))),
        rng_seed: RngSeed::Fixed(LANE_SEED),
        ..Config::default()
    })
}
