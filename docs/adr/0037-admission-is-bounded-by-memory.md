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
3. **The reject is a 503 with a jittered `Retry-After` and no `Reason`.** The
   initial-INVITE admission gate sends it from exact counts, ahead of the CPS
   bucket so it spends no token, through the INVITE server transaction. The
   stateless ingress brake does not: it cannot tell a new INVITE from a
   retransmission of one already admitted, and refusing that retransmission
   would end a call being set up. In-dialog requests are never refused, but
   for item 6.
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
   left. No other gate sees that stall: the ingress brake reads a queue the
   transaction layer drains at parse speed, and the gates of item 3 run in
   the router, behind it. No operator setting sizes the event queue, so the
   backlog's ceilings follow it: one queue of events for a new call, two for
   an emergency call and for an INVITE carrying a To-tag, whose dialog the
   layer cannot check. Item 2's rule on settings covers the host-dependent
   bounds of item 1. Past a ceiling the layer sends item 3's 503 statelessly,
   before any 100 Trying and after matching retransmissions of admitted
   INVITEs; a 503 to a re-INVITE leaves its dialog in place. Each refused
   INVITE is remembered for 64·T1 (at most 65 536 at once), so its copies
   draw the same 503 and its ACK ends in the layer; a copy arriving after
   that is judged afresh, as at the ingress brake. The ceiling counts every
   critical event, so a takeover burst of `CallQuiesced` events can refuse
   new calls until the router catches up.

## Consequences

- A call whose replica was left out has no backup until its next write lands;
  a primary failure in that window loses it. The shed is counted
  (`b2bua_repl_backup_shed_total`), never silent, and a drain in that window
  runs to its grace instead of exiting caught up.
- The counts are exact per decision, but INVITEs judged at once on a
  multi-thread runtime can pass together before either call exists: a call
  ceiling is overshot by at most one call per runtime worker thread, a backup
  count ceiling by at most one replica per other peer.
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
  INVITEs that linger for their timers, so it is sized from the offered rate,
  not from live calls alone.
