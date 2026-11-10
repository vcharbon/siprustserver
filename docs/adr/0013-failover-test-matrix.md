# Failover test matrix: safe-point CallScenario DSL, transparent-failover oracle, generated cell tests

**Status:** accepted (2026-06-04), built in `crates/failover-harness`.

The source, the axes in full, the keepalive cell at production cadence and the
complete-kill and CSeq-audit rules are in the
[appendix](appendix/0013-failover-test-matrix.md), under the same section
numbers.

**Takeover model (ADR-0014, reactive-only).** A failed-over dialog is taken over
**reactively**, only when the proxy reroutes its in-dialog traffic to a survivor;
a quiescent dialog is recovered by the rebooting primary's (smoothed) reclaim.
There is no eager or membership-driven takeover and no `Deactivate` handback. The
acting-backup **self-releases** its takeover copy on transaction completion,
deferring any terminal discharge to the primary. The harness asserts in cluster
vocabulary (`serves` / `is_synchronized_backup` / `memory_clean` /
`assert_single_owner` / `assert_call_fully_released`), not HA internals.

**Scope: `Early`/`CANCEL` are v2-disruptive, not transparent.** An early dialog
has a pending INVITE transaction, and transactions are not replicated, only
confirmed call context is. So killing the primary mid-early-INVITE is not
transparent, by the same quiescence rule that excludes
re-negotiating/terminating-in-flight. The transparent state axis is
`ConfirmedPreAck` (200 sent + replicated, ACK pending, so the ACK routes to the
backup) and `Established`.

## Context

`FailoverHarness` composes the SIP plane, a load-balancing `ProxyCore` and two
replicating `B2buaCore` workers under one paused clock, with crash, reboot,
partition and introspection. HA re-hydration, backup and switchback bugs
surfaced only under live endurance and chaos runs, though each fits this
harness. Hand-written tests repeat their setup and cover a handful of points
in a large space, so the gaps are invisible.
Detail: [appendix, source](appendix/0013-failover-test-matrix.md#source) and
[context](appendix/0013-failover-test-matrix.md#context-long-form).

## Decision

A two-layer framework on top of the existing `FailoverHarness` (not a rewrite).

### 0. Crate layout — a dedicated `failover-harness` crate

`crates/failover-harness/` holds all multi-node HA-failover test
infrastructure: `FailoverHarness`, the SUTs, the DSL, the oracle and the
matrix. Its normal deps include `call-limiter` and `http-net`, so the DSL
wires the genuine limiter stack over the simulated HTTP fabric.
`b2bua-harness` stays single-SUT.
Detail: [appendix §0](appendix/0013-failover-test-matrix.md#0-crate-layout--a-dedicated-failover-harness-crate).

### 1. `CallScenario` — a callflow as a step list with **safe-points**

A callflow is an ordered list of steps. The author marks safe-points, where
the call's replicated state is quiescent, so a failover injected there is
expected to be transparent. A new callflow declares its steps and safe-points
and inherits failover coverage.
Detail: [appendix §1](appendix/0013-failover-test-matrix.md#1-callscenario--a-callflow-as-a-step-list-with-safe-points).

### 2. Failover-matrix driver — generated cell tests

A `macro_rules!` matrix expands (scenario × safe-point × failure mode ×
recovery permutation), after a legality filter, into one named
`#[tokio::test]` per legal cell.
Detail: [appendix §2](appendix/0013-failover-test-matrix.md#2-failover-matrix-driver--generated-cell-tests).

### 3. The oracle = differential baseline **+** strict structural invariants

Each cell runs clean and with the failover injected, and asserts the same
ordered methods and statuses at each UA and the same final CDR disposition.
From/To tags and CSeq must match, so a takeover that re-mints the b-leg's
fails. Always-on invariants add no 481 or 5xx the baseline lacked and exactly
one CDR.
Detail: [appendix §3](appendix/0013-failover-test-matrix.md#3-the-oracle--differential-baseline--strict-structural-invariants).

### 4. Scope: **transparent failover first**

v1 covers only cells with zero visible external impact, all routed through
`ProxySut`. A repl partition during failover and a kill before replication
get bespoke expectations in a separate v2.
Detail: [appendix §4](appendix/0013-failover-test-matrix.md#4-scope-transparent-failover-first).

The axes are dialog state, in-dialog event category (terminating, generic
in-dialog by seeded random choice, B2BUA keepalive OPTIONS, nothing),
recovery permutation and failure mode (abrupt kill, graceful drain). Every
cell runs the genuine limiter logic on the fake clock. A kill drives the dead
pod out of the survivor's membership, and every cell runs the default RFC 3261
`cseq-in-dialog-order` audit. The keepalive cell runs at the production
300 s interval. The stale-CSeq race of a last flush dying with the primary
has no deterministic injection seam.
Detail: [axes](appendix/0013-failover-test-matrix.md#axes-legal-cross-product-transparent-v1),
[keepalive cell](appendix/0013-failover-test-matrix.md#keepalive-cell-at-production-cadence),
[complete kill and CSeq audit](appendix/0013-failover-test-matrix.md#complete-k8s-kill--default-rfc-cseq-audit).

## Consequences

- New callflow services become failover-tested by declaring safe-points; the
  matrix is the single source of truth for which cells exist.
- A generated-cell failure is named for its exact `(state, event, fault,
  recovery)` coordinates, so an endurance-class bug reproduces as a single
  `cargo test <name>` instead of an hours-long chaos run.
- The transparency oracle is strict on From/To/CSeq; a change to b-leg
  identifier handling trips it and is reviewed deliberately.
- Disruptive (partition / pre-replication-kill) coverage is not in the
  transparent matrix; it is a named follow-up.

## Considered and rejected

- Extending `b2bua-harness` in place: bloats its build and mixes HA cells with unrelated suites.
- Leaving `FailoverHarness` in `b2bua-harness`: splits one concern across two crates.
- One looping test: one failure masks the rest, no per-cell name.
- An `rstest` or `test-case` dependency: the workspace avoids it.
