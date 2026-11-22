# Test clock — driving time in paused-runtime tests

**Read this before writing or modifying any test that involves time** — timers,
keepalives, timeouts, reaping, failover. Most flaky or mysteriously-failing
tests in this repo have traced back to violating one of the rules below; each
rule has bitten us at least once.

For *which harness* to use, see [harness-layers.md](harness-layers.md). A
TypeScript test on real processes: [§ TypeScript tests](#typescript-tests).

## The model: one clock, one lever

- All behavioural time rides **`tokio::time` directly** (monotonic). A test
  declares `#[tokio::test(start_paused = true)]` and moves time with the
  harness `advance` helpers.
- Wall-clock timestamps come from `sip_clock::Clock`, which anchors a wall
  value to a `tokio::time::Instant` once. `Clock::test_at(0)` reads the *same*
  tokio timeline, so **one `advance` moves behaviour timers and report
  timestamps together**.
- There is **no separate fake-clock counter** to keep in sync, and no clock
  pump driving one — keep it that way.

## The primitives

| Helper | What it does |
|---|---|
| `Harness::advance(d)` | `testkit::advance_settled(d)`: every timer due inside the span fires at its own instant and the work it starts runs to idle before time moves on, so a detached request is answered at its wire instant, inside its budget. Call it *between* protocol events, after the message just sent has been `expect`ed. |
| `sip_clock::testkit::{advance_settled, settle, pump}` | The shared primitives. `advance_settled` sleeps the caller and lets tokio's auto-advance walk the due timers; `settle` yields (no time movement); `pump(d)` = `ceil(d/100 ms) + 1` settled 100 ms chunks, so work due inside a chunk runs at its own instant and a harness can sample after each chunk. |
| `sip_clock::testkit::run_paused_within(wall, scenario)` | Runs a scenario on its own paused runtime, `None` when it has not finished after `wall` of real time: the bound for a test whose failure mode is a task that never goes idle. |
| `b2bua_harness::settle_until(cond)` | Bounded yield-poll to drain async teardown (CDR write, reap) — moves no simulated time. |
| `FailoverHarness::advance(d)` | = `testkit::pump_sampled(d)` — drives the SIP *and* replication planes, sampling the views ledger after each chunk. Its granularity is 200 ms at least; a test that must stop at one instant steps `advance_settled` directly. |
| `FailoverHarness::pump_until(step, max, ready)` | Fine-grained pump toward a deadline you can't compute; prefer it over guessing one big advance. |
| `FailoverHarness::{settle_terminal, settle_lossy_cleanup, linger_peers}` | Teardown settles past keepalive/TTL windows; keep peer sockets draining post-teardown (avoids 481-relay-style teardown races). |

## The rules

1. **Pause the clock for anything timed.** A purely synchronous message flow
   may run unpaused, but any test that asserts on a timer, timeout, or
   interval must be `start_paused = true` and advance explicitly. (Also the
   lane policy: a real-clock test of 1 s or more leaves the default lane — see
   CLAUDE.md.)

2. **Advance *between* protocol steps, exactly to the deadline you want to
   trip.** Advance to the deadline; let the response / cancel land; then
   advance again. Advancing past two deadlines in one step fires both before
   you can react — e.g. advancing past a keepalive *and* its probe's Timer F
   terminates the call you meant to keep up.

3. **Never feed a paused test a real wall-clock signal.** Canonical flake:
   the failover harness rode the live ELU sampler (a real busy-fraction) under
   a paused clock, so at cold start the panic-ELU rung shed the very
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

5. **Paused-clock tests stay in the default lane whatever their length, but
   are NOT free.** Their cost is CPU (timer churn + recorded-trace scans) and
   it compounds super-linearly with per-sim-second traffic. Concrete case: one
   keepalive cell (~700 sim-seconds) with a 1 s OPTIONS probe cadence burned
   ~420 s of CPU; at 10 s cadence, ~10 s. A slow paused-clock test is never
   moved to the slow lane: cut the churn at its source (probe/keepalive
   cadence, traffic volume) — slower cadences are semantics-preserving wherever
   the test pumps for a condition instead of counting ticks.

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
| A request sent inside an advance times out although its peer answers within the budget | A raw `tokio::time::advance` or a fixed-chunk advance moved time past work still in flight; use `Harness::advance` |
| `Harness::advance` / `pump` never returns | A task that never goes idle (a `yield_now` loop, a `watch` clone whose `changed()` returns at once on a value already read, a `select!` arm on `recv()` of a closed channel without a `Some(..)` pattern) holds the auto-advance; bound the reproduction with `run_paused_within` |
| Teardown assertion flakes (481s, missing 200s) | Peers closed too early — use `settle_terminal` / `linger_peers` |

## TypeScript tests

A vitest test that spawns a process, opens a database or binds a socket runs
on the real clock, and a loaded box (load 40 and more on 24 cores, a node
spawn taking seconds to a minute) stretches each of its steps. Its verdict
never depends on how long a step took, and it still fails when the code under
test is wrong.

1. **The package's bound is the only bound.** Every `ts/*/vitest.config.ts`
   sets `testTimeout` and `hookTimeout` to 300 000. No test, describe or hook
   timeout below it on process, database or socket work, and no
   `spawnSync`/`execFile` `timeout` below it on a child the test needs to
   finish. A test of pure in-process protocol work may keep a small one.
2. **Wait on the event**, or on the end of the process it depends on.
   `ts/rig/test/waits.ts` holds the shape: `until(holds)` polls with no
   deadline of its own; `exitOf(child)` and `endOf(fiber)` follow an end;
   `phaseSeen(seen, phase, ended)` waits for a phase a process writes and
   rejects as soon as the process ended without it, saying how it ended. A
   package that does not depend on `@sip/rig` copies the few lines it needs.
3. **No fixed sleep for "it happened".** A sleep followed by an assertion that
   the other side acted waits instead on the evidence of the act: a row, a
   file, a log line, a phase, a database session. A sleep before asserting
   that something did *not* happen may stay.
4. **A bound handed to the code under test is roomy** (tens of seconds, or
   the package's bound) where the test does not assert it ran out. Where the
   test does, the outcome proves it: the message, the reason, or a trace or
   pid file the step wrote before it blocked, waited on as an event. Never a
   tight window or an elapsed upper bound. Where an elapsed time is the
   discriminator, the two durations it tells apart sit far apart (a quiet of
   30 s against a grace of 40 ms, the assertion well under the long one), or
   the test moves to the fake clock. A product's own bounds stay as shipped:
   the test hands its value through the fixture, the config it writes or an
   option.
5. **A fake clock around real processes moves only while the run owes no
   event.** A `TestClock` test whose effect runs processes advances the clock
   only once nothing is due at the current instant that a clock move cannot
   bring: a process still running, an answer due now. The owed events come
   from the code's own event sink; a step that only a clock move ends writes a
   marker before it blocks, so it is not owed. `stepped` in
   `ts/campaign-runner/test/schedule.test.ts` and `onFakeClock` in
   `ts/rig/test/cell-collectors.test.ts` are the shape. A clock moved per
   unit of real time shrinks each fake bound by the box's load.
6. **A stand-in meant to run until killed runs until killed**: `sleep 3600`
   or `exec sleep 3600`, never a few seconds. One that must finish at a known
   point waits on a file or a signal the test sends.
7. **A free port is held**: bind port 0 and pass the listening socket's port
   on, or keep the socket until the consumer binds. A port another process
   binds itself is picked below the ephemeral range, so no bind on port 0 nor
   outbound connect takes it first (`ts/reference-pack/test/hosted-workspace.ts`
   `freeAddresses`). A `listen` has an `error` handler.

| Shape | Smell | Fix |
|---|---|---|
| A. own deadline on a wait | a poll capped by a count, a `Date.now()` deadline or a `timeoutOption` around a process, file, socket or database event | rule 2 |
| B. timeout below the package's | `}, 60_000)` on a hook, `{ timeout: 20_000 }` on a describe that spawns | rule 1 |
| C. elapsed upper bound | `expect(elapsed).toBeLessThan(n)` around spawns or database work | rule 4 |
| D. stand-in that ends on its own | `sleep 30` meant to outlive the test's steps | rule 6 |
| E. fixed sleep for "it happened" | `sleep(200)`, then the other side's act asserted | rule 3 |
| F. short bound handed to the code | a fixture `collect_s = 0.5` that must not run out first, or must only after a step wrote what the test reads | rule 4 |
| G. free port by bind and release | a port number handed on after its socket closed | rule 7 |
| H. two real timings, nothing ordering them | a stand-in dying after 500 ms racing another process's answer | gate one on the other (rule 3 or 6) |
