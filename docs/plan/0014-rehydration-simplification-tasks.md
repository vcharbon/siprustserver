# ADR-0014 re-hydration simplification — implementation task list

Finishes the ADR-0014 simplification: split the single multiplexed pull stream
into two independent single-flow sockets (**Reclaim** / **Backup**), untangle the
global **watermark** (changelog position) from the per-call **`(p,b)`** version
vector, trim the wire message set, and delete every path made dead by removing the
`Deactivate` handback and the eager takeover.

See [ADR-0014 §Stream topology](../adr/0014-reactive-only-takeover-version-vector.md)
for the decisions; this is the work breakdown. Terms: see
[CONTEXT.md](../../CONTEXT.md) (Reclaim/Backup stream, Watermark, Current flag,
Readiness states, Bootstrap phase).

## 1. Wire / message set (`crates/repl-net`)

- [ ] `Op` becomes **`{Put, Delete}`** — **merge `Create`+`Update` into one idempotent
      `Put`**. Drop the first-sighting→Create / later→Update distinction everywhere.
- [ ] `Data` frame: `at, op, partition, call_ref, p, b, body_ttl_ms, indexes, body?`
      (`Put` carries a body; `Delete` none).
- [ ] **Remove `Ack`** (tag 1) and all references — it is a no-op stub today.
- [ ] **Remove `PullMode`** and the `Bootstrap`/`Replog` two-request handshake;
      `PullRequest` carries `{proto_ver, caller, partition, since}`. Server treats
      `since==(0,0)` as "store-scan bootstrap then tail," else "tail from since."
- [ ] Renumber frame tags densely; bump `PROTO_VER`. (Pre-production — no compat.)
- [ ] Confirm **`Deactivate`/tag 5** has zero remaining references (frame, codec,
      tests, comments).

## 2. Server send path + changelog (`repl/server.rs`, `repl/changelog.rs`)

- [ ] **Split `PeerLog` into per-partition compacted sub-logs** — one
      `entries: BTreeMap<counter,callRef>` + `by_ref` + `retained_floor` **per
      partition** (`Pri`, `Bak`). `bump` routes by the `partition` it already takes.
      A flow drains **only its sub-log** → no `(role,primary)` regroup, no `retain`
      filter. (Per-mutation cost stays `O(log L)`; the move-on-update keeps each
      sub-log bounded to its live set.)
- [ ] **Bounded batch drain:** `drain_since(peer, partition, since, limit=500)`
      returns the range `(since,..]` taking **`limit+1`** so the loop distinguishes
      `>500` (stream back-to-back) from `≤500` (flush + Noop).
- [ ] **Pure-poll send loop — DELETE the `Notify` machinery** (`subscribe`,
      `Subscription`, `subscribers` count, subscriber reap-immunity). Each connection
      `sleep`s ~100ms (tunable) between drains. While `>500`, loop immediately (no
      sleep); at `≤500`, flush + **Noop only on the catch-up edge + ~20s idle floor**.
      Rework `reap` to a plain idle-TTL drop (no subscriber guard).
- [ ] One shared serve routine parameterised by `(partition, caller)`: store-scan
      bootstrap (its partition) → catch-up-edge Noop → its-partition tail. One
      `PullRequest{partition, since}` per socket; `since==(0,0)` ⇒ scan-then-tail.
- [ ] `needs_reset` per-`(peer, partition)` (that partition's `retained_floor`).
- [ ] Keep `ResetToBootstrap` when `since` < the partition's compacted tail.

## 3. Puller (`repl/puller.rs`, `repl/supervisor.rs`)

**Design derived during §2 implementation (server contract is now fixed — match it):**
- The server sends **one** `PullRequest{partition, since}` per socket; `since==(0,0)`
  ⇒ the server does the store-scan bootstrap (stamped `at=W=head@scan-start`) then
  tails from `W`. There is **no** terminal bootstrap Noop and **no** second request
  — the first catch-up Noop of the tail ends the bootstrap.
- **Drop the client-side watermark apply-gate** (`at <= W → skip`). Idempotency now
  rests entirely on the `(p,b)` merge (a re-delivered/stale frame is rejected by
  dominance), so the gate is redundant and was the source of the bootstrap-frame
  collision (all bootstrap frames share `at=W`). Apply every `Data` under `(p,b)`.
- **Watermark advances only on `Noop(head)` and on post-bootstrap tail `Data.at`** —
  NOT on bootstrap frames (they share `at=W`; advancing mid-scan then disconnecting
  would skip the un-sent remainder). Keep a per-flow `bootstrapped` flag: set on the
  first `Noop`; before it, apply ungated and do not advance `W`.
- **`ApplyMode` stays but simplifies:** Backup flow = always "apply unless dominated"
  (Forward == Bootstrap rule — no mode distinction). Reclaim flow = Bootstrap rule
  before first Noop (take-as-is unless dominated), Reverse rule after (`p_in==sp &&
  b_in>sb`). So `ApplyMode` is a function of `(flow, bootstrapped)`.

Then:
- [ ] One FSM, two instances per peer, parameterised by `{partition, arm-timers?,
      role}`. **Reclaim** → `pri:{N}`, arm timers, primary role; **Backup** →
      `bak:{peer}`, no timers, backup role.
- [ ] **Per-(peer,flow) watermark** (two cursors), retained across reconnects.
- [ ] Boot order: open **Reclaim** streams first; `Ready` ⇔ all reachable Reclaim
      first-Noops (hard-timer bounded); **then** open **Backup** streams.
- [ ] Backup streams never feed readiness — only a **received-non-Noop-frame
      counter** metric.
- [ ] Delete the cold-Replog-re-pull-of-both-partitions logic + watermark-collision
      workarounds (puller.rs ~485-565), `PullMode`/`CHUNK`, `run_bootstrap`'s
      separate loop; bump `PROTO_VER` 2 → 3.

## 4. Apply / loopback (`store/mod.rs`, `repl/puller.rs`)

- [ ] **Invariant:** inbound apply mutates the store but **never appends to the
      local changelog** (verify `put_call(peer:None)`); add a regression test
      (`apply_inbound_does_not_append_changelog`).
- [ ] `(p,b)` merge rules unchanged (Forward always; Reverse iff `p_in==sp &&
      b_in>sb`; delete-wins; bootstrap take-as-is unless dominated). Confirm each is
      reached from the correct flow now that the partition is per-socket.

## 5. Reclaim / timers (`router.rs`)

- [ ] Reclaim-bootstrap completion → smoothed bulk timer re-creation (oldest-first).
- [ ] Reclaim-tail straggler → arm immediately (no smoothing).
- [ ] Smoothing stays in the reclaim handler, never the timer driver (CLAUDE.md).

## 6. HA test-harness impact (`ha-harness`, `failover-harness`, repl tests)

- [ ] **Insulated, verify only:** the *failover* harness asserts cluster vocabulary
      (`ReplicatedB2buaSut::{serves, is_synchronized_backup, memory_clean}`,
      `assert_single_owner`) — stream-count-agnostic. `is_synchronized_backup` reads
      the `bak:` store; unchanged. Confirm `runner.rs` settle/`reboot_and_reclaim`
      maps to **Reclaim-current**, not the old all-peer all_current.
- [ ] **`supervisor.rs`:** retained watermark + `current` flag become per-`(ordinal,
      flow)`. `all_current`/`await_current`/`is_current` (S7 + test introspection)
      **scope to the Reclaim flow** for readiness; Backup flow `current` is
      metrics-only.
- [ ] **Transport topology:** **one listen port, two independent connections** per
      peer pair (flow chosen by `PullRequest.partition`) — NOT two ports. One k8s
      Service port, one `listen()` in the sim. `SimulatedReplicationNetwork` carries
      two connections per pair.
- [ ] **Wire-frame tests** (`real_transport_tests`, `s5–s11`, `tests.rs`):
      `spawn_puller` becomes flow-typed; rewrite assertions on `mode`/`chunk`/`Ack`
      and the Bootstrap-then-Replog two-request sequence to the single
      `PullRequest{partition, since}` + scan-then-tail.
- [ ] **`ha-harness/report.rs`** frame renderer (line ~257) + `RecordingReplication
      Network`: re-render the trimmed frame set (`Put`/`Delete`/`Noop`/`PullRequest{
      partition}`/`ResetToBootstrap`); drop `Ack`/`mode`/`chunk`.

## 7. Dead-code sweep (the "go wider" mandate)

- [ ] Remove any remaining eager-takeover / `Deactivate` / handback / `all_current`-
      over-backup bookkeeping (readiness.rs, supervisor.rs, metrics.rs, s11_tests).
- [ ] Grep for now-dead symbols and delete them — no commented-out husks.
- [ ] Update/port tests (s5–s11, real_transport_tests) to the two-stream model and
      the cluster-vocabulary assertions.

## 8. Docs — `docs/ha-replication.html` full rewrite (FIRST-CLASS deliverable)

**This is an explicit implementation task, not an afterthought.** The current
`docs/ha-replication.html` is wholesale obsolete (single multiplexed stream,
`Deactivate` handback, eager takeover) and MUST be **rewritten from scratch,
concise** — not patched. Required sections, nothing more:
- [ ] The two flows (Reclaim / Backup), two sockets, the role/partition/timer table.
- [ ] One changelog → two partition-filtered cursors; watermark ⟂ `(p,b)`.
- [ ] The unified send loop (poll / 500-batch / catch-up-edge Noop / 20s idle).
- [ ] Boot order + readiness (Reclaim-first), backup deferred/metrics-only.
- [ ] `(p,b)` merge rules; reactive takeover + self-release; loopback invariant.
- [ ] The 4-frame wire set.
- [ ] Delete every diagram/paragraph describing removed mechanisms.

Acceptance: a new reader understands the model from this one page; no statement in
it contradicts ADR-0014 or CONTEXT.md.

- [x] ADR-0014 folds in the full model; ADR-0011 X4 marked superseded-in-part;
      CONTEXT.md glossary updated.

## Done-when

- 0 warnings, full repl test suite green under the two-stream model.
- No removed message/symbol remains anywhere in the tree.
- Live endurance re-run confirms re-hydration completeness (the ~203/3000 cliff is
  gone) and no monotonic CPU/queue drift.
