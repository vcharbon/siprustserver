# A departed peer's in-flight INVITE calls are taken over

**Status:** accepted. Amended by ADR-0022 X1 (D4), ADR-0039 decision 5 (D2), ADR-0050 §3
(D1, D2) and ADR-0055 §3–§7 (D2, D5, D7); the [amendment record](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#amendment-record)
lists each change.
**Amends:** ADR-0014 D1 (takeover is reactive only), ADR-0022 X4 and X5, ADR-0031's case
table and its D3 split rule (the lifecycle order the forward and reverse gates read),
ADR-0020 X3 (a copy whose primary departed discharges the call, D8).

Owner modules, test cells and the long form of each rule are in the
[appendix](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md).

## Context

The stateless front proxy relays a worker's 100 Trying (RFC 3261 §16.11, ADR-0022 X4). A
caller that heard it runs no Timer B and sends nothing more until a final (§17.1.1.2).
With reactive-only takeover (ADR-0014 D1), a call whose serving node dies while nothing
arrives never gets a final. Four windows are open: a new call with its routing decision in
flight (W1), a routed call with a silent callee (W1b), a replicated re-INVITE round with a
silent callee (W2b), and a re-INVITE round not yet in the replica (W2).
[Detail](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#context).

## Decision

### D1. A peer that leaves the membership set is announced once

The replication supervisor parks a peer whose ordinal left the membership and, in that
step, sends the router `PeerDeparted(ordinal)`, once per real stop. A front proxy's dead
verdict is the second trigger (ADR-0050 §3): the signed `X-Worker-Dead` notice on an
out-of-dialog OPTIONS. The proxy reads a peer dead when its `HEALTH_THRESHOLD`-th probe goes
unanswered (2.6 to 3.6 s after the death by default), or when a worker it read `Alive` flips
its endpoint not ready and not terminating. A notice naming this node is ignored.

The backup confirms the verdict on its own Backup flow from that peer: the stream ends with
no frame, or one immediate connect is refused or runs out its bound. A frame from the peer
after the verdict refutes it, unless it carries another incarnation generation, which
confirms it. A write of a later generation stops the verdict's sweep. Two independent signals
must agree, so a notice is advisory.
[Detail](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#d1-a-peer-that-leaves-the-membership-set-is-announced-once).

### D2. The departure sweep takes over the calls with an INVITE in flight

A task of its own reads the departed peer's backup partition and materialises as a takeover
every call that names an INVITE transaction in flight, an in-dialog request in flight other
than the keepalive, or, `Active`, an awaited answer to a request it sent off its turn
(ADR-0039 decision 5). A call already live here is left as it is. A call with nothing in
flight stays reactive (ADR-0014 D1). The takeover half runs on either trigger. The discharge
half (D8, ADR-0055 §5) runs on a departure only, as a peer read dead may live behind a
partition; its limiter release runs on the verdict too (D8).
[Detail](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#d2-the-departure-sweep-takes-over-the-calls-with-an-invite-in-flight).

### D3. A new call is written to its backup at its birth

The INVITE's turn flushes the call it creates, when the call has a backup, before the
decision is asked. The decision turn's write supersedes it, `(1,0)` then `(2,0)`. The replica
lands one stream hop after the caller's 100.
[Detail](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#d3-a-new-call-is-written-to-its-backup-at-its-birth).

### D4. An undecided call materialised with no message has its decision asked again

A call still waiting on its first routing decision, materialised by the sweep, a confirmed
verdict or its own node's reclaim, has the decision asked again on its own turn, with a setup
budget of its own (ADR-0022 X1). The caller is answered through the seeded INVITE server
transaction. The backend may be asked twice and the callee dialled twice. A copy its primary
wrote since asks nothing and applies no later answer (D7). A call whose caller cancelled
during the consult ends under her cancel.
[Detail](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#d4-an-undecided-call-materialised-with-no-message-has-its-decision-asked-again).

### D5. A final for a round the record lost is acknowledged and ends the call

An INVITE final on a confirmed dialog, with a CSeq above the last the record sent and no
relay pending, answers a lost round (`evidence-final`, ADR-0055 §4). The dialog sequence is
raised to the round's first, the final is ACKed and its ACK retained, and the call ends with
a BYE on every leg. One takeover per lost round.
[Detail](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#d5-a-final-for-a-round-the-record-lost-is-acknowledged-and-ends-the-call).

### D6. A node that left the slice applies no decision

A decision that returns to a node whose own endpoint left the slice is not applied. The node
refuses the caller 503, with no Retry-After. A node still in the slice (draining, ADR-0031
case 1) applies its decisions.
[Detail](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#d6-a-node-that-left-the-slice-applies-no-decision).

### D7. A returning primary takes its swept calls back

A reclaiming node writes once (one `p` increment) each call with an INVITE in flight or that
its backup wrote as acting owner. On the backup, that forward write or a forward delete is
handed to the router, which queues the copy's stand-down on the call's own queue. The copy
stands down when the primary's body covers every write the copy made and every request whose
final only the copy sees, or when the primary deleted the call. Standing down releases the
copy local-only: timers cancelled, transactions relinquished (ADR-0034), nothing written, no
CDR. One owner acts on an outside service's answer. An unanswered teardown never ends an
answered call: both replication gates order bodies by `call::helpers::lifecycle_behind`.
[Detail](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#d7-a-returning-primary-takes-its-swept-calls-back).

### D8. A call whose primary has departed is discharged by its backup, once

A takeover copy that reaches `Terminated` while its primary is departed writes the CDR,
releases the limiter key and deletes the call. A copy whose primary is a member, or was never
seen, defers (ADR-0020 X3). The departure sweep discharges every ended Element of the
departed peer once. A dead verdict that acted releases the limiter key of every ended call of
the peer, keyed by the call (ADR-0040); the CDR waits for the departure or the reconcile.
[Detail](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#d8-a-call-whose-primary-has-departed-is-discharged-by-its-backup-once).

## Residuals

A death no proxy reads dead stays reactive. A primary read dead while it lives behind a
two-plane partition may see its calls dialled twice. A node lost whole is taken over 6 to 8 s
after the verdict by default. A primary that neither returns nor leaves the set loses the CDRs
its backup deferred to it, and one that reconciled a deferred end then departed draws two.
Every residual with its cell:
[appendix](appendix/0047-a-departed-peers-in-flight-invite-calls-are-taken-over.md#residuals).
