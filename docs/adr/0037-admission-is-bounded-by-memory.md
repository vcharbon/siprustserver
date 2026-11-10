# Admission is bounded by memory; emergency calls keep a higher ceiling

**Status:** accepted

Each item in full, the ladder's rationale and the sizing notes are in the
[appendix](appendix/0037-admission-is-bounded-by-memory.md).

## Context

Admission sheds on rate (the CPS bucket), on the worker's own loop (the panic-ELU backstop)
and on ingress depth (the queue brake). None of them bounds what the worker holds. Long calls,
emergency calls and a takeover's burst all grow it, and a worker with no bound of its own grows
until the kernel kills it, dropping every call it serves.

## Decision

1. **Three quantities bound new calls:** live calls (takeover copies included), live SIP
   transactions, and process RSS. The counts cover the costs known ahead; RSS covers everything
   the counts do not.
2. **Each bound has two ceilings.** At the `normal` ceiling a new non-emergency call is refused,
   at the `emergency` ceiling every new call. The emergency ceiling sits strictly above the
   normal one (refused at boot otherwise). A call a takeover turns live meets no ceiling. Every
   ceiling is an explicit setting, off by default.
3. **The reject is a 503 with a jittered `Retry-After` and no `Reason`,** from one renderer,
   whichever rung refuses. Every copy of one INVITE draws the same bytes (RFC 3261 §8.2.7).
   In-dialog requests are never refused, but for item 6. A refusal is counted once on
   `b2bua_new_calls_total`. [Detail](appendix/0037-admission-is-bounded-by-memory.md#3-the-reject).
4. **Backup replicas have their own ceilings:** a count and an RSS ceiling, at least the
   emergency admission ones. At either, a replica of a call this node does not hold yet is
   refused, told to the primary, and the call has no backup for its whole life (ADR-0055 §3).
   So every call a peer admits can be backed.
   Updates and deletes of held replicas always apply.
   [Detail](appendix/0037-admission-is-bounded-by-memory.md#4-backup-replicas).
5. **RSS is sampled, counts are exact.** RSS is read every 100 ms through an injectable
   `SystemProbe`; tests drive it through a simulated one.
6. **The transaction layer's deferred backlog is an internal bound, always on.** Its ceilings
   follow the event queue: one queue of events for a new call, two for an emergency call and an
   INVITE carrying a To-tag. Past a ceiling the layer sends item 3's 503 statelessly and
   remembers the INVITE for 64·T1.
   [Detail](appendix/0037-admission-is-bounded-by-memory.md#6-the-deferred-backlog).
7. **An admitted call's requests are never refused, but by item 6.** This is the operator's
   sizing rule; the process does not compute it. With `C` the emergency call ceiling, `B` the
   backup count ceiling, `ρ` a held replica's cost, `N` the emergency transaction ceiling, `r`
   an in-dialog request rate per call, `q` the rate of held answered INVITEs, `h` their hold and
   `s` a slack:

   ```text
   reserve  = (C + B) · (2 · r · 64·T1 + 2) + q · h          transactions
   limit · (1 − s) ≥ base + (C + B) · call + (N + reserve) · txn
   RSS ceiling     ≤ limit · (1 − s) − B · (call − ρ) − reserve · txn
   ```

   [Detail](appendix/0037-admission-is-bounded-by-memory.md#7-the-sizing-rule).
8. **Teardown is never refused.** No table, queue or dispatch limit refuses the node's own BYE,
   CANCEL or ACK, or their answers. The kernel's socket buffers can lose them; the
   retransmission timers recover.
   [Detail](appendix/0037-admission-is-bounded-by-memory.md#8-teardown).
9. **The answer to a request a call sent off its turn is never refused.** The router admits it
   past every per-call bound, like a timer fire. Item 7 has no term for these answers.
   [Detail](appendix/0037-admission-is-bounded-by-memory.md#9-off-turn-answers).
10. **New calls have their own dispatcher thresholds.** A normal initial INVITE's turn also
    holds a new-call permit (`new_call_permit_share_percent`, 50 %), and opens a queue only
    below `per_call_queue_cap` less `new_call_queue_headroom_percent` (5 %).
    [Detail](appendix/0037-admission-is-bounded-by-memory.md#10-new-call-thresholds).
11. **The admission ladder is one table** (`b2bua::admission`, `LADDER`), in the order a new
    INVITE meets the rungs:

    | rung | site | input | normal | emergency | in dialog | Retry-After base |
    |---|---|---|---|---|---|---|
    | brake | arrival, before the datagram is queued | inbound queue depth | refused at the threshold | admitted | admitted | configured |
    | backlog | transaction layer, before any transaction | deferred events | refused at the normal ceiling | refused at the emergency ceiling | refused at the emergency ceiling | configured |
    | event queue, on arrival | transaction layer, before any transaction | the state the event-queue gate (ADR-0050 §9) last wrote, and the age of the queue's oldest event | refused while it refuses, or from the gate's delay plus its interval | admitted | admitted | configured |
    | capacity | router ingress | live calls (admitted INVITEs included), transactions, sampled RSS | refused at a normal ceiling | refused at an emergency ceiling | n/a | configured |
    | shed | router ingress | live per-call queues | refused at the cap less the headroom | refused at the cap | n/a | configured |
    | panic-ELU | router ingress | the worker's EWMA-ELU | refused above the backstop | admitted | n/a | configured |
    | bucket | router ingress | time to a CPS token | refused with no token | admitted | n/a | the time to a token, at least the configured base |
    | event queue, again | router ingress | the event-queue gate, read afresh | refused while it refuses | admitted | n/a | configured |

    A refusal at a router rung opens no per-call queue and spends no CPS token. A copy of a
    live call's INVITE is answered 482.
    [Detail](appendix/0037-admission-is-bounded-by-memory.md#11-the-admission-ladder).

## Consequences

- A call whose replica was refused has no backup for life. The refusal is counted
  (`b2bua_repl_backup_shed_total`) and the unbacked calls gauged (`b2bua_repl_calls_unbacked`).
- The counts are exact per decision. A backup count ceiling is overshot by at most one replica
  per other peer.
- RSS lags up to one sample and stays high after a drain, so the RSS ceilings leave room below
  the process limit.
- `B2BUA_MAX_RSS` is sized from the measured process, not the heap alone (ADR-0038).
- A transaction ceiling is sized from the offered rate, not from live calls alone.

Detail: [appendix, consequences](appendix/0037-admission-is-bounded-by-memory.md#consequences).
