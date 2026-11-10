# Appendix: Admission is bounded by memory; emergency calls keep a higher ceiling

The decision part is [ADR-0037](../0037-admission-is-bounded-by-memory.md).

## Context

Admission sheds on rate (the CPS bucket), on the worker's own loop (the
panic-ELU backstop) and on ingress depth (the queue brake). None of them
bounds what the worker holds. Long calls at an admitted rate keep growing it,
emergency calls bypass the bucket, and a takeover adds a peer's calls all at
once. Each live call costs a roughly fixed amount, but the cost depends on the
call's traffic, and the transaction table and the backup replicas held for
peers grow beside it. A worker with no bound of its own grows until the kernel
kills it, and a kill drops every call it serves.

## Decision

### 3. The reject

**The reject is a 503 with a jittered `Retry-After` and no `Reason`,**
whichever rung of the admission ladder (item 11) refuses a new INVITE,
from one renderer: the To-tag and the jitter roll are derived from the
request and the `Retry-After` is floored once. The rungs ahead of any
transaction share one memo of refused INVITEs, so every copy of one
INVITE draws the same bytes (RFC 3261 §8.2.7), and the brake spares a copy
of an INVITE a live server transaction holds; the rungs behind it send the
same 503 through the INVITE server transaction. In-dialog requests are
never refused, but for item 6. A refusal is counted once, on
`b2bua_new_calls_total`; a copy refused again is counted apart.

### 4. Backup replicas

**Backup replicas have their own ceilings:** a count and an RSS ceiling,
at least the admission ones, in a pool whose workers run one setting: the
count at least the emergency call ceiling, the RSS at least the emergency
RSS ceiling (refused at boot otherwise). So every call a peer admits can be
backed, and this node stores backups as long as it admits calls itself. At
either, a replica of a call this node
does not hold yet is refused: the backup tells the primary in a frame, its
reported position moves past the refused write, and it refuses the later
writes of that call until its delete or the refusal's TTL. The call has no
backup for its whole life (ADR-0055 §3). Updates and deletes of held replicas, and a node's own calls coming
back from a peer, always apply.

### 6. The deferred backlog

**The transaction layer's deferred backlog is an internal bound, always
on.** It holds the critical events a full event queue keeps for a router
that stops draining, among them every INVITE the layer admitted, whose
100 Trying leaves at the §17.2.1 bound when no turn releases it
(ADR-0055 §3). No other rung sees that stall: the ingress brake reads a queue the
transaction layer drains at parse speed, and the router's rungs (item 11)
run behind it. No operator setting sizes the event queue, so the
backlog's ceilings follow it: one queue of events for a new call, two for
an emergency call and for an INVITE carrying a To-tag, whose dialog the
layer cannot check (the backlog row of item 11). Item 2's rule on settings
covers the host-dependent bounds of item 1. Past a ceiling the layer sends
item 3's 503 statelessly, before any 100 Trying and after matching
retransmissions of admitted INVITEs; a 503 to a re-INVITE leaves its dialog in place. Each refused
INVITE is remembered for 64·T1 in the memo shared with the ingress brake
(65 536 at once, more behind a larger ingress queue), so its copies draw
the same 503 at either stage and its ACK ends in the layer; a copy
arriving after that is judged afresh. The ceiling counts every
critical event, so a takeover burst of `CallQuiesced` events can refuse
new calls until the router catches up.

### 7. The sizing rule

**An admitted call's requests are never refused, but by item 6.** This is
the operator's sizing rule; the process does not compute it. The new-call
ceilings sit far enough below the process's memory limit that all an
admitted call can still cost fits above them: its in-dialog transactions,
the backup replicas that turn live when a peer dies, every call's
teardown, and the transactions refused INVITEs and given-up client
INVITEs hold. With `C` the emergency call ceiling, `B` the backup count
ceiling, `ρ` a held replica's cost (the serialized body and its
metadata, not the decoded call: measured apart, below a live call's), `N`
the emergency transaction ceiling, `r` a generous in-dialog
request rate per call beyond setup and teardown (two transactions per
relayed request, the one it opens here and the one it is relayed on, one
per keepalive probe the node originates, each held 64·T1), two transactions for a teardown, `q` the rate of INVITEs answered
through a transaction and left held (a refusal until its ACK and Timer I,
64·T1 without one; a client INVITE given up, 64·T1), `h` that hold, the
per-call and per-transaction costs and the base measured on the
deployment under a held peak (its allocator footprint report), and a
slack `s` for RSS lag and allocator retention. The formula is the decision
part's, item 7.

The RSS ceiling already counts the replicas held, `B · ρ`; a takeover grows
each to a live call, `call − ρ` more.

Setup and teardown churn at the admission rate is part of `N`, which
counts every transaction. `q` is bounded only by the stateless brakes
ahead of the router: a refused flood past what the margin holds must be
shed there. Under overload the only refusal is a new call's 503.

### 8. Teardown

**Teardown is never refused.** No table, queue or dispatch limit refuses
the node's own BYE, CANCEL or ACK, or their answers: the transaction table
has no cap, a final matched to a client transaction and a `Timeout` are
critical events the layer never drops, the router admits transaction
outcomes past every per-call bound, and the brakes shed initial INVITEs
only. The kernel's socket buffers can lose them; the retransmission
timers recover.

### 9. Off-turn answers

**The answer to a request a call sent off its turn is never refused.** A
consult's fold, a service's HTTP or admit result and a limiter refresh's
answer each come back once, as an internal event, and nothing sends them
again: the router admits them past every per-call bound, like a timer
fire or a transaction outcome. The answers queued at once are at most the
requests in flight. A request leaves only on one of the call's turns: one
the node raised (a timer fire, an answer) or a peer's request admitted in
bounded room, which the call refuses while answers wait past its queue and
which its lifetime cap counts. Item 7's formula has no term for them: a
queued answer holds what its request brought back, an HTTP response body
among them, whose size nothing here caps. A deployment whose services send
a request per peer request sizes that margin itself. A request a peer sends
keeps its bounded room.

### 10. New-call thresholds

**New calls have their own dispatcher thresholds.** A normal initial
INVITE's turn holds a permit of a new-call share of the handler permits
(`new_call_permit_share_percent`, default 50 % of
`event_dispatch_concurrency`) besides its shared one, and its INVITE opens
a call's queue only below `per_call_queue_cap` less a headroom
(`new_call_queue_headroom_percent`, default 5 %), refused there by the
shed rung (item 11). An emergency INVITE and every other event draw the
shared pool alone and open a queue up to the full cap, so new calls on a
stalled decision backend leave the established calls their permits, and a
taken-over call's first request finds a queue. No rung reads a turn's wait
for its permits: the setup budget bounds it (ADR-0022 X1).

### 11. The admission ladder

**The admission ladder is one table** (`b2bua::admission`, `LADDER`),
in the order a new INVITE meets the rungs. The table is in the decision
part, item 11.

Each rung keeps its site. The backlog is decided by the transaction
layer's bound; every other rung by one function over its input and the
INVITE's class, the router reading its rungs' inputs in table order up to
the first refusal. The router's rungs run at ingress, before the
dispatch offer, so a refusal opens no per-call queue and waits for no
handler permit, the new-call share of item 10 included. The panic-ELU
backstop is judged before the bucket, and a CPS token is spent only when
the offer queues the admitted INVITE's turn: a refusal spends none. The
router's run loop is the bucket's only taker, so the token its rung saw
is still there when the turn is queued. An admitted INVITE counts as a
live call for the capacity rung from its admission until its turn creates
the call or ends without one. An INVITE on the identity and CSeq of a
live call, or of a new call admitted and not born yet, is that call's
copy (RFC 3261 §8.2.2.2): it is not judged, creates no call and is
answered 482, counted as a copy. An INVITE on the identity with another
CSeq (a retry after a 401/407/422/3xx, §8.1.3.5) is judged as a new call;
if a call on the identity is still here when its turn runs it is answered
500 with a Retry-After (§14.2), and behind that call's queued release it
waits for the release and is born on the call's next queue. A queue
another request opened admits nothing.
The turn keeps the store-fault probe (a 500, outside the table) and the
creation of the call.

The event-queue rung reads the same gate as the OPTIONS answer's
`cause=overload`: one state, one set of bounds, one hold (ADR-0050 §9).
The transaction layer judges a new INVITE as it arrives, before the 100
Trying and before it would wait in the backed-up queue: it reads the
state the gate's last reading wrote (one atomic load) and refuses with
the worker's one refusal, so a copy draws the same answer, counted once.
The router writes that state at each event it takes, each probe and each
INVITE it judges, and every 100 ms while idle and refusing; while it is
held in one turn the state is the one from before it. So the layer also
reads the age of the queue's oldest event itself (a clock read and a few
atomics) and refuses from the age that trips the gate at a single reading,
its delay plus its interval (300 + 150 ms): a router stopped on one turn
draws the refusal from about 0.45 s after the first event behind it, and a
gap the gate tolerates, a CPU-quota throttle or a slow turn, refuses nothing.
The age holds no state: the refusing the gate latches for its hold starts
at the router's next reading. An INVITE admitted under that age still hears
its 100 at the §17.2.1 bound and waits in the queue: at 1000 calls/s on two
workers about 225 INVITEs behind a stopped router, which reach no record
until it turns or the process exits (ADR-0033 §4).
An INVITE admitted on arrival is judged again at router ingress, after the
other rungs, on
a fresh reading: it catches an INVITE that queued before the gate
tripped. On arrival the rung comes right after the backlog, so an INVITE
that the capacity, shed, panic-ELU or bucket rung would also refuse is
counted under it; the re-check comes after the bucket. The rung refuses from the first reading that trips the gate,
before any probe reads the refusal, and keeps refusing the new calls
that still reach the worker (in flight, or placed on it while every
worker refuses) until the gate accepts. It judges as the panic-ELU rung,
a load signal: an emergency INVITE and one carrying a `To`-tag are
admitted. A refusal is counted once under `reason="event_queue"`. It reads the
transaction event queue, which no other rung does: under a CPU cap the
loop's busy ratio stays low and the inbound queue and deferred backlog
stay short while the router falls seconds behind.

## Consequences

- An ingress queue slot holds the parsed message when its receive path parsed
  it (every front-proxy datagram; a worker datagram the engaged brake judged;
  ADR-0051 §5): a few KiB for ordinary traffic, and under about 200 KiB for a
  64 KiB datagram, since the parser refuses a header block past 100 lines.
  `base` in item 7 counts `queue_max` such slots per socket.
- A call whose replica was refused has no backup for life; a primary failure
  loses it as a non-HA B2BUA would. The refusal is counted
  (`b2bua_repl_backup_shed_total`) and the live unbacked calls gauged
  (`b2bua_repl_calls_unbacked`), never silent.
- The counts are exact per decision: the router's run loop judges every new
  INVITE, and one it admits counts as a live call from its admission until
  its turn creates the call, so new calls never take the live calls past the
  call ceiling while their turns wait for a handler permit. A backup count
  ceiling is overshot by at most one replica per other peer.
- RSS lags up to one sample and stays high after a drain while the allocator
  keeps freed pages, so the RSS ceilings must leave room below the process
  limit; the counts react at once.
- `B2BUA_MAX_RSS` covers the live heap and what the allocator holds around
  it, so it is sized from the measured process, not from the heap alone
  (ADR-0038): what the allocator keeps of the preceding peak (freed slabs
  awaiting reuse and pages awaiting decay; the process itself holds ~15 MiB
  off the heap), then per live call and per live transaction the allocated
  bytes times the slab slack. At a held peak the slack follows the arena
  count (`active/allocated` 1.01 with one arena, 1.10 with four, 1.27 with
  one per thread, 63 k transactions held for 5 min); a ramp reads more while
  the heap grows. A setting that makes RSS follow the host instead (a
  heap-profile sample every 8 KiB costs a byte per live byte) is a hazard
  the startup line and `jemalloc_footprint_hazards` name; the ceilings assume
  none.
- A transaction ceiling also counts the transactions of rejected and finished
  INVITEs that linger for their timers, of answered client INVITEs (held
  Accepted 64·T1 past their 2xx, ADR-0034), and of client INVITEs that gave up
  (held 64·T1 past their `Timeout`, ADR-0058), so it is sized from the
  offered rate, not from live calls alone.
