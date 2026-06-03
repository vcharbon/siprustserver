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

- **Timer drivers over `DelayQueue`: never use a *stale* `Key`; epoch is the
  correctness backstop; physical removal is mandatory so per-call state is bounded.**
  A `DelayQueue` `Key` is a bare slab index with no generation: a freed slot is
  reused by the next insert and yields the *same* `Key`, so a *stale* `id → Key`
  map (one kept past the moment its entry left the queue) aliases and `try_remove`
  evicts the wrong live timer (silent, catastrophic — it killed the rescheduled
  keepalive in cycle 2). The B2BUA `timers.rs` driver carries **both** an `epoch`
  and the `Key` per `(call_ref, id)`:
  - **Epoch = correctness.** A fired entry is delivered only if its epoch still
    matches the live map; a superseded/cancelled entry drops as a tombstone.
    Correctness never depends on a removal having happened.
  - **Physical `try_remove` on Cancel/CancelAll/reschedule = bounded queue.**
    This is the **"all per-call state MUST be released at call end"** guarantee
    applied to timers. Logical-only cancellation is *correct* but leaves the slot
    until its original deadline; for a long-interval per-call timer (the 1 h
    `GlobalDuration`, default `max_duration` 3600 s in `rules/defaults.rs`)
    cancelled by a seconds-long call's BYE, that stranded entry lingers ~1 h.
    Under steady load the queue grew to ≈ `arrival_rate × 3600` (~850k entries at
    ~100 cps observed) and the oversized timing wheel drove a monotonic CPU climb
    that *looked like a call leak but wasn't* (`active_calls` was flat). So
    `CancelAll` on the `→ terminated` transition must free **every** queue slot
    the call owns, now — not at its deadline.
  - **Why `try_remove` is safe here despite the aliasing rule:** the single-task
    driver keeps `active` in lockstep with queue membership — an entry is removed
    from `active` in the same turn it fires, and every cancel/reschedule removes
    it from `active` *and* the queue together — so a stored `Key` never points at
    a reused slot. The hazard needs a *stale* key; this design never holds one.

  If you hand-roll a driver, copy this shape (epoch + lockstep `Key`), don't keep
  a loose `id → Key` map. If a timer "just doesn't fire," suspect aliasing or a
  cancel that hit the wrong entry — not the clock. If CPU/queue size climbs while
  `active_calls` is flat, suspect a cancel path that forgot to `try_remove` (watch
  the `b2bua_timer_queue_len` − `b2bua_timer_live` gap). Regressions:
  `timers::tests::reschedule_survives_aliasing_cancel` (no mis-fire),
  `cancel_physically_reclaims_the_queue_slot` + `reschedule_does_not_accumulate_tombstones`
  (bounded queue).

- **Drive the protocol *between* advances.** Advance exactly to the deadline you
  want to trip; let the response / cancel land; then advance again. Advancing
  past two deadlines in one step fires both before you can react (e.g. advancing
  past a keepalive *and* its timeout terminates the call you meant to keep up).

- **No post-mortem trace on failure (yet).** A failing scenario `panic!`s before
  `Harness::finish()`, so the recorded SIP trace is lost; debug with temporary
  `eprintln!` in `sip-net::simulated::deliver` and the timer driver until a
  panic-time dump exists.