# Plan — Reactive-only takeover + version-vector reconciliation (remove eager takeover & `Deactivate` handback)

**Status:** DRAFT — final design, code-validated (no code changed yet)
**Date:** 2026-06-05
**Branch:** `feat/sip-message-layer`
**Amends:** ADR-0011 Decision X11 (eager takeover + `Deactivate` handback) — removes the
*eager* takeover and the *watermark `Deactivate` handshake*; **keeps** reactive takeover;
**replaces** LWW-by-gen with a per-context `(p,b)` version vector.

---

## 0. TL;DR

Endurance `kill_worker` floods long-call failures (`unexpected_msg`) for ~one full
keepalive interval. Root cause: the **eager takeover** proactively probes the *whole*
quiescent partition with stale-CSeq keepalives. The fix is a strategy simplification,
not a patch:

- **Remove eager takeover** (proactive, membership-driven, the storm source).
- **Keep reactive takeover** (the backup serves an in-dialog request the LB reroutes to
  it during an outage/partition — required: SIP retransmission can't bridge the gap,
  real endpoints give up well before Timer F).
- **Remove the `Deactivate` handback entirely** (the watermark handshake + the go-active
  burst). The backup **self-releases** on transaction completion; durability of the
  reverse-flush replaces the handshake.
- **Reconcile by a per-context `(primary_counter, backup_counter)` version vector** with
  an asymmetric merge rule + unconditional bidirectional deletes — fixes a latent
  equal-gen divergence and removes all timing dependence from correctness.
- **Keepalive smoothing** (oldest-first 10× replay) is a **performance** anti-herd only;
  **no time-based settle** anywhere.
- **Take slack**: widen the UAC keepalive tolerance to 120 s for kill+restart+rehydrate.

Two things we worried about are **already handled by the existing code** (verified §6):
successive-takeover replica coherence, and propagation loop-freedom. The one genuinely
new subsystem is the `(p,b)` vector.

---

## 1. The model (converged)

**Load balancer (front proxy).** Transaction affinity: do **not** switch a call's target
mid-transaction on **switchback** (target healthy again) — finish the in-flight
transaction on the backup, then route new transactions to the reclaimed primary. (Switch
mid-transaction only when the target is *dead* — that's the original failover.)

**Backup (follower).** No per-call timers; never originates a keepalive. Purely reactive:
- On an LB-rerouted in-dialog request, `hydrate_from_replica` materialises a live copy and
  serves it (reactive takeover), mutating its **own** `bak:{primary}` replica in place and
  reverse-flushing each mutation to the primary.
- On the **last in-flight transaction** for a taken-over call reaching a **terminal state**
  (completed *or* failed/timed-out via SIP Timer B/F), **drop the live copy** (`drop_local`,
  live-only) — the `bak:` replica and the reverse-flushed deltas remain. No `Deactivate`,
  no waiting for the primary.
- Applies the primary's forward-replicated updates **always**, except a call it has
  **locally deleted** (tombstone — don't resurrect).

**Primary (authority).** Continuous owner after reclaim; originates all keepalives.
- On reboot: bootstrap-rehydrate → `ReclaimAll` → re-arm timers, keepalives via the
  smoothing schedule (§4). No handback burst.
- Applies a backup's reverse-flushed update **iff it has not itself mutated the context
  since the backup branched** (`p_in == p_cur`); otherwise keeps its own. **Deletes apply
  unconditionally.** Never resurrects a locally-deleted call.

**Reconciliation — the `(p,b)` version vector (§5).** Per context, two role-owned
monotonic counters. Each node bumps only its own; the *other* node's counter in an
incoming update is, by construction, the branch point. Correctness is **purely causal —
no timers anywhere** (a partition can route to the backup at any time, for any duration,
so any time-based rule is unsound).

---

## 2. Why this is safe where the old design was not

- The **storm** came from eager takeover restoring the *whole* quiescent partition's
  original (past-due) keepalive deadlines and probing on stale CSeq. With eager gone,
  quiescent calls are recovered only by the rebooting primary's reclaim (smoothed), and
  the backup never originates a probe → no population-wide stale-CSeq wave.
- The **double-serve leak** (`[[repl-reclaim-leak-x11]]`) is structurally gone: the backup
  holds a copy only while actively serving, and self-releases deterministically on
  transaction completion. There is no quiescent takeover copy to strand.
- The **SETTLE-vs-handback timing problem** (we measured handback completing as late as
  ~16 s, gated behind the `all_current` wait + 6-tick burst) **disappears**: there is no
  handback burst to wait for, and no settle. Correctness rests on `(p,b)` causality.

**Residual corner case (accepted):** the primary's keepalive (the one thing it originates
that isn't LB-routed) can race a backup transaction on the same leg. `(p,b)` *detects* it
(reject), but the backup's mutation already hit the wire → that one call may CSeq-regress
and drop. Unavoidable without cross-node coordination; fails by cleanly dropping one call.

---

## 3. "Take slack" — 120 s for kill + restart + re-hydrate + drain

Today: keepalive 300 s, UAC `recv OPTIONS timeout` **350 s** → **50 s** slack
([uac-long-options.xml:106](../../deploy/k8s/sipp/scenarios/uac-long-options.xml#L106)).

- **Change** the UAC `recv OPTIONS` timeout `350000 → 420000` (300 s + 120 s); update the
  "50 s headroom" header note ([uac-long-options.xml:34-38](../../deploy/k8s/sipp/scenarios/uac-long-options.xml#L34)).
- Audit `uac-hold.xml`, `uac-hold-failover.xml`, and any `pause`-based holds for the same
  constant.
- B2BUA config: `keepalive_interval_sec=300` unchanged; `reboot_budget_sec=450` (backup
  replica TTL) ≥ 300 + 120, OK — consider 600 for margin
  ([config.rs:83-84](../../crates/b2bua/src/config.rs#L83)).

This is the budget for reboot+rehydrate+smoothed drain before a quiescent call's UAC
times out — the worst-positioned quiescent call survives iff
`reboot + smoothing_drain ≤ 120 s`.

---

## 4. Keepalive smoothing — PERFORMANCE only (replaces the 300 s deferral)

On reboot, `ReclaimAll` re-materialises the whole `pri:{self}` partition; many keepalive
timers are past-due. Firing them all at once floods a freshly-rehydrated node. Smooth the
backlog; **this is purely load management and never a correctness mechanism** (so it has
no timing assumption to violate).

### Algorithm (batch, in the `ReclaimAll` handler, [router.rs:128-131](../../crates/b2bua/src/router.rs#L128))

```text
SPEEDUP = 10                          // keepalive_catchup_speedup (config)
now     = clock.now_ms()
calls   = reclaim_scan()              // whole pri:{self} partition
L_max   = max(0, max over calls of (now - kp.fire_at) for past-due keepalives)

for call in calls:
    for kp in call.timers where timer_type == Keepalive:
        L = now - kp.fire_at
        if L > 0:                            kp.fire_at = now + (L_max - L) / SPEEDUP
        // else (future): leave absolute
    materialize_if_absent(call); restore(call.timers)
```

- Oldest-first (most at-risk of UAC timeout fires first), drains over `L_max/SPEEDUP`,
  bounded to 10× the normal cadence. After the burst each call re-arms `+300 s`, naturally
  re-spreading load.
- **`ReclaimCall`** (single reactive straggler): fire immediately, no `L_max` batch.
- Optional cap `max_catchup_window_sec` for a pathological `L_max`.
- **No `SETTLE` floor.** The deferral existed only to avoid double-probing during the
  handback window; with no handback and `(p,b)` making any incidental overlap
  *non-corrupting-of-stored-state*, the floor is removed.
- **Lives in the reclaim handler, not `timers.rs`** — pre-computed `fire_at`, zero changes
  to the epoch/`Key` driver, no risk to the CLAUDE.md timer invariants.
- **Config:** add `keepalive_catchup_speedup` (10) and optional `max_catchup_window_sec`
  to `B2buaConfig`. Remove any `keepalive_reclaim_settle_sec` idea — not used.
- **`reclaim_into_live` ([router.rs:205-230](../../crates/b2bua/src/router.rs#L205)):** delete
  the fresh-300 s deferral loop (lines 220-224); keepalive `fire_at` is set by the batch
  smoothing; non-keepalive timers keep absolute deadlines. Rewrite the doc comment.

---

## 5. Reconciliation — the `(p,b)` per-context version vector

### 5.1 Semantics

Each context carries `(p, b)` = `(primary_counter, backup_counter)`. **Each node bumps only
its own counter** on a local mutation. Because the backup never touches `p`, the `p` value
it sends back is the branch point ("the primary version I started from"); symmetrically for
`b`.

**Apply rules** (direction = Forward pri→bak, or Reverse bak→pri, already resolved by
`store_target`/`replication_target`, [replication.rs:69-82](../../crates/b2bua/src/repl/replication.rs#L69)):

- **Reverse (backup → primary), the primary applies:**
  - `delete` → apply unconditionally.
  - else apply iff `p_in == p_cur` **and** `b_in > b_cur` (untouched-by-primary, and a
    genuinely newer backup mutation); else ignore (primary keeps its own).
  - never resurrect a `call_ref` the primary has tombstoned.
- **Forward (primary → backup), the backup applies:**
  - `delete` → apply unconditionally.
  - else apply **always** (follower defers to authority), except a `call_ref` the backup
    has locally deleted (tombstone) → ignore.
- **Bootstrap re-hydration** (either node recovering its own partition): take the replica's
  `(p,b)` as-is (recovery, not a merge).

Worked example: nominal `(0,0)→(10,0)`; backup serves a message → `(10,1)`; primary
rehydrates `(10,1)`, mutates → `(11,1)`; a later backup push `(10,2)` has `p_in=10 <
p_cur=11` → **reject (unless delete)**. ✓

### 5.2 Why a single counter is insufficient (verified — fixes a latent bug)

Today there is **one** per-context counter, `call_gen` = `CallTopology.gen`, bumped by
*whoever* mutates, with LWW "highest gen wins" ([puller.rs:707-708](../../crates/b2bua/src/repl/puller.rs#L707),
[frame.rs:214](../../crates/repl-net/src/frame.rs#L214)). It cannot disambiguate concurrent
primary+backup mutations:

- **Equal-gen divergence (latent split-brain):** both mutate from gen 11 → both reach 12;
  apply is "skip if `stored >= frame`," so each keeps its own → permanent divergence,
  same gen, different bodies. Untested (s8 takeovers are sequential).
- **Asymmetric counts (wrong winner):** primary →12, backup →13; `13 > 12` → backup wins,
  discarding the primary's mutation — the opposite of the authority rule.

`(p,b)` resolves both. So adopting it is not just for the new model — it **closes an
existing latent divergence**.

### 5.3 Implementation scope (split one field into two + new apply rule)

`call_gen` already gives us: a per-context version, carried in the frame, the changelog
`RefMeta`/`CallMeta`, and the serialised body; monotonic; restored on bootstrap. We split
it into the role-scoped pair and change the comparison:

| Touchpoint | File:line | Change |
|---|---|---|
| Wire frame | [frame.rs:199-220](../../crates/repl-net/src/frame.rs#L199) | `call_gen: i64` → `cv: (p:u64, b:u64)` (or add `b`, keep `call_gen` as `p`) |
| Codec (positional msgpack) | [codec.rs](../../crates/repl-net/src/codec.rs) | add the field position |
| Changelog meta | [changelog.rs:48-56](../../crates/b2bua/src/repl/changelog.rs#L48) | `RefMeta.call_gen` → `(p,b)` |
| Store meta | [store.rs:45-53,116-126](../../crates/b2bua/src/repl/store.rs#L45) | `CallMeta` + `current_call_gen` → `current_cv` |
| Body version | `CallTopology.gen` (call model) | split into `(p,b)`; each node bumps its own on mutation |
| Apply rule | [puller.rs:689-753](../../crates/b2bua/src/repl/puller.rs#L689) | replace LWW-by-gen with §5.1 direction-aware rule + delete-wins + tombstone-suppress |
| Write side | `put_call`/`store_target` ([store/mod.rs:231-243,436-469](../../crates/b2bua/src/store/mod.rs#L231)) | bump the local role's counter on local mutation |

Per-context, role-owned, monotonic, restored on bootstrap — all properties the existing
`call_gen` satisfies, so this is a focused refactor, not a new subsystem.

---

## 6. Code-verified: two concerns already handled (no work needed)

1. **Successive-takeover replica coherence.** The backup writes its takeover mutation into
   its **own** `bak:{primary}` replica in place ([store/mod.rs:459-468](../../crates/b2bua/src/store/mod.rs#L459));
   `hydrate_from_replica` leaves the replica in place ([store/mod.rs:158-185](../../crates/b2bua/src/store/mod.rs#L158));
   `drop_local` sheds **only** the live copy ([store/mod.rs:281-292](../../crates/b2bua/src/store/mod.rs#L281)).
   A second takeover reads the mutated replica → monotonic, no collision (proven by
   `convergence_after_takeover_and_reboot_highest_gen`, [s8_tests.rs:429-470](../../crates/b2bua/src/repl/s8_tests.rs#L429)).
   The new model's self-release must keep using `drop_local` (live-only) — **it already
   does the right thing.**
2. **Loop-freedom.** `flush` fires **only on local mutation** ([router.rs:698,712](../../crates/b2bua/src/router.rs#L698));
   the puller apply path is terminal — `put_call(..., PutOpts::default())` with `peer:
   None`, and the changelog only bumps when `peer` is `Some` ([puller.rs:722](../../crates/b2bua/src/repl/puller.rs#L722),
   [store.rs:272-275](../../crates/b2bua/src/repl/store.rs#L272)). Applying a remote update
   propagates nothing onward. The `(p,b)` change does not touch where flush fires → no new
   loop risk.

---

## 7. Code deletions

### 7.1 Eager takeover

| Symbol | File:line |
|---|---|
| `ReplCommand::TakeOverPeer` variant + `on_repl_command` arm | [router.rs:81,141-142](../../crates/b2bua/src/router.rs#L81) |
| `take_over_peer()` (+ 34-line doc) | [router.rs:147-196](../../crates/b2bua/src/router.rs#L147) |
| supervisor eager-emission block (keep Park / Removed-then-Added) | [supervisor.rs:303-314](../../crates/b2bua/src/repl/supervisor.rs#L303) |
| `reclaim_backup_scan()` (only caller was `take_over_peer`) | [store/mod.rs:377-398](../../crates/b2bua/src/store/mod.rs#L377) |
| `bump_repl_eager_takeover` + help | [metrics.rs:142,243](../../crates/b2bua/src/metrics.rs#L142) |

### 7.2 `Deactivate` handback (watermark handshake)

| Symbol | File:line |
|---|---|
| `Frame::Deactivate` wire variant + codec | [frame.rs](../../crates/repl-net/src/frame.rs), [codec.rs](../../crates/repl-net/src/codec.rs) |
| `send_handback()` + serve-loop `Deactivate` watch arm | [server.rs:355-406](../../crates/b2bua/src/repl/server.rs#L355) |
| puller `Frame::Deactivate` handler | [puller.rs:568-580](../../crates/b2bua/src/repl/puller.rs#L568) |
| `ReplCommand::Deactivate` arm + `deactivate_takeovers()` | [router.rs:138-139,232-265](../../crates/b2bua/src/router.rs#L138) |
| `deactivate_targets()` + its `position_of` use | [store/mod.rs:324-358](../../crates/b2bua/src/store/mod.rs#L324) |
| go-active handback: `handback_tx/rx` watch, `with_handback`, `watermark_src`, 6-tick loop | [b2bua_core.rs:170-209,257-260](../../crates/b2bua/src/b2bua_core.rs#L170) |
| `bump_repl_handback` + help | [metrics.rs:141,242](../../crates/b2bua/src/metrics.rs#L141) |

`position_of` ([changelog.rs:206-222](../../crates/b2bua/src/repl/changelog.rs#L206)) is
general — keep the fn, delete only the handback caller. After deletion the go-active task
is `all_bootstrapped → ReclaimAll → all_current → ReclaimAll`.

---

## 8. Code modifications & new wiring

1. **Backup self-release on transaction terminal state** (replaces the `Deactivate`-driven
   `deactivate_takeovers`). Add a per-call in-flight-transaction refcount for taken-over
   calls (`mark_takeover` set, [store/mod.rs:316-322,546](../../crates/b2bua/src/store/mod.rs#L316)
   reactive site); when the **last** in-flight transaction for a marked call reaches a
   terminal state (success or Timer B/F timeout) in the txn layer, `drop_local` the live
   copy (the `bak:` replica + reverse-flushed deltas remain). `mark_takeover` is repurposed
   from "deactivate_targets filter" to "drives self-release."
2. **`(p,b)` vector + asymmetric apply rule + bidirectional delete-wins** (§5.3).
3. **Keepalive smoothing + de-deferral** in `ReclaimAll`/`reclaim_into_live` (§4).
4. **LB transaction affinity** in the front proxy: do not re-target a call mid-transaction
   on switchback. (Verify current proxy routing — §11.)
5. **Fix B — guard `generate_ack_for_2xx`** ([generators.rs:578](../../crates/sip-message/src/generators.rs#L578),
   guard absent; mirror [generators.rs:489](../../crates/sip-message/src/generators.rs#L489)):
   ```rust
   let to_value = if dialog.remote_tag.is_empty() {
       wrap_uri(&dialog.remote_uri)
   } else { format!("{};tag={}", wrap_uri(&dialog.remote_uri), dialog.remote_tag) };
   ```
   Reachable via reactive mid-confirm takeover → relayed-2xx ACK ([relay.rs:374](../../crates/b2bua/src/rules/relay.rs#L374)).
6. **Fix D — retire `backup_held`**, use `meta_backup` (resident count) in dashboards/alerts
   ([store.rs:178-213](../../crates/b2bua/src/repl/store.rs#L178), [puller.rs:749](../../crates/b2bua/src/repl/puller.rs#L749)).
7. **Commit the reap-driver** (working-tree `M`, [main.rs:~506](../../crates/b2bua-runner/src/main.rs#L506)).

### Explicitly KEEP

`hydrate_from_replica` + reactive `mark_takeover`/`restore` ([router.rs:524-548](../../crates/b2bua/src/router.rs#L524));
the reclaim path (`reclaim_scan`, `peek_reclaimable`, `materialize_if_absent`,
`reclaim_into_live`, `bump_repl_reclaimed`); the reactive `ReclaimCall` emission
([puller.rs:674](../../crates/b2bua/src/repl/puller.rs#L674)); `drop_local` (now the
self-release shed); forward/reverse flush + apply-terminal loop-freedom; the backup
writing its takeover mutation into its own `bak:` replica.

---

## 9. Pending residuals — disposition

| Fix | Disposition |
|---|---|
| **A** stale-CSeq eager keepalive | **SUPERSEDED** — eager removed; smoothing + `(p,b)` reclaim. |
| **B** `generate_ack_for_2xx` panic | **KEEP** (§8.5) — reactive mid-confirm. |
| **C** dead-peer changelog bloat | **ELIMINATED** — the unbounded source was the *eager* reverse-flush of quiescent calls ([router.rs:192](../../crates/b2bua/src/router.rs#L192)); reactive reverse-flush is bounded/expected. |
| **D** `backup_held` gauge lies | **KEEP** (§8.6) — retire for `meta_backup`. |
| reap-driver | **KEEP** (§8.7) — commit. |

---

## 10. Documentation updates (required)

### ADRs
- **ADR-0014 (new) "Reactive-only takeover + `(p,b)` reconciliation."** Record: the storm
  root-cause; remove eager + `Deactivate`; keep reactive; the `(p,b)` vector with the
  asymmetric + delete-wins rules (and that it fixes the latent equal-gen divergence);
  self-release on transaction completion; smoothing-as-performance; no settle / no timing
  in correctness; the 120 s slack; the LB transaction-affinity requirement; the accepted
  keepalive-vs-takeover corner case. **Amends** ADR-0011 X11.
- **ADR-0011** X11 section ([0011…:186-280](../../docs/adr/0011-ha-replication-peer-to-peer.md)):
  add "Amended by ADR-0014 — eager takeover and the `Deactivate` watermark handshake
  removed; reactive takeover retained; reconciliation now `(p,b)`." Keep the reactive text.
- **ADR-0013** ([0013…:6-60](../../docs/adr/0013-failover-test-matrix.md)): fix the two
  2026-06-05 amendments naming "eager-takeover keepalive" / "eager `TakeOverPeer`" /
  "three interleaving timers" — now reactive-only; quiescent recovery is reboot-reclaim.

### CONTEXT.md (glossary, lines 82-156)
- **Remove/retire:** `Deactivate`, watermark `as_of` handshake, ghost-backup framing, and
  any "eager / membership-driven takeover" sense.
- **Re-scope:** *Takeover copy* → reactive-only (LB-rerouted request), self-released on
  transaction completion. *Reclaim* → the sole quiescent-recovery path. *Handback* →
  redefine as "backup self-release + primary `(p,b)` accept" (no signal).
- **Add:** *Version vector `(p,b)`* (per-context, role-owned counters), *authority/follower
  merge rule*, *tombstone-wins*, *branch point*.

### CLAUDE.md (lines 20-79)
- **KEEP** the timer-driver epoch/`Key` + clock/transit hazards (strategy-agnostic).
- **Add:** (a) keepalive catch-up pre-computes staggered `fire_at` *in the reclaim handler*
  (perf only, never move into the driver); (b) reconciliation is `(p,b)`-causal — **no
  time-based settle/handback anywhere**; a partition can route to the backup at any time.

### Code comments
- Rewrite: `reclaim_into_live` (deferral → smoothing-perf), the `ReplCommand` enum doc
  (drop `TakeOverPeer`/`Deactivate`), supervisor `reconcile_from_snapshot` (drop eager
  block), `store::meta_counts` / `meta_backup` (drop ghost-backup), the puller `repl_tx`
  doc (drop handback).
- Delete: all `Deactivate`/handback/`deactivate_*` comments.
- Add: a `(p,b)` apply-rule doc block at the merge site.
- Failover-harness ([runner.rs](../../crates/failover-harness/src/runner.rs),
  [harness.rs](../../crates/failover-harness/src/harness.rs)): drop
  `TakeOverPeer`/`simulate_peer_removed → eager` and "Deactivate handback broadcast"
  references; keep reclaim + reactive ones.

### docs/plan
- `failover-test-matrix-cells.md` / `on-proper-migration-of-lazy-pancake.md`: remove the
  eager-takeover cells and the `Deactivate` wire spec; document the `(p,b)` reconciliation
  and reactive self-release instead.

### Memory (`~/.claude/projects/.../memory/`)
- `repl-reclaim-leak-x11.md`: note the leak class is now structurally gone (no quiescent
  takeover copy) and the `Deactivate` handshake was removed.
- `repl-takeover-longcall-loss.md`: append the storm root-cause + this plan.
- `repl-reboot-reclaim-bootstrap-truncation.md`: **KEEP** — reclaim is now the sole
  quiescent-recovery path, so bootstrap correctness matters more.
- **New memory:** the reactive-only + `(p,b)` strategy and this plan. Update MEMORY.md hooks.

---

## 11. Test plan

**Delete** (removed behaviour):
- `eager_takeover_keeps_quiescent_dialog_alive_and_hands_back_once` ([failover.rs:606](../../crates/failover-harness/tests/failover.rs#L606)).
- `deactivate_targets_by_primary_and_pull_watermark` ([s11_tests.rs:92](../../crates/b2bua/src/repl/s11_tests.rs#L92)).
- `simulate_peer_removed`-driven eager assertions; `transparent_v1.rs` eager cells.

**Rewrite** (re-point eager → reactive/reboot; replace handback with self-release):
- `cseq_stays_in_order_across_eager_takeover_and_reclaim` ([failover.rs:752](../../crates/failover-harness/tests/failover.rs#L752))
  → drive takeover **reactively**; keep the RFC CSeq audit across reclaim + self-release.
- `reboot_reclaim_hands_back_exactly_one_owner` ([failover.rs:490](../../crates/failover-harness/tests/failover.rs#L490))
  → "exactly one owner after reclaim + backup self-release" (no `Deactivate`).
- `acting_backup_terminate_leaves_no_expired_context_for_reclaim` ([failover.rs:880](../../crates/failover-harness/tests/failover.rs#L880))
  → self-release on BYE terminal state.

**Keep** (reactive/reclaim unchanged): `drop_local_sheds_live_copy_but_keeps_backup_element`
([s11_tests.rs:126](../../crates/b2bua/src/repl/s11_tests.rs#L126), now the self-release
shed); `canonical_failover`; the `matrix_*` cells; `limiter_ha`;
`reclaim_scan_materialises_pri_partition_idempotently`. **Update for `(p,b)`:** the s8
convergence/LWW tests (`takeover_then_reclaim_keeps_backup_mutation`,
`convergence_after_takeover_and_reboot_highest_gen`) — assertions move from "highest gen
wins" to the `(p,b)` rule.

**Add**:
- **`(p,b)` reconciliation:** (a) concurrent primary+backup mutation from the same base →
  primary wins, backup converges (the equal-gen divergence regression); (b) asymmetric
  counts (backup mutates twice, primary once) → primary still wins; (c) backup update with
  `p` unchanged → accepted; (d) **delete-wins bidirectional** (backup BYE beats primary
  update; primary tombstone suppresses a late backup update — no resurrection).
- **Self-release:** backup serves an in-dialog transaction, drops the live copy on the
  last transaction terminal state, keeps the `bak:` replica; a **second** takeover during a
  sustained partition continues to the next `b` (no collision).
- **Smoothing (perf):** staggered past-due keepalives → OPTIONS oldest-first, burst
  `≈ L_max/10`, all within the 120 s budget; none deferred a full interval.
- **Fix B:** `generate_ack_for_2xx` empty `remote_tag` → tag-less `To`, no panic.
- **Fix D:** `backup_held`/`meta_backup` == resident Backup bodies after a `reap` wave.

> Harness caveat (prior analysis): the single-call functional harness still can't reproduce
> the production stale-CSeq race or the population-scale wave — endurance-gated (tracked:
> `stall_repl_to` fault primitive).

---

## 12. Sequencing

1. **Commit the reap-driver** (§8.7) standalone.
2. **Take slack** (§3): widen UAC timeouts; audit hold scenarios.
3. **`(p,b)` version vector** (§5) + apply-rule tests — land first; it's the correctness core
   and fixes the latent divergence independently of the rest.
4. **Smoothing + de-deferral** (§4) + tests.
5. **Backup self-release on transaction terminal state** (§8.1) + tests.
6. **Delete eager takeover** (§7.1) and **`Deactivate` handback** (§7.2); delete/rewrite
   tests (§11).
7. **LB transaction affinity** (§8.4) — after confirming current proxy behaviour (§13).
8. **Fix B** (§8.5) + **Fix D** (§8.6).
9. **Docs** (§10): ADR-0014, ADR-0011/0013 amendments, CONTEXT.md, CLAUDE.md, comments,
   memory.
10. `cargo test` workspace; `code-review`.
11. **User-gated endurance** (`run.sh up` rebuilds; `KEEP=1 ./endurance.sh run`).

**Endurance pass signals:** `kill_worker` long-call failures stay ~flat (no
keepalive-interval-wide plateau); post-reboot OPTIONS burst visibly *smoothed*; exactly one
owner after reclaim + self-release; survivor RSS flat (no eager reverse-flush growth);
`meta_backup` ≈ live backup count; zero `Empty To tag` panics; no divergence under
concurrent switchback traffic.

---

## 13. Risks / open items

- **Quiescent calls during *permanent* node loss.** Reactive takeover saves calls that get
  traffic during a permanent outage; a **quiescent** long call on a worker that never
  returns now dies after keepalive slack (the deliberate trade for killing the storm).
  *Safety net (deferred):* grace-period eager takeover for the quiescent remainder only —
  on peer-`Removed`, arm a presumed-dead timer; if the peer returns within grace, do
  nothing; if it expires, eagerly reclaim the quiescent residue. Budget
  `reboot_p99 < grace < keepalive_slack`; the 120 s slack makes the window fit. Re-add only
  if required.
- **Accepted corner case:** keepalive-vs-backup-transaction overlap → one call may
  CSeq-regress and drop (detected by `(p,b)`, not corrupting of stored state).
- **LB transaction affinity (§8.4)** — **confirm the current front proxy actually keeps a
  transaction on one target across switchback** before relying on it; this is the one
  external precondition the model needs. (Verify [sip-proxy](../../crates/sip-proxy/) routing.)
- **Bootstrap re-hydration is the sole quiescent-recovery path** — its correctness/throughput
  (`>5k ctx/s` floor; the `bbe0d20` truncation fix) is load-bearing; keep its regressions green.
- **`(p,b)` wire/codec change** is a replication format change — fine pre-production (no
  upgrade-compat constraint per CLAUDE.md), but bump the repl protocol version and ensure
  bootstrap rejects mismatched peers cleanly.
