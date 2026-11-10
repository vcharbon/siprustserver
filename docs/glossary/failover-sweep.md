# Failover sweep vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Failover sweep vocabulary".

The in-process proof of HA-blind services
([ADR-0055](../../docs/adr/0055-services-are-ha-blind.md) §11, §13), in
`crates/failover-harness`.

**Turn point**:
One sub-point of one turn a call takes at its primary, the turns numbered from
1: the turn's write handed to replication (`logged`), its write applied at the
backup (`applied`, only for a turn that wrote), its last outbox entry left
(`left`), in that order under the `applied` sync level with no cap hit, and
`logged`, `left`, `applied` under `off` or past the cap. Where a sweep incident
arms; the harness holds the turn there until the incident struck.
_Avoid_: "safe-point" (the transparent matrix's quiescent states, ADR-0013).

**Failover sweep**:
Every shape of a catalogue cut at every turn point by each sweep incident
(primary killed, primary killed unannounced, primary killed then rebooted,
backup killed, split, split then healed, stalled then killed, unbacked then
killed), at each replication sync level, one test per shape,
incident and level. Each cell must end with nothing left on any node and an
RFC-clean, fully terminated call the parties accept, with one CDR or, when the
primary dies, two; what an incident may cause beyond that is the **window
table**'s, opened by the cell's **tier**. A consumer adds shapes with
`failover_sweep!` and a ratchet of its own; a shape may name callee roles that
share the callee's address, told apart by the Request-URI user part, and may
stay out of the split that never heals. Its lane is `just sweep`;
`just sweep-report` counts its rows and tiers.
_Avoid_: "matrix" (the hand-listed transparent matrix of ADR-0013).

**Sweep ratchet**:
`failover-sweep.ratchet`: the cells that fail today, each with the ADR section
that owns its correction and its cause, and each test's count of Guaranteed
cells. A failing cell it does not list fails the test, and so does a listed
cell that passes, or a Guaranteed count other than the listed one; it only
shrinks.
_Avoid_: "allow-list" or "waiver" (a listed cell still fails; the ratchet
only records that it is known).

**Clean end**:
The system under test ending a call the way RFC 3261 allows after an incident:
a non-2xx final to an INVITE it holds unanswered, a BYE of every dialog the
record or the evidence names, a NOTIFY ending a subscription (ADR-0055 §1, §4).
A party it ends cleanly drops her remaining goals; once the caller is ended so,
the call is (`CallCtx::accept_clean_ends`, armed when the incident strikes). A
request left without its final or a dialog left open is never a clean end.
_Avoid_: "dropped call" for it (a clean end is a pass of the cell).

**Window table**:
Every deviation from a perfect call an incident may cause, one named row each
(`W-…`, the id of its ADR-0055 §11 bullet): an RFC-rule window, a
dialog-verdict waiver, a party outcome, a clean-end goal drop or a CDR count.
A row names the strongest **tier** it opens in, its incidents, its scope and
its predicate, and an upstream witness cell that must fire it. A test strikes
an incident (`FailoverHarness::strike`) and is given its rows; every accepted
finding names its row, the first in table order. A hand-written test's fault
no incident names keeps its windows under a transitional row (`T-…`).
_Avoid_: "waiver" (a waiver is not tied to an incident or to §11), "accepted
trade-off", "declared window" for a window a test opens itself (none does).

**Tier** (Guaranteed / Degraded / Off):
How much a cell may deviate, measured from its run (ADR-0055 §11).
**Guaranteed**: level `applied`, a kill of a cluster whose every call was
backed and bootstrapped at every turn up to the strike, no outbox entry hitting
the cap, no replication stream or replica tampered with: no row opens, the
evidence rule ends no call, but for a re-INVITE a proxy refuses after the kill,
whose ACK the copy reads as evidence (ADR-0055 §11). **Degraded**: level `applied`, any other case,
every row but the off-only ones. **Off**: level `off`, every row.
_Avoid_: "transparent" for Guaranteed (the transparent matrix is ADR-0013's).

**Declared window**:
An RFC-rule window or dialog-verdict waiver of the **window table**, open from
the strike of its incident, scoped to its rule and its message.
_Avoid_: "waiver" (a waiver is not tied to an incident or to §11), "accepted
trade-off".
