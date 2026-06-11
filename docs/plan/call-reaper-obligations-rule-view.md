# Implementation plan — RuleCall view → obligations → call reaper

Executes [ADR-0020](../adr/0020-call-reaper-obligations-rule-view.md). Three
slices, one commit each (the ADR-0016 slice discipline). Order is load-bearing:
the view first so the reaper's per-call context is born hidden from rules; the
obligation extraction second so the reaper's discharge path reuses it instead
of growing a twin.

Battery gate for every slice: `cargo test --workspace` green, 0 warnings, and
the b2bua-harness scenario suite (incl. the RFC audit gate) unchanged except
where a slice explicitly says otherwise.

---

## Slice 1 — `RuleCall<'a>` view (full candidate-2 narrowing, ADR-0020 X8)

**Crates touched:** `b2bua-sdk`, `b2bua`, `announcement`.

1. **Define the view** in `b2bua-sdk/src/model.rs` next to `RuleContext`:
   `pub struct RuleCall<'a>(&'a Call)` with a public constructor
   (`RuleCall::new(&Call)` — must be public, it is built from the `b2bua`
   crate) and read accessors only. No `Deref`, no `raw()` escape hatch.
   Initial accessor list (the compiler finalises it — add an accessor only
   when a real rule breaks, never speculatively):
   - identity/lifecycle: `call_ref()`, `state()`, `sm_cursors()`, `created_at()`
   - legs: `a_leg()`, `b_legs()`, `leg(leg_id)` (the `source_leg` resolution
     helper moves here), `a_leg_invite()`
   - routing/decision: `callback_context()`, `features()`, `active_peer()`,
     `tag_map()`
   - service slices: `transfer()`, `relay_first_18x()`, `promote_pem()`,
     `ext()`
   - bookkeeping (read-only): `cdr_events()`
   Excluded by design (grep-able list, keep in the doc comment): `topology`,
   `worker_index`, `sampled`, `trace_id`, `root_span_id`, `message_count`,
   `terminating_refresh_legs`, `a_leg_pending_vias`, `a_leg_pending_cseq`,
   `limiter_entries`, `timers`, `active_rules`, `policy_update_headers`,
   `policy_update_body`, `billing_context`, `emergency`.
2. **Swap the field**: `RuleContext.call: RuleCall<'a>`. The two construction
   sites ([`router.rs:786`](../../crates/b2bua/src/router.rs),
   [`rules/service.rs:65`](../../crates/b2bua/src/rules/service.rs)) wrap the
   `&Call` they already hold.
3. **Keep the framework on the full struct.** `execute_rules` /
   `ActionExecutor::execute` take the authoritative `&Call` as an explicit
   parameter instead of reaching through `ctx.call`
   (`executor.rs:63` `before` clone, `actions.rs:44` working copy,
   `machine_active`). The `ServiceDef::init` seed hook moves to
   `&RuleCall` too — services get no wider read surface at init than in
   handlers.
4. **Fix the breaks**: ~36 `ctx.call` sites in `b2bua/src/rules/*`, 6 in
   `crates/announcement`. All are semantic reads per the 2026-06-11 survey —
   expect zero design questions, pure mechanical accessor swaps.
5. **Shrink the SDK re-export**: `b2bua-sdk::rules` stops re-exporting `Call`;
   it exports `RuleCall` (+ `Leg`, `Dialog`, and the slice types the accessors
   return). If announcement tests need to *build* calls, they use the harness
   builders, not the SDK.
6. **Tests:** compile-driven; plus one new SDK test asserting the announcement
   crate's pattern (ext slice read, b_leg scan, a_leg provisional) works
   against the view. Docgen (`xtask state-machine-docs`) re-run — diagrams
   must be byte-identical.

**Done when:** no `ctx.call.<field>` direct field access survives outside the
`b2bua` framework internals; `cargo doc -p b2bua-sdk` shows no `Call` struct.

---

## Slice 2 — obligation extraction + the CDR lane reorder (ADR-0020 X7, X2)

**Crates touched:** `b2bua` only.

1. **`crates/b2bua/src/obligations.rs`** (new): `Obligation` (kind id + dedupe
   key), `ObligationKind` trait (`derive(&Call)` / `already_emitted(&Effects)`
   / `append(&Obligation, &mut Effects)` — pure over the snapshot, total over
   old serialised bodies, skip-aware), `ObligationSet { core() / with() /
   settle() / owed() }`.
2. **Extract verbatim** from `invariants.rs`:
   - `LimiterObligations` = lines 76–100 (fail-open skip + `(limiter_id,
     origin_window)` dedupe);
   - `CdrObligation` = lines 67–74 (single owed CDR — the exactly-one promise
     *is* this single-element derivation).
   `enforce(obligations: &ObligationSet, before, result)` keeps
   CancelAllTimers-first and RemoveCall-last itself; `settle()` fills the
   middle. Three `enforce` call sites get `&ctx.obligations`
   (`RouterCtx` gains `obligations: Arc<ObligationSet>`, built
   `ObligationSet::core()` in `b2bua_core.rs`).
3. **Equivalence gate** (the commit's centrepiece): a proptest generating
   arbitrary `Call` snapshots (vary `limiter_entries` incl. fail-open, state
   edges, pre-emitted decrement/WriteCdr effects) asserting old-`enforce` and
   new-`enforce` produce identical effect multisets. Keep the old body in the
   test module as the oracle.
4. **Lane reorder in `process_result`**: interpret the terminal `RemoveCall`
   **after** the buffered lane. Mechanically: peel `RemoveCall` off
   `effects.critical` (enforce already forces it last), run
   critical-minus-RemoveCall → outbound → soft → buffered, then
   `release_call(Terminated)`. Add a unit test with a recording CDR writer
   asserting write-then-remove ordering, and re-run the exactly-one-CDR
   battery.
5. **No behaviour change** elsewhere: this slice must be invisible in the
   harness traces (assert: scenario suite byte-identical reports).

**Done when:** `invariants::enforce` contains no hardcoded limiter/CDR
knowledge; the property test passes; CDR-write precedes store-delete in
`process_result`.

---

## Slice 3 — the call reaper (ADR-0020 X1, X3–X6)

**Crates touched:** `b2bua`, `b2bua-harness`; `b2bua-runner` diff empty.

### 3a. Config (`config.rs`)
`ReaperConfig { enabled: true, sweep_interval_sec: 30, idle_max_sec: 0 }` on
`B2buaConfig`; `idle_max_sec == 0` → derived `3 × keepalive_interval_sec` at
core spawn; `validate()` rejects explicit values `< 2 × keepalive_interval`.

### 3b. Store ledger (`store/mod.rs`)
- `touched: HashMap<String, i64>` side map in `inner` (never a `Call` field).
- `touch(call_ref, now_ms)` (monotonic-max, no-op on absent ref); set at
  `create`, fresh `hydrate_from_replica`, `materialize_if_absent`; cleared in
  `remove` and `drop_local` (orphans never enter — assert in test).
- `stale_candidates(now_ms, idle_max_ms) -> Vec<(String, i64 /*watermark*/)>`,
  **excluding takeover-marked refs**, one pass under the inner lock.
- Gauge: ledger size next to the store gauges (a stamp leak reads like a lock
  leak).

### 3c. Dispatcher hooks (`dispatch.rs`)
- `DispatchHooks { touched, handler_failed, queue_gone }` (plain
  `Arc<dyn Fn>`s, `noop()` for unit tests). `touched` fires at **worker
  dequeue** of an Event item; dropped enqueues do *not* touch.
- The panic swallow becomes:
  `Err(e) if e.is_panic() => hooks.handler_failed(call_ref, Panicked)`,
  `Err(e) if e.is_cancelled() => hooks.handler_failed(call_ref, Aborted)`.
- `abort_in_flight(call_ref)`: one `AbortHandle` slot per worker (FIFO ⇒ at
  most one in-flight body); aborting drops the body — and the per-call lock
  guard a hung handler holds.
- `queue_gone` fires at worker exit (where `bump_removal` lives) — the single
  point reaper strike/attempt state is forgotten.

### 3d. The reaper (`reaper.rs`, new)
- `Reaper::new(cfg, keepalive_interval, clock, reentry_tx, metrics)`,
  `dispatch_hooks()`, `start(...) -> JoinHandle` (pushed into
  `B2buaCore.tasks`, aborted by harness `crash()`); constructed **inside**
  `spawn_with_services` — never wired by callers.
- Constants: `REAPER_TOPIC = "reaper"`, outcomes `stale` / `fatal-error` /
  `discharge`.
- Sweep tick (one `tokio::time::interval`, `MissedTickBehavior::Delay`,
  `max_reaps_per_sweep` pacing — the keepalive-shed lesson): drain strike-2
  requests → discharge; for each stale candidate, inject
  `InternalEvent{topic, outcome: stale, payload: {watermark}}` or escalate
  (`abort_in_flight` after `escalate_after_sweeps` undelivered attempts);
  prune strike maps against the live map.
- `discharge_result(snapshot) -> HandlerResult`: pure — force
  `state = Terminated`, `ByeTimeout` on unresolved legs, one
  `CdrEvent{"reaper-discharge", reason}`, empty effects (enforce derives the
  rest). Panic-free by construction; unit-testable without a runtime.
- `confirm(call_ref, watermark) -> bool` for the router guard.

### 3e. Router (`router.rs`)
- First statement after the per-call lock for `topic == "reaper"` events:
  `confirm` or drop — **before** any hydration/on-demand-reclaim path can run
  (the anti-resurrection guard, ADR-0020 X5).
- Discharge branch (~6 lines): peek snapshot → `discharge_result` → existing
  `finalize/enforce/process_result`. Missing snapshot → existing orphan path.
- `process_result` / `release_call` to `pub(crate)`.

### 3f. Rules (`rules/defaults.rs`)
Two CORE_LAYER rules mirroring `terminating-safety-timeout`:
- `reaper-stale` (`Match::internal_event().topic("reaper").outcome("stale")`):
  `TerminateLeg{ByeTimeout}` per unresolved leg + `AddCdrEvent` +
  `BeginTermination` — works from Active *and* Terminating.
- `reaper-fatal-error` (outcome `fatal-error`): same shape, reason
  `handler-panic`.
`outcome: discharge` deliberately has **no rule** (router branch). Re-run
docgen; the global-call diagram gains the reap edges.

### 3g. Metrics (`metrics.rs`)
`handler_panics_total`, `reaper_swept_total`, `reaper_reaped_total{reason}`,
`reaper_discharged_total` (alarm ~0), `reaper_inject_failed_total`,
`reaper_strikes_live` gauge.

### 3h. Harness (`b2bua-harness`)
- `wedge_call(call_ref)` → dispatch a `std::future::pending()` body (the
  production hang shape: worker parked, per-call lock held).
- Panic injection via a panicking service rule through `spawn_with_services`.
- `assert_fully_reaped()` gains invariant #4: ledger size 0.
- Scenario suite: defaults must keep the reaper inert within scenario horizons
  (healthy calls are touched by their own keepalive events); any scenario that
  deliberately freezes longer sets `reaper.enabled = false`.

### 3i. Test matrix (each maps to an escape route or a design race)
| Test | Asserts |
|---|---|
| `panic_strike1_reaps_via_rules` | panicking handler → fatal-error rule → 1 CDR, limiter freed, creations == removals |
| `panic_strike2_discharges` | panicking *reaper rule* → discharge branch → 1 CDR, store delete propagated |
| `stale_active_reaped` | withhold all events past idle_max → exactly one reap, reason `stale` |
| `wedged_terminating_reaped` | lost TerminatingTimeout (cancel it manually) → reaper promotes + cleans |
| `dropped_bye_reaped` | fill the per-call queue so a BYE drops → call reaped, BYE-shaped CDR disposition |
| `hung_handler_aborted` | `wedge_call` → abort_in_flight → lock released → verdict flows through funnel |
| `watermark_discards_stale_verdict` | touch between sweep and delivery → reap event is a no-op |
| `no_resurrection_from_replica` | reap event for a *released* call → no hydration, no new call (the X5 race) |
| `reclaimed_long_call_not_reaped` | reclaim a 2 h-old call → stamp = reclaim time → survives sweeps |
| `takeover_copy_never_swept` | acting-backup copy idle past idle_max → untouched; self-release still works |
| `exactly_once_under_interleaving` (proptest) | arbitrary order of {BYE, stale verdict, discharge, panic} on one call ⇒ 1 CDR, each limiter key decremented once |
| paused-clock discipline | every test advances to exactly one sweep deadline at a time (CLAUDE.md: never two deadlines in one advance) |

**Done when:** the test matrix is green, the endurance dashboards gain the
reaper panels (`reaper_discharged_total` alarmed at > 0), and a full scenario
battery run shows zero reaper fires (`reaper_reaped_total == 0` on healthy
traffic).

---

## Out of scope (recorded so reviews don't re-suggest)

- Abandoned-Element CDR at the backup (ADR-0020 X3 — rejected).
- Durable CDR WAL / delete-after-CDR replica coupling (X2 — rejected).
- `RuleLeg` view (deferred until a leg internal actually leaks).
- Per-call-kind wedge deadlines as a trait (`WedgePolicy` — cut; config
  fields only until a second policy exists).
- Endurance validation run: user-gated, after slice 3 lands.
