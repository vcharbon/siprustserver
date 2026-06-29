# ADR-0014 — Reactive-only takeover + `(p,b)` version-vector reconciliation

**Status:** accepted (2026-06-05)
**Amends:** ADR-0011 Decision X11 — removes the *eager* (membership-driven) takeover
and the *`Deactivate` watermark handshake*; **keeps** reactive takeover; **replaces**
LWW-by-`gen` reconciliation with a per-context `(primary, backup)` version vector.
**Also amends ADR-0011 X4/X9** — the re-hydration "single pull stream" and the
`Bootstrap`/`Replog` two-request handshake are **replaced** by two independent
single-flow sockets (§Stream topology below) with a trimmed wire message set.
**Plan of record:** [docs/plan/passive-backup-reclaim-strategy.md](../plan/passive-backup-reclaim-strategy.md).
**Implementation task list:** [docs/plan/0014-rehydration-simplification-tasks.md](../plan/0014-rehydration-simplification-tasks.md).

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
   `drop_local`s its live copy (the `bak:` replica + the reverse-flushed deltas
   remain). On a **terminal** (BYE/CANCEL) the backup **does not discharge** — no
   CDR, no limiter release, no propagated delete: it records the terminal into the
   `(p,b)` Element and reverse-flushes it with a short grace TTL (its alive-timer)
   before dropping the live copy; the **primary is the sole discharge authority**
   (§Terminal reconcile). Durability of the reverse-flush replaces the handshake.

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

## Stream topology — two flows, two sockets (amends ADR-0011 X4)

The single per-peer connection that multiplexed both partitions behind one
watermark (and the `Bootstrap`→`Replog` two-request dance on it) is gone. It
straddled the two directions behind one cursor — the source of the cold-Replog
watermark collision that capped re-hydration at ~203/3000 — and conflated the
*global* changelog position with the *per-call* `(p,b)`. From a node N's view:

7. **Two streams, two sockets, distinct watermarks.** Each peer pair runs two
   single-flow sockets to distinct endpoints (no multiplexing):

   | Stream | N pulls | wire `partition` | store | timers | N's `(p,b)` role |
   |---|---|---|---|---|---|
   | **Reclaim** | calls **N is primary** for, a peer backed up | `Pri` | `pri:{N}` | **arm** | **primary** |
   | **Backup**  | calls a **peer is primary** for, N backs up | `Bak` | `bak:{peer}` | **none** | **backup** |

   Both are **permanent**: Reclaim `= bootstrap(reclaim my calls) → tail(catch a
   post-partition reverse-flush a live-but-partitioned primary missed)`; Backup
   `= bootstrap(load the peer's calls I back up) → tail(forward updates)`. Same
   FSM shape, parameterised only by `{store partition, arm-timers?, role}`; one
   shared reconnect/recovery path on a cut.

8. **One changelog, two partition-filtered cursors; watermark ⟂ `(p,b)`.** A node
   keeps **one** changelog (one `(gen,counter)` space). Each flow holds its own
   cursor, advanced only by frames of its partition. The watermark is **purely**
   a changelog position — never read from or written to a call's `(p,b)`. Each
   flow's `bootstrap` is a **store scan** (mandatory: a created-then-quiescent
   call's changelog entry compacts away, so only the scan is a complete snapshot),
   capturing `W = head at scan-start`, then the partition-filtered tail.

9. **Inbound apply is changelog-silent (loopback prevention).** Applying a received
   `Data` frame mutates the store but **never appends to the local changelog**.
   Only a **locally-originated** mutation (N processing a call it serves — as
   primary, or as acting-backup during a takeover) appends. A change N received as
   a backup therefore has no local changelog entry and can never be re-served to
   the peer that sent it. The puller's *cursor* still advances (its job, private);
   the *changelog* does not. (`put_call(peer:None)` must honour this — invariant +
   test.)

10. **Unified server send loop** (both flows, parameterised by `(partition,caller)`):
    a **pure `sleep` poll** (the `Notify`/subscriber/`Subscription` machinery is
    **deleted** — one mechanism, less state; ≤100ms latency, tunable lower). Each
    tick drains its sub-log range `(since,..]` taking **`limit+1` (=501)**: while
    `>500` pending, stream `Data` back-to-back to fill the TCP buffer (no Noop, no
    sleep — each `Data` carries its own `at`, so the cursor rides the data); at
    `≤500`, flush the remainder and sleep. A **`Noop` is emitted only on the catch-up
    edge** (drained to head — the *first* such Noop sets the puller's sticky
    `current` flag = "mostly caught up") **and on a ~20s idle floor** (liveness). A
    trickle needs no Noop.

    The drain is cheap by design (it runs ~10 Hz × peers × 2 flows): the per-peer
    changelog splits into **per-partition compacted sub-logs** (one
    `counter→callRef` BTreeMap + `by_ref` + `retained_floor` each), so a flow drains
    only its partition with **no cross-partition filtering** and the old
    `(role,primary)`-regroup/`retain` dance is gone. `Op` is **`{Put, Delete}`**
    (Create/Update merged → idempotent `Put`); `bump` does an `O(log L)` move (drop
    old counter, insert at the max) that bounds the log to the live set; apply is
    idempotent under the `(p,b)` gate. `needs_reset` is per-`(peer,partition)`.

11. **Readiness = Reclaim-first-Noop only.** `Ready` ⇔ every **Reclaim** stream to a
    *reachable* peer hit its first Noop (hard-timer bounded — a dead peer cannot hang
    readiness). **Backup** streams are opened only *after* `Ready`, are
    fire-and-forget, and **never gate** readiness (observable via the store + a
    received-non-Noop-frame counter, not a readiness sub-state). A cold node with no
    calls reaches `Ready` at once (empty scan → immediate terminal Noop). The
    **server** side is independent of local readiness: a NotReady node still answers
    peers' pulls — it simply has nothing to hand over until it processes calls.

12. **Wire message set (trimmed).** Pre-production, so tags renumber freely + proto
    version bumps. Frames:

    | Frame | Dir | Fields | Role |
    |---|---|---|---|
    | **PullRequest** | C→S | `proto_ver, caller, partition, since` | Open one flow. Bootstrap is **implicit** when `since==(0,0)`; else tail. |
    | **Data** | S→C | `at, op, partition, call_ref, p, b, body_ttl_ms, indexes, body?` | One mutation. `op∈{Put,Delete}` — **Create and Update are merged into one idempotent `Put`** (carries a body); `Delete` carries none (delete-wins). |
    | **Noop** | S→C | `at` | Catch-up edge (first ⇒ ready) + 20s idle keepalive. |
    | **ResetToBootstrap** | S→C | `reason` | `since` fell below the compacted tail → re-bootstrap from `(0,0)`. |

    **Removed:** `Ack` (was a no-op retention hint — retention now bounded by
    time/size, a too-slow puller re-bootstraps via `ResetToBootstrap`); the
    `PullMode` enum + the `Bootstrap`/`Replog` two-request handshake (collapsed into
    the `since==(0,0)` rule); `Deactivate`/tag 5 (already retired — ensure no
    vestige). **Every removed message and its dead handler/branch MUST be gone from
    the codebase when the plan completes** (task list, §Removals).

### Keepalive smoothing under the two-stream model

Smoothing (§4) is **lifecycle-wide**, not bootstrap-special-cased: whenever timers
are re-created for past-due keepalives, stagger them oldest-first. The Reclaim
*bootstrap* is merely the largest instance (the whole `pri:{N}` partition at once);
a single Reclaim *tail* straggler arms immediately (one call is not a flood). It
remains **performance-only** — `(p,b)` makes any incidental keepalive overlap
non-corrupting, so there is no correctness dependence and no settle/handback floor.

## Self-release mechanism (implementation)

The trigger is the transaction layer, push-based — the router never polls:

- The router, on a fresh reactive takeover (`hydrate_from_replica` → `mark_takeover`),
  arms `TransactionLayer::watch_self_release(call_ref)`.
- Server transactions are attributed to their call via the Request-URI `callRef`
  param (the dialog remote target = the B2BUA Contact); client transactions via the
  Via `cr` param. When the **last** transaction for a watched call leaves the map,
  the txn layer emits `TransactionEvent::CallQuiesced{call_ref}`.
- The router handles `CallQuiesced` by self-releasing: `drop_local` + cancel
  timers/txns + poison the dispatch queue + `bump_repl_self_release`, re-checking
  under the per-call lock (a fresh in-dialog request may have re-armed a transaction
  — a second takeover during a sustained partition continues to the next `b`). If
  the served call reached a **terminal** state, the backup first reverse-flushes that
  terminal with a short grace TTL (its alive-timer) and **defers** the discharge to
  the primary rather than writing a CDR / releasing the limiter / propagating a
  delete itself.

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
- **Forked-b-leg confirm lost on the kill instant** (the "confirm-race"). When a
  b-leg forks (≥2 early dialogs) and the primary crashes in the *narrow window*
  between processing the winning `200 OK` (which collapses the fork to the winner
  in memory and queues the confirm flush) and that flush replicating, the backup
  retains the pre-confirm `Early` snapshot carrying *both* forks. The winner's
  identity is then unrecoverable from replicated state alone: the a-face tag is a
  single consolidated value and the `stored_a_tag → winner` mapping is created
  *at the 200* (force-tag-consistency), so it died with the un-flushed confirm;
  the early snapshot names only the first (losing) fork. A reclaimed/taken-over
  copy therefore stays `Early` and may emit a phantom `CANCEL` (or a CSeq-desync'd
  BYE) at teardown for that one call. The stranded `Early` call is bounded — the
  SetupTimeout reaper sweeps it (~150 s); it is not a leak.

  **What is accepted, and the LIMIT of it.** Accepted: *only a call whose dialog
  state was changing within the kill window* — concretely, a call **confirming at
  the instant of the kill** (its confirm and the crash fall inside the ~ms flush
  window). This is the deliberate price of causal-only, flush-on-confirm
  replication (no synchronous confirm-replicate on the call-setup hot path).

  **NOT accepted — the protected set, which this trade-off must never touch:**
  - **Established calls** — any call confirmed more than the acceptance window
    (default 200 ms) before the kill has already flushed its collapsed `Confirmed`
    state, so it reclaims clean. A confirmed call dropping on a kill is a **bug**,
    not this trade-off.
  - **Ringing calls** — a still-`Early` (not-yet-confirming) call has no winner to
    lose; it must survive the failover or fail gracefully as an ordinary early
    dialog. Breaking one is a **bug**, not this trade-off.
  - Any failure whose dialog-state change was **outside** the acceptance window
    (e.g. a brand-new call created seconds after the reboot that desyncs) is a
    **distinct, genuine bug** — this acceptance does NOT cover it.

  We do **not** fix the confirm-race (the only real fix — reactive takeover +
  re-confirm on a b-leg `2xx` *response*, since the backup currently takes over
  only on a *request* — is disproportionate to a single call confirming exactly on
  the kill). Instead it is **classified, not masked**: see *Consequences for tests*.

## Consequences for tests

The failover harness now asserts in **cluster vocabulary**, not HA internals: a node
*serves* a call, a backup *is synchronized* (holds a current replica it could take
over), *memory is clean* (no per-call state left), and *exactly one owner* /
*fully released* across the cluster (`failover_harness::{assert_single_owner,
assert_call_fully_released}`, `ReplicatedB2buaSut::{serves, is_synchronized_backup,
memory_clean, holds_any_trace}`). External behaviour (Alice/Bob: responses, CSeq
order, call survival, clean teardown) is unchanged by this refactor.

**Chaos-aware classification of the confirm-race (the accepted trade-off above).**
The endurance load generator (`crates/loadgen`) is *chaos-aware*: the chaos driver
flags it at each fault instant (`POST /chaos`), and every call carries dialog-state
**phase markers** (`connected`, `reinvited`, `transferred`, …). A failed call is
auto-bucketed `chaos="near"` (accepted) **iff** a dialog-state transition fell
within the **acceptance window** of a fault — or the call was still mid-setup at
the fault — exactly encoding the accepted-vs-protected boundary above. The window
is a single knob, `--chaos-phase-tolerance-ms` (default **200 ms**,
`LOADGEN_CHAOS_PHASE_TOL_MS` in endurance). A **stably-established** call that
fails far from any fault stays `chaos="clear"` — the protected set. Only
timing-independent classes (`Panic`, `Unparseable`) are never excused. So the
confirm-race is **counted but not triaged** (`class=rfc_audit_fail,chaos="near"`),
while a genuine bug — an established call dropping, or a post-reboot fresh call
desyncing far from the kill — surfaces in `chaos="clear"` and must be investigated.
This is classification, never suppression: the `near` bucket is fully retained and
queryable.

## Terminal reconcile, deferred discharge & resurrection guard

Terminal handling is **purely causal — no reconciliation timers** — and rests on
three rules (the primary is the sole discharge authority; see ADR-0020 X3):

1. **The Reclaim-tail reconciles into the LIVE map, not just the replica store.**
   The tail catches a post-partition reverse-flush a live-but-partitioned primary
   missed, and folds a dominating reverse-flush (Reverse `(p,b)` rule) into the
   primary's **live** copy: a non-terminal `Put` updates it (notably the bumped
   b-leg `local_cseq`, so the primary's next request to the peer stays monotonic —
   the timely-flush alternative to the §"accepted trade-off" CSeq drop); a
   `Terminated` `Put` drives a discharge through the reaper funnel.

2. **The acting-backup defers the terminal.** A takeover copy reaching terminal
   reverse-flushes it (short grace TTL = its alive-timer) and `drop_local`s — it
   does **not** discharge. The primary is the sole discharge authority; if it never
   returns, the backup's alive-timer fallback discharges (durability rests on
   *primary OR backup* restarting).

3. **`flush` carries the authoritative `(p,b)`; a delete is tombstoned.** `flush`
   embeds the authoritative `(p,b)` in the replicated **body** (encoding the
   pre-bump clone would branch a backup one `p` stale — invisible vs a crashed
   primary, but it silently breaks every reverse-flush to an **alive** primary).
   And because the Reverse `(p,b)` rule structurally cannot let a backup's
   discharged state apply to a primary that has bumped `p`, a deleted `call_ref` is
   **tombstoned** (`ReplicatingCallStore`): `put_call` rejects a re-creating `Put`
   within the window (apply-side delete-wins), so a late reverse-flush cannot
   resurrect a just-discharged call.
