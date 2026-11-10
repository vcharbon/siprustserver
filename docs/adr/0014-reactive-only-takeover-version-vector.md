# ADR-0014: Reactive-only takeover and `(p,b)` version-vector reconciliation

**Status:** accepted. Amends ADR-0011 X4, X9 and X11. Amended by ADR-0047 (its §1–§2),
ADR-0050 (its §5) and ADR-0055 (its §2–§4, §7, §9).
Plan: [plan of record](../plan/passive-backup-reclaim-strategy.md),
[task list](../plan/0014-rehydration-simplification-tasks.md). [Detail](appendix/0014-reactive-only-takeover-version-vector.md).

## Context

Eager takeover caused a stale-CSeq OPTIONS storm. One `call_gen` LWW could not order
primary and backup mutations. [Detail](appendix/0014-reactive-only-takeover-version-vector.md#context).

## Decision

1. **Remove eager takeover.** The rebooting primary's reclaim (smoothed, §4) recovers
   quiescent failed-over dialogs, or reactive takeover does earlier when the LB reroutes
   in-dialog traffic to a survivor. SIP retransmission cannot bridge an outage. A call with
   an INVITE in flight is taken over when its primary leaves the membership (ADR-0047).
2. **Remove the `Deactivate` handback.** The acting backup self-releases when the last
   in-flight transaction of a taken-over call ends. On a terminal it writes no CDR and
   propagates no delete, and releases the limiter key only when its primary reads dead at a
   verdict (ADR-0047 D8).
   It reverse-flushes the terminal with a grace TTL and drops its live copy; the
   primary is the sole discharge authority (§Terminal reconcile). Detail: [appendix §2](appendix/0014-reactive-only-takeover-version-vector.md#2-remove-the-deactivate-handback).
3. **Reconcile by a per-context `(p,b)` version vector.** `p` is the primary counter, `b`
   the backup counter. Each node bumps only its own, so the other counter on an incoming
   update is the branch point. The partition gives the direction.
   - **Delete:** apply unconditionally, both directions.
   - **Forward** (primary to backup) Create/Update: apply always, except a `call_ref` the
     backup locally deleted and a backup regression (refused, lifecycle progress folded,
     ADR-0031 D3).
   - **Reverse** (backup to primary) Create/Update: apply iff `p_in == p_cur && b_in > b_cur`,
     else keep our own. No local copy: accept.
   - **Bootstrap** (a node recovering its own partition): take the replica's `(p,b)` as-is,
     skipping only a strictly-dominated copy.

   Correctness is purely causal, with no timers. A partition can route to the backup at
   any time for any duration, so any time-based rule is unsound.
4. **Keepalive smoothing is performance-only.** The reclaim handler staggers the past-due
   keepalives of a reclaimed `pri:{self}` partition oldest-first
   (`keepalive_catchup_speedup`). It has no correctness role and no settle floor. Detail: [appendix §4](appendix/0014-reactive-only-takeover-version-vector.md#4-keepalive-smoothing-is-performance-only).
5. **LB transaction affinity comes from statelessness.** The front proxy keeps no
   per-dialog state. The ACK/CANCEL exemption and liveness routing (ADR-0050) keep an
   in-flight transaction on its target. Detail: [appendix §5](appendix/0014-reactive-only-takeover-version-vector.md#5-lb-transaction-affinity).
6. **Take slack.** UAC `recv OPTIONS` timeout 420 s (300 s keepalive, plus 120 s for kill,
   restart, rehydrate and smoothed drain). `reboot_budget_sec` 600.

## Stream topology: two flows, two sockets (amends ADR-0011 X4)

7. **Two streams, two sockets, distinct watermarks.** Each peer pair runs two single-flow
   sockets to distinct endpoints, with no multiplexing. From a node N's view:

   | Stream | N pulls | wire `partition` | store | timers | N's `(p,b)` role |
   |---|---|---|---|---|---|
   | **Reclaim** | calls **N is primary** for, a peer backed up | `Pri` | `pri:{N}` | **arm** | **primary** |
   | **Backup**  | calls a **peer is primary** for, N backs up | `Bak` | `bak:{peer}` | **none** | **backup** |

   Both are permanent: bootstrap, then tail. Detail: [appendix §7](appendix/0014-reactive-only-takeover-version-vector.md#7-two-streams).
8. **One changelog, two partition-filtered cursors; watermark ⟂ `(p,b)`.** Each flow's
   cursor advances only on its partition's frames. The watermark is a changelog position,
   never a call's `(p,b)`. Bootstrap is a store scan, then the tail. Detail: [appendix §8](appendix/0014-reactive-only-takeover-version-vector.md#8-one-changelog).
9. **Inbound apply is changelog-silent.** A received `Data` frame never appends to the
   local changelog, so it is never re-served to its sender. Detail: [appendix §9](appendix/0014-reactive-only-takeover-version-vector.md#9-inbound-apply-is-changelog-silent).
10. **Unified server send loop.** Both flows run one `sleep` poll (≤100 ms) parameterised
    by `(partition, caller)`. A `Noop` goes out only on the catch-up edge and on an idle
    interval (500 ms, ADR-0045). Detail: [appendix §10](appendix/0014-reactive-only-takeover-version-vector.md#10-unified-server-send-loop).
11. **Readiness is both flows' first Noop, Reclaim first.** `Ready` ⇔ every stream to a
    reachable ready peer hit its first Noop. A node reads ready only
    once it holds the calls its peers named it backup of. A hard timer bounds a peer never
    reached (ADR-0011 X5). Detail: [appendix §11](appendix/0014-reactive-only-takeover-version-vector.md#11-readiness).
12. **Wire message set.** `PullRequest` (C→S, bootstrap when `since==(0,0)`, else tail),
    `Data`, `Noop`, `ResetToBootstrap` (S→C) and `Position` (C→S, the drain's exit,
    ADR-0031 D2). Detail: [appendix §12](appendix/0014-reactive-only-takeover-version-vector.md#12-wire-message-set).

### Keepalive smoothing under the two-stream model

Smoothing (§4) applies whenever past-due keepalives are re-armed. A lone Reclaim tail
straggler arms at once.

## Self-release mechanism (implementation)

`CallQuiesced` fires when a takeover's last transaction leaves the map. [Detail](appendix/0014-reactive-only-takeover-version-vector.md#self-release-mechanism-implementation).

## Why this is safe where the old design was not

No probe storm, no double-serve. [Detail](appendix/0014-reactive-only-takeover-version-vector.md#why-this-is-safe-where-the-old-design-was-not).

## Accepted trade-offs

A quiescent call on a lost node dies after the slack. A keepalive racing a backup
transaction may drop one call. A call confirming at the kill instant may stay `Early`.
[Detail](appendix/0014-reactive-only-takeover-version-vector.md#accepted-trade-offs).

## Consequences for tests

The failover harness asserts in cluster vocabulary. [Detail](appendix/0014-reactive-only-takeover-version-vector.md#consequences-for-tests).

## Terminal reconcile, deferred discharge and resurrection guard

The primary alone discharges (ADR-0020 X3), and a delete is tombstoned. [Detail](appendix/0014-reactive-only-takeover-version-vector.md#terminal-reconcile-deferred-discharge-and-resurrection-guard).

## Amendment: skew re-anchoring at the replication boundary (accuracy only)

Restored instants are re-anchored by the sender's clock stamp. [Detail](appendix/0014-reactive-only-takeover-version-vector.md#amendment-skew-re-anchoring-at-the-replication-boundary-accuracy-only).

## Amendment: a materialisation re-seeds the INVITE transactions it is in the middle of

[Detail](appendix/0014-reactive-only-takeover-version-vector.md#amendment-a-materialisation-re-seeds-the-invite-transactions-it-is-in-the-middle-of).

## Amendment: lifecycle progress folds over a refused reverse flush

[Detail](appendix/0014-reactive-only-takeover-version-vector.md#amendment-lifecycle-progress-folds-over-a-refused-reverse-flush).

## Amendment: a counter counts writes that change the call, not progress

[Detail](appendix/0014-reactive-only-takeover-version-vector.md#amendment-a-counter-counts-writes-that-change-the-call-not-progress).

## Amendment: `(p,b)` orders one call incarnation

[Detail](appendix/0014-reactive-only-takeover-version-vector.md#amendment-pb-orders-one-call-incarnation).
