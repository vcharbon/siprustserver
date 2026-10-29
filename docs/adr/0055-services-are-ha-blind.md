# Services are HA-blind

**Status:** proposed (2026-10-23)

**Amends:** ADR-0011 X5 (fencing by identity, §6), ADR-0014 (the replication
write path, the confirm-race trade-off, the INVITE-only seeds, §2–§4), ADR-0016
(the SDK surface, §10), ADR-0020 X3 (the CDR discharge, §5), ADR-0022 (the first
provisional of an INVITE, §3), ADR-0031 D2 and D4 and its per-axis fold (§3,
§7, §8), ADR-0037 items 4 and 6 (a replica left out makes the call non-HA, §3),
ADR-0042 §2 (the business re-check moves into timer scopes, §10), ADR-0045 (the
outbox reads the applied position, §3), ADR-0047 D5 and its residuals (the lost
round becomes the rule of §4, any method; the creation write before the first
provisional, §3).

**Owner:** `crates/call` (the request record, the subscription record, timer
scopes), `crates/b2bua-core` (the method table, the settle paths, the evidence
rule), `crates/b2bua` (the turn engine, the replication write path, the store
and its envelope, materialise, reclaim, departure, stand-down), `crates/repl-net`
(send on write, explicit replica refusal), `crates/sip-txn` (seeds for every
method, the first provisional handed to the turn engine), `crates/sip-proxy` (the
routing rule, the Via cookie on every method), `crates/b2bua-sdk` (the seam),
`crates/failover-harness` (placement,
steering, turn points, the sweep), `scripts/purity.py` (the blind/aware
boundary).

## Terms

- **Call record**: the replicated `Call`.
- **Request record**: one transaction the call takes part in (§2).
- **Call incarnation**: the RFC 3261 §8.1.3.5 retry identity of a call on one
  `call_ref` (`call/src/incarnation.rs`).
- **Worker identity**: one boot of one worker ordinal (§6).
- **Owner**: the primary or the backup of a call; a call with no backup has one
  owner.

## Context

The call record is the unit of replication (ADR-0008, ADR-0011), so a service's
own state (`ext`, cursors, the timer ledger) replicates without an opt-in, and
`RuleCall` exposes no topology and no takeover mark
(`b2bua-sdk/src/rule_call.rs:21`). The SDK as a whole is not HA-free:
`CallEvent::CallQuiesced` (`b2bua-sdk/src/event.rs:94-97`), the takeover and
reclaim knobs of `B2buaConfig` (`config.rs:110-113, 429`) and
`CallEvent::Sip.matched_client_txn` (`event.rs:17-24`) sit in it. Below the seam,
three facts make every in-dialog method a separate HA problem:

1. **The wire runs ahead of the replica, and the replica can miss a write.** A
   turn replaces the live copy, queues its flush and sends at once
   (`router/interpret.rs:77-171`). The flush goes through a bounded channel whose
   `try_send` drops the write when full (`store/terminate_writer.rs:1-3, 69`); the
   backup polls every 100 ms (`repl/server.rs:65`); a backup at its memory
   ceiling leaves a new call's replica out (ADR-0037 item 4). The transaction
   layer sends the 100 Trying of every INVITE before any turn
   (`sip-txn/src/layer/server.rs:342-364`). ADR-0014 accepts the window as the
   confirm-race; corrections inside it were written per case (`lost_round`,
   `ack-lost-round`).
2. **Only INVITE is a round.** Takeover seeds INVITE transactions only
   (`router/materialise/seeds.rs`, `sip-txn/src/seed.rs:15-40`); the fold, the
   stand-down weighing and the proxy's response failover know INVITE rounds
   only (`sip-proxy/src/core/response.rs:137-161`; the Via cookie covers INVITE,
   CANCEL and ACK, `core/request/via_cookie.rs:51-53`). A request in flight lives
   in `inbound_pending_requests`, `pending_invite_txn`, five 2xx/ACK fields
   (`ack_branch`, `pending_reinvite_2xx`, `awaited_ack_cseq`, `answered_2xx`,
   `emitted_ack`), `owed_answer`, `own_notify`, `RetiredDialog::open_relays`, the
   `provisionals` ladder, and records of its own or none for BYE, keepalive
   OPTIONS and a service's own request. `handle-timeout` sits behind four
   per-method intercepts.
3. **Reconciliation is per axis.** A rounds-only fold keeps the local copy's
   service state while taking the other copy's dialog facts
   (`call/src/helpers/round.rs:76-82`), so service progress made only on the
   other copy is dropped; the lifecycle order the fold reads is partial
   (`call/src/helpers/lifecycle.rs:55-75`).

In the history of HA corrections that touched in-dialog behaviour, about two in
five needed rule or per-method code to learn about takeover, and the same
correction was made again for a second method or phase at least eight times.

## Decision

### 1. The contract

A service or per-method author writes single-node logic. HA lives only in the
aware crates (§11). The blind crates hold no HA type, no HA term and no HA
branch.

What a service may rely on, and all it must know:

- every per-call fact lives in the call record (`ext`, a typed slice, the
  ledger);
- an off-turn request is delivered at least once: the backend may receive it
  more than once, and its answer may come back as "no answer" at the deadline
  though the backend acted (ADR-0039); only the first answer the call awaits
  reaches the service. No idempotency key rides the request while no backend
  reads one;
- a call may end cleanly at any instant (§4);
- it keeps durations or timers in its own state, never absolute times (§9
  re-anchors the platform's instants only).

None of these is an HA statement: a single node already behaves this way
(`http-net/src/backend/retry.rs:38-52` re-sends after a timeout or a reset).

### 2. One request record for every transaction of a call

Every transaction the call takes part in, the dialog-creating INVITEs included,
is one request record: direction (received and owed an answer, or sent and
awaited), origin (relayed from the peer leg, own, held for a later answer, or
queued and not sent yet), method, CSeq, branch, the request image as sent, the
linked request record on the peer leg for a relay, the deadline, the owed ACK of
an INVITE 2xx, and the dialog usage it belongs to (the INVITE session, or a
subscription by `Event`/`id`, RFC 6665 §4.5, RFC 5057). A request record
outlives its dialog's retirement until it settles (a late fork 2xx is ACKed and
BYEd, RFC 3261 §13.2.2.4). Offer/answer is a fact of the session, not of a
request record: an offer may open in one transaction and close in another (a
reliable 18x and its PRACK, RFC 3262 §5; a 2xx and its ACK; an UPDATE, RFC 3311).

One state machine settles a request record on a final, a timeout, a CANCEL, a
crossing BYE, the leg's end, the call's release and a takeover. It replaces the
fields listed in the Context, `OwnNotify`, the four `handle-timeout` intercepts
and the per-method verdicts of `dialog_gate` and `teardown`.

What differs by method is one table that knows no HA:

- INVITE: offer/answer, the reliable-provisional ladder and its RSeq/RAck
  mapping (RFC 3262 §3), an owed ACK on 2xx, 491 on glare (RFC 3261 §14);
- UPDATE: offer/answer, 491 on glare (RFC 3311 §5.2);
- PRACK: may carry the answer (RFC 3262 §5);
- any non-INVITE: a final ends the exchange; the client side holds through
  Timer K (§17.1.2);
- REFER: opens an implicit subscription (RFC 3515);
- NOTIFY: carries subscription state; after `terminated` no NOTIFY is sent on
  that subscription (RFC 6665 §4.2.2); NOTIFYs of one subscription leave in
  order;
- BYE: ends the dialog; it is queued behind an owed ACK and behind an own NOTIFY
  in flight (RFC 3261 §15).

A held request record lets a service answer a received non-INVITE request on a
later turn. It carries a deadline under the originator's Timer F, 64·T1, with
a margin, and is answered by the platform when it passes (RFC 4320 §4.1,
RFC 5057 §5.1). An INVITE is never held: one a service asks to hold is answered
500 at once.

Takeover seeds a transaction from every request record, any method, client or
server, through one path. A seeded client transaction retransmits the request
record's image under its branch (§17.1.2.2). The record may be ahead of the
wire (§3), so a seed delivers what it holds as sent: a re-sent INVITE goes
behind the ACK of the dialog's 2xx before it, which its record carries
(§14.1), the caller's INVITE answered non-2xx is seeded Completed on the
final its record keeps, sent on Timer G from the seed until her ACK
(§17.2.1), and on a takeover her unanswered INVITE re-sends once at the seed
the latest provisional its record keeps, which a copy of her INVITE draws
again (§13.3.1.1); a reliable one is left to its own ladder (RFC 3262 §3). A
reclaim re-sends none: its record may be behind what the takeover copy sent
her (§8). The final of a received non-INVITE request is kept on the record
while its server transaction is Completed, Timer J from the final
(§17.2.2), and seeded Completed on it: a copy of the request that reaches the
node serving the call draws that final, which the record may hold before the
wire saw it, and the rules never read it as a new request (a second relay, a
REFER read as glare).

### 3. The replication write path

- **Order.** A turn writes the call record into the replication change log
  before any effect of the turn leaves the node. The order lives in the turn
  engine alone.
- **One entry per call, never dropped.** The change log holds one entry per
  call; a newer write moves it, and the terminal write (the delete) is the
  last. The channel that drops a write when full
  goes: the turn writes into the change log directly. A delete is kept until
  every backup of the call applied it.
- **Sent on write.** Replication stays pull-only: the backup opens the
  connection. The serving side sends a write as soon as it is in the change log;
  there is no poll cadence.
- **Calls without a backup.** A call has no backup, for its whole life, when it
  is admitted while the backlog toward its backup is above a high-water mark,
  or when its backup refuses the replica at its own memory ceiling (ADR-0037
  item 4), or when a write of it cannot reach the backup (its body does not
  encode, or the transport refuses its frame): its entry toward the backup
  then becomes a delete. The refusal is a frame to the primary, and the
  backup's applied position moves past it. The backup refuses that call
  incarnation's later writes until the call's delete or a TTL that outlives
  the writes in flight (10 s); it keeps a bounded number of refusals, the
  oldest evicted first, and a new call on the same identity is not refused. The backlog is measured per backup peer: the entries
  above that peer's applied position and the age of the oldest. New calls get a
  backup again once the backlog is under a low-water mark. No call is refused
  for the backlog. A gauge counts the live calls with no backup; such a call
  runs as on a non-HA B2BUA.
- **The replication sync level: `off` or `applied`.** Every effect a turn
  sends off the node leaves through the call's one ordered outbox, at either
  level: entries leave in the order the call's turns queued them, a later
  entry never overtaking an earlier one, and no turn waits on the outbox. The
  level decides only when an entry is released. Under `off` the write is
  logged, then the entry leaves at once. Under `applied` (the default) an
  entry leaves once the backup applied the write that records it, or once a
  cap expires, whichever comes first. "Applied" means the backup's applied
  position reached the call's change-log entry, re-read on every wake: a
  later write of the call moves the entry, and the wait follows it, while the
  cap stays counted from the entry's own enqueue, so a later write never makes
  an earlier entry wait longer than the cap. A call with no live, bootstrapped
  backup waits for nothing, and an entry whose backup's flow goes down leaves
  then. No entry waits without a cap: a lost peer would stall every call.
- **What the outbox carries.** Outbound SIP (a request, a response, a
  retained datagram, a media-server leg's SIP included; the node sends no
  RTP), the requests to outside services a turn starts (a routing or service
  consult, a release consult, the limiter admit), the limiter releases, and
  the transaction effects a queued send depends on (relinquishing an answered
  INVITE's server transaction), so they run after the sends ahead of them.
  Timers, the CDR, metrics and re-entries stay in the turn. An answer
  deadline (ADR-0039) is armed at the turn with a 2 s margin, which absorbs
  the cap.
- **The first provisional, the gate and the end of a call.** The caller's
  100 is an outbox entry waiting for the call's creation write; the
  transaction layer sends it itself at the §17.2.1 bound if no entry released
  one. The answer the dialog gate gives a request on a live call is an entry
  too: when it took the request in order (the dialog's CSeq advanced), the
  turn writes that change to the change log and the answer waits for it; a
  change to the message ring alone is not written. The turn that ends a call
  enqueues the CDR, then logs the delete, and its entries wait for the
  delete like any other, which the earlier rule (the final a terminal turn
  sends skips the hold) did not: a kill then adds a second CDR, never loses
  one (§5). While its outbox drains, the call stays resident and takes no
  turn, and neither the reaper nor the departure sweep acts on it; it is
  released once the outbox is empty. A takeover copy's terminal turn waits
  for its reverse-flushed write the same way. A stand-down, a self-release
  and a discharge by the backup send what the outbox still holds at once:
  the copy's write records those sends. A drain and a shutdown exit only once
  every outbox is empty, at most the cap after the last entry.
- **Cap hits are reported.** The turn probe's `Left` mark of a turn whose
  entry left at the cap says so; one released because its backup's flow went
  down says that instead. A counter counts the cap hits and a gauge the
  queued entries. No per-call state outlives the call (ADR-0037), and the
  outbox needs no admission term: it is bounded by the message rate times the
  cap. The services stay blind: the outbox lives in the aware crates.
- **A write is abandoned only with its backup.** It stays in the change log
  until the backup applies it, or until the replication link to that backup is
  down (ADR-0045 link liveness) and the call loses its backup.
- **Settings.** The level defaults to `applied` and the cap to half the
  §17.2.1 bound (100 ms); the transaction layer's own 100 leaves at the bound
  itself (200 ms). The backlog marks default to 10 000 entries or 5 s (high)
  and 1 000 entries and 1 s (low). `B2BUA_REPL_SYNC` (`off` | `applied`),
  `B2BUA_REPL_SYNC_CAP_MS` and `B2BUA_REPL_BACKLOG_{HIGH,LOW}_{ENTRIES,AGE_MS}`
  set them; a boot refuses another level, a cap not above zero and below T1
  and the §17.2.1 bound, at either level, or a low mark not under its high
  one.

A graceful drain is transparent: it exits once the backup has caught up and
every outbox is empty. A call with no backup has no peer holding it, so while
one lives the drain waits for its calls or its grace, as a non-HA B2BUA does. A
kill is not transparent: under `applied` the backup holds every write whose
effects left before the cap, and an ACK the kill takes after the backup applied
its write is re-sent by the copy to the callee's 2xx repeat (RFC 3261
§13.2.2.4); what left at the cap, and under `off` a final or a relay that left
inside the replication window, is evidence §4 ends the call on. Full
transparency is not the goal; the window is the transport delay.

### 4. Evidence the call record cannot explain: one rule, any method

One rule, in `b2bua-core`, fires on any node; on a single node only a broken or
forged peer can trigger it. It is a stage of its own, read before every rule of
the chain (user ruling): no service rule sees or pre-empts a message only a lost
write explains, and whether another owner served the call reaches that stage
alone, never a rule's context. On a single node it changes nothing. Its triggers
are closed:

1. a final in a dialog whose CSeq is strictly above the dialog's local sequence,
   any method;
2. a final at or below the local sequence, under a branch of ours that no
   request record holds, at the CSeq of a request the record holds open under
   another branch (the copy re-used a sequence number the dead owner had sent,
   and this final answers the dead owner's request);
3. an ACK naming a dialog by its To-tag and an INVITE on a leg for which the
   call record holds no final sent; the caller's own INVITE counts only on a
   call a takeover copy served (this node is the copy, or the record holds a
   write a copy made): otherwise the copy that answered her is live and its
   write is on its way;
4. a BYE from the callee on a dialog the call record holds as early (RFC 3261
   §15 forbids it before a 2xx); on a call one node served it is a broken
   callee, whose caller holds no 2xx, and the default `relay-bye` ends the call
   on it;
5. a 2xx whose via-extension (§12) names a leg the call record lacks;
6. a re-INVITE from the caller naming her dialog by its To-tag, on a call a
   takeover copy served, while the call record holds her INVITE unanswered: a
   re-INVITE needs a confirmed dialog, so it proves the 2xx she holds (user
   ruling). The node relinquishes her INVITE's server transaction, which draws
   no final, answers the re-INVITE 487, BYEs her dialog under its tag, and
   CANCELs each callee INVITE the record holds unanswered, a confirmed callee
   dialog BYEd.

A final with no request record that matches none of these is stray and is
discarded or re-ACKed as today (§17.1.1.2, §18.1.2). A caller's BYE on an early
dialog, an UPDATE, PRACK or INFO on an early dialog (each allowed there), and a
request with an unknown To-tag (481, §12.2.2) are not evidence. A response for a call no node
holds is answered as an unknown dialog would be, whichever node sent the
request: a 2xx is ACKed and BYEd from its own headers on this node's Via, a
non-2xx INVITE final is ACKed from the via-extension.

The rule sends what RFC 3261 requires for the message (the ACK of an INVITE
final, §13.2.2.4, §17.1.1.3) and ends the call cleanly over every dialog the
call record or the evidence names:

- a dialog the evidence names is BYEd with the evidence's tags and the route
  set the call record holds; this is construction, the call record is not
  repaired;
- a caller the call record holds as unanswered, when the evidence proves a
  callee leg answered (trigger 4 on a call a copy served, trigger 5), may hold a
  2xx the call record never saw: the node sends the owed non-2xx at once, under
  the tag that 2xx carried (a UAC that has the 2xx discards it, RFC 6026), holds
  that dialog as possibly confirmed, and BYEs it on her ACK naming it or at
  64·T1; the call stays terminating until then. Every tag a B2BUA mints toward the caller is derived from a per-call
  seed, drawn from the keyed identifier stream at the call's birth and
  replicated with its record, and from what the tag fronts (the leg and the
  callee's tag; RFC 3261 §19.3), so any owner computes the same tag. A BYE that
  meets no dialog draws a 481 and does no harm; an ACK proves its INVITE
  answered, so that INVITE's transaction draws no final here; a re-INVITE's
  final proves the dialog it rides confirmed and the INVITE that dialled it
  answered, so that INVITE draws no CANCEL;
- one CDR (§5), every hold released.

Every effect outside SIP is keyed per call: a limiter admit is keyed and expires
by lease unless an owner refreshes it (ADR-0040), and a consult whose route
fold the record never applied counts as one that may have sent its admit; an
off-turn request is delivered at least once (§1); the CDR is written by
whoever ends the call (§5).

### 5. The CDR: whoever ends the call writes it

The CDR carries the `call_ref` and the call incarnation. No CDR key, no
consumer dedupe and no discharged marker: the node that ends the call writes
its CDR — the primary, a takeover copy whose primary departed (ADR-0047 D8),
the departure sweep, the TTL reap; a copy whose primary is still a member
defers it to the primary (ADR-0020 X3). An HA corner case may write a second
CDR, and it is accepted: a primary killed after ending a call whose end its
backup never applied, a split. By user ruling a kill of a primary with a backup
never loses the CDR: a split may, and a call with no backup is lost with its
primary as on a non-HA B2BUA, leaving one CDR or none (user ruling).

The departure sweep takes over every call of a departed primary with an INVITE
transaction to serve, and every `Terminating` one with any request record open
(its own BYE in flight, a request it held) or a caller dialog held possibly
confirmed (§4); a `Terminating` call with nothing
left to serve finished its ending on the departed primary and is discharged
with its CDR, which may be the second (the primary wrote one when it ended the
call, and only its delete was lost). By user ruling, a quiet call of a departed
primary stays reactive (ADR-0014 §1): no eager takeover. The trade-off is a
ghost call whose parties stay silent: it ends at the endpoints' own timers
(session refresh, RTP inactivity) and at the replica's TTL, with no BYE; the
reap that evicts the replica of a departed primary's call ends it, off the
wire, with its CDR. A takeover copy that sheds once the transactions it served
cleared leaves the call to that reap the same way; a copy that holds the end
of a call whose primary departed (the call ending, or a 2xx awaiting its ACK)
never sheds, as no other node runs those deadlines, and it writes the CDR once
the call ends.

### 6. One author per ordinal: identity, compared for equality

Each worker identity is the orchestrator's pod UID (the informer's
`targetRef.uid`, its own from the downward API), or a per-boot token in a static
pool. Membership carries it (`topology::Peer`), and a changed identity is handled
as a changed address. A worker that finds its own ordinal bound to another
identity enters `Superseded`: it stops authoring at once (no SIP, no write, no
CDR) and exits without draining. Peers need no refusal rule: replication is
pull-only (§3) and a peer pulls from the address membership binds to the ordinal
(`repl-net/src/frame.rs:136-200`). A puller states its identity when it opens
a flow, and the serving side counts a flow's applied positions only from the
identity membership binds to the puller's ordinal. A request record names a relay's owner by
ordinal and worker identity. No order is drawn from a clock.

A worker cut off from the orchestrator is not fenced; its writes meet §7 at the
heal and its SIP meets §4.

### 7. Two writers after a split: containment

Containment holds for two copies of one call incarnation, under pull-only
replication, and with no vector ever adopted without its body (the adoption mark
is deleted).

Each copy carries a state stamp in the store envelope: the vector of its last
write that changed a compared field, set at the one site that bumps the vector.
Of two concurrent copies, R contains L when L's stamp is at or below R's vector,
component by component.

- One contains the other: take it whole and merge the exempt fields; the merged
  copy's vector is the component-wise maximum and its stamp is the container's.
- Each contains the other: take the primary's.
- Neither: the node that meets the other copy ends the call cleanly (§4), from
  the copy ranked higher by `lifecycle_behind` (the primary's on a tie), over
  the union of both copies' dialogs by tags (ADR-0041): each dialog's state is
  the further of the two with that copy's INVITE exchange and the answer it
  owes (RFC 3264 §4), so the ending's
  teardown ACKs and BYEs a 2xx either copy took (RFC 3261 §13.2.2.4, §15), the
  BYE goes at the greater CSeq plus one, answered is the OR of both, CDR events
  are the union.

A leg id names one leg in both copies: the record holds two leg-id spaces,
the node the call was created on mints `b-{n}` and any other node acting on
the call `c-{n}` (user ruling), so two copies acting at once never mint one id
for two legs, the union pairs legs by id, and every lookup by leg id is
unchanged. The space a node mints from is an exempt field (the container's
value stands); a takeover copy that plays on from the primary's copy keeps its
own space and its own exempt ledger entries, which are what it runs.

The exempt fields are a closed list; every other field is compared, so a new
field fails safe:

- maximum: the vector, each dialog's local and remote CSeq (§12.2.1.1), the
  message and turn counters, the timer generation, the limiter's change
  counter (no change number either copy reserved is reused);
- union: CDR events, each leg's message ring, a limiter release either copy
  owes;
- the container's or the base copy's value stands: keepalive request records,
  keepalive and refresh ledger entries and the ladder rungs a quiet turn moves
  (none changes a dialog), the skew offsets of §9, trace fields, the leg-id
  space a node mints from.

A node that meets the other copy's ending discharges it keeping every dialog
either copy opened, at the higher of the two sequences. A dialog it holds
further along than the ending does (confirmed where the ending holds it
early) is its own to end: it ACKs the 2xx and BYEs the dialog. An answer
that ACK owes rode the caller's ACK, which the other copy took: no ACK this
node awaits brings it, so the 2xx is ACKed with every stream rejected (RFC
3264 §6). There is no per-axis fold.

### 8. The proxy routes on one rule

A response, and an in-dialog request toward the B2BUA, goes to the owner that
holds its transaction; if that owner is not alive, to the call's other owner; a
call with one owner goes to it. The rule is the same for every method, and the
proxy's Via cookie is pushed on every method. The call's backup is stated by the
worker on every request it sends for the call, and the proxy signs it into that
leg's cookie, so every leg names the same backup.

A worker's own in-dialog requests keep following the route set through the
proxy, which forwards them without an ownership check. While two copies run (a
split, or the moment before an owner learns the other took a dialog), both can
mint one CSeq on a dialog; §11's split cells declare that window as an RFC
deviation, and §7 ends a call whose copies diverged.

A worker reclaims its own calls in two steps. Once recovered, whether or not
its own endpoint reads ready, it makes every call of its partition resident
and parks it: no timer armed, no transaction seeded, nothing sent. A call with
an INVITE to serve is written once (ADR-0047 D7), so the backup's takeover
copy stands down on that write, and the burst of those writes drains, before
the proxy routes anything to the worker. The worker arms the parked calls —
their timers, their transactions, the refusal of an undecided call — only
while its own endpoint reads ready in the slice the proxy routes from. While
it does not, the proxy holds no address for it, so the response to a request
it sends goes to the call's other owner, which reads it as evidence (§4) and
ends the call. The evidence rule stays as it is, every method alike
(`reclaim_waits_for_a_routable_endpoint.rs`,
`swept_call_answered_once_routable.rs`). The worker and the proxy read the
slice through informers of their own, so the two can see the endpoint ready a
few milliseconds apart; a message reaching a parked call arms it first.

A call answered at the returned worker while a proxy still reads it `Unknown`
sends the caller's ACK to the backup (ADR-0050 §4). When the answer's write
reaches the backup past the cap of §3, the backup reads that ACK as evidence
(§4, trigger 3) and ends the call from the copy it holds, CANCELling the
callee's INVITE it holds unanswered, and the worker, meeting that ending with
the callee's dialog confirmed, ACKs and BYEs it (§7): the call ends cleanly
(§1; `swept_call_answered_once_routable.rs`, the cell whose answer write is
late).

### 9. Every instant in the call record carries its own anchor

Every absolute instant the call record holds (ledger deadlines, `sent_at`
stamps, the INVITE's send time, the creation time) is replicated with the skew
composed across hops, and a local write resets the offset, so it survives any
number of takeovers unshifted. Two copies that meet, in the store or at a live
copy, are read in one clock frame first, and an item of a union both hold (a
CDR event, a ring entry) is held once when its two instants read within twice
the re-anchor deadband of each other.

### 10. The seam carries no HA value

- A leg's ring state is `Silent` or `Rang`. A restore that finds no 18x on
  record keeps the core's ring deadline running from the INVITE, inside the
  core; services read the leg as not having rung.
- A timer declares its scope (ADR-0042), and a scope "the call's setup", over
  while any leg holds a confirmed dialog and once the call is terminating,
  joins `Call`, `Attempt`, `Leg` and `Obligation`
  (`call/src/model/timer.rs:259-264`). A fire whose scope ended, on a restored
  ledger or not, is dropped by the turn engine. A rule never re-checks for a
  stale ledger, so a service overriding a core rule takes on nothing.
- `CallEvent` carries no `matched_client_txn` and no `CallQuiesced`;
  `B2buaConfig` carries no HA field. Both move to the aware crates.

### 11. The proof

- **The blind/aware boundary.** Every crate's manifest declares
  `[package.metadata.ha] side = "blind" | "aware"`; `scripts/purity.py` checks
  that a blind crate's normal dependencies are all blind. Blind: `call`,
  `b2bua-core`, `b2bua-sdk`, consumer service and decision crates. Aware:
  `b2bua`, `repl-net`, `sip-proxy`, the runners, the harnesses and every
  end-to-end test crate. The vector leaves the call record for the store
  envelope; the topology cookie leaves `b2bua-core`; a consumer's decision crate
  depends on a blind seam crate, not on `b2bua`.
- **A word net** over the blind crates refuses `takeover`, `reclaim`,
  `tombstone`, `materialise`/`materialize`, `stand-down` and `replica` as a whole
  word. These words have no SIP or domain meaning there. No allowlist.
- **A failover sweep** in `failover-harness`, on the paused clock, in a lane
  of its own: one test per shape, incident and sync level. Every shape
  of a catalogue (the caller's CANCEL, the callee's reject, re-INVITE, UPDATE,
  REFER with its NOTIFYs, INFO, PRACK, a BYE crossing each, the keepalive, a
  forked answer, a media-server leg) is
  cut at every turn point, each split into write in the change log, effect
  left and write applied at the backup, by these incidents, at `applied` and
  at `off`:
  - primary killed, backup killed: each cell ends with an RFC-clean, fully
    terminated call, one CDR, full cleanup; a cell killing the primary may
    leave two (the dead primary's and the survivor's, §5), never none;
  - primary killed unannounced (ADR-0031 case 3): the proxy's probe reads it
    dead 2 s later and tells the backup, which takes over the calls with work
    in flight but an undecided one (ADR-0047 D1); its endpoint reads not ready
    but stays in the slice 40 s later, and leaves it when the evicted pod is
    force-deleted 5 min later, when the backup settles and discharges what is
    left; judged as a kill of the primary;
  - primary killed and rebooted (ADR-0031 case 2): restarted empty at a new
    address 1 s later, published once ready, it reclaims its calls from the
    backup; judged as a kill of the primary;
  - split, healed or not: judged like a split that never heals below;
  - replication stalled past the cap of §3, then a kill: judged like a split
    (user ruling), on leaks, cleanup and the CDR count — one or two (the
    stalled primary's and the departure sweep's, §5), never none — and on a
    final for every request that reaches a living worker: the stale copy
    contradicts what the stalled primary released at the cap as a split's
    other copy would;
  - a backlog over the high-water mark, or a backup at its memory ceiling, then
    a kill: a call with no backup is lost as on a non-HA B2BUA.

  The parties' verdict is checked in every cell (user ruling). The parties
  are RFC 3261 transactions over UDP: every request but ACK goes again on
  Timer A or E until it is answered, and every final to an INVITE until its
  ACK; a party that plays a broken peer opts out by name. The uncut call must
  succeed. A party honours a 500's Retry-After on a re-INVITE (RFC 3261
  §14.1) and ends a dialog on a 481 or 408 to a request inside it
  (§12.2.1.2). A request left without its final, or a dialog left open,
  fails the cell, except as a row of the window table below declares.

  **Tiers** (user ruling). Each cell is placed in a tier by what it measured,
  strongest first: the tier of the cell is the weakest of any call its
  cluster held.
  - **Guaranteed**: level `applied`; every call was backed and bootstrapped
    at every turn up to the strike (the turn probe reports it at the logged
    sub-point); no outbox entry hit the cap (a release because the backup
    left, or one the harness's own hold of an applied sub-point caused, is
    no cap hit); the test tampered with no replication stream or replica;
    the incident is a kill. The cell is perfect: no row of any kind opens,
    every party goal is met, the evidence rule (§4) ends no call, and the CDR
    count is §5's.
  - **Degraded**: level `applied`, any other case: every row below opens.
  - **Off**: level `off`: Degraded's rows, and the off-only rows a user
    ruling adds.

  **The window table** (`failover-harness`, `windows`). Every deviation an
  incident may cause is one row: an RFC-rule window, a dialog-verdict waiver,
  a party outcome, a clean-end goal drop or a CDR count. A row names the
  strongest tier it opens in (it is open in every weaker one, never in
  Guaranteed), its incidents and sub-points, what it covers, its scope and
  its predicate; a predicate that differs by tier is a row of its own. A test
  never picks a window: it strikes an incident and is given that incident's
  rows; its measured tier decides which of the rows that fired it accepts.
  Each accepted finding names its row, and of two rows that could take a
  finding the first in table order takes it. Each row names an upstream
  witness cell, which must fire it. A hand-written test whose fault no
  incident names yet keeps its windows under transitional rows (`T-…`) until
  the sweep gains the incident. The rows, each with the id the table
  carries:
  - `W-clean-end`: once the incident strikes, the system under test may end
    the call cleanly at any instant (§1): a party it ends cleanly — a non-2xx
    to her INVITE, a BYE she answers 200, a NOTIFY ending her subscription
    (`Subscription-State: terminated`) — drops her remaining goals, and a
    caller so ended ends the call cleanly, which the verdict says. Every
    incident but a split.
  - a split, healed or not, and a stall past the cap then a kill (user
    ruling): what two acting copies, or a stale copy and what the stalled
    primary released at the cap, send —
    `W-split-ended-dialog`, the stale copy's requests into a dialog the other
    copy ended, each closed by the 481 it draws;
    `W-split-cancel-answered`, one copy's CANCEL of an INVITE the other copy
    saw answered, and its repeats after the heal;
    `W-split-non-2xx-after-final`, one copy's non-2xx after the final the
    other copy sent the caller, under whatever To-tag, which she discards,
    and its repeats;
    `W-split-early-bye`, one copy's BYE of a caller dialog it holds early,
    which the other copy answered;
    and, for a split only, `W-split-two-byes`, two BYEs of one dialog at one
    CSeq, and `W-split-cseq`, a CSeq one copy re-uses after the other sent
    it;
  - `W-split-outcome`: a split, healed or not, is judged on leaks, cleanup
    and the CDR count only, never on the parties' verdict (user ruling);
    `W-cdr-split`: a split that never heals leaves none to two CDRs;
    `W-cdr-two`: a healed split, or a stall then a kill, one or two;
  - `W-stall-outcome`: a stall then a kill is judged like a split (user
    ruling), every request that reaches a living worker drawing its final
    there, a transaction the dead primary took being its own; a stalled copy
    answering an UPDATE 500 with Retry-After, its INVITE unanswered, is RFC
    3311 §5.2;
  - `W-stall-cancel-reason`, a stall (user ruling): a copy's repeat of a
    CANCEL that differs from the dead owner's only in its Reason header
    field; every other difference keeps the finding;
  - `W-stall-cseq`, a stall (user ruling): a copy's requests at the CSeqs the
    dead owner sent on the dialog during the stall, each closed by the 481 it
    draws;
  - `W-unbacked-lost`, a call with no backup killed with its primary (user
    ruling): every dialog, the caller's included, may be left to its
    endpoint's own timers; a final is still owed for every request that
    reaches a living worker, a transaction the dead primary took being its
    own, its repeats included wherever they land. A re-INVITE of such a call
    that drew only the dead primary's 100 is a non-HA death.
    `W-cdr-unbacked`: it leaves one CDR or none;
  - `W-kill-cancel-answered`, a kill (user ruling): a copy's one CANCEL of a
    callee's INVITE the dead owner saw finally answered, 2xx or not; the
    callee answers it and keeps what its final left it, a dialog its own
    timers end or none;
  - `W-clean-end-481`, a kill of the primary inside the replication window, a
    stall then a kill and a call with no backup (user ruling): the §4 clean
    end's BYE, or terminating NOTIFY, into a dialog the other owner already
    ended, closed by its 481, once per dialog. A kill at an applied sub-point
    past which the primary wrote nothing opens no such window; a healed
    split's is `W-split-ended-dialog`;
  - `W-kill-teardown-lost`, a kill of the primary before the backup holds the
    turn's write with its effects left — a logged or left sub-point under
    `off` or past the cap, a logged or applied one under `applied` (user
    ruling): a dialog left to its endpoint's own timer when the write of a
    teardown the node began was not applied at the kill; a quiet call of a
    departed primary stays reactive (§5);
  - `W-kill-effects-lost`, such a kill at a turn taken on something no peer
    repeats — a response, a timer, an off-turn answer (user ruling): the
    turn's effects are lost with the node, the parties wait for them in vain,
    and every dialog, early or confirmed, is left to its endpoint's own
    timers. A turn taken on a request is not in this window: under `applied`
    the message that stops its sender repeating it waits for the write (§3);
  - `W-kill-answer-lost`, a kill of the primary once the backup holds the
    write of a turn that sent a request to an outside service (a routing
    service, the limiter), or after which the call awaited one's answer (its
    deadline in the ledger, ADR-0039), whether that request left or not, the
    backup applying no later turn of the primary (user ruling): the answer is
    lost with the dead node, a turn that took it included, and the copy stays
    reactive (§5); every dialog is left to its endpoint's own timers, and
    when the turn's effects had not left, every request that reaches a living
    worker still draws its final there;
  - `W-off-answer-lost`, off only: a kill of the primary after a turn's
    request to an outside service left and before the backup applied the
    turn's write (user ruling): the answer returns to the dead node and is
    lost, a consequence of not waiting for the write; the parties wait for
    its outcome in vain, never for the teardown, every dialog is ended
    cleanly or left to its endpoint's own timer, and every request that
    reaches a living worker still draws its final there;
  - `W-kill-cseq`, a kill inside the replication window: a copy re-uses a
    CSeq the dead primary sent before the backup applied the write, at most
    one rejected request per dialog; the call still ends or continues
    cleanly. A call with no backup is inside the window for its whole life:
    the node answering a response no node holds (§4) re-uses a CSeq the same
    way. A kill at an applied sub-point is inside the window only when the
    primary already wrote a later turn;
  - `W-kill-rseq`, a kill inside the replication window, and a split that
    heals (user ruling): a copy's reliable provisional under the dialog's
    derived tag whose RSeq cannot continue the sequence the dead primary sent
    (RFC 3262 §4), once per dialog; the call still ends cleanly;
  - `W-kill-non-2xx-after-2xx`, a kill inside the replication window (user
    ruling, where no 2xx the copy could build would repeat the dead primary's
    byte for byte): a copy's non-2xx to a caller's INVITE the dead primary
    answered 2xx before the kill; she discards it (RFC 6026 §8.4) and owes it
    no ACK.

  A split that never heals is not supposed to last: it cuts a representative
  few shapes (the re-INVITE, the REFER), whose cells check the worst case, no
  leaked resource (the call ends on every side, full cleanup, the limiter
  released). A consumer adds shapes through the plug-in base (ADR-0049) and
  nothing else; its sweep uses the same table and tiers, declares no row and
  checks no witness. A consumer shape that needs HA code of its own is a
  defect of this layer.
- **A ratchet.** A cell that fails before its correction lands is listed with
  the change that owns it and its cause, and so is a Guaranteed cell that
  fires a row; each test's count of Guaranteed cells is listed too and must
  not fall (a rise is recorded). A new failure fails the test, so does a
  listed cell that passes. Each change flips the tests that pin the residual
  it removes (`new_call_replication_window.rs`, `lost_round_rules.rs`, …) in
  the same commit. The list only shrinks and is empty when §2–§12 have
  landed.

### 12. A request the worker originates names its leg

The worker's own Via carries a via-extension (§20.42): the leg, the request's
Request-URI and whether it carried a session offer, signed by the worker with
the cluster key over the decoded `cr`, `lg` and `ci` of the Via, its branch and
the request's method, bounded in size and encoded as a token, so any owner
that holds no request record of a dialled INVITE can CANCEL it or ACK its
non-2xx from the callee's response (§9.1, §17.1.1.3), and ACK and BYE its 2xx,
with the answer an offer the 2xx makes is owed (§13.2.2.4, RFC 3264 §4). A
response for a call no node holds draws nothing without an extension that
verifies. An extension whose signature fails is ignored; a worker with no
cluster key states none and trusts none, and a worker with replication on
refuses to start without the key (fail closed). The deployment provides the
key as a Secret it keeps; nothing draws it at render time.

### 13. Harness verbs

`failover-harness` gains: placement of a call on a named primary and backup (it
picks a Call-ID that lands there); steering of a party's next request or of its
responses to a named role, carried out through real causes (health, kill,
partition), never by bypassing the proxy; turn points with their three
sub-points, so an incident arms at turn k; and views named by role (primary,
backup, copy). Under `applied` with no cap hit the sub-points run `Logged →
Applied → Left`; under `off`, or past the cap, `Logged → Left → Applied`.
`Left` is reached when the turn's last outbox entry left, possibly after later
turns were taken: it carries the version the turn logged and names the turn
by it, as `Applied` does, and says why the entry left. A cap release while the
harness holds an applied sub-point of the call is the harness's own doing.

### 14. A leg states one Contact

A leg's Contact is allocated once, when the leg is created, and stored in the
call record: every message of the leg that states a Contact states that one,
whichever copy sends it, so a takeover never moves the peer's remote target
and a copy's repeat of a response the other owner sent states the same
Contact.
Behind the proxy the Contact names the proxy's peer-facing address; a
standalone worker's names the worker. The worker's mode is explicit, and a
worker whose parameters do not match its mode refuses to start. The Via keeps
naming the node that sends.

## Considered and rejected

- **Wait for the backup with no cap** before an effect leaves. A lost or slow
  peer would stall every call; §3 waits at most the cap, and only for a live,
  bootstrapped backup.
- **A `sent` level**, releasing an entry once its write left for the backup.
  It was discussed and is not part of §3.
- **Drop a write when the queue is full.** A dropped confirm makes §4 end an
  established call at the next kill. §3 keeps one entry per call and gives new
  calls no backup instead.
- **Refuse new calls on replication backlog.** A non-HA call is better than a
  refused one.
- **Repair by inference** on the copy. Each inference is per state and per
  method. §4 constructs the end from the evidence and repairs nothing.
- **A per-axis fold, or a lifecycle winner after a split.** The lifecycle order
  is partial and undefined on concurrent writes. §7 decides by containment and
  uses the lifecycle rank only to pick the base of an ending.
- **An ordered epoch.** The worker generation is wall-clock derived and can step
  backward (`repl/changelog.rs:366-386`); "refuse the elder" would fence the
  live node.
- **The proxy refusing a worker's own request when it routes the call
  elsewhere.** It would close the duplicate-CSeq window of a split, but a health
  flap would refuse a healthy worker's keepalive or BYE, and the two proxy
  replicas can disagree. §8 declares the window instead.
- **A word list over every crate.** "fold", "seed", "incarnation", "epoch",
  "backup" carry non-HA meanings in the blind crates. §11 makes the boundary a
  dependency rule.
- **Exactly-once CDR.** No design without consensus delivers it across a split.
- **A returned worker holding a call's messages until its copy met the
  backup's takeover copy.** A stand-down writes nothing, so the hold needs a
  new replicated fact, and a copy that plays on (ADR-0047 D7) keeps it for a
  transaction's lifetime. It would not cover the caller's ACK either, which
  the proxy routes to the backup while it reads the worker `Unknown`. §8's
  parking makes the meeting precede routability instead.
- **The proxy routing a response to a takeover copy's transaction, or keeping
  a returned worker's calls on the backup until it declares its reclaim
  met.** The proxy holds no call state, its replicas flip apart, a backup
  write between the declaration and the flip still diverges, and neither
  covers the caller's ACK.

## Consequences

- A call whose write was lost at a kill, or whose copies diverged across a
  split, ends cleanly. With replication sent on write the window is the
  transport delay; at a few hundred state-changing turns a second, it is under
  one call per kill.
- Under `applied`, every effect a turn sends waits for the backup, by at most
  the cap; under `off`, none does.
- Under a replication backlog or a backup at its memory ceiling, new calls run
  without HA; the gauge shows how many.
- The call record, `Dialog` and the SDK change shape; no compatibility is kept.
- The rule files that know about takeover today lose that knowledge; a new
  in-dialog method or service gets HA by a row in the method table and a shape in
  the catalogue.
- The CDR writer's overload buffer is outside this decision; the sweep reads
  CDRs from the in-memory writer.

## Implementation order

Each step lands on its own with the ratchet green; a step that changes the SDK
lands with the consumer's pointer bump.

1. Harness verbs with sub-points (§13), the sweep and its ratchet (§11), on
   today's code.
2. Worker identity (§6); anchors (§9). Independent of each other.
3. The replication write path (§3).
4. The request record, in four changes:
   a. the Via cookie on every method and the one routing rule (§8);
   b. non-INVITE request records and their seeds (§2);
   c. the INVITE fields folded into the request record (§2);
   d. the subscription record, with the consumer's queue deleted.
5. The via-extension (§12), the evidence rule (§4) and the CDR of §5.
6. Containment with the state stamp in the store envelope (§7).
7. The timer scope and the seam (§10).
8. The blind/aware boundary and the word net (§11).
