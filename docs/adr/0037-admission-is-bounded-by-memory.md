# Admission is bounded by memory; emergency calls keep a higher ceiling

**Status:** accepted (2026-09-25)

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

1. **Three quantities bound new calls:** live calls (takeover copies
   included), live SIP transactions, and process RSS. The counts cover the
   costs known ahead; RSS covers everything the counts do not.
2. **Each bound has two ceilings.** At the `normal` ceiling a new
   non-emergency call is refused. At the `emergency` ceiling every new call is
   refused. Emergency calls keep priority between the two, and the higher
   ceiling still protects the process. Every ceiling is an explicit setting;
   none is derived from the host or the container, and all are off by default.
3. **The reject is a 503 with a jittered `Retry-After` and no `Reason`,**
   whichever rung of the admission ladder (item 11) refuses a new INVITE,
   from one renderer: the To-tag and the jitter roll are derived from the
   request and the `Retry-After` is floored once. The rungs ahead of any
   transaction share one memo of refused INVITEs, so every copy of one
   INVITE draws the same bytes (RFC 3261 §8.2.7), and the brake spares a copy
   of an INVITE a live server transaction holds; the rungs behind it send the
   same 503 through the INVITE server transaction. In-dialog requests are
   never refused, but for item 6. A refusal is counted once, on
   `b2bua_new_calls_total`; a copy refused again is counted apart.
4. **Backup replicas have their own ceilings:** a count and an RSS ceiling,
   set lower than the admission ones. At either, a replica of a call this node
   does not hold yet is not stored. Updates and deletes of held replicas, and a
   node's own calls coming back from a peer, always apply. A replica left out
   is stored by the call's next write that finds room, since every write
   carries the whole body. Until then the backup's flow reports no position
   past the write it left out, so a draining primary never reads that call as
   held (ADR-0031 D2).
5. **RSS is sampled, counts are exact.** RSS is read every 100 ms through an
   injectable `SystemProbe`; tests drive it through a simulated one.
6. **The transaction layer's deferred backlog is an internal bound, always
   on.** It holds the critical events a full event queue keeps for a router
   that stops draining, among them every INVITE whose 100 Trying already
   left. No other rung sees that stall: the ingress brake reads a queue the
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
7. **An admitted call's requests are never refused, but by item 6.** This is
   the operator's sizing rule; the process does not compute it. The new-call
   ceilings sit far enough below the process's memory limit that all an
   admitted call can still cost fits above them: its in-dialog transactions,
   the backup replicas that turn live when a peer dies, every call's
   teardown, and the transactions refused INVITEs and given-up client
   INVITEs hold. With `C` the emergency call ceiling, `B` the backup count
   ceiling, `N` the emergency transaction ceiling, `r` a generous in-dialog
   request rate per call beyond setup and teardown (two transactions per
   request, the one it opens here and the one it is relayed on, each held
   64·T1), two transactions for a teardown, `q` the rate of INVITEs answered
   through a transaction and left held (a refusal until its ACK and Timer I,
   64·T1 without one; a client INVITE given up, 64·T1), `h` that hold, the
   per-call and per-transaction costs and the base measured on the
   deployment under a held peak (its allocator footprint report), and a
   slack `s` for RSS lag and allocator retention:

   ```text
   reserve  = (C + B) · (2 · r · 64·T1 + 2) + q · h          transactions
   limit · (1 − s) ≥ base + (C + B) · call + (N + reserve) · txn
   RSS ceiling     ≤ limit · (1 − s) − B · call − reserve · txn
   ```

   Setup and teardown churn at the admission rate is part of `N`, which
   counts every transaction. `q` is bounded only by the stateless brakes
   ahead of the router: a refused flood past what the margin holds must be
   shed there. Under overload the only refusal is a new call's 503.
8. **Teardown is never refused.** No table, queue or dispatch limit refuses
   the node's own BYE, CANCEL or ACK, or their answers: the transaction table
   has no cap, a final matched to a client transaction and a `Timeout` are
   critical events the layer never drops, the router admits transaction
   outcomes past every per-call bound, and the brakes shed initial INVITEs
   only. The kernel's socket buffers can lose them; the retransmission
   timers recover.
9. **The answer to a request a call sent off its turn is never refused.** A
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
10. **New calls have their own dispatcher thresholds.** A normal initial
    INVITE's turn holds a permit of a new-call share of the handler permits
    (`new_call_permit_share_percent`, default 50 % of
    `event_dispatch_concurrency`) besides its shared one, and its INVITE opens
    a call's queue only below `per_call_queue_cap` less a headroom
    (`new_call_queue_headroom_percent`, default 5 %), refused there by the
    shed rung (item 11). An emergency INVITE and every other event draw the
    shared pool alone and open a queue up to the full cap, so new calls on a
    stalled decision backend leave the established calls their permits, and a
    taken-over call's first request finds a queue.
11. **The admission ladder is one table** (`b2bua::admission`, `LADDER`),
    in the order a new INVITE meets the rungs:

    | rung | site | input | normal | emergency | in dialog | Retry-After base |
    |---|---|---|---|---|---|---|
    | brake | arrival, before the datagram is queued | inbound queue depth | refused at the threshold | admitted | admitted | configured |
    | backlog | transaction layer, before any transaction | deferred events | refused at the normal ceiling | refused at the emergency ceiling | refused at the emergency ceiling | configured |
    | capacity | router ingress | live calls (admitted INVITEs included), transactions, sampled RSS | refused at a normal ceiling | refused at an emergency ceiling | — | configured |
    | shed | router ingress | live per-call queues | refused at the cap less the headroom | refused at the cap | — | configured |
    | panic-ELU | router ingress | the worker's EWMA-ELU | refused above the backstop | admitted | — | configured |
    | bucket | router ingress | time to a CPS token | refused with no token | admitted | — | the time to a token, at least the configured base |

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

## Consequences

- A call whose replica was left out has no backup until its next write lands;
  a primary failure in that window loses it. The shed is counted
  (`b2bua_repl_backup_shed_total`), never silent, and a drain in that window
  runs to its grace instead of exiting caught up.
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
  the heap grows. A setting that makes RSS follow the host instead — a
  heap-profile sample every 8 KiB costs a byte per live byte — is a hazard
  the startup line and `jemalloc_footprint_hazards` name; the ceilings assume
  none.
- A transaction ceiling also counts the transactions of rejected and finished
  INVITEs that linger for their timers, and of client INVITEs that gave up
  (held 64·T1 past their `Timeout`, ADR-0028), so it is sized from the
  offered rate, not from live calls alone.
