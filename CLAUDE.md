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

## Writing a new b2bua / failover test

Do NOT hand-roll the INVITE/180/200/ACK dance — it lives once in
`scenario_harness::callflow`. Single-SUT b2bua test: use `B2buaScene::new(name)`
(alice :5060 / bob :5070 / b2bua :5080, routes to bob) then `scene.establish()`
→ interesting part → `scene.hangup(&mut dialog)` → `scene.finish()`; for a
non-default decision use `B2buaScene::with_b2bua(name, |bob_port| …builder…)`.
HA failover test: `scenario_harness::callflow::establish(&alice,&bob,proxy.addr())`
(or `Call::new(..).no_ring()` for the 200-only variant) and `hangup` for teardown.
ONLY for the uninterrupted happy-path setup — any dance that asserts on the 18x,
reads the relayed cookie/SDP, or injects a crash/partition mid-handshake stays
hand-rolled (those are the subject of the test). When unsure, hand-roll it.

## Test-runtime policy (default vs slow lane)

**An integration test that takes >60 s of wall-clock on the REAL clock must not
run by default.** Mark it `#[ignore = "real-clock >60s — slow lane (just
test-slow)"]` and keep a fake-clock (`start_paused`) equivalent of the scenario
in the default lane — writing one if missing is the point of the rule. Lanes
live in the `justfile`: `just test` (default), `just test-slow`
(`cargo test --release -- --ignored`).

Paused-clock tests are exempt from the 60 s rule but are NOT free: their cost
is CPU (timer churn + recorded-trace scans), and it compounds super-linearly
with per-sim-second traffic. Concrete case: the failover harness's REAL OPTIONS
health probe at the production 1 s cadence made ONE keepalive cell (~700
sim-seconds) burn ~420 s of CPU; at the harness's 10 s cadence it is ~10 s.
Before `#[ignore]`-ing a slow paused-clock test, cut the churn at its source
(probe/keepalive cadence, traffic volume per sim-second) — slower cadences are
semantics-preserving wherever the test pumps for a condition instead of
counting ticks.

## Agent & build concurrency (WSL2 resource limits)

**Never run more than ONE agent (subagent / workflow stage) at a time that
compiles or runs tests.** One `cargo build`/`cargo test` already parallelizes
across all cores; two concurrent ones — or a build/test racing a running load
generator or SUT process — took the whole WSL VM down (20 GB VM,
`vm.overcommit_memory=1` → OOM freeze, 2026-07-03). Sequence compiling/testing
agents strictly (`await` each before the next; parallelism is fine only for
non-compiling work: reads, docs, analysis). Inside an agent the same rule holds:
one cargo command at a time, and never a build/test concurrent with a load run.
Cap heavy commands on this box:
`systemd-run --user --scope -q -p MemoryMax=12G -p CPUQuota=1200% nice -n 10
cargo build … --jobs 6`; long-lived test processes (SUT, loadgen, e2e-web) get
their own small scopes (e.g. `-p MemoryMax=2G -p CPUQuota=400%`).

## Chaos collateral acceptance (endurance triage)

A failover/kill failure is NOT automatically a SUT bug. The accepted boundary
(ADR-0014 "Accepted trade-offs" / "Consequences for tests"): a call whose **dialog
state changed within the acceptance window (default 200 ms,
`LOADGEN_CHAOS_PHASE_TOL_MS`) of a kill** may take a small impact — that is the
forked-b-leg confirm-race (a call confirming AT the kill). What we **protect** is
**ringing** and **established** calls; a confirmed call dropping, or a failure whose
state change was outside the window, is a genuine bug. The loadgen is chaos-aware
and auto-buckets accepted collateral as `chaos="near"` vs genuine `chaos="clear"`
(`POST /chaos` + per-call phase markers); triage `chaos="clear"`. See ADR-0014 and
`crates/loadgen/README.md`.

**Host clock-skew artifact (NOT a SUT bug), WSL2 endurance only.** A failed-over
call whose backup fires an in-dialog keepalive OPTIONS onto a leg **~one keepalive
interval early** (e.g. a keepalive at T+10 s on a 300 s cadence, racing the
failed-over re-INVITE that triggered the takeover) is a **host clock-skew**
artifact, not a failover bug. Each pod anchors its wall clock once at start
(`sip_clock::Clock::system`) and all kind nodes share the WSL2 host clock, so when
that clock STEPS (WSL2 post-sleep drift "corrected" by systemd-timesyncd's SNTP
step) pods anchored either side of the step diverge permanently;
`TimerService::restore` rebuilds a taken-over call's timers as `(fire_at_from_dead_
node − our now_ms)`, so the offset makes a future keepalive PAST-DUE → it fires the
instant the backup takes over. The b2bua's restore is meant to absorb only
ms–seconds of skew — a ~interval-sized past-due fire means the HOST clock jumped.
Fix is infra, never the SUT: keep the host awake for the whole run + use slewing
chrony, not stepping timesyncd (`deploy/k8s/lib/host-checks.sh::check_clock`
warns; `run.sh up` resyncs once before pods anchor). Root case: endurance-20260630
`reinvite/unexpected/clear/0.html`. This is the production twin of the
paused-clock harness's known single-clock fidelity gap (`b2bua/src/timers.rs`
"Wall-time reliance": the harness rides ONE clock, zero cross-node skew, so it
cannot reproduce this — see `failover.rs::reboot_reclaim_exactly_one_owner…`,
which does kill→re-INVITE-on-backup with STRICT receives and passes).

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

- **HA reconciliation is `(p,b)`-causal — no time-based settle/handback anywhere**
  (ADR-0014). A partition can route a dialog to the backup at any time, for any
  duration, so correctness must NOT depend on a timer/settle window. The merge is
  the per-context `(primary, backup)` version vector; the acting-backup
  **self-releases** a takeover copy on the served transaction's terminal state
  (a `CallQuiesced` push from the txn layer), never on a clock. Do not reintroduce
  a `Deactivate`/watermark handback or a "wait N seconds then drop" rule.

- **Keepalive catch-up smoothing lives in the reclaim handler, never in the timer
  driver** (ADR-0014 §4). On reboot, `router::reclaim_all` pre-computes staggered
  absolute `fire_at` (in `smooth_keepalives`) for **both** keepalive cohorts a
  rehydrated node carries, so it does not flood: *past-due* ones oldest-first,
  bounded to `keepalive_catchup_speedup`× cadence; *future-dated* ones (a clean
  reboot restores ~the whole partition with deadlines clustered in one interval —
  left alone they fire as one burst a cadence later: the 2026-06-12 endurance
  throughput collapse, ~550 OPTIONS/s vs ~20/s at ~4000 dialogs/worker, which
  starved the single-task front proxy for ~2 min) de-correlated into
  `[now, fire_at]` by a deterministic per-`callRef` hash — **earlier only**, since
  delaying a probe risks the UAC keepalive timeout. This is **performance only** —
  no correctness role, no timing assumption. Keep the epoch/`Key` driver in
  `timers.rs` untouched; never move smoothing into it.
## Clock-skew hardening (replicated timer re-anchoring; ADR-0014 amendment)

Replicated `TimerEntry.fire_at` is an ABSOLUTE epoch-ms deadline minted on the
ORIGIN node's `Clock`. Pods anchor wall time once at start (`Clock::system`,
monotonic-derived), so two pods on opposite sides of a host NTP **step** disagree
permanently; on failover the takeover node rebuilt the timer as `(fire_at −
now_ms).max(0)` — unbounded trust in the dead node's clock. Under skew that reaped
a healthy RINGING/established call at takeover, extended a policy cap, or fired an
immediate keepalive OPTIONS racing the failed-over transaction (endurance-20260630
`reinvite/unexpected "got OPTIONS expected 200"`).

**SUT now bounds restore skew to ~replication latency** (accuracy only —
`(p,b)` reconciliation stays the sole correctness mechanism; the `timers.rs` driver
is untouched). The `Data` frame carries `origin_now_ms` (sender `Clock::now_ms()` at
flush); the receiver persists `skew_offset_ms = receiver_now − origin` next to
`expiry_at_ms`; the ONE router seam `sanitize_restored_timers` re-anchors every
restored `fire_at` by it on EVERY hydration path (bulk/reactive/on-demand/
reverse-flush), with a sub-second deadband ignoring pure latency, then drops the
stale `KeepaliveTimeout`, applies a defensive floor (unknown-offset paths only), and
cohort-smooths. **Never move re-anchor/smoothing into the driver.** Observability:
`clock_wall_divergence_ms` gauge + rate-limited warn (>500 ms); `Clock::system`
panics on a pre-epoch clock. The infra advice (WSL2 host clock-skew note: slewing
chrony, host kept awake) STILL STANDS — the SUT bounds the residual, it does not
license drift. The failover harness's `with_worker_clock_offset` injects
deterministic inter-node skew on the one monotonic timeline (closing the
single-clock fidelity gap): `failover.rs::skew_ahead_backup_no_immediate_options_
at_takeover` is the endurance-20260630 twin (fails pre-fix, passes after).

## Initial-INVITE final-response guarantee (ADR-0022 — read before touching the decision seam or `invariants::enforce`)

Once sip-txn auto-sends **100 Trying** for an initial INVITE (born with the
server txn, BEFORE the router/decision run), the caller MUST get a final: accept
+ forward, or a **503** (no dialog) within the decision deadline. Two mechanisms,
do not remove either:

- **`DeadlineDecisionEngine`** (`decision/mod.rs`) wraps the injected engine at
  `B2buaCore::spawn_with_overload` (after the `tune` seam, so nothing bypasses it).
  Bounds `new_call` + `call_failure` — the caller-blocking calls — with
  `call_control_timeout_ms` (default 5000, `B2BUA_CALL_CONTROL_TIMEOUT_MS`, `<=0`
  disables). **NOT `call_refer`** (its 202 is already out; bounded by
  `refer_subscription_expiry_sec`/`refer_overall_safety_sec` — a documented
  divergence from TS `callControlReferTimeoutMs`, and the reason the
  `refer_gating`/`refer_reject` hang tests still model a 60s subscription expiry).
  The `<=0` escape hatch is what `reaper.rs::wedged_setup_is_aborted_and_reaped`
  uses to exercise the abort-escalation ladder against a genuinely wedged await.
- **`invariants::enforce` unanswered-a-leg synthesis.** On `→ terminated`, if the
  a-leg entered the turn `Trying|Early` and nothing answered it, the ONE funnel
  appends a 503 (`reason="unanswered_at_termination"`). Closes the reaper /
  panic / txn-sweep silent paths in one place (subsumes the message-cap fix).

**Critical HA split — the `answer_unanswered_a_leg` flag is a correctness knob,
not cosmetics.** `true` on live-serving funnels (rules, initial-INVITE,
limiter-refresh, reaper `discharge_as_own`): the node holds the live server txn,
the caller waits on US → answer. `false` on the two HA discharge helpers for
already-terminal reclaimed/folded bodies (`discharge_materialized_terminal`,
`discharge_folded_terminal`): a reclaiming/takeover node has NO live server txn
for that a-leg (the serving node answered, or its txn died with it ≥
`reboot_budget` ago), and ADR-0014 reclaim-discharge is `(p,b)`-causal and NEVER
touches the SIP wire. Passing `true` there would put a spurious final on a wire
the node doesn't own → the double-serve class ADR-0014 structurally removed. Same
boundary as ADR-0020 X3: the node with the live txn answers; the node holding
only terminal state settles silently.

**Full-queue completeness (ADR-0022 X6).** The per-call global cap
(`per_call_queue_cap`) is the one full-queue path that fires BEFORE a call exists
— `dispatch` would silently drop a new call_ref's body. `router::on_event` sheds a
new initial INVITE at cap with a stateless 503 (`would_drop_new_at_cap` +
`build_stateless_overload_503`) before dispatch; in-dialog at-cap events keep the
silent drop (orphan → 481/resend). So NO INVITE that heard a 100 is ever silent,
at any scale. Tier-1 ingress brake (≥70% UDP queue) + Tier-3 CPS gate shed far
earlier with proper 503s; the sip-txn events channel never drops the INVITE
Message (`emit_critical`→deferred).

**LB proxy (ADR-0022 X4) is transaction-less BY DESIGN and needs no timer.** It
never emits a 100 and absorbs the worker's 100 (`core/response.rs`), so a caller
behind the LB keeps its own Timer B armed and the CALLER owns downstream-blackhole
give-up (local 408 at 64·T1). Do NOT add a Timer C / proxy-synthesized final —
`stateless_final_response_contract.rs` pins both the never-100 and blackhole-silence
halves. All proxy-INTERNAL errors already answer immediately (483/420/400/403,
503+Retry-After for shed).
