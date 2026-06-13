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
