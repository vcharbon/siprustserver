# A departed peer's in-flight INVITE calls are taken over

**Status:** accepted (2026-10-12)
Amended (2026-10-23): D5 and its residuals, the lost round becomes the evidence rule of
any method (ADR-0055 §3, §4); a removed process superseded by its replacement authors
nothing (ADR-0055 §6); D2's sweep also serves a departed primary's `Terminating` calls
and discharges those with nothing left to serve (ADR-0055 §5); D7's stand-down reads
containment, and the lifecycle order its gates read only ranks the base of a diverged
call's end (ADR-0055 §7).
Amended (2026-10-28): D1, a proxy's dead verdict is a second trigger; D2, the sweep's
takeover half runs on either trigger, and its discharge half and the refusal of an
undecided call on a departure only; the residual of a death the peers are not told of
(ADR-0050 §3).
Amended (2026-10-29): D2, the sweep also takes over a call with an in-dialog request in
flight, and one that awaits the answer to a request sent off its turn, which the copy
sends again (ADR-0039 decision 5).

**Amends:** ADR-0014 D1 (takeover is reactive only), ADR-0022 X4 and X5, ADR-0031's
case table and its D3 split rule (the lifecycle order the forward and reverse gates read),
ADR-0020 X3 (a copy whose primary departed discharges the call, D8).

**Owner:** `crates/b2bua/src/repl/supervisor.rs` (the departure signal),
`router/dead_verdict.rs` (a proxy's dead verdict),
`repl/self_endpoint.rs` (leaving the slice), `router/departure.rs` (the sweep),
`router/quiesced.rs` (the self-release turn), `router/undecided.rs` (the undecided
call), `router/initial_turn.rs` (the creation write, the decision a node that left
the slice does not apply), `rules/evidence.rs` and
`rules/actions/evidence.rs` (the lost round, now ADR-0055 §4's evidence rule), `sip-message`'s
`generate_ack_for_non_2xx_in_dialog`, `router/reclaim/bulk.rs` (the reclaim write),
`repl/puller/apply.rs` and `repl/router_sink.rs` (an applied primary write handed up),
`router/stand_down.rs` (the stand-down), `store/takeover.rs` (the takeover marks),
`store/adoption.rs` and `repl/store/admission.rs` (an adopted version's write),
`router/reclaim.rs` (the reverse fold of a copy's ending), `call`'s
`helpers/lifecycle.rs` (the lifecycle order), `sip-txn`'s `layer/relinquish.rs`,
`repl/departed.rs`, `router/interpret.rs` and `router/departure.rs` (the discharge of a departed primary's call), `router/dropped_on_delete.rs` (the drop of a live copy on the backup's delete).

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

A front proxy's **dead verdict** on a peer is the second trigger (ADR-0050 §3): the
signed `X-Worker-Dead` notice an out-of-dialog OPTIONS carries, taken once per verdict.
The proxy reads a death the membership does not show (ADR-0031 case 3a) about 2 s after
it, and from then on routes the peer's traffic to its backups, so the backups act on
that verdict rather than wait for the endpoint to leave the slice. A notice naming this
node is ignored (`router/dead_verdict/tests.rs`;
`read_dead_primary_taken_over.rs`).

**D2 — The departure sweep takes over the calls with an INVITE in flight.** The
router starts a task of its own over the departed peer's backup partition — never the
router loop's, which serves its own calls meanwhile — tracked by the dispatcher, so a
crash of the node stops it (`router/departure/tests.rs`). It
reads the bodies one at a time, yielding as it goes, and for every call that names an
INVITE transaction in flight — exactly the calls a materialisation would seed — or an
in-dialog request in flight but the keepalive (a relay, a request of its own or one it
holds), or awaits the answer to a request it sent off its turn, whose task died with the
peer (ADR-0039 decision 5), takes the call lock and materialises the call as a takeover —
on a dead verdict, a call whose only work is such a request once that request is due
(its answer deadline, a REFER authorization's send time plus the decision deadline and
its margin, an in-dialog request's Timer F), re-checked on this node then: its timers re-arm at their
replicated deadlines, its INVITE transactions are rebuilt, the self-release watch is
armed. A call already live here is left as it is. A call with nothing in flight is
not touched: it stays reactive, for the reason ADR-0014 D1 removed the eager takeover
(re-arming every quiet dialog at once is a stale-CSeq storm). A ringing call so taken
over ends at its replicated no-answer deadline, as the dead node would have ended it,
whatever the callee sent, unless the node returns first (D7)
(`failover-harness/tests/it/ringing_call_taken_over_on_departure.rs`);
a replicated re-INVITE round gives up at its INVITE bound
(`reinvite_round_gives_up_after_takeover.rs`, the departure cell), a round answered
2xx and awaiting its ACK is a transaction too — the copy resumes the 2xx's ladder,
which nobody else repeats once the caller heard a 100 (RFC 3261 §13.3.1.4;
`reinvite_server_side_at_the_takeover.rs`, the departure cell) — and a message that
arrives afterwards finds the copy live (`reinvite_answer_failover.rs`, the departure
cell). A takeover copy's self-release is a turn of its own call's queue, so the turns
of the messages that cleared its last transactions run first (ADR-0010 X2) and none of
them takes the call over again (`reinvite_round_not_yet_replicated.rs`); a copy with
no transaction left is shed before a timer of its own runs, so a deadline its primary
keeps — the give-up of a 2xx ladder the primary's copy discharged — is not kept twice
(`answer_lost_with_primary_no_answer.rs`, the cell where the ACK reaches the reclaimed
copy first).

The sweep has two halves. The **takeover half** (the calls above) runs on either
trigger. The **discharge half** (D8's discharge of an ended Element and of a
`Terminating` call with nothing left to serve, ADR-0055 §5) and D4's refusal of an
undecided call run on a departure only: a peer read dead may live behind a partition
and decide or discharge those calls itself (user ruling). A copy taken over on a verdict that ends while its peer is still a member
defers its CDR (ADR-0020 X3), and the departure's sweep discharges it. A call already
live here when the departure's sweep runs is left as it is. A primary that comes back
from the partition meets the copies through D3's forward guard, D7's stand-down and
ADR-0055 §7's containment, as it meets a reactive takeover.

**D3 — A new call is written to its backup at its birth.** The INVITE's turn flushes
the call it creates, when the call has a backup, before the decision is asked for;
the decision turn's write supersedes it — `(1,0)` then `(2,0)`, a forward
write the backup applies — and a fast decision's two writes fold into one changelog
entry, as every write of one call does (`repl/tests.rs`,
`update_compacts_and_moves_counter_forward`). The replica lands one stream hop after
the caller's 100, before the decision answers (`new_call_replication_window.rs`).

**D4 — An undecided call materialised with no message is refused 503.** A call the
departure sweep takes over on a departure, or that its own node reclaims after a
restart (the bulk reclaim and the straggler reclaim), carries no message to serve. A
proxy's dead verdict leaves an undecided call to the departure: its decision may be in
flight at a primary that lives behind a partition (user ruling). One still waiting on
its first routing decision — live, the caller unanswered, no decision applied, no
attempt, no leg dialled — is refused as a decision the backend could not give
(503 Service Unavailable, ADR-0022 X3) through the INVITE server transaction the
materialisation rebuilt, with **no Retry-After**: the caller takes it as a 500
(§21.5.4), and whether to retry is its own or its upstream's call — nothing behind the
stateless front proxy holds state that a delay would wait out. The decision is never
asked again, so nothing it causes happens twice. The copy's primary has departed, so
the copy writes the CDR (D8). When the caller's INVITE branch is
already another call's transaction on this node — its retransmission placed here as a
call of its own — the undecided call ends with nothing on the wire, as the 503 would
answer that other call; when the transaction layer cannot be asked, nothing is settled
(`undecided_call_settled_on_departure.rs`, `router/departure/tests.rs`).

**D5 — A final for a round the record lost is acknowledged and ends the call.** An
INVITE final on a confirmed dialog whose CSeq is above the last the record sent on that
dialog, with no relay pending for it, answers a round the record lost
(`evidence-final`, ADR-0055 §4). The dialog's sequence is raised to the round's first, so no later
request reuses a number the peer saw (§12.2.1.1), and the round becomes the dialog's
INVITE transaction. A 2xx is ACKed end to end, an offer it makes answered with every
stream rejected (RFC 3264 §6), the ACK retained so a repeat is re-ACKed (§13.2.2.4); a
non-2xx is ACKed hop by hop on the response's own branch, built from the dialog's
remote target and route set where the INVITE went (§17.1.1.3), the ACK retained too so
a repeat is re-ACKed (`re-ack-evidence`, §17.1.1.2). Then the call ends with a BYE on
every leg. Once the call is ended, a later copy of the final finds a terminal record
and takes nothing over: one takeover per lost round
(`reinvite_round_not_yet_replicated.rs`, `b2bua-core/tests/it/evidence_rules.rs`). The
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
its own calls with an INVITE in flight, or one its backup wrote as the call's acting
owner (its `b` past zero), in the bulk reclaim or a straggler reclaim, writes it once:
one `p` increment, recording that the node serves the call again. A copy taken over on
a verdict or by a message stays resident while its transactions last, timers armed,
after the call went quiet, so the write reaches it too
(`swept_call_primary_returns.rs`, the node-down cell). A quiet call the backup never
wrote has no copy and is not written, so the reclaim of a large partition stays
write-free for it. On the backup that write is a forward flush. Applied to the Element of a call the
backup serves as a takeover copy, on the stream's tail or inside a bootstrap scan of
the returned node, it is handed to the router with its body; so is an applied forward
delete of the call, with the incarnation it names. The puller reads the router's live
takeover marks first, so no other call's write reaches the router. The router queues
the copy's **stand-down** as a turn of the call's own queue, so a message already
queued for the copy is served first (ADR-0010 X2). Under the per-call lock the copy
stands down when:

- the primary's body is the same call and **covers every write the copy made**: its
  `b` is at or past the copy's live `b`, the copy stands ahead of it on no re-INVITE
  round (`call::helpers::round_ahead`), and it holds every request whose final only the
  copy sees: each round the copy relayed itself and holds open (an answered round's final
  is taken and its ACK follows the route set), as the
  same transaction or behind a later one (`call::helpers::holds_rounds_of`, read from
  the transactions, never from a sequence a refusal raised), and each other request the
  copy relayed itself, as the same relay (`call::helpers::holds_relays_of`;
  `PendingRequest::relayed_by` names the relaying node). A relay the copy holds as the
  primary's own returns its final to the primary and pins nothing. A leg's dialling
  INVITE the copy sent and that awaits its final pins the copy whatever the primary's
  record holds: the proxy's cookie on that INVITE names the copy, so its responses
  return to the copy alone (ADR-0055 §8). When the final reaches the copy, the
  primary that takes the copy's write carrying it writes the call once, and the
  copy stands down on that write. One increment
  of a copy's `b` is one write this node made as the call's acting backup; a quiet
  turn, a fold or an adopted version moves nothing. A primary reaches a `b` only by
  reading the copy's write that carries it — applied or folded into its record, or
  refused (a body behind its record, or one with no lifecycle progress its record
  lacks) and its counter adopted (ADR-0031 D3). A refused request is in no record but
  the copy's, and its final follows the copy's Via (RFC 3261 §18.2.2), so the copy
  plays on with it until its final (`round_the_primary_never_read.rs`,
  `relay_the_primary_never_read.rs`, `ending_crossing_a_round.rs`;
  `router/stand_down/tests.rs`). A copy that wrote
  nothing is covered by any write of its primary. The returning node's recovery pulls
  the copy's writes before it reclaims, so its reclaim write covers them; it makes that
  write before it is routable, its calls parked until then (ADR-0055 §8); or
- the primary deleted the call (the same incarnation, or one the delete does not name),
  and the copy holds no request whose final only it sees.

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
before (ADR-0031 D3). A copy that plays on from the primary's copy sends each CANCEL
that copy records on an INVITE whose transaction the copy holds: the primary decided
it, but a CANCEL goes on its INVITE's transaction (RFC 3261 §9.1), and only the copy
holds that one (`router/reclaim/carried_cancel.rs`).

One owner acts on an outside service's answer. A copy awaiting the answer to a request
the primary's reclaim write awaits too is covered by that write on that request: it stands down, an answer reaching
it afterwards takes nothing over (ADR-0039 decision 5), and the primary alone acts on
the answer, the backend possibly asked twice.

**An unanswered teardown never ends an answered call.** Both replication gates order
two bodies of one call the same way (`call::helpers::lifecycle_behind`): the answer
ranks first, so an answered body is never behind an unanswered one however far toward
its end the unanswered one went, and between bodies alike on the answer the one less
far toward its end is behind; between bodies alike on both, the one holding unfinished a
re-INVITE round the other saw end, or lacking one the other opened (ADR-0031, "a
re-INVITE round is a third chain").

- **Reverse.** A backup's `p` may be one it adopted without the body that carried it,
  so the live-map fold of a reverse flush takes the vector's verdict (`p` unchanged,
  `b` advanced) only for a body not behind the primary's and not marked as adopted
  (ADR-0031, "an adopted counter is not the body it counts"); any other body folds
  through its lifecycle or round progress alone (`router/reclaim/tests.rs`,
  `adopted_counter_reverse_fold.rs`).
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

**D8 — A call whose primary has departed is discharged by its backup, once.** A
primary is departed for a node that pulled it, then parked it when it left the
membership set (D1), until it rejoins. A takeover copy that reaches `Terminated` while
the call's primary is departed discharges the call as a primary does: it writes the
CDR, releases the limiter key and deletes the call, the delete propagating as any
write of the copy. There is no primary to defer to, and after a scale-down none
returns. A copy whose primary is still a member, or was never seen, defers as ADR-0020
X3 states (`router/departure/tests.rs`; `repl/departed_tests.rs` pins which peers have
departed; `outbound_invite_after_the_departure.rs`, the scale-down cell of
`undecided_call_settled_on_departure.rs` and the transparent matrix's `stay_dead`
cells measure the discharge).

The departure sweep (D2) discharges, the same way and once, every ended Element of
the departed peer: a copy here ended the call and deferred it while the peer was a
member, and the peer left without reconciling it
(`deferred_end_of_a_departed_primary.rs`; `limiter_release_by_call.rs`, the departing
primary cell, counts one release).

A node of the departed ordinal that applies the backup's delete on its reclaim stream
while it holds the call live — the departed process itself, still running (ADR-0031
case 3b), or a replacement that reclaimed the call before the backup saw it rejoin —
drops that live copy local-only: timers cancelled, transactions relinquished, nothing
sent, no CDR (`router/dropped_on_delete.rs`). Only a delete naming the live call's
incarnation drops it, and only a backup's delete rides up as the notice; a primary's
forward delete keeps its D7 meaning (`discharged_call_dropped_by_its_primary.rs`, both
cells; `router/dropped_on_delete/tests.rs`; `repl/puller/tests.rs`). A node that
rejoins after the delete finds the call deleted and reclaims nothing of it (the
transparent matrix's `*__bye_*__kill__reboot*` cells;
`fault_primitives.rs::crash_stops_every_frame_and_datagram_from_the_node`). Each way
the call has one CDR.

## Residuals

- **A death no proxy reads dead stays reactive.** A restart in place (ADR-0031 case
  4) never leaves the membership set and is read by the tombstone, not the probe, and
  a proxy with no cluster key tells no verdict: a message takes the call over
  (`inflight_resend.rs`, `prack_takeover.rs`, each on a proxy that tells no verdict).
  An undecided call stays reactive under a verdict too: a CANCEL takes it over (the
  CANCEL cell of `undecided_call_settled_on_departure.rs`), and a restarted node
  answers its own undecided calls on its reclaim (the restart cell there).
- **A primary read dead while it lives.** A flap or a partition between the proxy and
  the primary alone: the backup takes over the primary's calls with an INVITE or a
  teardown in flight while the primary serves them, so the callee may get the dialled
  INVITE twice; an undecided call is left to its primary, which decides and answers it
  alone; a call whose only work is a request in flight — an outside answer it awaits,
  an in-dialog request — waits for those requests' deadlines, by which the live primary
  took its answers and its write took them off the record, so nothing is asked or
  relayed twice (`cancel_and_retransmission_follow_a_live_hop.rs`, the cells that tell
  the verdict).
  `live_primary_read_dead.rs` pins a flap: the second
  verdict takes over afresh the re-INVITE round a stood-down copy left open, and the
  callee absorbs the re-sent INVITE.
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
- **A caller answered twice.** A CANCEL during the decision round trip: the backup
  holds the call as born and refuses it 503 after the caller's 487, on the branch the
  caller's transaction completed on (`undecided_call_settled_on_departure.rs`, the
  CANCEL-during-decision cell). A removed node whose ordinal is bound to its
  replacement is superseded and answers nothing, nor writes a CDR (ADR-0055 §6, the
  replacement-decision cell).
- **A returning node's answer inside the replication lag of the deadline.** The
  copy's ring deadline fires before the returned node's reclaim and answer reach the
  backup: the copy CANCELs the callee after its answer and refuses the caller 480
  after its 200, repeated to Timer H (`swept_call_answered_inside_the_lag.rs`).
- **A retransmission placed as a call of its own.** The departed call it duplicates is
  settled off the wire but still discharged, with a CDR, by the backup (D8)
  (`undecided_call_settled_on_departure.rs`, the retransmission cell).
- **A primary that neither returns nor leaves the set loses the CDRs its backup
  deferred to it.** A copy that ends a call while its primary is a member defers
  (ADR-0020 X3); with no departure the sweep never runs, and the ended Element is
  reaped past its TTL, CDR lost (`call_terminate_on_backup.rs`, C4 and C5).
- **A primary that reconciled a deferred end and departed before its delete landed.**
  The primary wrote the CDR and sent its delete; the departure parks both of the
  backup's flows at once, so the delete never lands, and the sweep discharges the
  Element again: two CDRs. The window runs from the copy's flush to the park, about two
  replication polls. A departed primary still running keeps pulling the backup, so it
  can reconcile a deferral after the sweep ran, with the same outcome. The Element
  keeps no trace of the primary's discharge once the delete is cut, so the backup
  cannot tell the two apart; reading the peer's last applied position instead trades
  the duplicate for a lost CDR
  (`deferred_end_of_a_departed_primary.rs`, the reconciled-then-departed cell).
