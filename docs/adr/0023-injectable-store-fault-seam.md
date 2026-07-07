# 0023 — Injectable store-fault seam and defined store-failure semantics

**Status:** accepted (2026-07-07)

## Context

The B2B callflow specs carry a family of "dialog KO" cases modeling a
reference implementation's Redis dialog-store outages: the dialog/call lookup
fails during the initial INVITE, during an established-call BYE, or during an
audit (keepalive) cycle, each with a defined outcome. Nothing here could be
exercised, and the product-side semantics were **undefined**, not merely
untested:

- The live call-handling lookups are **infallible in-memory sync reads**
  (`CallState::resolve_from_sip_key_sync` / `peek`) — there is no failure to
  observe and no store-failure → SIP-error path.
- The fallible seam exists but is persistence/failover-only: `CallStore`
  returns `Result<_, StoreError>` yet never surfaces into live handling.
- The test harness hardcoded `InMemoryCallStore` with no override
  (`spawn_b2bua_core`).

Constraint: live handling stays **synchronous in-memory** — this ADR does NOT
convert live lookups into async store reads. The seam makes the existing sync
lookups *fallible* by consulting a probe before the map read.

## Decision X1 — one generic fault seam, two halves, one handle

`b2bua::store::faults` provides:

- **`FaultInjectingCallStore`** — a decorator over any `Arc<dyn CallStore>`;
  each op (`get_call`/`put_call`/`delete_call`/`refresh_call`/`get_index`/
  `scan_calls`) can be made to return `StoreError::Backend`.
- **`StoreFaults`** — the shared, clone-cheap control handle: atomic per-path
  switches (`arm`/`disarm`, one `StoreFaultPoint` per decorator op and per live
  probe site) a test flips MID-CALL. `Default` = no faults, so the production
  wiring pays one relaxed atomic read per guarded path.
- **The live-path probe** — the same handle, threaded through `B2buaDeps`
  (default no-fault) into `RouterCtx`; the router consults it *before* the
  sync map read at exactly three live-serving sites: the initial-INVITE
  dialog-existence check, the in-dialog per-event call fetch, and the
  keepalive/audit timer read.

The seam is generic: no callflow-specific logic, only "this read fails now".

## Decision X2 — the defined store-failure semantics (deterministic)

| Path (probe point) | Semantics |
|---|---|
| Initial INVITE (`LiveInitialInvite`) | **Fail closed: 500** `Server Internal Error` final through the INVITE server txn; **no call created**, nothing leaked (the per-call dispatch ephemera is reclaimed via the orphan teardown, exactly like the Tier-3 overload reject). Composes with ADR-0022: the caller who heard the auto-100 always gets a final. Probed *before* the retransmit `peek` — a faulted store cannot answer "does this dialog already exist", so its answer is not trusted. |
| In-dialog request — BYE, re-INVITE, … (`LiveInDialog`) | **Fail closed: 500** to that request; the call and its state stay **untouched**. Deliberately distinct from the `481` lookup-*miss* (the call may well exist; the store just cannot say). A retry after the store recovers proceeds normally. ACK is never answered (RFC 3261 §17) — dropped, as the orphan path drops it. |
| Audit/keepalive timer (`LiveAudit`) | **Fail open**: skip the probe cycle, keep the call up, and **re-arm** the `Keepalive` timer at the config cadence so liveness detection resumes next interval. A store fault alone must never tear down an established call — the protected-calls invariant ([ha-acceptance.md](../testing/ha-acceptance.md)). Observable via `b2bua_store_fault_audit_skipped_total`. |

Status-code note: **500**, not the ADR-0022 X3 canonical 503. The X3 rationale
(transient, retryable, matches the decision/overload paths) is about *setup
inability*; a store fault is an internal server error the specs pin at 500,
and keeping the codes distinct lets a trace tell "decision/overload shed"
(503) from "store outage" (500) at a glance. No Reason header (the bare
canonical reject shape); the fail-closed paths are counted on
`b2bua_store_fault_rejected_total`.

Deliberately **un-probed**: SIP responses, CANCEL/timeout/internal events, and
every non-`Keepalive` timer. Those paths owe no store-derived answer, and
absorbing e.g. a keepalive OPTIONS-200 under a fault would convert a store
fault into a `KeepaliveTimeout` teardown of a healthy call — the exact outcome
X2 forbids.

## Decision X3 — HA scoping: the reclaim/reconcile plane is out of bounds

The probe sits ONLY on live-serving lookup sites. The HA replication/reclaim
paths — reclaim discharge, the `(p,b)` reconciliation, `hydrate_from_replica`'s
replica read, the terminate writer — change **no** behavior: they already ride
the fallible `CallStore` seam and keep their existing error handling (ADR-0014:
reclaim discharge never touches the SIP wire; ADR-0022 X5). Wrapping a
replicating store in the decorator is possible (the decorator is
implementation-agnostic) but is not done by any production wiring.

## Decision X4 — harness injection

`B2buaSutBuilder` gains `.with_store(Arc<dyn CallStore>)` (default: the
historical fresh `InMemoryCallStore`) and `.with_store_faults(StoreFaults)`.
The latter uses the SAME handle twice: wraps the store in
`FaultInjectingCallStore` and threads the live-path probe — one control drives
both halves. `B2buaSpawnParams` carries both as defaulted `Option`s;
`failover-harness` passes `None`/`None` (behaviour identical).

## Consequences

- The downstream dialog-KO spec cases become mechanical: Scene + armed
  `StoreFaults` + assert the defined outcome
  (`b2bua-harness/tests/store_fault.rs` pins all three semantics).
- Two new counters: `b2bua_store_fault_rejected_total` (fail-closed 500s) and
  `b2bua_store_fault_audit_skipped_total` (fail-open skipped cycles); both 0
  unless a fault is armed.
- A future *genuinely* fallible live store (e.g. remote dialog storage) has its
  semantics pre-decided: implement X2 at the same three sites.

## Alternatives considered

- **Convert live lookups to async `CallStore` reads.** Rejected: re-architects
  the sync single-task hot path (and its FIFO/lock discipline) for a
  testability need; the probe delivers the same observable semantics.
- **503 instead of 500.** Rejected (X2 note): the specs pin 500 for a store
  outage, and the distinct code separates store faults from decision/overload
  sheds in traces and CDR-less rejects.
- **Fail-open on the in-dialog request too.** Rejected: silently processing a
  request whose call state could not be trusted (had the store been real)
  invites divergent dialog state; a 500 is honest, retryable, and leaves the
  call intact.
- **Tear down the call on an audit-read failure.** Rejected outright: violates
  the protected-calls invariant — a store fault alone must never drop an
  established call.
