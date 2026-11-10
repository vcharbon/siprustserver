# Admission holds are keyed by the call

**Status:** accepted. Superseded in part by [ADR-0043](0043-outbound-backend-client-policy.md):
the breaker's rule, probe and env keys are the limiter client's backend breaker. Amended by
ADR-0047 D8 (a takeover copy's release).

**Owner:** `crates/b2bua/src/limiter/` owns the admission set; its call side is
`call::model::limiter`.

Each decision in full, the metrics and the config keys: [appendix](appendix/0040-admission-holds-keyed-by-the-call.md).

## Context

The call limiter counts concurrent calls per limiter id across every worker. A call may hold
several ids, and a reroute replaces the set it holds. A hold keyed by `(id, window)` knows no
call: a reroute's new set counts beside the old one, a second release takes a neighbour's count,
and a lost release cannot be retried.

## Decision

1. **The server keeps, per call, the entries the call holds (a multiset of ids, each with its
   cap), the change number of the last admit it answered, and a lease.** The call is named by
   a **limiter key**, the `call_ref` plus a nonce, minted at creation and replicated with the
   call. [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-1).
2. **`admit(key, change, entries, release_on_refusal)` replaces the call's whole set**, checked
   net of the set it holds, all or none. An admit numbered at or below the known number is
   `superseded`. Every answer but a release fence's states the server's set. An admit carries
   the call's held set, re-created with no cap check for an unfenced key the server lost.
   [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-2).
3. **`release(keys)` is idempotent per key.** It drops the set and fences the key for one lease.
   An admit that drops the set fences it against refresh until the next admit. A refused
   refresh names its fence, `released` or `dropped`.
   [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-3).
4. **`refresh` extends the lease** of every call it names. For an unknown, unfenced call it
   re-creates the set the refresh carries, with no cap check. A fenced call stays refused.
   [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-4).
5. **A set whose lease lapses is dropped and counted.** The lease is the backstop for every
   release the server never received.
6. **A call that sent an admit request releases its key at its end**, whatever the answer. An
   admit that sent no request owes nothing by itself. Refresh is reserved to a set the server
   confirmed. [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-6).
7. **Only a counted Active call refreshes**, never waiting on the limiter in its turn. Every
   turn re-arms a missing or past-due `LimiterRefresh`. A Terminating call stops refreshing.
   [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-7).
8. **The call state is `{key, release_owed, held, held_change, target, target_change, change,
   runs_on}`**, one replicated field. Every change is computed from `held`. The call is
   *counted* iff `held` is not empty, runs *fail open* iff `target` names an id `held` lacks,
   and runs *uncounted* iff it runs fail open or `runs_on` names an id `held` lacks. A call
   materialised on another node moves its change counter to its next epoch (2^32).
   [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-8).
9. **The release never holds the call.** Every end path hands the key to the worker's release
   queue, so the CDR and the removal never wait. The queue batches, backs off, gives up an entry
   after one lease or at its cap, and is not replicated. A planned exit flushes it within
   `B2BUA_DRAIN_RELEASE_FLUSH_MS`. [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-9).
10. **No call pays the timeout of a limiter that keeps failing.** Each worker runs the limiter
    client's backend breaker (ADR-0043 §8). Open, an admit sends nothing and answers at once,
    and the release queue and refresh batch are held. Its close resumes both. A limiter whose
    address is not known starts open. [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-10).
11. **A refresh is one request per worker per tick, and no call waits on it.** The batch sends
    every key due, at most `LIMITER_REFRESH_BATCH_MAX` per request. Each answer that states
    another set returns to its call as an internal event.
    [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-11).
12. **The worker learns the lease from the limiter.** Every answer states `lease_ms` (at least
    1 s); one without it is a bad answer, so the limiter ships before its workers. The refresh
    period is at most a third of the lease, the batch window at most a sixth.
    [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-12).
13. **A service replaces the call's admission set and is told the outcome.**
    `ReplaceAdmissionSet { correlation_id, entries, moves_call }` sends one admit off the call's
    turn; the call re-enters with a `limiter-admit-result` event. With `moves_call` the call
    runs on `entries` whatever the outcome.
    [Detail](appendix/0040-admission-holds-keyed-by-the-call.md#decision-13).

## Lease, refresh and the replica TTL

The lease (120 s) is refreshed every 40 s within a 1 s tick, so one refresh may be missed. The
replica TTL (600 s) is longer than the lease and takeover is reactive. A call whose primary
crashed and that nobody took over loses its set one lease after its last refresh. The node that
materialises the call re-registers it within one refresh period (decision 4).
[Detail](appendix/0040-admission-holds-keyed-by-the-call.md#lease-refresh-and-the-replica-ttl).

## Consequences

- The breaker trades counts for latency: calls started while it is open stay uncounted for
  their life, and a cap can be passed by them.
- Re-registration knows no cap: a stale counted copy, or the sets re-registered after a limiter
  restart, can read above the cap on `limiter_admission_max`.
- A refresh answered `released` is not always a fault (a partitioned backup's reap).
- No call waits on its release. A release lost with its worker is freed by the lease, so the
  counts read high for up to one lease.
- A call holds one set under one key whatever legs it creates.
- Replica bodies decode strictly: a body without the limiter key is dropped.

Metrics, config keys and the long form of each consequence:
[appendix](appendix/0040-admission-holds-keyed-by-the-call.md#consequences).
