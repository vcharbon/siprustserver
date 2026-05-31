# SIP Rust server

Project is ultra early, not in production, do not worry about upgrade compatibility when designing solutions
Ongoing port of https://github.com/vcharbon/sipjsserver to rust to improve perfs.

Read the [strategy](./docs/MIGRATION_STRATEGY.md), it is currently beta and will be enriched with consolidated decision 

## Overall Action when migrating a module

For each Layer to be migrated, [update migration](./MIGRATION_STATUS.md) file with the exact release used as a source
Port the Layer interface an implementation, the test implementation, including the property test and Layer comparison
Port an pass all test of the given layer. Provide a full list of un-ported test with precise justification for the case where it is not.

## Hints

When porting scenario, you can get reference traces of the sipjs behavior under ../sipjsserver/test-results/fake-clock/

## Test-time clock & timers (read before touching timer or paused-clock code)

Behaviour rides `tokio::time` directly (monotonic) — there is **no** separate
fake-clock counter to keep in sync. Tests use `#[tokio::test(start_paused = true)]`
+ `Harness::advance` (100 ms chunks). `Clock::test_at(0)` reads the same tokio
time, so one `advance` moves behaviour timers *and* report timestamps together.
This is deliberately simpler than the TS `TestClock` pump — keep it that way.

Hazards (each has bitten us at least once; some twice across both codebases):

- **Transit delay must be ≥ 1 ms** — enforced in `SimulatedSignalingNetwork::new`
  (`with_transit_delay(_, 0)` is coerced to 1). Zero transit under a paused
  runtime is non-deterministic: delivery is a spawned `sleep(0)` that races the
  txn → router → dispatcher → net pipeline, so a response is processed a turn
  late and a timer cancel can land *after* the timer fired. Never reintroduce 0.

- **Timer drivers over `DelayQueue` must cancel logically, never by `Key`.**
  A `DelayQueue` `Key` is a bare slab index with no generation: a freed slot is
  reused by the next insert and yields the *same* `Key`, so any stale `id → Key`
  map aliases and `try_remove` evicts the wrong live timer (silent, catastrophic
  — it killed the rescheduled keepalive in cycle 2). The B2BUA `timers.rs` driver
  uses epoch/tombstone cancellation (live epoch per id; stale/absent epochs are
  dropped at expiry); copy that pattern, don't hand-roll `Key` bookkeeping. If a
  timer "just doesn't fire," suspect aliasing or a cancel that hit the wrong
  entry — not the clock. Regression: `timers::tests::reschedule_survives_aliasing_cancel`.

- **Drive the protocol *between* advances.** Advance exactly to the deadline you
  want to trip; let the response / cancel land; then advance again. Advancing
  past two deadlines in one step fires both before you can react (e.g. advancing
  past a keepalive *and* its timeout terminates the call you meant to keep up).

- **No post-mortem trace on failure (yet).** A failing scenario `panic!`s before
  `Harness::finish()`, so the recorded SIP trace is lost; debug with temporary
  `eprintln!` in `sip-net::simulated::deliver` and the timer driver until a
  panic-time dump exists.