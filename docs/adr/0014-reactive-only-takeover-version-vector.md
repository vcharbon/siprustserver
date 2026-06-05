# ADR-0014 — Reactive-only takeover + `(p,b)` version-vector reconciliation

**Status:** accepted (2026-06-05)
**Amends:** ADR-0011 Decision X11 — removes the *eager* (membership-driven) takeover
and the *`Deactivate` watermark handshake*; **keeps** reactive takeover; **replaces**
LWW-by-`gen` reconciliation with a per-context `(primary, backup)` version vector.
**Plan of record:** [docs/plan/passive-backup-reclaim-strategy.md](../plan/passive-backup-reclaim-strategy.md).

---

## Context

Endurance `kill_worker` flooded long-call failures (`unexpected_msg`) for ~one
keepalive interval. Root cause: the **eager takeover** (X11) proactively
materialised the *whole* `bak:{dead-primary}` partition on the survivor and re-armed
each quiescent dialog's *original* (past-due) keepalive — a population-wide wave of
stale-CSeq OPTIONS. Two further defects rode along: the `Deactivate` handback
completed as late as ~16 s (gated behind an `all_current` wait + a 6-tick burst),
and the single per-context `call_gen` LWW could not disambiguate concurrent
primary+backup mutations (a latent equal-`gen` divergence).

## Decision

1. **Remove eager takeover.** Quiescent failed-over dialogs are recovered by the
   rebooting primary's **reclaim** (smoothed, §Keepalive smoothing), or earlier if
   they get in-dialog traffic the LB reroutes to a survivor (**reactive takeover**,
   kept — SIP retransmission cannot bridge an outage; real endpoints give up well
   before Timer F).

2. **Remove the `Deactivate` handback** (wire frame tag 5 + the watermark handshake
   + the go-active burst). The acting-backup **self-releases**: when the **last
   in-flight transaction** for a taken-over call reaches a terminal state, it
   `drop_local`s the live copy (the `bak:` replica + the reverse-flushed deltas
   remain). Durability of the reverse-flush replaces the handshake.

3. **Reconcile by a per-context `(p, b)` version vector** — `p` = primary counter,
   `b` = backup counter; each node bumps **only its own** on a local mutation, so
   the *other* counter on an incoming update is, by construction, the branch point.
   Apply rules (direction resolved by the partition):
   - **Delete** → apply unconditionally (delete-wins, both directions).
   - **Forward** (primary → backup) Create/Update → apply **always** (follower
     defers to authority), except a `call_ref` the backup locally deleted.
   - **Reverse** (backup → primary) Create/Update → apply iff `p_in == p_cur && b_in > b_cur`
     (untouched-by-primary since the backup branched, and a genuinely newer backup
     mutation); else keep our own. No local copy → accept.
   - **Bootstrap** (a node recovering its own partition) → take the replica's
     `(p,b)` as-is (recovery, not a merge), skipping only a strictly-dominated copy.

   Correctness is **purely causal — no timers anywhere**. A partition can route to
   the backup at any time for any duration, so any time-based rule is unsound. This
   also closes the latent equal-`gen` divergence the single counter suffered.

4. **Keepalive smoothing is performance-only.** On reboot, `ReclaimAll`
   re-materialises the whole `pri:{self}` partition; many keepalives are past-due.
   The reclaim handler staggers them oldest-first — `fire_at = now + (L_max - L)/speedup`,
   bounded to `speedup`× cadence (`keepalive_catchup_speedup`, default 10; optional
   `max_catchup_window_sec`) — so a freshly-rehydrated node is not flooded. This is
   load management with **no** correctness role (`(p,b)` makes any incidental
   keepalive overlap non-corrupting), so there is **no settle/handback floor**.
   `fire_at` is pre-computed in the reclaim handler, never in the timer driver
   (CLAUDE.md timer invariants are untouched).

5. **LB transaction affinity is satisfied by statelessness.** The front proxy
   ([sip-proxy](../../crates/sip-proxy/)) stays **stateless**: it re-derives the
   target from the signed `w_pri/w_bak` cookie on every in-dialog request against a
   fresh registry snapshot — no per-dialog state. "Finish the in-flight transaction
   on the current target, switch only when it is dead" is met by the **ACK/CANCEL
   exemption** (an alive primary owns its in-flight UAS state) and the **fresh-pod
   guard** (a just-rebooted primary's first window routes in-dialog reqs to the
   backup that still holds the call), not by affinity state.

6. **Take slack.** UAC `recv OPTIONS` timeout 350 s → 420 s (300 s keepalive +
   120 s for kill+restart+rehydrate+smoothed-drain); `reboot_budget_sec` 450 → 600.

## Self-release mechanism (implementation)

The trigger is the transaction layer, push-based — the router never polls:

- The router, on a fresh reactive takeover (`hydrate_from_replica` → `mark_takeover`),
  arms `TransactionLayer::watch_self_release(call_ref)`.
- Server transactions are attributed to their call via the Request-URI `callRef`
  param (the dialog remote target = the B2BUA Contact); client transactions via the
  Via `cr` param. When the **last** transaction for a watched call leaves the map,
  the txn layer emits `TransactionEvent::CallQuiesced{call_ref}`.
- The router handles `CallQuiesced` by self-releasing (`drop_local` + cancel
  timers/txns + poison the dispatch queue + `bump_repl_self_release`), re-checking
  under the per-call lock (a fresh in-dialog request may have re-armed a transaction
  — a second takeover during a sustained partition continues to the next `b`).

"Last transaction left the map" — not merely "reached its final response" — is
deliberate: a **2xx INVITE** server transaction lingers in `Completed` until Timer H
(the 2xx ACK reuses a *different* branch, so it does not terminate the INVITE txn),
so self-release naturally fires at Timer H, **after** the ACK has been relayed. A
non-INVITE clears at Timer J; a failed leg at Timer B/F.

## Why this is safe where the old design was not

- The **storm** is gone: the backup never originates a probe and quiescent calls are
  recovered only by the (smoothed) reboot reclaim → no population-wide stale-CSeq wave.
- The **double-serve leak** ([[repl-reclaim-leak-x11]]) is structurally gone: the
  backup holds a copy only while actively serving and self-releases deterministically.
- The **settle-vs-handback timing problem** disappears: there is no handback burst
  and no settle; correctness rests on `(p,b)` causality.

## Accepted trade-offs

- **Quiescent call on a permanently-lost node** dies after the keepalive slack (no
  eager takeover to keep it probing). The deliberate price of killing the storm; a
  grace-period eager takeover for the quiescent remainder is a deferred safety net
  (plan §13) to re-add only if required.
- **Keepalive-vs-backup-transaction overlap** on the same leg: the primary's
  keepalive (the one thing it originates that the LB does not route) can race a
  backup transaction. `(p,b)` *detects* it (reject), but the backup's mutation
  already hit the wire → that one call may CSeq-regress and drop. Unavoidable
  without cross-node coordination; fails by cleanly dropping one call.

## Consequences for tests

The failover harness now asserts in **cluster vocabulary**, not HA internals: a node
*serves* a call, a backup *is synchronized* (holds a current replica it could take
over), *memory is clean* (no per-call state left), and *exactly one owner* /
*fully released* across the cluster (`failover_harness::{assert_single_owner,
assert_call_fully_released}`, `ReplicatedB2buaSut::{serves, is_synchronized_backup,
memory_clean, holds_any_trace}`). External behaviour (Alice/Bob: responses, CSeq
order, call survival, clean teardown) is unchanged by this refactor.
