# TODO — Fix "call terminated on the backup" (TDD-first)

Status: **planning / failing-tests phase**. No production fix lands until the
matrix below is red and the model decision is made.
Branch: `feat/sip-message-layer`.

---

## 1. The problem we are managing

A call can be **terminated by the backup**, not only by its primary. Today the
backup's teardown of a takeover copy goes through `release_call(SelfRelease)` →
`drop_local`, which by design does **no CDR, no limiter release, no delete
propagation** ([store/mod.rs `drop_local`](crates/b2bua/src/store/mod.rs),
[router.rs `CallQuiesced` handler](crates/b2bua/src/router.rs)). The clean path
(peer answers the BYE → call reaches `Terminated` → discharge) only works when
the dialog cleanly completes *before* the served transactions clear. Whenever it
does not, we get the endurance symptom: leaked limiter holds, no CDR, and a body
left resurrectable in `pri:{primary}` → the rebooted primary re-hydrates a swarm
of zombies and pins the limiter cap until each one's keepalive-timeout.

**The previous (narrow) plan was wrong** because it only patched the
post-reboot reclaim case. The backup can serve a terminal request in many more
situations, and several of them have the *opposite* risk — **double** discharge
(two CDRs / double limiter release) if both nodes act on the same call:

- **Backup serves BYE while the primary is ALIVE.** ADR-0014: a partition can
  route a dialog to the backup *at any time, for any duration* — no kill
  required. Both nodes can now believe they own the call.
- **Backup serves BYE, primary CRASHED, never returns.**
- **Backup serves BYE, primary CRASHED, then reboots + reclaims.**
- **Backup-driven terminal with NO inbound BYE** — keepalive-timeout /
  max-duration fires *on the backup* (the 1780-like resurrection).
- **Peer leg never answers the BYE** — the call sticks in `Terminating`; the
  served txns clear at Timer F (~32 s) and `CallQuiesced` preempts both
  `TerminatingTimeout` and the reaper.
- **Split-brain:** primary AND backup both serve a BYE for the same dialog.

We need ONE story that is correct for **all** of these.

---

## 2. The universal invariants (every test asserts these)

For *every* cell, after the call ends and the cluster settles:

1. **Exactly ONE CDR** for the call, across *all* nodes' CDR sinks (today only
   per-node `cdr_records()` exists — see §5). Not zero (lost), not two
   (double-billed).
2. **The call is OVER** — `assert_single_owner` reports 0 serving nodes and
   `assert_call_fully_released` (no `holds_any_trace` anywhere, both nodes
   `memory_clean`). No resurrection survives a subsequent reboot + long wait.
3. **Limiter released exactly once** — the shared `LimiterServer`'s
   `store.stats().current_total == 0`, and it was decremented once (no double
   release driving it negative / no leak holding it at 1).
4. **No context "sent back"** — nothing terminal lingers in any partition for a
   later reclaim to revive.

These four are the spec. The model we pick is whatever makes all four hold for
the whole matrix with the least reasoning load.

---

## 3. The decision the tests must settle

### Model A — "Mixed": whoever ends the call, discharges it
The terminating node (primary *or* backup) writes the CDR, releases the limiter,
propagates the delete, cleans memory.
- **Pro:** resources freed immediately by whoever is live; no "wait for primary".
- **Con — the hard one:** the two nodes have **independent** CDR writers,
  limiter clients, and ObligationSets. Exactly-once across the partition becomes
  a distributed problem. The split-brain and primary-alive-misroute cells force
  cross-node idempotency (a shared "this call's obligations are discharged"
  fact) that does not exist today. Easy to get a second CDR or a double release.

### Model B — "Primary-only": the primary is the sole discharge authority
The backup's terminal responsibility is **SIP continuity only** — answer the
wire so alice/bob's BYE completes — plus **record the terminal state** into the
replicated context and reverse-flush it. It **never** writes a CDR, touches the
limiter, or deletes. The **primary** discharges exactly once, via the reaper
funnel, when it next owns/reclaims the call (immediately if alive; on reboot
reclaim if it had crashed).
- **Pro (why this is the user's lean):** there is exactly ONE node that ever
  writes CDRs or moves the limiter, so invariants #1 and #3 hold *by
  construction* — no distributed exactly-once. Trivial to reason about. The
  backup's takeover copy is purely transient.
- **Con / requirement:** the limiter hold + the CDR + the context are pinned
  until the primary reclaims → **the (crashed) primary must reboot quickly** to
  bound the pin. CDRs are delayed to reclaim (acceptable — not real-time).
- **Open design question the tests must answer (the crux of Model B):** in the
  **primary-alive misroute** cell, the backup marks the context terminal and
  reverse-flushes, but the primary's *live in-memory* copy is still `Active`.
  Something must reconcile that terminal state into the primary's live map and
  trigger discharge. Candidate mechanisms (the tests will discriminate):
  - **B1:** the backup never keeps a live takeover copy for a *primary-alive*
    dialog — it forwards the BYE statelessly; the primary, still owning the
    dialog, sees the in-dialog request (or its reverse-flushed terminal) and
    discharges its own live copy.
  - **B2:** the primary's puller, on applying a `Terminated` body that dominates
    its live copy, drives a discharge of the live map (new "reconcile terminal
    into live" path).
  - **B3:** the backup pushes an explicit `CallTerminatedRemotely` signal the
    primary turns into a reaper discharge.

> **Recommendation to validate:** Model B. Let the **primary-alive misroute** and
> **split-brain** cells be the discriminator — they are where Model A needs new
> distributed machinery and Model B needs only a single local reconcile path.

---

## 4. Build on what exists (ADR-0013), don't reinvent

The matrix framework is already here:
- `transparent_matrix! { name: State, Event, Fault, Recovery, seed; }`
  ([lib.rs](crates/failover-harness/src/lib.rs)) generates one
  `#[tokio::test(start_paused = true)]` per cell.
- Axes in [scenario.rs](crates/failover-harness/src/scenario.rs): `DialogState`,
  `Event` (incl. `Bye(Party)`), `Fault {Kill, Drain}`,
  `Recovery {StayDead, RebootNoTraffic, RebootAfterTakeover}`.
- `assert_call_fully_released` + `assert_single_owner`
  ([lib.rs](crates/failover-harness/src/lib.rs)) — invariant #2 already covered.
- Limiter wiring + `store.stats().current_total`
  ([limiter_ha.rs](crates/failover-harness/tests/limiter_ha.rs)).

**What is missing** (this is the infra work in §5): a no-kill misroute fault, a
peer-silent axis, and the CDR-count + limiter gates folded into the universal
sweep.

---

## 5. Test infrastructure to add FIRST (before any prod change)

1. **`Fault::MisrouteToBackup` (primary stays ALIVE).** New `Fault` variant:
   the proxy/harness delivers the in-dialog event to the backup while the
   primary is healthy and never killed. Mechanism: a **test-only header**
   `X-Test-Deliver: backup` stamped by alice/bob in the scenario, honored by the
   harness proxy's routing classifier (gated behind a `cfg(test)` /
   harness-only knob so it can never affect production routing). This is the
   user's "force to backup even without killing."

2. **Peer-silent axis.** Extend the BYE event (or add `Event::ByeNoPeerAnswer`)
   so bob does **not** `respond(200)` to the relayed BYE — the call sticks in
   `Terminating` and `CallQuiesced` fires first. This is the real-world trigger.

3. **Universal post-condition helper** — fold invariants #1, #3 into the sweep:
   ```rust
   pub async fn assert_call_fully_over(
       nodes: &[&ReplicatedB2buaSut],
       call_ref: &str,
       limiter: &WindowStore,   // shared LimiterServer store
   ) {
       assert_single_owner(nodes, call_ref);          // 0 owners after terminal
       assert_call_fully_released(nodes, call_ref).await;
       assert_eq!(total_cdrs_for(nodes, call_ref), 1, // NEW: exactly one CDR
           "expected exactly one CDR for {call_ref} across the cluster");
       assert_eq!(limiter.stats().current_total, 0,   // NEW: limiter released
           "limiter hold for {call_ref} not released exactly once");
   }
   ```
   `total_cdrs_for` counts matching records across every node's `cdr_records()`.
   Wire this into `assert_cell_transparent` so the whole generated matrix gets it.

4. **Mutualised long-wait `settle_terminal()` on `FailoverHarness`** — the
   user's "long fake-clock wait, mutualised for all HA tests." Advances under the
   settle/advance/settle pump past `keepalive_interval + keepalive_timeout`
   (config-derived) so any resurrected zombie *would* have armed a keepalive,
   probed a dead peer, and surfaced. Replaces the ad-hoc
   `for _ in 0..40 { advance }` loops in `limiter_ha.rs`.

---

## 6. The failing-test matrix (write these RED first)

Each row asserts the §2 invariants via `assert_call_fully_over`. Group by where
the terminal happens and the primary's fate. `✗` = expected to fail today.

| # | Test name | Terminal served by | Primary fate | Peer answers? | Today |
|---|-----------|--------------------|--------------|---------------|-------|
| C1 | `bye_on_primary__no_fault` | primary | alive | yes | ✓ control |
| C2 | `bye_on_backup__primary_alive__misroute` | backup | **alive** (misroute) | yes | ✗ |
| C3 | `bye_on_backup__primary_alive__peer_silent` | backup | alive (misroute) | **no** | ✗ |
| C4 | `bye_on_backup__primary_crashed__stay_dead` | backup | crashed, never returns | yes | ✗ (CDR? limiter?) |
| C5 | `bye_on_backup__primary_crashed__peer_silent__stay_dead` | backup | crashed | no | ✗ |
| C6 | `bye_on_backup__primary_crashed__reboot_reclaim` | backup | crash → reboot | yes | ✗ |
| C7 | `bye_on_backup__primary_crashed__peer_silent__reboot` | backup | crash → reboot | no | ✗ (the endurance bug) |
| C8 | `keepalive_timeout_on_backup__reboot` | backup (no inbound BYE) | crash → reboot | n/a | ✗ (1780-like) |
| C9 | `max_duration_on_backup__reboot` | backup (GlobalDuration) | crash → reboot | n/a | ✗ |
| C10 | `bye_split_brain__primary_and_backup` | **both** | alive | yes | ✗ (double-CDR risk) |
| C11 | `reinvite_on_backup__primary_alive__no_terminal` | backup (non-terminal) | alive | n/a | ✓ must stay no-discharge |

Notes:
- **C11 is the guard rail**: a non-terminal served txn (re-INVITE/UPDATE/OPTIONS)
  must keep the `SelfRelease` / no-mutation behavior — the call genuinely
  continues at the primary. Any fix that discharges here is wrong.
- **C10** is the exactly-once stress: both nodes terminate → still exactly one
  CDR, one release. This is the cell that most cleanly separates Model A
  (needs cross-node idempotency) from Model B (only the primary ever discharges).
- C2/C3 (primary-alive misroute) exercise Model B's open question (§3, B1/B2/B3).

These map onto / extend the existing `transparent_matrix!` axes — add
`Fault::MisrouteToBackup`, the peer-silent flag, and emit the new rows there so
they also get the transparency check for free.

---

## 7. Implementation guidance per model (fill after the matrix is red)

### Reused funnel (both models)
Extract the reaper discharge into one helper so there is a single discharge
implementation ("manage as its own call, via the reaper"):
```rust
async fn discharge_as_own(ctx, call_ref, now_ms) {
    let call = ctx.state.peek(call_ref)?;
    let result = invariants::enforce(&ctx.obligations, &call,
        invariants::finalize(reaper::discharge_result(call.clone(), now_ms)));
    process_result(ctx, call_ref, result, now_ms).await; // CDR + limiter + delete + cleanup
}
```
Lifted verbatim from the reaper `OUTCOME_DISCHARGE` branch
([router.rs](crates/b2bua/src/router.rs)). `discharge_result` forces legs
terminal with **no wire traffic** — verify it does not re-BYE the already-probed
peer.

### If Model A (mixed)
- `CallQuiesced` handler branches on call state: `Active` → `SelfRelease`
  (unchanged, satisfies C11); `Terminating`/`Terminated` → `discharge_as_own`.
- **Must add cross-node exactly-once** for C10 / C2: a discharged-marker in the
  `(p,b)` context that the other node honors before writing a CDR / releasing.
  This is the expensive part; spell it out only if the matrix shows B failing.

### If Model B (primary-only) — recommended
- Backup terminal path: answer the wire, set `state = Terminated` in the
  context, **reverse-flush**, `drop_local` (no CDR, no limiter, no delete).
- Primary discharge path:
  - **reboot/reclaim:** `reclaim_into_live` (or `materialize_if_absent`) seeing a
    `Terminated` body immediately routes it through `discharge_as_own` instead of
    arming keepalives. Covers C4–C9.
  - **primary-alive misroute (C2/C3):** pick B1/B2/B3 — the puller applying a
    dominating `Terminated` body drives a live-map reconcile + `discharge_as_own`
    (B2 is the smallest local change). This is the one new path Model B needs.
- Guarantees: keepalive (`call_state == Active`) and limiter-refresh
  (re-arm only while `Active`) already refuse to touch a `Terminated` call, so a
  terminal context can never be "kept alive" — no zombie probing.
- Cost to document: limiter/CDR pinned until reclaim ⇒ **reboot-time SLA** on the
  primary; surface a `b2bua_terminal_pending_discharge` gauge so the pin is
  observable and the endurance gate can watch it.

---

## 8. First steps (do these in order)

1. **Infra (§5):** add `Fault::MisrouteToBackup` + `X-Test-Deliver` header in the
   harness proxy, the peer-silent axis, `assert_call_fully_over` (CDR-count +
   limiter gates), and `FailoverHarness::settle_terminal()`. Retrofit
   `limiter_ha.rs`'s manual advance loops onto `settle_terminal`.
2. **Red matrix (§6):** write C1–C11. Expect C1/C11 green, C2–C10 red. Record the
   *failure signature* per cell (which of the four invariants breaks: missing CDR
   / double CDR / limiter≠0 / trace survives) — this maps the blast radius and is
   itself the evidence for the model decision.
3. **Decide the model** from the C2/C9/C10 signatures (does mixed-mode need
   distributed exactly-once, or does primary-only's single reconcile path
   suffice?). Default to **Model B** unless C2/C3 prove the reconcile path is
   costlier than expected.
4. **Implement** the chosen model behind the now-red tests; turn the matrix
   green; only then re-run the reboot-focused endurance run to confirm the
   `limiter=N±3` gate passes and out-of-call no longer ramps.

### Re-enforce HA first (the very first commit)
The single most valuable first commit is purely test-side and ships **before**
any decision: add `assert_call_fully_over` (the one-CDR + limiter + no-trace
gate) and fold it into the existing `transparent_matrix!` sweep. That instantly
upgrades every existing HA cell from "transparent + no-leak" to also "exactly one
CDR + limiter released," which will flush out the current under/over-discharge
across the whole matrix and give us the failure map for free.

---

## 9. DECISION (2026-06-13) — Model Y + durable-backup fallback

Matrix state when decided: **7 pass / 5 fail** (C2, C3, C7, C10, C11). The
handoff proposed **B1** ("backup forwards statelessly when the primary is
alive"). **B1 is rejected:**

- It needs a *liveness oracle* — the only thing separating C2/C3/C11 (primary
  alive) from C4–C9 (crashed) is membership (`simulate_peer_removed`). Gating
  takeover on membership is the **eager/failure-detector** takeover ADR-0014
  amended out.
- "Forward to the primary" isn't viable: the proxy routed to the backup *because*
  it deems the primary dead, and pod-direct is forbidden. A B2BUA also can't
  stateless-forward a re-INVITE (independent a/b-leg CSeq spaces — C11).
- "Backup never discharges" regresses C4/C5: a permanently-dead primary means
  nobody writes the CDR.

### The chosen model — primary-preferred discharge, backup durable fallback

ONE discharge implementation (the reaper funnel `discharge_as_own`); **no second
cleanup path** (esp. none in the puller). Exactly-once is **causal**
(`(p,b)` + delete-wins), no reconciliation timer.

1. **Backup defers discharge.** On a reactive takeover that drives a terminal
   (BYE/CANCEL, or a timer-driven terminal), the backup answers the wire (SIP
   continuity), records the terminal into the `(p,b)` Element, reverse-flushes
   it, and **retains its takeover context with the call's own timers** — it does
   **not** discharge and does **not** `drop_local`. (Today's a2dcf4c
   discharge-on-`CallQuiesced` is replaced by defer-and-retain.)

2. **The reconcile seam (NEW).** The primary's **Reclaim-tail** puller — which
   ADR-0014 already created "to catch a post-partition reverse-flush a
   live-but-partitioned primary missed" — currently stops at the replica store
   (`repl/store.rs put_call/delete_call`; `repl/` never touches the live map
   `store/mod.rs inner.calls`). Extend it to reconcile into the **live map** when
   the primary holds the call live, `(p,b)`-gated:
   - Reverse **Put** that dominates AND held live → fold into the live copy; if
     the folded state is terminal → `discharge_as_own`. *(Fixes C2/C3; the
     non-terminal Put fixes C11's b-leg CSeq.)*
   - Any applied **Delete** (delete-wins) → evict the local live copy if present.
     *(Exactly-once: the discharge winner's delete evicts the loser's retained
     copy.)*

3. **Backup fallback when the primary is gone.** If the primary never reconciles
   (crashed / stays dead — C4/C5), the backup's **retained context** is the
   durable owner and discharges it. Trigger = **OPEN QUESTION below.**

4. **Guarantee (→ ADR amendment).** Exactly one CDR holds as long as the primary
   **or** the backup survives — and since the terminal state + obligations ride
   the replicated `(p,b)` Element, a backup that **restarts** recovers the
   Element and re-arms its timers via reclaim. The accepted loss window is "both
   primary and backup permanently gone." **Put the guarantee on backup
   durability/restart.** Amends ADR-0020 X3 (acting-backup terminal contract:
   defer, don't discharge-on-BYE) and ADR-0014 (Reclaim tail reconciles into the
   live map; self-release retains a terminal-deferred copy).

### RESOLVED (2026-06-13) — fallback = the existing replica-TTL "alive timer"

Confirmed current impl (reuse, do not duplicate):
- The "alive timer" IS `CallMeta.expiry_at_ms` (`repl/store.rs:57`), re-stamped to
  `now + ttl` on **every** primary forward-flush (`store/mod.rs flush` → `put_call`)
  — i.e. re-initiated on each change from the primary, exactly as specified.
- Default `CALL_TTL_MS` = 1h; runner retunes to 1.5× refresh cadence; the
  failover-harness injects nothing (1h).
- Expiry is **lazy + periodic `reap`** (runner loop `b2bua-runner/src/main.rs:685`),
  today a **silent `delete_call`, no CDR** — that silent drop IS the ADR-0020 X3
  accepted gap.
- This is NOT the reaper stamp (reaper = live-map, SIP-traffic-refreshed, excludes
  backup copies). The alive timer is replication-refreshed.

**The change:** TTL-expiry of a backup-held **terminal/deferred** Element
discharges via the funnel instead of silent-deleting — emitted as a synthetic
event into the router reentry channel (the reaper's verdict→router→`discharge_as_own`
pattern; the store has no `RouterCtx`). Obligations derive from the replica
snapshot (ADR-0020 X7), so no live copy is required.

**Two coupling facts:**
1. `1h ≫ settle_terminal (~345s)`. The harness must inject a **keepalive-scale**
   alive TTL (the reaper's 3× idea) so C4/C5 discharge in-window.
2. "Backup defers" and "expiry discharges" are ONE unit — landing defer without
   the fallback regresses C4/C5 (which pass today via immediate backup discharge).

### STATUS: COMPLETE (2026-06-13, branch `feat/fix-call-terminate-model-y`)

**Matrix 12/12.** Gates: b2bua --lib 129, transparent_v1 19, failover 14,
limiter_ha 2 — all green. Commits: `3b79198` (defer+reconcile+flush bug fix),
`5a7196e` (P3 fallback + resurrection tombstone), `1c4daa2` (HA tests → Model-Y
contract), + ADR-0020 X3 / ADR-0014 amendments. The competing **Model X +
discharged-marker** lives only as a handoff (`/tmp/handoff-model-x-discharge-marker.md`);
the X agent confirmed both models need the same tombstone, so the comparison
resolved toward Y (implemented + complete). Open follow-up (handed off): the LB
proxy does not relay the b2bua's 481 back to a caller's BYE client transaction
(C10's bob never receives its 481 — fire-and-forget in the test for now).

### Phased plan (discharge/BYE first, C11 last — per scope decision)

- **P1 — reconcile seam (foundation).** Puller, on an applied reverse mutation,
  signals the router (synthetic `InternalEvent`); the router, under the per-call
  lock, reconciles into the **live map** if held: Delete → evict (delete-wins);
  dominating terminal Put → `discharge_as_own`. (Non-terminal Put handled in P4.)
- **P2 — backup defers.** Replace the a2dcf4c `CallQuiesced`→discharge of a
  Terminating/Terminated takeover copy with defer-and-retain (record terminal,
  reverse-flush, keep the replica Element + its alive TTL; `drop_local` the live
  copy per ADR-0014 self-release — X11-safe).
- **P3 — fallback discharge.** Replica-TTL expiry of a terminal backup Element →
  funnel discharge (not silent delete). Harness injects a keepalive-scale TTL.
  → C2/C3 green, C4/C5 still green.
- **P4 — C11 (last).** Non-terminal dominating reverse Put → reconcile b-leg CSeq
  into the primary's live map. Gated by `(p,b)`; a racing primary keepalive that
  rejects it is ADR-0014's accepted CSeq-drop trade-off.
- **P5 — ADRs.** Amend 0020 X3 (defer, not discharge-on-BYE; TTL-expiry discharges
  — closes the gap) and 0014 (Reclaim tail reconciles into the live map; the
  exactly-once guarantee rests on primary-or-backup durability/restart).
- C7 (reclaim-vs-fallback) and C10 (split-brain) ride delete-wins; a surviving
  simultaneous race needs a discharged-marker on the Element (separate pass).

### (superseded) earlier open question — what triggers the fallback?

A deferred **Terminated** call on the backup has **no natural timer** (terminal
state cancels timers), yet C4/C5 must discharge inside `settle_terminal`
(~345 s). Candidates:

- **(A) Causal — primary leaves membership.** k8s removes the dead primary's
  endpoint (`simulate_peer_removed`); on that event the backup discharges the
  terminal copies it holds for that primary. Purely causal (no timer → best
  ADR-0014 fit), prompt (passes C4/C5). Cost: NEW membership→router hook (today
  membership reaches only the supervisor/pullers, never the router).
- **(B) Timer — the call's own retained cap.** Keep GlobalDuration (+ a
  Terminating watchdog) on the deferred copy; it fires → backup discharges.
  Matches "keep the context for timer," no new wiring — but it's a time-based
  discharge (the thing ADR-0014 warns against), and default GlobalDuration is
  3600 s ≫ 345 s, so C4/C5 would need a *new* short deferred-discharge timer.
- **(C) Both:** (A) as the prompt causal trigger, GlobalDuration as the ultimate
  backstop.

### Work order (per the scope decision)
**Discharge/BYE first, C11 last.** 1) reconcile seam (Delete-evict + terminal
Put → `discharge_as_own`) + backup defer-and-retain + the chosen fallback
trigger → turns C2/C3 green. 2) C11 (non-terminal Put → live-map CSeq reconcile)
last. C7 (reclaim-vs-fallback race) and C10 (split-brain) ride delete-wins; if a
truly-simultaneous race survives, they need a discharged-marker on the Element
(separate pass).
