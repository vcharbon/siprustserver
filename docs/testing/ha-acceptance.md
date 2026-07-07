# HA / chaos acceptance — what is a SUT bug and what is not

Triage guide for failover, chaos, and endurance failures, plus the HA design
invariants that tests (and fixes) must not violate. The authority is
[ADR-0014](../adr/0014-reactive-only-takeover-version-vector.md) — especially
its "Accepted trade-offs", "Consequences for tests", and the skew re-anchoring
amendment; this page is the operational summary.

## The protection boundary (ADR-0014)

A failover/kill failure is **not** automatically a SUT bug.

- **Protected:** ringing and established calls. A **confirmed call dropping**
  is always a genuine bug.
- **Accepted collateral:** a call whose dialog state changed **within the
  acceptance window (default 200 ms, `LOADGEN_CHAOS_PHASE_TOL_MS`) of a kill**
  may take a small impact — the forked-b-leg confirm race (a call confirming
  AT the kill).
- Any failure whose state change was **outside** the window is genuine.

The loadgen is chaos-aware and auto-buckets accepted collateral (`POST /chaos`
+ per-call phase markers) as `chaos="near"` vs genuine `chaos="clear"`.
**Triage `chaos="clear"`.** See
[crates/loadgen/README.md](../../crates/loadgen/README.md).

## Known non-SUT artifact: host clock skew (WSL2 endurance)

A failed-over call whose backup fires an in-dialog keepalive OPTIONS **~one
keepalive interval early** (e.g. at T+10 s on a 300 s cadence, racing the
failed-over re-INVITE that triggered the takeover) is a **host clock-step
artifact**, not a failover bug. Each pod anchors wall time once at start
(`sip_clock::Clock::system`); when the shared WSL2 host clock **steps**
(post-sleep drift "corrected" by stepping timesyncd), pods anchored on either
side of the step diverge permanently, and a replicated absolute `fire_at` from
the dead node becomes past-due on the takeover node. Root case:
endurance-20260630 `reinvite/unexpected "got OPTIONS expected 200"`.

Two halves, do not conflate them:

- **The SUT bounds the residual (accuracy only).** The replication `Data`
  frame carries `origin_now_ms`; the receiver persists `skew_offset_ms`; the
  ONE router seam `sanitize_restored_timers` re-anchors every restored
  `fire_at` on EVERY hydration path (with a sub-second deadband, stale
  `KeepaliveTimeout` drop, defensive floor, cohort smoothing). Observability:
  `clock_wall_divergence_ms` gauge + rate-limited warn (>500 ms). This is the
  ADR-0014 amendment; `(p,b)` reconciliation stays the sole correctness
  mechanism, and **the re-anchor never moves into the timer driver**.
- **The infra advice still stands** — the SUT bounds the residual, it does not
  license drift. Keep the host awake for whole endurance runs; use slewing
  chrony, not stepping timesyncd. `deploy/k8s/lib/host-checks.sh::check_clock`
  warns; `run.sh up` resyncs once before pods anchor.

The paused-clock harness closes the skew fidelity gap with
`with_worker_clock_offset` (deterministic inter-node skew;
`failover.rs::skew_ahead_backup_no_immediate_options_at_takeover` is the
endurance twin) but still cannot reproduce a host clock stepping *mid-run*.

## Design invariants — never reintroduce

- **Reconciliation is `(p,b)`-causal; no time-based settle/handback,
  anywhere.** A partition can route a dialog to the backup at any time, for
  any duration; correctness must not depend on a timer or settle window. The
  acting-backup **self-releases** a takeover copy on the served transaction's
  terminal state (`CallQuiesced`), never on a clock. Do not reintroduce a
  `Deactivate`/watermark handback or a "wait N seconds then drop" rule.
- **Keepalive catch-up smoothing lives in the reclaim handler
  (`router::reclaim_all` → `smooth_keepalives`), never in the timer driver**
  (ADR-0014 §4). Both cohorts: past-due oldest-first (bounded speed-up) and
  future-dated de-correlated **earlier only**. Performance only — no
  correctness role, no timing assumption. (Skipping it reproduced the
  2026-06-12 reboot keepalive-burst throughput collapse.)
- **Reclaim discharge stays off the SIP wire**
  ([ADR-0022 X5](../adr/0022-initial-invite-final-response-guarantee.md)):
  `answer_unanswered_a_leg = false` on the two HA discharge helpers for
  already-terminal reclaimed/folded bodies — a reclaiming node holds no live
  server transaction, and putting a final on a wire it doesn't own recreates
  the double-serve class ADR-0014 structurally removed. `true` only on
  live-serving funnels. This flag is a correctness knob, not cosmetics.
- **Pristine reboot.** A restarted node (including a backup) must come back
  with zero live calls before reclaim; the failover harness hard-asserts it
  and endurance treats it as an invariant.

## References

- [ADR-0014](../adr/0014-reactive-only-takeover-version-vector.md) — takeover
  model, accepted trade-offs, consequences for tests, skew amendment
- [ADR-0022](../adr/0022-initial-invite-final-response-guarantee.md) — the
  final-response guarantee and its HA split (X5)
- [crates/loadgen/README.md](../../crates/loadgen/README.md) — chaos
  correlation, report buckets
- `deploy/k8s/lib/host-checks.sh` — `check_clock` and other pre-run checks
- `crates/failover-harness/tests/failover.rs` — e.g.
  `reboot_reclaim_exactly_one_owner…`, `skew_ahead_backup_no_immediate_options_at_takeover`
