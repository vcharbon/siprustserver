# A departed peer's in-flight INVITE calls are taken over

**Status:** accepted (2026-10-12)

**Amends:** ADR-0014 D1 (takeover is reactive only), ADR-0022 X4 and X5, ADR-0031's
case table and its D3 split rule (the lifecycle order the forward and reverse gates read).

**Owner:** `crates/b2bua/src/repl/supervisor.rs` (the departure signal),
`repl/self_endpoint.rs` (leaving the slice), `router/departure.rs` (the sweep),
`router/quiesced.rs` (the self-release turn), `router/undecided.rs` (the undecided
call), `router/initial_turn.rs` (the creation write, the decision a node that left
the slice does not apply), `rules/lost_round.rs` and
`rules/actions/lost_round.rs` (the lost round), `sip-message`'s
`generate_ack_for_non_2xx_in_dialog`, `router/reclaim/bulk.rs` (the reclaim write),
`repl/puller/apply.rs` and `repl/router_sink.rs` (an applied primary write handed up),
`router/stand_down.rs` (the stand-down), `store/takeover.rs` (the takeover marks),
`store/adoption.rs` and `repl/store/admission.rs` (an adopted version's write),
`router/reclaim.rs` (the reverse fold of a copy's ending), `call`'s
`helpers/lifecycle.rs` (the lifecycle order), `sip-txn`'s `layer/relinquish.rs`.

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
router loop's, which serves its own calls meanwhile — tracked by the dispatcher, so a
crash of the node stops it (`router/departure/tests.rs`). It
reads the bodies one at a time, yielding as it goes, and for every call that names an
INVITE transaction in flight — exactly the calls a materialisation would seed — takes
the call lock and materialises the call as a takeover: its timers re-arm at their
replicated deadlines, its INVITE transactions are rebuilt, the self-release watch is
armed. A call already live here is left as it is. A call with nothing in flight is
not touched: it stays reactive, for the reason ADR-0014 D1 removed the eager takeover
(re-arming every quiet dialog at once is a stale-CSeq storm). A ringing call so taken
over ends at its replicated no-answer deadline, as the dead node would have ended it,
whatever the callee sent, unless the node returns first (D7)
(`failover-harness/tests/it/ringing_call_taken_over_on_departure.rs`);
a replicated re-INVITE round gives up at its INVITE bound
(`reinvite_round_gives_up_after_takeover.rs`, the departure cell), and a message that
arrives afterwards finds the copy live (`reinvite_answer_failover.rs`, the departure
cell). A takeover copy's self-release is a turn of its own call's queue, so the turns
of the messages that cleared its last transactions run first (ADR-0010 X2) and none of
them takes the call over again (`reinvite_round_not_yet_replicated.rs`); a copy with
no transaction left is shed before a timer of its own runs, so a deadline its primary
keeps — the give-up of a 2xx ladder the primary's copy discharged — is not kept twice
(`answer_lost_with_primary_no_answer.rs`, the cell where the ACK reaches the reclaimed
copy first).

**D3 — A new call is written to its backup at its birth.** The INVITE's turn flushes
the call it creates, when the call has a backup, before the decision is asked for;
the decision turn's write supersedes it — `(1,0)` then `(2,0)`, a forward
write the backup applies — and a fast decision's two writes fold into one changelog
entry, as every write of one call does (`repl/tests.rs`,
`update_compacts_and_moves_counter_forward`). The replica lands one stream hop after
the caller's 100, before the decision answers (`new_call_replication_window.rs`).

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
(`undecided_call_settled_on_departure.rs`, `router/departure/tests.rs`).

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
a `terminating` endpoint then leaves) is not applied: the callee is never dialled for
a call its backups took over. The node refuses the caller 503, with no Retry-After, on
the transaction it holds — its backup may hold no replica to answer from
(`undecided_call_settled_on_departure.rs`, the no-replica cell and the
replacement-decision cell). A node still in the slice — draining in ADR-0031 case 1 —
applies its decisions.

**D7 — A returning primary takes its swept calls back.** A node that reclaims one of
its own calls with an INVITE in flight, in the bulk reclaim or a straggler reclaim,
writes it once: one `p` increment, recording that the node serves the call again. A
quiet call is not written, so the reclaim of a large partition stays write-free for
it. On the backup that write is a forward flush. Applied to the Element of a call the
backup serves as a takeover copy, on the stream's tail or inside a bootstrap scan of
the returned node, it is handed to the router with its body; so is an applied forward
delete of the call, with the incarnation it names. The puller reads the router's live
takeover marks first, so no other call's write reaches the router. The router queues
the copy's **stand-down** as a turn of the call's own queue, so a message already
queued for the copy is served first (ADR-0010 X2). Under the per-call lock the copy
stands down when:

- the primary's body is the same call and **covers every write the copy made**: its
  `b` is at or past the copy's live `b`. One increment of a copy's `b` is one write
  this node made as the call's acting backup; a quiet turn, a fold or an adopted
  version moves nothing. A primary reaches a `b` only by reading the copy's
  write that carries it — applied or folded into its record, or refused (a body
  behind its record, or one with no lifecycle progress its record lacks) and its
  counter adopted (ADR-0031 D3) — so its record decides everything the copy did, and
  the copy holds nothing to keep. A copy
  that wrote nothing is covered by any write of its primary. The returning node's
  recovery pulls the copy's writes before it reclaims, so its reclaim write covers
  them; or
- the primary deleted the call (the same incarnation, or one the delete does not name).

The rule is read at the turn, not at the apply: the copy may have written again in
between. Standing down releases the copy local-only: its timers are cancelled, nothing
is written, no CDR (ADR-0020 X3). Its transactions are **relinquished**, deleted with
nothing sent, rather than orphaned (ADR-0034). They are images of the primary's own,
so an orphan's ACK and BYE would end the dialog the primary holds, and every request
the copy sent carries the route cookie naming the primary, so a response reaches the
primary once the proxy reads it alive; one that still reaches the backup takes the
call over afresh (`stood_down_round.rs`: a copy stood down in the middle of a re-INVITE
round it relayed; the callee's 200 takes the call over afresh and the round completes).
Any other copy plays on and self-releases at quiescence, and a refused flush folds as
before (ADR-0031 D3).

**An unanswered teardown never ends an answered call.** Both replication gates order
two bodies of one call the same way (`call::helpers::lifecycle_behind`): the answer
ranks first, so an answered body is never behind an unanswered one however far toward
its end the unanswered one went, and between bodies alike on the answer the one less
far toward its end is behind; between bodies alike on both, the one holding unfinished a
re-INVITE round the other saw end (ADR-0031, "a re-INVITE round is a third chain").

- **Reverse.** A backup's `p` may be one it adopted without the body that carried it,
  so the live-map fold of a reverse flush takes the vector's verdict (`p` unchanged,
  `b` advanced) only for a body not behind the primary's; lifecycle progress folds as
  before (`router/reclaim/tests.rs`).
- **Forward.** While the vector is split, a forward `Put` behind the Element is a
  branch and is refused (ADR-0031 D3); an answered body is not behind an Element
  holding a copy's ending of a call it never saw answered, so the authority's answer
  replaces that Element. At the Element's own `(p,b)` — the authority adopted the
  copy's `b` when it refused the copy's ending and republished its own call at that
  version — the frame is no re-delivery: it applies, and the Element heals
  (`swept_call_answered_inside_the_lag.rs`, the cut-stream cell and the node-dies
  cell). The refusal stays where it is owed: a backup that answered a call its
  primary never saw answered keeps its answered Element against the primary's
  unanswered teardown (`forward_flush_never_regresses_backup_progress.rs`), and one
  that answered and then ended the call keeps its ending against the primary's
  earlier answer (`repl/puller/tests.rs`, the split-Element cell).
- **Adoption.** A backup's write of a version it adopted never lands over the same
  call held at that version or past it: the primary may have refused the copy's
  write, adopted its `b` and republished its own call at that version first, and
  the Element keeps it (`router/reclaim/tests.rs`,
  `a_backups_adoption_never_overwrites_the_primarys_body_at_the_adopted_version`).

The cells are in `swept_call_primary_returns.rs`. The callee stays silent and the
returned primary alone CANCELs it, ACKs its 487 and refuses the caller 480. The callee
answers at the returned node. The reclaim reaches the backup inside a bootstrap scan.
The copy wrote before the return and stands down on the covering reclaim; then the
callee answers, stays silent, or refuses, or the caller CANCELs at the returned node:
one final, one node ending it. The callee refuses at the node, which deletes the call
before the backup reads anything of it: the copy stands down on the delete. Two
guards: the callee answers at the backup while the node is down, or before it is
routable. In `answer_lost_with_primary_no_answer.rs`, the cell where the ACK reaches
the reclaimed copy first, a 2xx reaches the backup after the stand-down.

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
- **A caller answered twice.** When a removed node refuses a call its backup's sweep
  answered too, the caller hears a second 503 on the branch its transaction completed
  on, and does not ACK it (`undecided_call_settled_on_departure.rs`, the
  replacement-decision cell). A CANCEL during the decision round trip draws the same:
  the backup holds the call as born and refuses it 503 after the caller's 487 (the
  CANCEL-during-decision cell).
- **An abrupt removal writes two CDRs.** In ADR-0031 case 2 the removed process and
  the replacement that reclaims the ended call each discharge it — the dual-owner
  window of ADR-0031 D3 (`undecided_call_settled_on_departure.rs`, the
  replacement-decision cell). A decision slower than the process's grace leaves one
  final, one CDR, one consult (the replacement cell).
- **A returning node's answer inside the replication lag of the deadline.** The
  copy's ring deadline fires before the returned node's reclaim and answer reach the
  backup: the copy CANCELs the callee after its answer and refuses the caller 480
  after its 200, repeated to Timer H (`swept_call_answered_inside_the_lag.rs`).
- **A retransmission placed as a call of its own.** The departed call it duplicates is
  settled off the wire but still discharged, with a CDR, by its primary on its return
  (`undecided_call_settled_on_departure.rs`, the retransmission cell).
- **A node that never returns loses the CDRs of the calls its backups ended.** A
  takeover copy defers its CDR to the call's primary (ADR-0020 X3); after a scale-down
  the ordinal does not come back and the ended Element is reaped past its TTL, CDR
  lost — the double-failure residual of ADR-0020, routine here
  (`undecided_call_settled_on_departure.rs`, the scale-down cell).
