# A departed peer's in-flight INVITE calls are taken over

**Status:** accepted (2026-10-12)

**Amends:** ADR-0014 D1 (takeover is reactive only), ADR-0022 X4 and X5, ADR-0031's
case table.

**Owner:** `crates/b2bua/src/repl/supervisor.rs` (the departure signal),
`repl/self_endpoint.rs` (leaving the slice), `router/departure.rs` (the sweep),
`router/quiesced.rs` (the self-release turn), `router/undecided.rs` (the undecided
call), `router/initial_turn.rs` (the creation write, the decision a node that left
the slice does not apply), `rules/lost_round.rs` and
`rules/actions/lost_round.rs` (the lost round), `sip-message`'s
`generate_ack_for_non_2xx_in_dialog`.

## Context

The stateless front proxy relays a worker's 100 Trying (RFC 3261 §16.11, ADR-0022
X4). A caller that heard it is `Proceeding`: it runs no Timer B and sends nothing
more until a final (§17.1.1.2). Takeover was reactive only (ADR-0014 D1): a backup
made a call live only when a message for it arrived. When the node serving a call
died and nothing arrived, the caller never got a final. Four windows:

- **W1 — a new call, its routing decision in flight.** The call was first written
  to its backup by the turn that applied the decision, so a death between the 100
  and that write left no replica at all.
- **W1b — a routed call, the callee silent** (no answer, a 100 alone, a 180 and then
  nothing). The call's deadlines and the callee's INVITE transaction lived on the
  dead node only; the callee's responses, the only messages that could reach the
  backup, never came.
- **W2b — a re-INVITE round in the replica, the callee silent.** Same: nothing
  triggers the takeover.
- **W2 — a re-INVITE round not yet in the replica.** The callee's final fails over
  on the proxy's Via cookie to the survivor, which holds no transaction for it:
  nothing took the final, the copy released itself, and each retransmission of a
  2xx (§13.3.1.4) took the call over again, never ACKed.

## Decision

**D1 — A peer that leaves the membership set is announced once.** The replication
supervisor parks a peer whose ordinal left the membership while its flows ran, and
in that same step sends the router `PeerDeparted(ordinal)` — once per real stop:
never on a periodic reconcile of a peer already parked, never for a condition flip of
a peer still in the set, never on this node's own shutdown
(`repl/departure_signal_tests.rs`).

**D2 — The departure sweep takes over the calls with an INVITE in flight.** The
router starts a task of its own over the departed peer's backup partition — never the
router loop's, which serves its own calls meanwhile (`router/departure/tests.rs`). It
reads the bodies one at a time, yielding as it goes, and for every call that names an
INVITE transaction in flight — exactly the calls a materialisation would seed — takes
the call lock and materialises the call as a takeover: its timers re-arm at their
replicated deadlines, its INVITE transactions are rebuilt, the self-release watch is
armed. A call already live here is left as it is. A call with nothing in flight is
not touched: it stays reactive, for the reason ADR-0014 D1 removed the eager takeover
(re-arming every quiet dialog at once is a stale-CSeq storm). A ringing call so taken
over ends at its replicated no-answer deadline, as the dead node would have ended it,
whatever the callee sent (`failover-harness/tests/it/ringing_call_taken_over_on_departure.rs`);
a replicated re-INVITE round gives up at its INVITE bound
(`reinvite_round_gives_up_after_takeover.rs`, the departure cell), and a message that
arrives afterwards finds the copy live (`reinvite_answer_failover.rs`, the departure
cell). A takeover copy's self-release is a turn of its own call's queue, so the turns
of the messages that cleared its last transactions run first (ADR-0010 X2) and none of
them takes the call over again (`reinvite_round_not_yet_replicated.rs`).

**D3 — A new call is written to its backup at its birth.** The INVITE's turn flushes
the call it creates, when the call has a backup and asks a decision — not a setup its
caller CANCELled before the turn, nor one refused at once — before the decision is
asked for; the decision turn's write supersedes it — `(1,0)` then `(2,0)`, a forward
write the backup applies — and a fast decision's two writes fold into one changelog
entry, as every write of one call does (`repl/tests.rs`,
`update_compacts_and_moves_counter_forward`). The replica lands one stream hop after
the caller's 100, before the decision answers (`new_call_replication_window.rs`). A
CANCEL while the decision is in flight — answered 200 and 487 by the transaction
layer (§9.2) — is written to the backup at once, one version ahead of the record its
turn still holds, so a backup that takes the call over finds its caller answered
(`undecided_call_settled_on_departure.rs`, the CANCEL-then-departure cell).

**D4 — An undecided call materialised with no message is refused 503.** A call the
departure sweep takes over, or that its own node reclaims after a restart (the bulk
reclaim and the straggler reclaim), carries no message to serve. One still waiting on
its first routing decision — live, the caller unanswered, no decision applied, no
attempt, no leg dialled — is refused as a decision the backend could not give
(503 Service Unavailable, ADR-0022 X3) through the INVITE server transaction the
materialisation rebuilt, with **no Retry-After**: the caller takes it as a 500
(§21.5.4), and whether to retry is its own or its upstream's call — nothing behind the
stateless front proxy holds state that a delay would wait out. The decision is never
asked again, so nothing it causes happens twice. A takeover copy defers the CDR to the
call's primary (ADR-0020 X3); a reclaim writes it. When the caller's INVITE branch is
already another call's transaction on this node — its retransmission placed here as a
call of its own — the undecided call ends with nothing on the wire, as the 503 would
answer that other call; when the transaction layer cannot be asked, nothing is settled
(`undecided_call_settled_on_departure.rs`, `router/departure/tests.rs`). A reclaimed
setup its caller CANCELled before any decision ends as its `Cancelled` turn would have,
nothing on the wire.

**D5 — A final for a round the record lost is acknowledged and ends the call.** An
INVITE final on a confirmed dialog whose CSeq is above the last the record sent on that
dialog, with no relay pending for it, answers a round the record lost
(`ack-lost-round`). The dialog's sequence is raised to the round's first, so no later
request reuses a number the peer saw (§12.2.1.1), and the round becomes the dialog's
INVITE transaction. A 2xx is ACKed end to end, an offer it makes answered with every
stream rejected (RFC 3264 §6), the ACK retained so a repeat is re-ACKed (§13.2.2.4); a
non-2xx is ACKed hop by hop on the response's own branch, built from the dialog's
remote target and route set where the INVITE went (§17.1.1.3), the ACK retained too so
a repeat is re-ACKed (`re-ack-lost-round`, §17.1.1.2). Then the call ends with a BYE on
every leg. Once the call is ended, a later copy of the final finds a terminal record
and takes nothing over: one takeover per lost round
(`reinvite_round_not_yet_replicated.rs`, `b2bua/tests/it/lost_round_rules.rs`). The
failure rule for a dialled leg matches only a leg still dialling: a non-2xx on a
confirmed leg is never read as the leg failing.

**D6 — A node that left the slice applies no decision.** A decision that returns to a
node whose own endpoint left the slice (ADR-0031 D6: the latch moves on to `absent` when
a `terminating` endpoint then leaves) is not applied: its backups took the call over
and answered it, so the call ends there with nothing on the wire, and a call with no
backup is refused 503. The callee is never dialled for a call already refused
(`undecided_call_settled_on_departure.rs`, the replacement-decision cell). A node still
in the slice — draining in ADR-0031 case 1 — applies its decisions.

## Residuals

- **A death the peers are not told of stays reactive.** A partition or a crash
  before the orchestrator acts (ADR-0031 case 3a) and a restart in place (case 4)
  never leave the membership set, so no sweep runs: a message takes the call over
  (`inflight_resend.rs`, `prack_takeover.rs`, and the CANCEL cell of
  `undecided_call_settled_on_departure.rs`), and a restarted node answers its own
  undecided calls on its reclaim (the restart cell there).
- **The creation write lands one stream hop after the 100.** A death inside that
  hop leaves no replica (`new_call_replication_window.rs` measures it).
- **A route applied before its write replicated.** A node that dies after dialling
  the callee and before the routed call reached its backup leaves the backup an
  undecided call: the caller is refused 503 and the callee rings on, CANCELled by no
  one (`undecided_call_settled_on_departure.rs`, the route-not-replicated cell).
- **The caller's re-INVITE of a lost round gets no final.** Its transaction existed
  only on the dead node (§17.1.3); the BYE ends its dialog instead
  (`reinvite_round_not_yet_replicated.rs`). Answering it would need the 100 held on
  a synchronous replication, which ADR-0014 rejects.
- **A callee that sent a 100 alone holds its CANCEL.** A 100 leaves no dialog in the
  record, so the taken-over INVITE has drawn no provisional here, and its CANCEL
  waits for the callee's next one (§9.1)
  (`ringing_call_taken_over_on_departure.rs`, the 100 cell).
- **An abrupt removal writes two CDRs.** In ADR-0031 case 2 the removed process and
  the replacement that reclaims the ended call each discharge it — the dual-owner
  window of ADR-0031 D3 (`undecided_call_settled_on_departure.rs`, the
  replacement-decision cell). A decision slower than the process's grace leaves one
  final, one CDR, one consult (the replacement cell).
- **A retransmission placed as a call of its own.** The departed call it duplicates is
  settled off the wire but still discharged, with a CDR, by its primary on its return
  (`undecided_call_settled_on_departure.rs`, the retransmission cell).
- **A node that never returns loses the CDRs of the calls its backups ended.** A
  takeover copy defers its CDR to the call's primary (ADR-0020 X3); after a scale-down
  the ordinal does not come back and the ended Element is reaped past its TTL, CDR
  lost — the double-failure residual of ADR-0020, routine here
  (`undecided_call_settled_on_departure.rs`, the scale-down cell).
