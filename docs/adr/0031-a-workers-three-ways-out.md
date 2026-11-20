# 0031. A worker's ways out: graceful, abrupt, vanished, restarted in place

**Status:** accepted. Amended by ADR-0047, ADR-0050 §3 and §11, and ADR-0055 §3, §7, §8, which
replaces D2, D4 and the per-axis fold of D3 with containment and one routing rule.

**Source:** `crates/failover-harness/tests/it/withdrawn_primary_answers_in_its_window.rs`.
Amends ADR-0012 D4 and ADR-0014 §3 and §12. Builds on the departed-address tombstone
(`crates/sip-proxy/src/registry/tombstone.rs`).

Case table, diagrams, long form of each rule: [appendix](appendix/0031-a-workers-three-ways-out.md).

## Context

A worker leaves service in four ways, read from the EndpointSlice conditions `ready`, `serving`
and `terminating` (absent `serving` defers to `ready`, absent `terminating` is false).

1. **Graceful** (rolling update, drain, scale-down; the frequent case). SIGTERM at +0,
   `ready=false, terminating=true`, still in the slice until the pod is gone (14 s at most).
2. **Abrupt** (`delete --force --grace-period=0`). Out of the slice at +0, the process runs
   2 s more, the same ordinal starts at a new address from +0.4 s.
3. **Vanished** (node crash, partition). 3a: the slice unchanged for about 40 s. 3b:
   `ready=false`, then `terminating=true`, still in the slice. The process may be alive.
4. **Restarted in place** (OOM-kill, readiness flap). `ready=false, terminating=false`, same
   address.

Leaving the slice, or in case 3a the proxy's probe verdict, starts the backups' takeover of
the calls with an INVITE in flight (ADR-0047).

Two hazards drive the decision. In case 1 the peers park the worker the instant its endpoint
drops, so what its drain authors lives on one node, and the reclaimed or takeover copy authors
a second final onto an ACKed INVITE (RFC 3261 §17.2.1, §13.3.1.4). In case 3 a healed
primary's forward flush would regress or wipe the survivor's Element: `(p+1, 0)` is never
dominated by `(p, 1)`.

## Decision

With one worker, D1 and D2 have no peer and the drain is quiescence-or-grace.

**D1. A member still in the slice stays a replication peer; only a `ready` member is a routing
target.** The informer emits every endpoint in the slice as a `Peer` carrying `ready` and
`terminating`. A condition flip is a delta, never a host move. *Pullable* is present in the
slice, whatever its conditions (never keyed on `serving`). *Routable* is `ready`:
`WorkerSet::recompose` filters at the projection, and a `ready=false` member departs and its
address is tombstoned. With no member ready, every member present and not `terminating` is
routable on its probe's answers (ADR-0050 §11). A member that is only pullable is not a backup
target.
Detail: [appendix D1](appendix/0031-a-workers-three-ways-out.md#d1).

**D2. The drain of a withdrawn worker exits when its backups hold its calls.** All three hold:
(i) the worker observed its own withdrawal (D6); (ii) `B2BUA_DRAIN_MIN_MS` (1000 ms) elapsed
since SIGTERM; (iii) for every live call, the Backup flow from `topology.bak` is connected and
applied this worker's changelog head, per the puller's position frame (stale after two `Noop`
intervals). The grace (`B2BUA_DRAIN_GRACE_MS`, 5 s) is the ceiling and
live calls clearing is an exit. Exit reasons are counted: `quiescent`, `caught_up`,
`grace_peers_behind`, and `grace` for a worker that is not withdrawn, which keeps
quiescence-or-grace. Every exit first flushes the queued limiter releases within
`B2BUA_DRAIN_RELEASE_FLUSH_MS` (3 s, ADR-0040 decision 9). Exiting `caught_up` with live calls
is a replicated crash, by design.
Detail: [appendix D2](appendix/0031-a-workers-three-ways-out.md#d2).

**D3. A forward flush never regresses backup progress.** The puller's mode splits into `Forward`
and `Bootstrap`; bootstrap takes the replica's `(p,b)` as-is (ADR-0014 §3). In `Forward`, a
`Put` is refused when its `b` is behind the Element's, or when, the vector split, its body is a
branch behind the Element's. A `Delete` is refused only when the Element is `Active`, its
caller was answered, and the authority never published its own answer. Both refusals are
counted. A refused body folds into a live copy only as progress on the chain (amendment
below). When the primary folds the backup's reverse flush it adopts `b`, and the split heals.
Detail: [appendix D3](appendix/0031-a-workers-three-ways-out.md#d3).

**D4. Routing around a leaving worker has two signals, one branch.** The membership tombstone
(cases 1, 2, 4) and the OPTIONS probe (case 3a) both land on `Dead`, and the request and response
paths take the backup on it. The probe's verdict is told to the workers, signed (ADR-0050 §3);
the tombstone's is not. The response path takes the backup for INVITE responses only (RFC 3261
§17.1.2). The `w_pri`/`w_bak` cookie on the Record-Route and the pushed Via carries the dialog
(ADR-0055 §8). The tombstone re-arms on every response for its address, so it lives Timer H
past the last sign of life; a worker rejoining at the address clears it.
Detail: [appendix D4](appendix/0031-a-workers-three-ways-out.md#d4).

**D5. The leaving worker keeps serving what still reaches it, and stands down only through the
drain.** Convergence is by folding, never by a primary going passive. A withdrawal observed with
no SIGTERM following runs the same D2 drain and the process exits. A worker never stays
`Draining` while it runs.
Detail: [appendix D5](appendix/0031-a-workers-three-ways-out.md#d5).

**D6. A worker observes its own endpoint.** One predicate on its own `targetRef.name` reads it
withdrawn: `terminating` and not `ready`, or absent, once it has seen itself `ready`. On that
signal it latches `Draining` even if SIGTERM is late. A worker on a static membership never
observes itself. Detail: [appendix D6](appendix/0031-a-workers-three-ways-out.md#d6).

Case 1 engages D1, D2, D3, D6; case 2 only the tombstone (the elder's own timers in its 2 s are
lost); case 3 D3, and D1 in 3b; case 4 D1. Detail: [appendix, the cases](appendix/0031-a-workers-three-ways-out.md#the-cases-after-this-decision).

## Amendment: progress is a chain, and every refusal moves the counter

ADR-0055 §7 replaces this fold with containment.

- Progress is a chain on two monotone axes, *answered* and *ending*. A refused flush folds
  only when it made every step the live copy made and one more.
- A re-INVITE round is a third chain, per dialog, ordered by CSeq; its end or opening folds
  alone (`call::helpers::merge_rounds`).
- A fold takes `max(p)` and `max(b)` and is not a write. A refusal adopts the other owner's
  axis without a bump, and flushes the adopted view.
- No fold or refusal lowers a dialog sequence (RFC 3261 §12.2.1.1).
- An adopted counter is marked (`CallTopology::gen_adopted`); a marked body folds only through
  its progress.
- The `Delete` guard keys on the answer, carried on the delete's tombstone
  (`Frame::Data.answered`).
- Only a primary discharges a call ended by a fold.

Detail: [appendix, amendment](appendix/0031-a-workers-three-ways-out.md#amendment).

## Considered and rejected

- Stop authoring on SIGTERM: a direct-bound worker must finish its calls (D5).
- A fixed tombstone TTL from departure: it expires under a live elder whose grace was raised.
- A compaction that keeps a `Put` ahead of a `Delete`: every call end ships a body plus a
  `Delete`, and a re-bootstrap still sees the `Delete` alone.
- A push-based flush: replication stays pull-only (ADR-0011).

## Consequences

- `topology::Peer` carries `ready` and `terminating`; `peers_from_slices` emits every endpoint.
- The puller sends a position frame per applied batch. Counted: drain exit reasons, refused
  forward flushes; a histogram of time in drain; a gauge of pulled-not-ready peers.
- Residuals: the dual-owner window of a partition (ADR-0014). A partitioned primary that
  discharged a call the survivor kept serving loses the answered call's record. The `Delete`
  guard protects the answer only, never a later in-dialog turn.
