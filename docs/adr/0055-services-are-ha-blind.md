# Services are HA-blind

**Status:** accepted (2026-10-23)

The [appendix](appendix/0055-services-are-ha-blind.md) holds each section in full under the
same number, the window table and the rejected alternatives. Read it to change the mechanism.

**Amends:** ADR-0011 X5, ADR-0014, ADR-0016, ADR-0020 X3, ADR-0022, ADR-0031 D2 and D4,
ADR-0037 items 4 and 6, ADR-0042 §2, ADR-0045, ADR-0047 D5. The appendix names each part.

## Terms

- **Call record**: the replicated `Call`.
- **Request record**: one transaction the call takes part in (§2).
- **Call incarnation**: the RFC 3261 §8.1.3.5 retry identity of a call.
- **Worker identity**: one boot of one worker ordinal (§6).
- **Owner**: the primary or the backup of a call.

## Context

A service's state replicates with the call record, without an opt-in. Below the seam, each
in-dialog method is its own HA problem: the wire runs ahead of the replica, only INVITE is a
round, and a per-axis reconciliation drops progress made on the other copy.

## Decision

### 1. The contract

A service or per-method author writes single-node logic. HA lives only in the aware crates
(§11). A service relies on four facts, each already true on a single node:

- every per-call fact lives in the call record;
- an off-turn request is made for its answer alone: the backend may receive it twice, or never
  for an ending call (ADR-0039);
- a call may end cleanly at any instant (§4);
- a service keeps durations or timers in its own state, never absolute times.

### 2. One request record for every transaction of a call

Every transaction of a call, any method, received or sent, is one request record. It outlives
its dialog until it settles. One settle entry answers what the call holds or owes when a leg
ends, the call is released or termination begins. Discharge, reclaim and takeover never call
it. A service may hold a received non-INVITE request for a later turn, under a deadline below
Timer F. An INVITE is never held (500). Takeover seeds a transaction from every request record.

### 3. The replication write path

- A turn writes the call record into the change log before any effect leaves the node.
- The log holds one entry per call and never drops it. The delete is last.
- Replication is pull-only and sent on write.
- A call has no backup, for its whole life, when it is admitted over the backlog high-water
  mark, when its backup refuses the replica at its memory ceiling, or when its write cannot
  reach the backup. No call is refused for the backlog.
- Every effect a turn sends leaves through the call's one ordered outbox. Under sync level
  `applied` (the default) an entry leaves once the backup applied its write, or at the cap
  (100 ms). Under `off` it leaves once the write is logged. No entry waits without a cap.
- A graceful drain is transparent. At its grace it ends each call no peer is shown to hold. A
  kill is not: what left at the cap, or under `off` before the backup applied it, is evidence
  for §4.

### 4. Evidence the call record cannot explain: one rule, any method

One rule in `b2bua-core`, read before every rule of the chain, ends the call cleanly when a
message proves the record is behind the wire. Its triggers are closed:

1. a final whose CSeq is above the dialog's local sequence;
2. a final under a branch of ours no request record holds, at the CSeq of an open request;
3. an ACK for a final the record never sent, only on a call a takeover copy served;
4. a callee's BYE on a dialog the record holds as early;
5. a 2xx whose via-extension (§12) names a leg the record lacks;
6. a caller's re-INVITE while the record holds her INVITE unanswered, on a call a copy served.

The rule sends what RFC 3261 requires for the message, BYEs every dialog the record or the
evidence names, writes one CDR and releases every hold. It repairs nothing.

### 5. The CDR: whoever ends the call writes it

The CDR carries the `call_ref` and the call incarnation, with no key and no dedupe. A kill of a
primary with a backup never loses the CDR and may write two. A split may lose it. A call with no
backup leaves one or none. A quiet call of a departed primary stays reactive: the replica's TTL
reap ends it and writes its CDR.

### 6. One author per ordinal: identity, compared for equality

A worker identity is the pod UID, or a per-boot token in a static pool. A worker that finds its ordinal bound to another identity enters `Superseded`: it stops authoring
at once and exits without draining. No order is drawn from a clock.

### 7. Two writers after a split: containment

Each copy carries a state stamp, the vector of its last write that changed a compared field. R
contains L when L's stamp is at or below R's vector. One contains the other: take it whole and
merge the exempt fields. Each contains the other: take the primary's. Neither: end the call
cleanly (§4) over the union of both copies' dialogs. The exempt fields are a closed list, so a
new field is compared. Leg ids come from two spaces (`b-{n}` on the creating node, `c-{n}` on
any other).

### 8. The proxy routes on one rule

Every leg's cookie names the call's primary and its backup, on every method. An in-dialog
request, or a response to a leg's dialling INVITE, goes to the primary while it is alive, else
to the other owner. Any other response goes to the owner that sent its request. With no owner
alive the proxy drops the message. A returned worker parks its calls with nothing armed or
sent, and arms them only while its endpoint reads ready.

### 9. Every instant in the call record carries its own anchor

Every absolute instant replicates with the skew composed across hops, and a local write resets
the offset.

### 10. The seam carries no HA value

A leg's ring state is `Silent` or `Rang`. A timer declares its scope (ADR-0042), and the turn
engine drops a fire whose scope ended, so a rule never re-checks for a stale ledger. `CallEvent`
and `B2buaConfig` carry no HA field.

### 11. The proof

- Each crate declares `[package.metadata.ha] side`. `scripts/purity.py` checks that a blind
  crate depends only on blind crates.
- A word net refuses `takeover`, `reclaim`, `tombstone`, `materialise`, `stand-down` and
  `replica` in the blind crates, with no allowlist.
- A failover sweep cuts every catalogue shape at every turn point, by every incident, at both
  sync levels. A cell's tier is Guaranteed (`applied`, backed, no cap hit, a kill: no
  deviation), Degraded or Off. Each accepted deviation is a row of the window table.
- A ratchet lists the failing cells and each test's Guaranteed count. The list only shrinks.

No tier guarantees an unreliable provisional across a crash.

### 12. A request the worker originates names its leg

The worker's own Via carries a signed via-extension, so an owner holding no request record of
a dialled INVITE can CANCEL it, ACK its final and BYE its 2xx from the response. A worker with
replication on refuses to start without the signing key.

### 13. Harness verbs

`failover-harness` places a call on a named primary and backup, steers a party through real
causes, and arms an incident at a turn's sub-point (`Logged`, `Applied`, `Left`).

### 14. A leg states one Contact

A leg's Contact is allocated once, at the leg's creation, and stored in the call record. Every
copy states that one. Behind the proxy it names the proxy's peer-facing address.

## Considered and rejected

Reasons in the appendix: waiting for the backup with no cap; a `sent` sync level; dropping a
write on a full queue; refusing new calls on backlog; repair by inference; a per-axis fold; an
ordered epoch; the proxy refusing a worker's own request; a word list over every crate; an
exactly-once CDR; three ways of letting a takeover copy keep a call its primary returned for.

## Consequences

- A call whose write was lost at a kill, or whose copies diverged across a split, ends cleanly.
- A new in-dialog method gets HA by a row in the method table and a shape in the sweep.
