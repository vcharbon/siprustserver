# Failover test matrix — enumerated cells (v1 transparent)

Companion to **ADR-0013**. The exhaustive cell list the `macro_rules!` matrix
generates, one named `#[tokio::test]` per row. All cells route through the
simulated LB (`ProxySut`) and run with the **genuine limiter logic over the
simulated HTTP fabric** — `HttpCallLimiter` → `LimiterServer` + `WindowStore`
over `SimulatedHttpNetwork` on the fake `Clock::test_at(0)` (no socket, no
wall-clock; not a `NoopLimiter`/mock).

Home: the new **`crates/failover-harness/`** crate (ADR-0013 §0) — `FailoverHarness`
+ DSL + matrix macro in `src/`, generated cells in `tests/transparent_v1.rs`.

## Status (2026-06-05 — BUILT, keepalive cell landed)

- Crate `failover-harness` created; `FailoverHarness`/`ReplicatedB2buaSut`/`ProxySut`
  + the 10 pre-existing failover tests + `limiter_ha` moved in; `b2bua-harness`
  slimmed (shed `sip-proxy`→dev-dep, dropped `repl-net`/`topology`/`ha-harness`).
- DSL: `scenario.rs` (`Cell`/`DialogState`/`Event`/`Fault`/`Recovery`/`Party`),
  `oracle.rs` (differential `Observation` + `TeardownSweep`), `runner.rs`
  (`run_cell`), `transparent_matrix!` macro.
- **19 transparent cells GREEN** + **5 oracle teeth unit tests** (prove the
  differential catches a re-minted b-leg tag, and the sweep catches leaked calls
  / outstanding limiter holds / missing CDRs) + the 11 failover tests + 1
  limiter_ha, all green.
- **Keepalive cell landed (2026-06-05):** `established__keepalive__kill__reboot_no_traffic`
  is in the matrix (was `#[ignore]`). The harness now runs **production timers
  globally** (`keepalive_interval_sec: 300`) so the AS-generated OPTIONS cadence
  is representative — this is the long-call-on-reboot loss the endurance run
  flagged. A reboot re-arms the keepalive a *fresh* interval out, so the deadline
  is not computable in advance; the new `FailoverHarness::pump_until` +
  `Agent::try_receive_tolerating` drive a fine-grained sub-reap pump instead of a
  fixed `advance` (which would overshoot the 5 s dead-peer reap and tear the call
  down). Happy-path foundation: `failover.rs::successful_long_call_with_as_generated_options`.
  Teeth verified by dropping the reclaim keepalive re-arm (cell fails).
- **Quiescence reclassification:** `Early`/`CANCEL` moved to v2-disruptive — a
  pending INVITE transaction is not replicated (only confirmed call context is),
  so killing the primary mid-early-INVITE is not transparent. `ConfirmedPreAck`
  (200 sent + replicated, ACK pending → ACK routes to backup) is the transparent
  boundary and is covered.

The cell tables below are the v1 *intent*; the as-built names live in
`tests/transparent_v1.rs` (`<state>__<event>__<fault>__<recovery>`).

## Oracle (every cell)

Run the scenario twice — **clean baseline** vs **failover injected at the
safe-point** — and assert:
1. The ordered method/status sequence each UA (alice, bob) observes matches the
   baseline.
2. **Strict:** every message's From/To tags + CSeq are correct/consistent —
   including the **b-leg** identifiers the takeover must *preserve* (replicated
   state, not re-minted).
3. Final CDR disposition matches; no 481/5xx the baseline lacked.
4. No internal leak: `active_calls`, `lock_count`, limiter holds all balance to
   the baseline's end-state; exactly-one-owner after handback.

### Universal teardown sweep (every cell, baseline + variant)

**Every scenario drives the call to full termination** — non-terminating-event
cells (Generic, Nothing) append a final BYE — so one post-condition holds for
all cells. A few simulated seconds after the call ends (CDR flush window), assert
on **both** nodes (primary *and* backup, whichever are alive):

- `active_calls() == 0` and `lock_count() == 0` — no held call context anywhere.
- no residual `bak:`/`pri:` Element for the call ref (`scan_backed_up` /
  `scan_primary` clear) — nothing left for a later reboot to resurrect.
- a **CDR was written** for the call (exactly one end-event, correct disposition).
- limiter `store.stats().current_total == 0` — every hold released.

## Legality (why the state axis is 3, not 5)

A **safe-point demands quiescence** (last mutation settled to the backup).
`re-negotiating-in-flight` and `terminating-in-flight` are mid-transaction =
not-yet-replicated → **inherently disruptive → v2**, not safe-points. v1 states:
`early` · `confirmed-pre-ACK` · `established`.

Recovery legality: `Nothing` pairs only with `reboot-no-traffic` (the event *is*
traffic for the others); `Terminating`/`Generic` pair with `stay-dead` and
`reboot-after-takeover` (`reboot-no-traffic` contradicts a routed event).

## Cells — KILL fault

| # | name | state | event (Cat) | recovery | asserts (beyond oracle) | maps to user ask |
|---|------|-------|-------------|----------|--------------------------|------------------|
| 1 | `early__cancel__kill__stay_dead` | early | CANCEL → 487 (term) | backup serves, primary stays dead | early dialog torn down on backup, no leak | "cancel by backup" |
| 2 | `early__cancel__kill__reboot_no_resurrect` | early | CANCEL → 487 (term) | reboot after backup terminated | primary does NOT resurrect the cancelled early dialog | "not put back on nominal" |
| 3 | `early__nothing__kill__reboot_reclaim` | early | none | reboot, no traffic while dead | reclaim restores early dialog; 200 then proceeds normally | "nothing processed, verify ok" |
| 4 | `confirmed_pre_ack__ack__kill__stay_dead` | confirmed-pre-ACK | ACK absorbed by backup | backup serves, primary stays dead | ACK-exemption→backup; dialog established on backup | "200OK/ack on backup" |
| 5 | `confirmed_pre_ack__ack__kill__reboot_handback` | confirmed-pre-ACK | ACK absorbed by backup | reboot → reclaim → handback | switch on backup for 200/ACK **then back to nominal**; one owner | **must-have #1** |
| 6 | `established__bye__kill__stay_dead` | established | BYE → 200 (term) | backup serves, primary stays dead | clean teardown on backup, Reverse DELETE, no leak | "BYE by backup" |
| 7 | `established__bye__kill__reboot_no_resurrect` | established | BYE → 200 (term) | reboot after backup terminated | terminated call NOT reclaimed/resurrected on primary | **"not put back on nominal at re-hydration"** |
| 8 | `established__generic__kill__stay_dead` | established | generic in-dialog (seeded) | backup serves rest of call | takeover, timers re-armed, call completes on backup | "rest of call served on backup" |
| 9 | `established__generic__kill__reboot_handback` | established | generic in-dialog (seeded) | reboot → reclaim live call → handback | exactly-one-owner; reclaimed gen highest; in-dialog reroutes to nominal | **"re-INVITE by backup, ok after"** |
| 10 | `established__keepalive__kill__stay_dead` | established | AS keepalive OPTIONS | takeover then backup serves | keepalive re-armed on backup; **limiter hold refreshed**; dead-peer detect | keepalive/limiter |
| 11 | `established__keepalive__kill__reboot_handback` | established | AS keepalive OPTIONS | reboot → reclaim → handback | keepalive + limiter hold migrate to reclaimed primary; no double-OPTIONS | keepalive/limiter |
| 12 | `established__nothing__kill__reboot_reclaim` | established | none | reboot, no traffic while dead | idle call survives silent primary death; reclaim re-arms timers/limiter | **"nothing processed, verify ok"** |

## Cells — DRAIN fault (graceful SIGTERM; representative overlay)

Drain exercises the `decode_stickiness` drain-grace path (in-flight completes on
the draining primary; routing flips to backup only after grace). Applied to the
canonical rows:

| # | name |
|---|------|
| 13 | `established__bye__drain__reboot_no_resurrect` |
| 14 | `established__generic__drain__reboot_handback` |
| 15 | `confirmed_pre_ack__ack__drain__reboot_handback` |
| 16 | `established__nothing__drain__reboot_reclaim` |

## Generic-category seed rotation

The `generic` rows (8, 9, 14) take a seed selecting `{method ∈ re-INVITE |
UPDATE | INFO | in-dialog OPTIONS} × {direction ∈ alice | bob}`. Instantiate each
generic row under **4 seeds** so the suite rotates through the method×direction
space without per-state permutation (e.g. `established__generic_s0..s3__...`).

## Deferred to v2 — disruptive (bespoke expectations, NOT the transparency oracle)

- **Partition during failover** — repl link cut at the failover moment; reverse-
  propagation best-effort, re-converge on heal. (cf. `matrix_partition_during_failover`.)
- **Immediate-kill-before-replication** — kill a message before it propagated;
  expected *visible* impact, asserted per-case.
- **re-negotiating-in-flight / terminating-in-flight at kill time** — mid-
  transaction, non-quiescent; per-case expectations.
- **Double-fault** — crash + transient repl fault (cf. `matrix_double_fault`).
