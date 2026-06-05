# 0013 — Failover test matrix: safe-point CallScenario DSL, transparent-failover oracle, generated cell tests

**Status:** accepted (2026-06-04); **built** (2026-06-04) — `crates/failover-harness`,
19 transparent cells + 5 oracle teeth tests green (the keepalive cell landed
2026-06-05; see the keepalive amendment).

**Amendment (2026-06-04, during build):** `Early` state / `CANCEL` are
reclassified from the transparent matrix to **v2-disruptive**. By the same
quiescence rule that excludes re-negotiating/terminating-in-flight, an early
dialog has a *pending INVITE transaction* — and transactions are not replicated,
only confirmed call context is — so killing the primary mid-early-INVITE is not
transparent. The transparent state axis is therefore `ConfirmedPreAck` (200 sent
+ replicated, ACK pending → ACK routes to the backup) and `Established`.

**Amendment (2026-06-05) — keepalive cell landed at production cadence.** The
`established__keepalive__kill__reboot_no_traffic` cell is now in the matrix (was
`#[ignore]`). Two changes made it representative *and* fixed the cadence:

1. **Production timers, globally.** The harness no longer shortcuts the keepalive
   to 30 s — every worker runs the real `keepalive_interval_sec: 300`
   (`harness.rs`). A long quiescent call is flushed (and its backup `Element` TTL
   refreshed) *only* by its in-dialog OPTIONS, so dead-peer/limiter-refresh/
   backup-TTL cadence is only representative at 300 s. Under `start_paused` the
   advance is free. `reboot_budget_sec` (default 450 s) ≥ 300 s keeps the backup
   alive across one keepalive gap (`config.rs::validate`). This is exactly the
   long-call-on-reboot loss the endurance run flagged ([[repl-takeover-longcall-loss]],
   [[repl-reboot-reclaim-bootstrap-truncation]]): a rebooted primary reclaims a
   quiescent dialog and must re-arm its keepalive so the next AS OPTIONS keeps it
   alive — the cell now asserts that transparently against the clean baseline.
2. **A fine-grained pump replaces fixed advances.** The deadline is one interval
   out from whenever the keepalive was last (re-)armed — *establish* in the
   baseline, *reclaim* in the variant (`reclaim_into_live` re-arms fresh). It is
   not computable in advance, so a fixed `advance(300s)` overshoots the variant's
   deadline **and** its 5 s dead-peer reap, tearing the call down (the CLAUDE.md
   keepalive hazard). New primitive `FailoverHarness::pump_until(step, max, ready)`
   advances in sub-reap (2 s) steps and stops the instant an async probe is
   satisfied; `Agent::try_receive_tolerating` is the non-blocking drain it polls
   with. The runner answers both legs inside their 5 s window regardless of the
   three interleaving timers (eager-takeover keepalive, reclaimed-primary
   keepalive, dead-peer reap). Teeth verified: deliberately dropping the reclaim
   keepalive re-arm fails the cell.

The happy-path foundation is `failover.rs::successful_long_call_with_as_generated_options`
(no fault: a long call kept alive across two AS-generated OPTIONS cycles at 300 s,
then a clean BYE). Core rearm-on-takeover stays covered by
`failover.rs::hydrated_call_rearms_keepalive_and_reaps_dead_peer`.

**Amendment (2026-06-05) — complete k8s kill + default RFC CSeq audit.** Two
representativeness upgrades targeting the endurance `unexpected_msg` long-call
loss (handoff `handoff-eager-takeover-residuals-2026-06-05.md` Fix A):

1. **The kill is now the COMPLETE k8s death signal.** A `Kill`/`Drain` cell no
   longer only marks the worker dead at the proxy (reactive, proxy-reroute
   takeover) — `inject_failover` also drives the dead pod's endpoint OUT of the
   survivor's membership (`simulate_peer_removed`), the signal the supervisor
   turns into an eager `TakeOverPeer`; the reboot re-adds it
   (`simulate_peer_added`, a StatefulSet restart = Removed-then-Added). So every
   kill cell now exercises the eager-takeover path that serves quiescent long
   calls, not just proxy reroute.

2. **A default RFC 3261 audit over the recorded SIP layer, every cell.** New
   `sip-net::CSeqInDialogOrderRule` (a `CrossMessageAuditRule` installed in the
   `scenario-harness` default `ScopedAuditOptions`) replays every received request
   from the recording and flags an in-dialog CSeq regression per
   `(receiving endpoint, Call-ID, From-tag)` — RFC §12.2.2 out-of-order, which a
   real UA rejects (`unexpected_msg`) but the test UAs answer blindly.
   `run_cell` calls `FailoverHarness::assert_sip_rfc_clean` for baseline + variant
   — the SIP-plane analogue of the universal teardown sweep, on ALL endpoints.
   Rule teeth: `sip-net::rfc_audit::tests`. A representative end-to-end guard with
   in-dialog CSeq churn through the full kill→eager-takeover→reboot→reclaim cycle
   is `failover.rs::cseq_stays_in_order_across_eager_takeover_and_reclaim`.

   **Framework finding (open).** The genuine production *race* behind Fix A — the
   last in-dialog flush dying with the primary so the survivor probes from a stale
   CSeq snapshot — is **not yet reproducible** here: under the simulated repl
   fabric the backup replica stays current (a `partition()` does NOT sever an
   established forward-replication stream — only a crash closes streams, and the
   primary must be alive to create the lag; observed the backup gen advancing
   4→8 across a partition). So the audit is in place and proven to have teeth, but
   the stale-CSeq lag itself needs a new seam to inject deterministically (e.g. a
   store-level "hold the replica back" hook, or a fault that cuts forward
   replication on a live primary). Until then the CSeq-regression bug is guarded
   against but not actively reproduced in CI. Fix A in the b2bua remains a
   PROPOSAL (pending the ADR-0011 X11 grill) and is **not** applied.

**Source:** this codebase. The HA re-hydration / backup / switchback paths
accumulated a string of bugs that were each long to detect, reproduce, and debug
because they only surfaced under the **live endurance + chaos** suite (see
`MEMORY.md`: timer-tombstone CPU drift, repl reclaim/reboot bootstrap
truncation, orphan-reject dispatch leak, takeover long-call loss, limiter netcut
OOM). All of them are, in principle, expressible under the existing fake-clock /
simulated-network harness (`FailoverHarness`, ADR-0011 X10 tier-2). This ADR
records how we turn that one-off harness into a *matrix framework* so the whole
class is covered in CI rather than discovered in endurance.

## Context

`crates/b2bua-harness/src/failover.rs` already composes, under one paused clock:
the SIP plane (`scenario-harness`), a real load-balancing `ProxyCore` over a
`SimulatedWorkerRegistry`, and two replicating `B2buaCore` workers over a
recording `SimulatedReplicationNetwork`. It exposes `crash()`, `reboot()`,
`partition()/heal()`, `set_health()`, `is_ready()`, and call introspection
(`active_calls`, `lock_count`, `call_gen`, `scan_backed_up`, `scan_primary`,
`get`). Eight hand-written tests use it (`canonical_failover`,
`hydrated_call_rearms_keepalive_and_reaps_dead_peer`,
`reboot_reclaim_hands_back_exactly_one_owner`,
`acting_backup_terminate_leaves_no_expired_context_for_reclaim`, four
`matrix_*` fault cases).

Two problems: (1) each test repeats ~50 lines of identical setup + cookie
discovery + the `(b1, b2)` borrow-swap dance; (2) coverage is a handful of points
in a large space (dialog state × in-dialog event × failure mode × recovery
sequence), so the gaps are invisible — nothing forces us to confront every cell.

## Decision

A two-layer framework on top of the existing `FailoverHarness` (not a rewrite).

### 0. Crate layout — a dedicated `failover-harness` crate

All multi-node HA-failover test infrastructure moves into a new dedicated crate
(`crates/failover-harness/`):

- **Moved in** from `b2bua-harness`: `FailoverHarness`, `ReplicatedB2buaSut`,
  `ProxySut` (today `b2bua-harness/src/failover.rs`) and the 8 existing failover
  tests (`b2bua-harness/tests/failover.rs`).
- **New** here: the `CallScenario` / safe-point DSL, the transparency oracle, and
  the `macro_rules!` matrix (§1–§4 below). `src/lib.rs` = DSL + macro; `tests/` =
  the generated cells.
- **Normal deps**: `b2bua`, `sip-proxy`, `repl-net`, `topology`, `ha-harness`,
  `scenario-harness`, **plus `call-limiter` + `http-net`** (promoted from
  dev-deps so the DSL can wire the genuine limiter stack — `HttpCallLimiter` +
  `LimiterServer` + `WindowStore` over the **simulated** HTTP fabric
  (`SimulatedHttpNetwork`) on the fake clock, *not* a `NoopLimiter`/mock — in
  library code; the reason a dedicated crate is warranted rather than extending
  `b2bua-harness`).

Consequently **`b2bua-harness` slims back** to single-SUT b2bua testing
(`B2buaSut`) and **sheds** `sip-proxy` / `repl-net` / `topology` / `ha-harness`
from its normal deps (they existed only for the failover module). Rejected:
extending `b2bua-harness` in place — it would bloat that crate's compile/test
target and mix HA cells with the unrelated refer/prack/media suites; and a crate
that left `FailoverHarness` behind in `b2bua-harness` — splitting one concern
across two crates.

### 1. `CallScenario` — a callflow as a step list with **safe-points**

A callflow is expressed declaratively as an ordered list of steps (the SIP
actions + expected reactions for a clean run). Between steps the author marks
**safe-points**: boundaries where the call's replicated state is *quiescent* (the
last authoritative mutation has settled to the backup), so a failover injected
there is **expected to be transparent**. New callflows (REFER, forking, complex
callflow services) declare their steps + safe-points and inherit failover
coverage with no bespoke failover test. *This is the extensibility hook the
author asked for: "flag expected safe points for crash/recovery relative to the
call scenario."*

### 2. Failover-matrix driver — generated cell tests

A `macro_rules!` matrix expands `(scenario × safe-point × failure-mode ×
recovery-permutation)`, after a legality filter, into **one individually-named
`#[tokio::test]` per legal cell** (e.g.
`transparent__established__bye_alice__kill__reboot_after_takeover`). No new
dependency; each cell is isolated, parallel, and individually runnable. (Rejected:
a single looping test — one failure masks the rest, no per-cell CI name; an
`rstest`/`test-case` dependency the workspace otherwise avoids.)

### 3. The oracle = differential baseline **+** strict structural invariants

Each cell runs the scenario twice — **clean** (baseline) and with the failover
**injected at the safe-point** — and asserts the externally observable behavior
matches: the ordered sequence of methods/status each UA observes, plus the final
CDR disposition. The check is **strict**: every message's From/To tags and CSeq
must stay correct/consistent. Transparency *requires* the takeover to preserve
the **b-leg's** From/To tag and CSeq (replicated dialog state, not re-minted), so
a takeover that re-mints them fails the differential — that is the bug-catching
teeth. Layered on top: a few always-on external invariants (never a 481/5xx the
baseline didn't have; exactly one CDR with the right disposition).

### 4. Scope: **transparent failover first**

v1 covers only cells whose expected behavior is *zero visible external impact* —
the kill is injected at a safe-point, after replication has settled, so one
uniform oracle applies to every cell and the dimensions multiply cheaply.
Everything routes through the simulated LB (`ProxySut`), never direct-to-worker.

**Deferred to a separate, smaller v2 (bespoke per-case expectations, NOT the
transparency oracle):** repl **partition** during failover, and
**immediate-kill-before-replication** (a message killed before it propagated).
These are *expected* to be externally impactful (`MEMORY.md`: partition/netcut
cases), so they get distinct expectations rather than being forced through the
transparency oracle.

## Axes (legal cross-product, transparent v1)

- **A. Dialog state at the safe-point:** early · confirmed-pre-ACK · established ·
  re-negotiating · terminating-in-flight.
- **B. In-dialog event the backup processes**, as behavioral *categories*:
  - *Terminating* — `BYE` (confirmed/established → 200) or `CANCEL` (early → 487);
    same invariant family (release takeover copy + lock + `bak:` Element,
    propagate Reverse DELETE, no resurrection on reboot).
  - *Generic in-dialog* — re-INVITE / UPDATE / INFO / in-dialog OPTIONS ping;
    behaviorally interchangeable, so the driver picks `{method × direction}` by
    **seeded random** rather than permuting (keeps the matrix from exploding).
  - *AS-generated keepalive OPTIONS* — its own category: refreshes both dead-peer
    detection **and** the call-limiter keepalive hold on the owner.
  - *Nothing* — no event routed to the backup; pure re-hydration on reboot.
- **D. Recovery permutation:** kill-and-stay-dead (rest of call served on backup) ·
  kill-then-reboot-with-no-traffic-while-dead (clean reclaim) ·
  kill-then-reboot-after-backup-served-part (takeover → reclaim → handback).
- **Failure mode:** abrupt kill · graceful drain (both must be transparent at a
  safe-point).
- **Direction (C)** and the concrete generic method are folded into the seeded
  selection, not a full axis.

The **whole matrix runs with the genuine limiter logic** — the b2bua's
`HttpCallLimiter` client talking to a real `LimiterServer` + `WindowStore`, but
over the **simulated** HTTP transport (`SimulatedHttpNetwork`) and the same fake
`Clock::test_at(0)`, shared across crashes/reboots. "Genuine" means *not* a
`NoopLimiter`/mock — there is no real socket and no wall-clock; the HTTP
round-trips and the window/TTL expiry ride the one paused `tokio::time` that
`FailoverHarness::advance` drives, exactly like the SIP and replication sims. So
the limiter-hold-migrates/refreshes-on-takeover and released-on-teardown
invariant is asserted, deterministically, in every cell — not just the keepalive
category.

## Consequences

- New callflow services become failover-tested by declaring safe-points; the
  matrix is the single source of truth for which cells exist.
- A generated-cell failure is named for its exact `(state, event, fault,
  recovery)` coordinates, so an endurance-class bug reproduces as a single
  `cargo test <name>` instead of an hours-long chaos run.
- The transparency oracle is strict on From/To/CSeq; legitimate future changes to
  b-leg identifier handling will trip it and must be reviewed deliberately.
- Disruptive (partition / pre-replication-kill) coverage is explicitly *not* in
  the transparent matrix; it is a known, named follow-up, not an accidental gap.
