# Appendix: ADR-0014, reactive-only takeover and `(p,b)` version-vector reconciliation

Decision part: [ADR-0014](../0014-reactive-only-takeover-version-vector.md).

## Amendment record

ADR-0047 amends §1 (a departed peer's calls with an INVITE in flight are taken over at
its departure) and §2 (a backup whose primary reads dead at a verdict releases the
limiter key, ADR-0047 D8). ADR-0050 amends §5 (affinity is the ACK/CANCEL exemption and
liveness routing, with no fresh-pod guard). §11 makes readiness wait for the Backup
streams too. ADR-0055 amends the replication write path, the confirm-race trade-off and
the INVITE-only seeds (ADR-0055 §2–§4), the apply rules and the fold read containment
(ADR-0055 §7), and the skew stamp (ADR-0055 §9).

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

### 2. Remove the `Deactivate` handback

2. **Remove the `Deactivate` handback** (wire frame tag 5 + the watermark handshake
   + the go-active burst). The acting-backup **self-releases**: when the **last
   in-flight transaction** for a taken-over call reaches a terminal state, it
   `drop_local`s its live copy (the `bak:` replica + the reverse-flushed deltas
   remain). On a **terminal** (BYE/CANCEL) the backup **does not discharge** — no
   CDR, no limiter release (unless its primary reads dead at a verdict, ADR-0047
   D8), no propagated delete: it records the terminal into the
   `(p,b)` Element and reverse-flushes it with a short grace TTL (its alive-timer)
   before dropping the live copy; the **primary is the sole discharge authority**
   (§Terminal reconcile). Durability of the reverse-flush replaces the handshake.

### 4. Keepalive smoothing is performance-only

4. **Keepalive smoothing is performance-only.** On reboot, `ReclaimAll`
   re-materialises the whole `pri:{self}` partition; many keepalives are past-due.
   The reclaim handler staggers them oldest-first — `fire_at = now + (L_max - L)/speedup`,
   bounded to `speedup`× cadence (`keepalive_catchup_speedup`, default 10; optional
   `max_catchup_window_sec`) — so a freshly-rehydrated node is not flooded. This is
   load management with **no** correctness role (`(p,b)` makes any incidental
   keepalive overlap non-corrupting), so there is **no settle/handback floor**.
   `fire_at` is pre-computed in the reclaim handler, never in the timer driver
   (CLAUDE.md timer invariants are untouched).

### 5. LB transaction affinity

The front proxy ([sip-proxy](../../../crates/sip-proxy/)) stays stateless. It re-derives
the target from the signed `w_pri/w_bak` cookie on every in-dialog request against a
fresh registry snapshot, with no per-dialog state. "Finish the in-flight transaction on
the current target, switch only when it is dead" is met by the ACK/CANCEL exemption (an
alive primary owns its in-flight UAS state) and by liveness routing (ADR-0050), not by
affinity state.

### 7. Two streams

Reclaim is `bootstrap(reclaim my calls)`, then `tail` (catch a post-partition
reverse-flush a live-but-partitioned primary missed). Backup is `bootstrap(load the
peer's calls I back up)`, then `tail` (forward updates). Both have the same FSM shape,
parameterised only by `{store partition, arm-timers?, role}`, and one shared
reconnect and recovery path on a cut.

### 8. One changelog

8. **One changelog, two partition-filtered cursors; watermark ⟂ `(p,b)`.** A node
   keeps **one** changelog (one `(gen,counter)` space). Each flow holds its own
   cursor, advanced only by frames of its partition. The watermark is **purely**
   a changelog position — never read from or written to a call's `(p,b)`. Each
   flow's `bootstrap` is a **store scan** (mandatory: a created-then-quiescent
   call's changelog entry compacts away, so only the scan is a complete snapshot),
   capturing `W = head at scan-start`, then the partition-filtered tail.

### 9. Inbound apply is changelog-silent

9. **Inbound apply is changelog-silent (loopback prevention).** Applying a received
   `Data` frame mutates the store but **never appends to the local changelog**.
   Only a **locally-originated** mutation (N processing a call it serves — as
   primary, or as acting-backup during a takeover) appends. A change N received as
   a backup therefore has no local changelog entry and can never be re-served to
   the peer that sent it. The puller's *cursor* still advances (its job, private);
   the *changelog* does not. (`put_call(peer:None)` must honour this — invariant +
   test.)

### 10. Unified server send loop

10. **Unified server send loop** (both flows, parameterised by `(partition,caller)`):
    a **pure `sleep` poll** (no `Notify`/subscriber machinery: one mechanism, less
    state; ≤100ms latency, tunable lower). Each
    tick drains its sub-log range `(since,..]` taking **`limit+1` (=501)**: while
    `>500` pending, stream `Data` back-to-back to fill the TCP buffer (no Noop, no
    sleep — each `Data` carries its own `at`, so the cursor rides the data); at
    `≤500`, flush the remainder and sleep. A **`Noop` is emitted only on the catch-up
    edge** (drained to head — the *first* such Noop sets the puller's sticky
    `current` flag = "mostly caught up") **and on an idle interval** (liveness; 500 ms,
    and the puller leaves a stream silent for 2 s, ADR-0045). A trickle needs no
    Noop.

    The drain is cheap by design (it runs ~10 Hz × peers × 2 flows): the per-peer
    changelog splits into **per-partition compacted sub-logs** (one
    `counter→callRef` BTreeMap + `by_ref` + `retained_floor` each), so a flow drains
    only its partition with **no cross-partition filtering** and the old
    `(role,primary)`-regroup/`retain` dance is gone. `Op` is **`{Put, Delete}`**
    (Create/Update merged → idempotent `Put`); `bump` does an `O(log L)` move (drop
    old counter, insert at the max) that bounds the log to the live set; apply is
    idempotent under the `(p,b)` gate. `needs_reset` is per-`(peer,partition)`.

### 11. Readiness

11. **Readiness = both flows' first Noop, Reclaim first.** `Ready` ⇔ every stream
    to a *reachable* ready peer hit its first Noop, the **Reclaim** stream and then
    the **Backup** stream, which opens once every Reclaim stream has: a node reads
    ready only once it holds the calls its peers named it backup of, so an
    orchestrator that waits for ready before its next disruption waits for the
    copies the returned node can get. Hard-timer bounded
    (`B2BUA_REPL_BOOTSTRAP_HARD_TIMEOUT_MS`, ADR-0011 X5) for a peer never reached:
    a Backup stream never reached is released by its own hard timer, or by one
    failed connect of its own once the peer's Reclaim stream was never reached by
    its hard timer; a dead peer cannot hang readiness. A stream that reached its
    peer and then stalled holds readiness while the peer stays listed ready, the
    strict X6 gate. A cold node with no calls reaches `Ready` at once (empty scan
    → immediate terminal Noop). The
    **server** side is independent of local readiness: a NotReady node still answers
    peers' pulls — it simply has nothing to hand over until it processes calls.

### 12. Wire message set

Tags renumber freely with a proto version bump. Frames:

    | Frame | Dir | Fields | Role |
    |---|---|---|---|
    | **PullRequest** | C→S | `proto_ver, caller, partition, since` | Open one flow. Bootstrap is **implicit** when `since==(0,0)`; else tail. |
    | **Data** | S→C | `at, op, partition, call_ref, p, b, body_ttl_ms, origin_now_ms, indexes, body?, answered, incarnation?` | One mutation. `op∈{Put,Delete}` — **Create and Update are merged into one idempotent `Put`** (carries a body); `Delete` carries none (delete-wins), and states in `answered` whether the deleting node had answered the call (ADR-0031 D3). A `Put` names the call incarnation its body is of, a `Delete` the one it removed (§"Amendment: `(p,b)` orders one call incarnation"). |
    | **Noop** | S→C | `at` | Catch-up edge (first ⇒ ready) + idle keepalive (ADR-0045). |
    | **ResetToBootstrap** | S→C | `reason` | `since` fell below the compacted tail → re-bootstrap from `(0,0)`. |
    | **Position** | C→S | `at` | "I have applied everything up to `at`" — the drain's exit signal only (ADR-0031 D2), never a retention or readiness input. |

Not in the set: an `Ack` retention hint (retention is bounded by time and size, and a
too-slow puller re-bootstraps through `ResetToBootstrap`), a `PullMode` enum and a
`Bootstrap`/`Replog` two-request handshake (the `since==(0,0)` rule covers both), and
`Deactivate`/tag 5. No handler or branch for any of them stays in the codebase (task
list, §Removals).

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
deliberate: a **2xx INVITE** server transaction lingers in RFC 6026 `Accepted` until
Timer L (the 2xx ACK takes a *different* branch, so it neither ends nor detaches the
INVITE txn), so self-release naturally fires at Timer L, **after** the ACK has been
relayed. A 2xx's Accepted seed (below) holds a takeover copy the same way, up to
Timer L from the seed. A non-INVITE clears at Timer J; a failed leg at Timer B/F.

## Why this is safe where the old design was not

- The **storm** is gone: the backup never originates a probe and quiescent calls are
  recovered only by the (smoothed) reboot reclaim → no population-wide stale-CSeq wave.
- The **double-serve leak** is structurally gone: the
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
  without cross-node coordination; fails by cleanly dropping one call. The reused
  number rides a **fresh `branch`** (every `send_request_to_leg` mints one), so a
  compliant UAS does not fold it away as a retransmission (§17.2.3, branch-keyed):
  it is a new server transaction whose sequence number did not advance and is
  rejected out of order (§12.2.2 — 500 when the number is strictly lower, an
  implementation-defined reject when both owners mint the same value). The
  failover suites encode this as a
  deviation window scoped to the takeover (a row of ADR-0055 §11's window
  table, opened by `FailoverHarness::strike`) and still assert the accepted
  outcome — call fully over, one CDR, limiter released; the same rule gates in full
  on establishment and on every no-fault cell.
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
    before the kill has already flushed its collapsed `Confirmed`
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
order, call survival, clean teardown) is the same as without HA.

The confirm-race (the accepted trade-off above) is classified by the HA tooling, which
attributes each impact to its incident window and budget (ADR-0049 §3.5). Loadgen
carries no chaos classifier.

## Terminal reconcile, deferred discharge and resurrection guard

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
   returns, the backup's replica reap releases the call's limiter key when the
   alive-timer expires and counts its CDR lost (ADR-0020 X3).

3. **`flush` carries the authoritative `(p,b)`; a delete is tombstoned.** `flush`
   embeds the authoritative `(p,b)` in the replicated **body** (encoding the
   pre-bump clone would branch a backup one `p` stale — invisible vs a crashed
   primary, but it silently breaks every reverse-flush to an **alive** primary).
   And because the Reverse `(p,b)` rule structurally cannot let a backup's
   discharged state apply to a primary that has bumped `p`, a deleted `call_ref` is
   **tombstoned** (`ReplicatingCallStore`): `put_call` rejects a re-creating `Put`
   within the window (apply-side delete-wins), so a late reverse-flush cannot
   resurrect a just-discharged call. The tombstone buries the call incarnation
   the delete removed, not the `call_ref` (amendment below).

## Amendment: skew re-anchoring at the replication boundary (accuracy only)

Replicated `call::TimerEntry.fire_at` is an absolute epoch-ms deadline minted on
the origin node's `Clock`. Each pod anchors its wall clock once at start
(`sip_clock::Clock::system`, monotonic-derived), so two pods anchored on opposite
sides of a host NTP step disagree permanently. Rebuilding a restored timer as
`(fire_at − local now_ms).max(0)` trusts the dead node's clock without bound: under
medium skew it reaps a healthy ringing or established call at takeover, extends a
policy cap by the skew, or fires a keepalive OPTIONS the instant the backup takes
over, racing the failed-over transaction (`reinvite/unexpected "got OPTIONS expected
200"`).

The fix is for accuracy and performance only; correctness stays `(p,b)`-causal. The
replication `Data` frame carries an `origin_now_ms` stamp (the sender's
`Clock::now_ms()` at flush-send time). The receiving store persists
`skew_offset_ms = receiver_now_ms − origin_now_ms` beside `expiry_at_ms`, the same
receiver-side re-anchor idiom as `body_ttl_ms`. One router seam,
`sanitize_restored_timers`, runs on every hydration path (bulk reclaim, reactive
takeover, on-demand reclaim, reverse-flush straggler). It re-anchors every restored
instant of the copy by `skew_offset_ms` (a sub-second deadband ignores pure
replication latency), then cohort-smooths. The stamp is read in the frame of the
body's instants, so the offset composes across hops, and a write of a node's own
body clears it (ADR-0055 §9). No path restores with an unknown offset. The offset
includes one transit latency, acceptable at in-cluster ms scale, so restore skew is
bounded to about the replication latency.

There is no wall-clock-dependent correctness rule, settle window or handback, and the
`timers.rs` epoch+lockstep-`Key` driver holds no skew or smoothing logic. A periodic
`clock_wall_divergence_ms` gauge and a rate-limited warn make a live host step
observable. `Clock::system` fails loudly on a pre-epoch clock rather than anchoring to
1970. Infra discipline still applies (slewing chrony, host kept awake): the SUT bounds
the residual, it does not license drift. The failover harness has a per-node
clock-anchor-offset knob (`FailoverHarness::with_worker_clock_offset`) that injects
deterministic inter-node wall skew on the single monotonic timeline.

## Amendment: a materialisation re-seeds the INVITE transactions it is in the middle of

Decision 1 says "SIP retransmission cannot bridge an outage". That holds for a
quiescent dialog and is why takeover stays reactive. It does not hold for an
INVITE in flight at the kill: the peers keep retransmitting for Timer A/B or
Timer G/H, well past a failover, and a survivor that holds no transaction for
that INVITE answers with the wrong thing (a TU-composed ACK sent once, a final
sent raw with no Timer G, a 481 to the caller's CANCEL).

**A materialisation (reactive takeover, on-demand or bulk reclaim) re-seeds the
INVITE transactions the replicated record names**, in one router module
(`b2bua/src/router/materialise`) that every path runs:

- a `Proceeding` client INVITE for each INVITE handle still open (the record
  says which; a handle kept for a taken 2xx's re-ACK is not in flight), with
  the INVITE bound armed and no Timer A ladder; one the record holds no
  provisional for opens `Trying` and re-sends its INVITE once (ADR-0043);
- a `Proceeding` server INVITE for the a-leg's initial INVITE while the a-leg
  is unanswered (request rebuilt from the replicated snapshot, To-tag pinned),
  and for each pending relayed INVITE, holding the originator's request rebuilt
  from its relay snapshot (Via stack, From, To, Call-ID, CSeq): it absorbs the
  retransmission and replays a 100, answers a CANCEL 200 + 487, and is answered
  at the call's release;
- a server INVITE in RFC 6026 Accepted for each 2xx a dialog retains awaiting
  its ACK, holding that 2xx: a copy of the INVITE draws the 2xx in the layer and
  never reaches the rules, whose glare check would answer it 491. A transaction
  the copy itself opened before the record was read, with no final, takes the
  seed's 2xx and sends it once. Answering a copy with the 2xx is this stack's
  choice, not RFC 6026's (§8.7 absorbs it silently): the takeover node never sent
  the 2xx the copy may have missed. The seed holds a takeover copy up to Timer L
  from the seed, as the 2xx's ACK takes a new branch and never detaches it. An
  answered round awaiting its ACK is an INVITE
  transaction a live copy serves: the departure sweep takes it over and resumes
  its 2xx ladder (RFC 3261 §13.3.1.4).

The datagram that caused the materialisation is re-offered to the transaction
layer after the seed, so the layer owns every hop ACK, Timer D re-ACK and
Timer G ladder, and the rules see a final like any other. The self-release
watch is armed after the seed: a takeover copy is resident exactly while a
transaction it holds is live, the definition in "Self-release mechanism". One
asymmetry follows: the a-leg seed is attributed to the call while the admit
path attributes an initial INVITE to no call, so a ringing call's takeover copy
whose b-leg is rejected releases when the caller ACKs the relayed final (or on
Timer H), where the primary's own copy released on the turn it went Terminated.

Three contracts go with it. The transaction layer invents no transaction for a
non-2xx INVITE final on a branch it never saw (the final leaves raw and
`server_final_unseen_branch` counts it), and does not answer a CANCEL it cannot
match: the CANCEL is handed up, and the router answers the
RFC 3261 §9.2 481 when nothing seeded claims it. Teardown answers every pending
relayed INVITE 487 before the BYEs (§15.1.2). And the takeover lookup refuses a
`Terminated` replica (`repl_takeover_refused_terminated`), which the backup
retains on purpose for the primary to fold or reclaim; `Terminating` is served.

The front proxy's CANCEL memo (Decision 5's ACK/CANCEL exemption) re-resolves a
dead pinned worker through the stickiness cookie's own health ladder, so the
caller's give-up reaches the survivor that holds the seeded INVITE.

**A fold into the call's record seeds the answered INVITEs it brings.** A body
folded into the primary's live copy (ADR-0031 D3) may hold a round the other copy
answered and whose 2xx awaits its ACK; its Accepted server INVITE is seeded as at a
materialisation, so the originator's copy of the INVITE, routed to the primary once
the proxy reads it alive, draws the retained 2xx. A round still open is not seeded
there: its target's responses follow the Via of the node that relayed it, so only
that node can CANCEL it or take its final, and a client seed elsewhere would run
out its bound on a round another node finishes.

**A non-INVITE relay is resumed from its record (ADR-0055 §2).** The record
holds both halves of a non-INVITE relay: a takeover seeds the server transaction
of the originator's request, which absorbs its retransmission, and the relay's
client transaction, which retransmits the relayed request under its branch; the
target repeats its final for it (RFC 3261 §17.2.2), and the proxy hands that
final to the call's owner alive (ADR-0055 §8). Residual: when the relaying node
lives — the proxy read it dead for a moment and routed a Timer E rung here — both
nodes hold the relay, and the originator absorbs a second final as a
retransmission.

Pinned by `failover-harness/tests/it/{prack_takeover,inflight_resend,released_copy,
reinvite_server_side_at_the_takeover,retained_2xx_folded_into_the_record,
relay_resumed_at_the_takeover}.rs`, `sip-txn/tests/it/{seed,accepted}.rs` and
`b2bua/src/router/materialise/tests.rs`.

## Amendment: lifecycle progress folds over a refused reverse flush

The Reverse apply rule (`p_in == p_cur && b_in > b_cur`) reads counters, and a
counter bump on a reclaimed copy is not progress: a rebooted primary reclaims a
still-ringing body, the proxy keeps routing to the survivor for the readiness
lag, the survivor answers the caller, and any in-dialog message that reaches
the primary first (the caller's ACK) persists a turn on the stale record and
bumps `p`. The survivor's answer is then refused, both nodes serve the call,
and the primary's restored ring deadline authors a second final on an INVITE
transaction the caller already ACKed (RFC 3261 §17.2.1). The accepted
keepalive-vs-takeover CSeq drop is harmless; a dropped answer or teardown is a
lost call.

**A reverse flush the vector refuses is handed to the call model with its
body, not dropped.** The router folds it when it carries lifecycle progress
the live copy lacks — unanswered < caller answered < terminating < terminated —
and refuses it otherwise (`repl_reverse_flush_refused_total`). A fold makes the
live timer service follow the folded ledger: entries the folded body no longer
carries are cancelled, the ones it carries are armed through the restore
hygiene seam. The `(p,b)` vector stays the only gate at the store, and the
only rule for everything that is not lifecycle progress.

The fold rule is sharpened by [ADR-0031](../0031-a-workers-three-ways-out.md)'s "Amendment —
progress is a chain": the four-step order above is two monotone axes a body must not regress
on, and the fold adopts both counters.

Not closed here: an answer version that died with the primary before any copy
held it. No record can carry that fact; closing it needs the answer durable at
the backup before the 2xx leaves (an accepted trade-off above rejects it) or a wire check at
the ring deadline. Pinned ignored in
`failover-harness/tests/it/answer_lost_with_primary_no_answer.rs`; the folds are
pinned live in the same file.

## Amendment: a counter counts writes that change the call, not progress

Each owner bumps its own axis of `(p,b)` once per handler turn that changes
the replicated call state, and flushes that turn. A dialog-level
retransmission turn changes no fact a CDR, a peer or a node restoring the call
needs: it is **quiet** — the live copy is replaced (the armed rung advances),
no axis moves, nothing flushes. The quiet list is closed
(`b2bua::effects::QuietTurn`); a further member is a line here first:

1. a rung of this node's own un-ACKed 2xx ladder (RFC 3261 §13.3.1.4),
2. a rung of this node's own reliable-provisional ladder (RFC 3262 §3),
3. the re-ACK of a repeated inbound 2xx (RFC 3261 §13.2.2.4),
4. an inbound ACK the rules absorb: it discharges nothing this node awaits
   and no ACK is owed toward the peer for it (RFC 3261 §13.2.2.4).

A rung is quiet only while the ladder goes on: a copy left and the next rung
is armed, so the body differs from the last write by the rung index and the
rung timer's due instant. The class is decided from the turn's effect set,
not from the action that repeated: a quiet turn is one outbound repeat of the
retained datagram on an `Active` call, and for a rung its own re-arm, and
nothing else; an absorbed ACK is a turn with no effect at all that leaves the
body as the rules read it. The rung that ceases the ladder, a spent fire, a give-up, a
keepalive OPTIONS turn, a request refused into the message ring, a re-ACK a
rule emits beside a BYE or a CDR event, a re-ACK on a terminating call and
every other turn is a write and bumps. A transaction-level retransmission
never reaches the call model. A turn that trips the per-call message cap is
the teardown, never quiet.

**What the vector answers.** Two questions only: equality on the reader's own
axis — has the other owner written since I branched — and strict greater on
the writer's axis — is this a newer write of yours; that is the Reverse rule
as it stands (`p_in == p_cur && b_in > b_cur`). It never orders two owners'
progress; the body does (the lifecycle chain, the answer latch, ADR-0031's
amendment). An adopted `p` names a version without its body, and the body says
so (`CallTopology::gen_adopted`): the Reverse rule never folds such a body
whole (ADR-0031, "an adopted counter is not the body it counts").

**One increment means** one turn on that owner changed the replicated call
state, and the flush carrying that body is on the wire. **It does not mean** a
message was received or sent, that a rung was walked, that the call is further
along than a copy with a lower count on the other axis, or that the cap
counter moved: a rung of this node's own ladder never counts toward
`max_messages_per_call`; a repeated inbound 2xx does, and its count rides in
the next write.

**A fold is not an increment.** A node that folds the other owner's body — the
reverse reconcile, or a refused flush folded in either direction — moves
neither axis and flushes nothing: its live copy carries `max` of the two
views on each axis, and its store keeps the version it last flushed. What the
live copy holds depends on the path. The reverse reconcile of a flush the
vector took holds the other owner's body at the version it published. A
refused flush folded whole holds the other owner's content at `(max p, max b)`,
a vector neither owner published. A round's end or opening folded alone
(ADR-0031, "a round's end folds alone") holds this node's own content with the
round merged in, at that same vector. Every path holds the
higher of both views' dialog sequences. On every path the other owner's next write orders
against what it published: a fold that bumped would read that write as made on
a version this node has moved past and refuse it. Timers a fold takes are
re-anchored into this node's clock and a stale keepalive guard is dropped,
which no peer reads; the live copy's own timers stay where it armed them
(`a_round_fold_leaves_the_live_copys_own_timers_where_it_armed_them`).

**Accepted degradation.** A node restoring the call — the reclaiming primary,
since a survivor's takeover copy is reactive and sheds when its last
transaction clears — hydrates the body of the last replicated write and
restarts the ladder from there: the rung index that write held, its past-due
rung entry fired at once, then the schedule from that rung, bounded by the
give-up deadline the replicated timer ledger carries. The caller sees a few
tag-identical duplicates, within the retransmission schedule, sooner than the
schedule would have paced them; the give-up fires at the deadline the ledger
carried, not later. The message count a quiet re-ACK advanced is not
replicated by that turn; it rides the next write, so a node restoring the
call reads the last written count, not zero — a runaway callee's repeats
since that write are counted again from it, bounded by the keepalive reset.
An alive
primary that folds a dominating reverse flush restarts its ladder from the
folded write, bounded by the same deadline.

**Dry-run.** Per rule, the scenario that exercises the ordinary path and the
cell that pins the rule:

| Rule | Ordinary path | Pinned by |
| --- | --- | --- |
| Own 2xx rung is quiet, does not count toward the cap | `b2bua-harness/tests/unacked_2xx_reap.rs`, `unacked_reinvite_2xx_reap.rs`, `failover-harness/tests/it/inflight_resend.rs` | `b2bua-harness/tests/it/retransmission_turn_and_message_cap.rs` (`a_2xx_ladder_rung_does_not_count_toward_the_message_cap`), `failover-harness/tests/it/retransmission_turn_is_quiet.rs` (`a_reclaim_after_one_rung_…`, `…three_rungs_…`) |
| Own §3 rung is quiet, does not count toward the cap | `b2bua-harness/tests/it/prack_reliable_ladder.rs` | `retransmission_turn_and_message_cap.rs` (`a_reliable_provisional_rung_does_not_count_toward_the_message_cap`), `failover-harness/tests/it/prack_takeover.rs` |
| Re-ACK is quiet, the repeated 2xx counts | `b2bua-harness/tests/it/reack_retransmitted_2xx.rs` | `retransmission_turn_is_quiet.rs` (`a_re_ack_of_a_repeated_2xx_replicates_nothing`), `retransmission_turn_and_message_cap.rs` (`a_repeated_inbound_2xx_counts_toward_the_message_cap`) |
| The vector answers equality and strict-greater only | `failover-harness/tests/it/forward_flush_never_regresses_backup_progress.rs` (both `HealAt` cells), `answer_lost_with_primary_no_answer.rs`, `b2bua/src/repl/s12_tests.rs` | the same cells, unchanged |
| A restored ladder restarts from the last write, bounded by the deadline | — (a residual, not a rule) | `retransmission_turn_is_quiet.rs` (`a_reclaim_after_three_rungs_restarts_the_2xx_ladder_from_the_answer`) |
| An absorbed ACK is quiet; the ACK that discharges is a write | `b2bua-harness/tests/it/duplicate_ack_absorbed.rs`, `reack_retransmitted_2xx.rs` | `duplicate_ack_absorbed.rs` (`a_duplicate_ack_on_a_fresh_branch_draws_no_second_b_leg_ack`, `a_duplicate_reinvite_ack_draws_no_third_b_leg_ack`) |
| A fold is not an increment | `call_terminate_on_backup.rs` (`c11_reinvite_on_backup__primary_alive__no_terminal`), `reinvite_answer_failover.rs`, `forward_flush_never_regresses_backup_progress.rs`, `answer_lost_with_primary_no_answer.rs` | `b2bua/src/router/reclaim/tests.rs` (`a_reverse_fold_holds_the_backups_version_so_its_next_write_folds`, `a_forward_answer_folds_into_a_ringing_takeover_copy_without_ending_it`, `a_refused_flush_answering_the_round_the_live_copy_holds_open_folds`, `a_round_fold_keeps_what_the_live_copy_wrote_since_the_branch`) |

`b2bua_repl_quiet_turns_total{kind}` counts the turns persisted this way, so
N rungs in `b2bua_retransmits_total` against 0 in `b2bua_repl_flush_propagated_total`
is the rule holding. ADR-0032 X2 states the ladder side.

## Amendment: `(p,b)` orders one call incarnation

A `call_ref` carries successive calls: a caller retrying after a 401/407/422 or
a 3xx re-sends the INVITE with the same Call-ID and From tag (RFC 3261 §8.1.3.5,
§22.2), and the retry is born on the `call_ref` of the call it retries, its
vector restarted at `(1,0)`. The vector orders versions of **one call
incarnation** (`call::Call::incarnation`, the limiter key, unique over time),
so every write names its incarnation beside `(p,b)`: the store keeps it with
the body, a `Put` frame carries it, a `Delete` frame names the call it removed,
and a tombstone names the call it buries.

- **A tombstone buries a call, not a ref.** Every call a ref carried inside the
  window stays buried, each for its own window: a `Put` of a buried incarnation
  is ignored, as is a `Put` naming none while nothing is held; a `Put` of
  another incarnation is stored and replicated at once. A tombstone naming none
  (the deleting node held no body and the delete named none) buries only
  unnamed writes. A `Delete` of another call than the held one buries that
  call alone and propagates nothing.
- **A write of another incarnation is a new record.** The call it replaces is
  buried as replaced, its body and index keys go, and none of the per-ref facts
  the store kept about it (the authority's answer, the backup ordinal, the skew
  offset) carry over.
- **Another incarnation is not a version.** A Bootstrap or Forward `Put` of
  another incarnation than the Element's replaces it whatever the vectors say:
  the authority serves a new call on the ref, and a backup that missed the old
  call's delete (the changelog keeps one entry per ref, so a delete and the
  retry's first `Put` inside one poll drain as that `Put`) would otherwise keep
  the old call at a version the retry's first writes do not dominate. An acting
  backup's own write (`Reverse`) never replaces another incarnation, and a
  Reverse `Put` of one is refused: either is a takeover copy's view of a call
  the authority has since replaced. A fold (the reverse reconcile, a refused
  flush) reads a body of the live call's own incarnation only.
- **A replaced deferred terminal is settled like an expired one.** Nobody
  settles a call a write of another replaced: its primary moved on with no
  delete. The backup that held it hands its last body to the replica reap
  (`take_displaced`) when that body is a version a backup authored (`b > 0`;
  the authority settles its own), and the reap releases its own limiter key
  and counts its CDR lost when it is a terminal, as for an expired deferred
  terminal (ADR-0020 X3). A takeover copy's writes after the replacement go
  the same way, each re-stamping the burial so the call stays buried while a
  copy still writes it. A release with no resident copy deletes nothing.
- **An event names its incarnation.** A timer fire, a transaction timeout and
  a callout result carry the incarnation that armed, sent or awaited them, and
  every Via and Contact the stack emits carries its mark (`ci`); the router
  drops an event of another incarnation than the live call's before any rule
  reads it (`b2bua_other_incarnation_dropped_total`). One stating none belongs
  to the live call. Pinned by `b2bua-harness` `reoffer_late_events_of_the_first_call`.

Writes that name no incarnation (unit fixtures, harness surgery) compare by
`(p,b)` alone; the `ha-harness` writes name one call per ref. Pinned by
`failover-harness` `retry_on_a_challenged_identity` (both timings), the
`b2bua` `repl` S13 cells (a late reverse flush of the first of two challenged
calls, in the Reclaim bootstrap and in its tail; a takeover write of a
replaced call before and after the retry lands; a replaced deferred terminal
settled once; a delete of a replaced call leaves the retry), the `router`
reclaim cells `a_refused_flush_of_another_call_never_ends_the_live_one`,
`a_reverse_reconcile_of_another_stored_call_never_ends_the_live_one`,
`a_deferred_terminal_another_call_replaced_is_released_by_its_own_key`, and the
`repl` unit cells `a_delete_buries_its_call_and_not_the_next_one_on_its_ref`,
`a_new_call_on_the_ref_replaces_an_element_its_version_does_not_dominate`,
`a_reverse_flush_of_a_replaced_call_folds_into_nothing`. The resurrection the
tombstone exists for stays pinned by `resurrection_tombstone_pruned_by_reap`,
`answer_lost_with_primary_no_answer` and the two forward-`Delete` cells of
`forward_flush_never_regresses_backup_progress`, which fail with the guard off.

Residuals. A replaced deferred terminal's CDR is lost, not written: the
discharge funnel serves one call per ref, and the ref now carries the new
call. When the primary reclaimed and discharged a backup's deferred terminal
(`b > 0`) and its delete compacted into the retry's `Put`, the backup counts
that CDR lost although it was written (the limiter release is a no-op). A new call replaces an Element a
takeover answered and the authority never saw (ADR-0031 D3's protected case):
the authority starts a new call on a ref only after ending the old one, so the
answered call's record is lost with it.