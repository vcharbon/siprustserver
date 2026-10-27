# HA / chaos acceptance: what is a SUT bug and what is not

Triage guide for failover, chaos and endurance failures, and the HA design
invariants a fix must keep. The authority is
[ADR-0055](../adr/0055-services-are-ha-blind.md) §11; this page says where it
is enforced and how to read a failure against it.

## What an incident may cost

ADR-0055 §11 states, per incident, what a call may be left with: the CDR count,
what the parties may be left waiting for, and the **declared windows**, the
only RFC deviations an incident may cause. Nothing else is accepted. The
incidents are a primary killed, a backup killed, a split (healed or not), a
replication stall then a kill, and a call with no backup then a kill.

Read a failure in this order:

1. **Which incident, which turn point, which sync level.** A sweep cell is named
   `<shape>/<incident>/t<k>-<point>` ([harness-layers](harness-layers.md#the-failover-sweep)).
   The sweep runs at the replication sync level `B2BUA_REPL_SYNC` names
   (`applied` when unset). Under `applied` a turn's sub-points run `logged →
   applied → left`, so a kill at `applied` finds the write at the backup and
   none of the turn's effects on the wire; under `off`, or past the cap, they
   run `logged → left → applied` (ADR-0055 §3, §13). The turn's `Left` mark
   says why its entry left: applied, capped, at once, or its backup lost.
   A hand-written failover test states its incident in its module doc.
2. **Inside a declared window of that incident?** Then it is accepted, and the
   cell still asserts the outcome: no leak, full cleanup, the CDR count. The
   windows are opened in `failover-harness` (`sweep/cell.rs`,
   `sweep/stall_cseq.rs`, `sweep/stall_repeat.rs`) through the
   `accept_rfc_deviations_*` family and `accept_clean_end_requests`, each
   justified by the §11 entry it implements. The parties' verdict per incident
   is `sweep/outcome.rs`.
3. **Anything else is a SUT defect.** A call that ends cleanly once the incident
   struck is a pass (ADR-0055 §1, §4); a request left without its final, a
   dialog left open outside a lost call, a leak, or a CDR count outside §5 is
   not. A deviation no window covers is never given a new window without an
   amendment of ADR-0055 §11.

On a real cluster the HA tooling attributes every impact to the incident
window it fell in and judges it against that window's budget (`tooling/hakit`,
[ADR-0049](../adr/0049-the-ha-tooling-is-a-pluggable-base.md) §3.5). An impact
outside every window is a finding.

## Known non-SUT artifact: host clock skew (endurance on a VM host)

A failed-over call whose copy fires an in-dialog keepalive OPTIONS about one
keepalive interval early (at T+10 s on a 300 s cadence, racing the failed-over
re-INVITE that triggered the takeover) is a host clock-step artifact. Each pod
anchors wall time once at start (`sip_clock::Clock::system`). When the shared
host clock steps (post-sleep drift corrected by a stepping timesyncd), pods
anchored on either side of the step diverge for good. Its symptom is
`reinvite/unexpected "got OPTIONS expected 200"`.

The SUT bounds the residual; it does not fix the host:

- **Anchors (ADR-0055 §9).** A replication `Data` frame carries
  `origin_now_ms`; the receiver keeps `skew_offset_ms`, composed across every
  hop, and a local write resets it. Every read of a copy re-anchors its
  instants once (`router::restore_hygiene::reanchor`, 1 s deadband), then
  `sanitize_restored_timers` drops a stale `KeepaliveTimeout` and caps the
  keepalives. The `clock_wall_divergence_ms` gauge, and a warn on its rising
  edge past 500 ms, show the skew. Accuracy only: containment (§7) stays the
  correctness mechanism.
- **Infra.** Keep the host awake for whole endurance runs and use slewing
  chrony, not stepping timesyncd. `deploy/k8s/lib/host-checks.sh` `check_clock`
  warns on a WSL host.

The paused-clock harness reproduces a fixed inter-node skew
(`with_worker_clock_offset`; `failover.rs::skew_ahead_backup_no_immediate_options_at_takeover`
is the endurance twin). It cannot reproduce a host clock stepping mid-run.

## Design invariants: never reintroduce

- **No time-based settle or handback.** Two copies of a call are reconciled by
  containment (ADR-0055 §7), never by a timer or a settle window. A takeover
  copy is shed when the transactions it serves clear (the transaction layer's
  `CallQuiesced`, `router::quiesced`) or stands down on a write of its primary
  that contains it (`router::stand_down`), never on a clock.
- **Keepalive smoothing lives in the bulk reclaim**
  (`router::reclaim::bulk::reclaim_all` passes `Smoothing` to
  `sanitize_restored_timers`), never in the timer driver (ADR-0014, "Keepalive
  smoothing under the two-stream model"). Performance only: no correctness
  role, no timing assumption.
- **A discharge of a body already terminal stays off the caller's wire**
  ([ADR-0022 X5](../adr/0022-initial-invite-final-response-guarantee.md)): those
  funnels enforce with `UnansweredCaller::Leave`, and only a live-serving
  funnel answers an unanswered caller (`Answer`). The node holds no server
  transaction for her INVITE, and a final from it would serve her twice.
- **Pristine reboot.** A restarted node, a backup included, comes back with
  zero live calls before reclaim; the failover harness hard-asserts it.
- **Only the paced sweep evicts expired replica bodies**
  (`router::reap_expired_replicas`, the node's `replica_reap` step). No read
  evicts one: a body evicted out of band loses its limiter release, its lost-CDR
  count or the CDR of a ghost call (ADR-0055 §5).

## References

- [ADR-0055](../adr/0055-services-are-ha-blind.md) §11: the per-incident
  acceptance and the declared windows; §5 the CDR counts; §9 the anchors
- [harness-layers.md](harness-layers.md): the failover sweep, its turn points
  and the ratchet
- [ADR-0022](../adr/0022-initial-invite-final-response-guarantee.md) X5: the
  final-response guarantee and its HA split
- [ADR-0049](../adr/0049-the-ha-tooling-is-a-pluggable-base.md) §3.5: incident
  windows and impact budgets on a real cluster
- `deploy/k8s/lib/host-checks.sh`: `check_clock` and the other pre-run checks
