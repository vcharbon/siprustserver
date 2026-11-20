# Appendix: Failover test matrix: safe-point CallScenario DSL, transparent-failover oracle, generated cell tests

The decision part: [ADR-0013](../0013-failover-test-matrix.md).

## Source

**Source:** this codebase. The HA re-hydration / backup / switchback paths
accumulated a string of bugs that were each long to detect, reproduce, and debug
because they only surfaced under the **live endurance + chaos** suite (see
`MEMORY.md`: timer-tombstone CPU drift, repl reclaim/reboot bootstrap
truncation, orphan-reject dispatch leak, takeover long-call loss, limiter netcut
OOM). All of them are, in principle, expressible under the existing fake-clock /
simulated-network harness (`FailoverHarness`, ADR-0011 X10 tier-2). This ADR
records how we turn that one-off harness into a *matrix framework* so the whole
class is covered in CI rather than discovered in endurance.

## Context, long form

`FailoverHarness` composes, under one paused clock:
the SIP plane (`scenario-harness`), a real load-balancing `ProxyCore` over a
`SimulatedWorkerRegistry`, and two replicating `B2buaCore` workers over a
recording `SimulatedReplicationNetwork`. It exposes `crash()`, `reboot()`,
`partition()/heal()`, `set_health()`, `is_ready()`, and call introspection
(`active_calls`, `lock_count`, `call_gen`, `scan_backed_up`, `scan_primary`,
`get`). Several hand-written tests use it (`canonical_failover`,
`hydrated_takeover_copy_self_releases_without_leaking`,
`reboot_reclaim_exactly_one_owner_after_self_release`,
`acting_backup_terminate_leaves_no_expired_context_for_reclaim`, four
`matrix_*` fault cases).

Two problems: (1) each test repeats ~50 lines of identical setup + cookie
discovery + the `(b1, b2)` borrow-swap dance; (2) coverage is a handful of points
in a large space (dialog state × in-dialog event × failure mode × recovery
sequence), so the gaps are invisible — nothing forces us to confront every cell.

## 0. Crate layout — a dedicated `failover-harness` crate

All multi-node HA-failover test infrastructure lives in the dedicated crate
`crates/failover-harness/`:

- `FailoverHarness`, `ReplicatedB2buaSut`, `ProxySut` and the hand-written
  failover tests.
- The `CallScenario` / safe-point DSL, the transparency oracle, and the
  `macro_rules!` matrix (§1–§4 below). `src/lib.rs` = DSL + macro; `tests/` =
  the generated cells.
- **Normal deps**: `b2bua`, `sip-proxy`, `repl-net`, `topology`, `ha-harness`,
  `scenario-harness`, **plus `call-limiter` + `http-net`** (promoted from
  dev-deps so the DSL can wire the genuine limiter stack — `HttpCallLimiter` +
  `LimiterServer` + `CallStore` over the **simulated** HTTP fabric
  (`SimulatedHttpNetwork`) on the fake clock, *not* a `NoopLimiter`/mock — in
  library code; the reason a dedicated crate is warranted rather than extending
  `b2bua-harness`).

`b2bua-harness` is single-SUT b2bua testing (`B2buaSut`), without `sip-proxy`,
`repl-net`, `topology` or `ha-harness` in its normal deps. Rejected:
extending `b2bua-harness` in place — it would bloat that crate's compile/test
target and mix HA cells with the unrelated refer/prack/media suites; and a crate
that left `FailoverHarness` behind in `b2bua-harness` — splitting one concern
across two crates.

## 1. `CallScenario` — a callflow as a step list with **safe-points**

A callflow is expressed declaratively as an ordered list of steps (the SIP
actions + expected reactions for a clean run). Between steps the author marks
**safe-points**: boundaries where the call's replicated state is *quiescent* (the
last authoritative mutation has settled to the backup), so a failover injected
there is **expected to be transparent**. New callflows (REFER, forking, complex
callflow services) declare their steps + safe-points and inherit failover
coverage with no bespoke failover test. *This is the extensibility hook the
author asked for: "flag expected safe points for crash/recovery relative to the
call scenario."*

## 2. Failover-matrix driver — generated cell tests

A `macro_rules!` matrix expands `(scenario × safe-point × failure-mode ×
recovery-permutation)`, after a legality filter, into **one individually-named
`#[tokio::test]` per legal cell** (e.g.
`transparent__established__bye_alice__kill__reboot_after_takeover`). No new
dependency; each cell is isolated, parallel, and individually runnable. (Rejected:
a single looping test — one failure masks the rest, no per-cell CI name; an
`rstest`/`test-case` dependency the workspace otherwise avoids.)

## 3. The oracle = differential baseline **+** strict structural invariants

Each cell runs the scenario twice — **clean** (baseline) and with the failover
**injected at the safe-point** — and asserts the externally observable behavior
matches: the ordered sequence of methods/status each UA observes, plus the final
CDR disposition. The check is **strict**: every message's From/To tags and CSeq
must stay correct/consistent. Transparency *requires* the takeover to preserve
the **b-leg's** From/To tag and CSeq (replicated dialog state, not re-minted), so
a takeover that re-mints them fails the differential — that is the bug-catching
teeth. Layered on top: a few always-on external invariants (never a 481/5xx the
baseline didn't have; exactly one CDR with the right disposition).

## 4. Scope: **transparent failover first**

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

The legality filter keeps `ConfirmedPreAck` and `Established` in transparent v1. `Early`,
re-negotiating, terminating-in-flight and `CANCEL` are listed for completeness and run as
disruptive cases with their own expectations.

- **A. Dialog state at the safe-point:** early · confirmed-pre-ACK · established ·
  re-negotiating · terminating-in-flight.
- **B. In-dialog event the backup processes**, as behavioral *categories*:
  - *Terminating* — `BYE` (confirmed/established → 200) or `CANCEL` (early → 487);
    same invariant family (release the takeover copy + lock, **retain** the `bak:`
    Element as a short-TTL deferred terminal reverse-flushed to the primary — the
    backup propagates **no** delete and discharges nothing; the reclaiming primary
    is the sole discharge authority; no resurrection on reboot via the delete
    tombstone — ADR-0014 / ADR-0020 X3).
  - *Generic in-dialog* — re-INVITE / UPDATE / INFO / in-dialog OPTIONS ping;
    behaviorally interchangeable, so the driver picks `{method × direction}` by
    **seeded random** rather than permuting (keeps the matrix from exploding).
  - *B2BUA-generated keepalive OPTIONS* — its own category: refreshes both dead-peer
    detection **and** the call-limiter keepalive hold on the owner.
  - *Nothing* — no event routed to the backup; pure re-hydration on reboot.
- **D. Recovery permutation:** kill-and-stay-dead (rest of call served on backup) ·
  kill-then-reboot-with-no-traffic-while-dead (clean reclaim) ·
  kill-then-reboot-after-backup-served-part (takeover → reclaim → self-release).
- **Failure mode:** abrupt kill · graceful drain (both must be transparent at a
  safe-point).
- **Direction (C)** and the concrete generic method are folded into the seeded
  selection, not a full axis.

The **whole matrix runs with the genuine limiter logic** — the b2bua's
`HttpCallLimiter` client talking to a real `LimiterServer` + `CallStore`, but
over the **simulated** HTTP transport (`SimulatedHttpNetwork`) and the same fake
`Clock::test_at(0)`, shared across crashes/reboots. "Genuine" means *not* a
`NoopLimiter`/mock — there is no real socket and no wall-clock; the HTTP
round-trips and the window/TTL expiry ride the one paused `tokio::time` that
`FailoverHarness::advance` drives, exactly like the SIP and replication sims. So
the limiter-hold-migrates/refreshes-on-takeover and released-on-teardown
invariant is asserted, deterministically, in every cell — not just the keepalive
category.

## Keepalive cell at production cadence

The `established__keepalive__kill__reboot_no_traffic` cell runs in the matrix at
production cadence. Two things make it representative *and* fix the cadence:

1. **Production timers, globally.** Every worker runs the real
   `keepalive_interval_sec: 300`
   (`harness.rs`). A long quiescent call is flushed (and its backup `Element` TTL
   refreshed) *only* by its in-dialog OPTIONS, so dead-peer/limiter-refresh/
   backup-TTL cadence is only representative at 300 s. Under `start_paused` the
   advance is free. `reboot_budget_sec` (default 450 s) ≥ 300 s keeps the backup
   alive across one keepalive gap (`config.rs::validate`). This is exactly the
   long-call-on-reboot loss the endurance run flagged ([[repl-takeover-longcall-loss]],
   [[repl-reboot-reclaim-bootstrap-truncation]]): a rebooted primary reclaims a
   quiescent dialog and must re-arm its keepalive so the next AS OPTIONS keeps it
   alive. The cell asserts that transparently against the clean baseline.
2. **A fine-grained pump, never a fixed advance.** The deadline is one interval
   out from whenever the keepalive was last (re-)armed — *establish* in the
   baseline, *reclaim* in the variant (`reclaim_into_live` re-arms fresh). It is
   not computable in advance, so a fixed `advance(300s)` overshoots the variant's
   deadline **and** its 5 s dead-peer reap, tearing the call down (the CLAUDE.md
   keepalive hazard). `FailoverHarness::pump_until(step, max, ready)`
   advances in sub-reap (2 s) steps and stops the instant an async probe is
   satisfied; `Agent::try_receive_tolerating` is the non-blocking drain it polls
   with. The runner answers both legs inside their 5 s window regardless of the
   interleaving timers (the reclaimed-primary keepalive re-arm and the dead-peer
   reap). Teeth verified: deliberately dropping the reclaim keepalive re-arm fails
   the cell.

The happy-path foundation is `failover.rs::successful_long_call_with_b2bua_generated_options`
(no fault: a long call kept alive across two B2BUA-generated OPTIONS cycles at 300 s,
then a clean BYE). Core rearm-on-takeover stays covered by
`failover.rs::hydrated_takeover_copy_self_releases_without_leaking`.

## Complete k8s kill + default RFC CSeq audit

Two rules make the cells representative of the endurance `unexpected_msg`
long-call loss:

1. **The kill is the COMPLETE k8s death signal.** A `Kill`/`Drain` cell does not
   only mark the worker dead at the proxy — `inject_failover` also drives the dead
   pod's endpoint OUT of the survivor's membership (`simulate_peer_removed`), which
   keeps the survivor's membership view honest (it drives no eager takeover,
   ADR-0014); the reboot re-adds it
   (`simulate_peer_added`, a StatefulSet restart = Removed-then-Added). Takeover
   itself stays reactive: the survivor takes a dialog over only when the proxy
   reroutes its in-dialog traffic to it.

2. **A default RFC 3261 audit over the recorded SIP layer, every cell.**
   `cseq-in-dialog-order` (a `CrossMessageAuditRule` installed in the
   `scenario-harness` default `ScopedAuditOptions`) replays every received request
   from the recording and flags an in-dialog CSeq regression per
   `(receiving endpoint, Call-ID, From-tag)` — RFC §12.2.2 out-of-order, which a
   real UA rejects (`unexpected_msg`) but the test UAs answer blindly.
   `run_cell` calls `FailoverHarness::assert_sip_rfc_clean` for baseline + variant
   — the SIP-plane analogue of the universal teardown sweep, on ALL endpoints.
   Rule teeth: `sip-net::rfc_audit::tests`. A representative end-to-end guard with
   in-dialog CSeq churn through the full kill→failover→reboot→reclaim cycle
   is `failover.rs::cseq_stays_in_order_across_failover_and_reclaim`.

   **Framework finding (open).** The genuine production *race* — the last in-dialog
   flush dying with the primary so the survivor probes from a stale CSeq snapshot —
   is **not yet reproducible** here: under the simulated repl fabric the backup
   replica stays current (a `partition()` does NOT sever an established
   forward-replication stream — only a crash closes streams, and the primary must
   be alive to create the lag; observed the backup gen advancing 4→8 across a
   partition). So the audit is in place and proven to have teeth, but the stale-CSeq
   lag itself needs a new seam to inject deterministically (e.g. a store-level "hold
   the replica back" hook, or a fault that cuts forward replication on a live
   primary). The underlying b2bua behaviour is settled (ADR-0014 reactive-only;
   ADR-0020 Model-Y terminal handling); the only open item is this
   deterministic injection seam.
