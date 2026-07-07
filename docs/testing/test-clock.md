# Test clock — driving time in paused-runtime tests

**Read this before writing or modifying any test that involves time** — timers,
keepalives, timeouts, reaping, failover. Most flaky or mysteriously-failing
tests in this repo have traced back to violating one of the rules below; each
rule has bitten us at least once.

For *which harness* to use, see [harness-layers.md](harness-layers.md).

## The model: one clock, one lever

- All behavioural time rides **`tokio::time` directly** (monotonic). A test
  declares `#[tokio::test(start_paused = true)]` and moves time with the
  harness `advance` helpers.
- Wall-clock timestamps come from `sip_clock::Clock`, which anchors a wall
  value to a `tokio::time::Instant` once. `Clock::test_at(0)` reads the *same*
  tokio timeline, so **one `advance` moves behaviour timers and report
  timestamps together**.
- There is **no separate fake-clock counter** to keep in sync. This is
  deliberately simpler than the TS `TestClock` pump — keep it that way.

## The primitives

| Helper | What it does |
|---|---|
| `Harness::advance(d)` | Advances in 100 ms chunks (`testkit::advance_in_100ms_chunks`). Call it *between* protocol events, after the message just sent has been `expect`ed. |
| `sip_clock::testkit::{advance_in_chunks, settle, pump}` | The shared primitives. `settle` yields (no time movement); `pump` = settle → chunked advance → settle. |
| `b2bua_harness::settle_until(cond)` | Bounded yield-poll to drain async teardown (CDR write, reap) — moves no simulated time. |
| `FailoverHarness::advance(d)` | = `testkit::pump(d)` — drives the SIP *and* replication planes. |
| `FailoverHarness::pump_until(step, max, ready)` | Fine-grained pump toward a deadline you can't compute; prefer it over guessing one big advance. |
| `FailoverHarness::{settle_terminal, settle_lossy_cleanup, linger_peers}` | Teardown settles past keepalive/TTL windows; keep peer sockets draining post-teardown (avoids 481-relay-style teardown races). |

## The rules

1. **Pause the clock for anything timed.** A purely synchronous message flow
   may run unpaused, but any test that asserts on a timer, timeout, or
   interval must be `start_paused = true` and advance explicitly. (Also the
   lane policy: a default-lane test must not need >60 s of real clock — see
   CLAUDE.md.)

2. **Advance *between* protocol steps, exactly to the deadline you want to
   trip.** Advance to the deadline; let the response / cancel land; then
   advance again. Advancing past two deadlines in one step fires both before
   you can react — e.g. advancing past a keepalive *and* its timeout
   terminates the call you meant to keep up.

3. **Never feed a paused test a real wall-clock signal.** Canonical flake:
   the failover harness rode the live ELU sampler (a real busy-fraction) under
   a paused clock, so at cold start the Tier-3 overload gate shed the very
   first INVITE with a 503. Every environmental signal (ELU, health, load)
   must be injected in its simulated form (e.g. `spawn_with_overload` with a
   simulated ELU source).

4. **Transit delay must be ≥ 1 ms** — `SimulatedSignalingNetwork::new` coerces
   0 to 1. Zero transit under a paused runtime is non-deterministic: delivery
   is a spawned `sleep(0)` racing the txn → router → dispatcher → net
   pipeline, so a response can be processed a turn late and a timer cancel can
   land *after* the timer fired. Never reintroduce 0. (Defaults:
   scenario-harness 100 ms — traces show `received = sent + 100`;
   failover-harness 1 ms.)

5. **Paused-clock tests are exempt from the 60 s wall-clock rule but are NOT
   free.** Their cost is CPU (timer churn + recorded-trace scans) and it
   compounds super-linearly with per-sim-second traffic. Concrete case: one
   keepalive cell (~700 sim-seconds) with a 1 s OPTIONS probe cadence burned
   ~420 s of CPU; at 10 s cadence, ~10 s. Before `#[ignore]`-ing a slow
   paused-clock test, cut the churn at its source (probe/keepalive cadence,
   traffic volume) — slower cadences are semantics-preserving wherever the
   test pumps for a condition instead of counting ticks.

6. **Don't hand-roll timer drivers.** Use `b2bua::timers::TimerService`. If
   you genuinely must roll one, copy its shape — **epoch as the correctness
   backstop + physical `try_remove` with the `Key` kept in lockstep with queue
   membership** — and read the `crates/b2bua/src/timers.rs` module doc first
   (a `DelayQueue` `Key` is a bare slab index; a stale `id → Key` map aliases
   and cancels the wrong live timer).

7. **Cross-node clock skew is injectable, deterministically.**
   `FailoverHarness::with_worker_clock_offset(ordinal, offset_ms)` gives one
   worker a different *wall anchor* on the single monotonic timeline — exact
   inter-node skew under a paused runtime (e.g.
   `failover.rs::skew_ahead_backup_no_immediate_options_at_takeover`). What
   the harness still cannot reproduce is a host clock *stepping mid-run*; that
   residual is an infra concern — see
   [ha-acceptance.md](ha-acceptance.md).

## Debugging a failing timed test

- **Read the panic dump first.** Any panic before `finish()` triggers
  `PanicDump`: a compact one-line-per-message wire trace on stderr, including
  `[UNDELIVERED]` markers. No per-test instrumentation needed.
- A harness dropped without `finish()` still runs the RFC hard gate
  (`CseqGate` Drop backstop), so a "passing" test that skipped `finish()` can
  fail at drop — that is the gate working, not a framework bug.
- For deeper pipeline-ordering questions, temporary `eprintln!` in
  `sip-net::simulated::deliver` and the timer driver remain the fallback.

## Failure smells → likely cause

| Symptom | Suspect |
|---|---|
| A timer "just doesn't fire" | `Key` aliasing or a cancel that hit the wrong entry — not the clock (rule 6) |
| CPU/queue size climbs while `active_calls` is flat | A cancel path missing physical `try_remove` — watch the `b2bua_timer_queue_len` − `b2bua_timer_live` gap |
| Response processed one turn late; cancel loses to the timer | Zero transit delay reintroduced somewhere (rule 4) |
| Cold-start 503 / shed in a paused test | A real wall-clock signal leaked in (rule 3) |
| Call died during an advance that should have been quiet | One advance leapt two deadlines (rule 2) |
| Teardown assertion flakes (481s, missing 200s) | Peers closed too early — use `settle_terminal` / `linger_peers` |
