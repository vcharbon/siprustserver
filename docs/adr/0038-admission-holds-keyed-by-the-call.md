# Admission holds are keyed by the call

**Status:** accepted (2026-09-27)

## Context

The call limiter counts concurrent calls per limiter id across every worker.
A call may hold several ids at once, and a reroute (a failover route, a
release reroute) replaces the set it holds. A hold keyed by `(id, window)`
knows no call: a reroute admitted the new set while the old one still counted,
so an id the call kept was refused at its cap; a release matched any hold on
the key, so a second release (a stale replica after a crash, a reboot reclaim
and the backup's reap of the same deferred terminal) took a neighbour's count;
and a lost release could not be retried.

## Decision

1. **The server keeps, per call, the multiset of ids the call holds and a
   lease.** The call is named by a **limiter key** minted once at the call's
   creation and unique over time: the `call_ref` plus a nonce. The `call_ref`
   alone is reused by a retried INVITE (same Call-ID and From tag, RFC 3261
   §8.1.3.5), which would meet the first call's fence. The key is
   replicated with the call, so a takeover, a reclaim and the lossy reap
   release the same call.
2. **`admit(key, entries, release_on_refusal)` replaces the call's whole
   set**, checked net of the set it already holds: an id the call keeps or
   reduces never refuses, an id it adds is checked against the cap the entry
   states; the same id twice takes two slots; all or none. On a cap refusal
   the old set stays, unless `release_on_refusal` drops it in the same step.
   The initial route admits with it off; every route fold with it on (a
   refused replacement frees the ended leg before the failure is consulted).
3. **`release(keys)` is idempotent per key.** One request names one or
   more calls; for each it drops the set and fences the key for one lease:
   an admit or a refresh landing after the call ended re-creates
   nothing and is refused with its own reason, so every order of admit,
   refresh and release on one call is safe, and a second release from another
   node frees nothing. An admit that drops the set without replacing it (a cap
   refusal with `release_on_refusal`, an empty replacement) fences the key
   against refresh the same way, until the next admit of the key: a refresh
   that left before the drop and lands after it re-creates nothing. A
   refused refresh names its fence: `released` (the call was released) or
   `dropped` (an admit of the key dropped the set).
4. **`refresh` extends the lease** of every call it names, each `(key, ids)`
   answered on its own terms, in order. For a call the server knows it
   extends the lease. For a call the server does not know and has not fenced
   it **re-creates the set from the ids the refresh carries, with no cap
   check**: the call exists and was admitted; its set lapsed while nobody
   refreshed it, or the server restarted empty. A fenced call stays refused.
5. **A set whose lease lapses is dropped and counted.** The lease is the
   backstop for every release the server never received.
6. **A call that sent an admit request releases its key at its end**,
   whatever the answer: admitted, refused at a cap, refused by a release
   fence, a timeout, a transport error, a bad or non-200 answer. The request
   may have landed; by decision 3 the release frees exactly what the server
   holds for the call and nothing otherwise, and fences the key, so an admit
   landing after it re-creates nothing. An admit that sent no request (no
   limiter configured, a local guard refusing to send, an open breaker,
   decision 10) owes nothing by itself. Refresh stays reserved to a set the
   server confirmed: a failed **initial** admit leaves the call uncounted (no
   refresh), so a set its late admit created lives until the call's release
   or its lease, whichever comes first. A counted call whose **reroute**
   admit fails technically stays counted: its refresh extends whichever set
   the server holds for the key (the old one, or the new one when the admit
   landed) or re-registers the ids the server last confirmed, and its release
   frees it; until a refresh or a later fold states otherwise, `ids` (the
   decision snapshot, the CDR) name the set the server last confirmed, which
   a landed admit may have replaced. When the lost admit landed and dropped
   the set (an empty replacement, a cap refusal), the refresh answers
   `dropped` and the call goes uncounted, still owing its release: a set its
   own admit dropped is never re-registered. A refresh refused by a release
   fence leaves the call counted (see Consequences).
7. **Only a counted Active call refreshes**, never waiting on the limiter
   in its turn: its `LimiterRefresh` turn marks the key due on the worker's
   refresh batch (decision 11). It always has its refresh armed: the
   invariant layer re-arms a missing or past-due `LimiterRefresh` on every
   turn of the call, so a refresh that never fired heals on the
   call's next turn. For an idle established call that turn is its
   keepalive, so the call may run uncounted for up to the keepalive interval
   plus one refresh period. A Terminating call stops refreshing: a teardown
   may outlast the lease (a release consult, then the sliding 32 s backstop),
   in which case the set lapses early and the terminal release is a no-op.
8. **The call state is `{key, counted, release_owed, ids, generation}`**, one
   replicated field; `generation` moves on each time a route fold (or a
   refresh answer) restates the set. `counted` is refresh eligibility and
   implies `release_owed`, which the first admit request sets and nothing
   clears; every end path reads it
   to send the release: the terminal settle, the primary's discharge of a
   takeover copy's terminal, a reclaim, the lossy reap of a deferred
   terminal, a fold landing on a gone call. A route fold's
   dispatching task replaces the set before the fold reaches the call and the
   fold states the outcome (`SetLimiterState`): the admitted route, the call
   as it was after a lost answer, or uncounted when a refusal dropped the set
   and the consult resolves otherwise (a reject, a redirect, a relay, the
   local teardown). A fold landing on a gone call releases the key it carries
   when owed; a fold naming another key (an earlier call under the same
   `call_ref`) releases that key when owed and leaves the resident call
   alone.
9. **The release never holds the call.** Every end path hands the key to
   the worker's release queue and goes on, so the call's CDR is written and
   the call removed in its last turn whatever the limiter does. The queue
   sends at once, every waiting key in one request, under a release budget
   longer than the admit's (2 s). A failed send keeps its keys, and the next
   send waits a backoff that doubles with each consecutive failure: one
   limiter, one backoff, so the waiting keys stay one batch. An entry is the
   key and its lease expiry, nothing of the call: about 200 bytes with an
   80-byte key, so one lease of calls ending at 500/s holds about 12 MB. An
   entry that has waited one lease is given up, since the limiter has let
   the call's set lapse; at the queue's cap the oldest entry is given up;
   both are counted. A given-up entry may still land if a send carrying it
   was in flight. The queue is not replicated: a worker that dies loses it
   and the lease frees what it held. A planned exit flushes it before the
   process leaves: every waiting key leaves at once whatever the backoff,
   and the exit waits for the queue to empty within its own bound, and is
   taken only with the queue empty. A queue held by an open breaker, at the
   flush or during it, is not waited for: the lease frees its entries, and
   waiting on the probe's cadence would stretch every exit toward the full
   bound. What is still queued at the bound or held is given up, counted
   (`reason=shutdown`) and logged; the drain's residual stays a count of
   live calls.
   Its sender is supervised: one that
   panics is restarted with the queue intact, and counted. A circuit breaker
   drives the queue through `hold` and `resume`: nothing is sent while it is
   held, and a resume sends every waiting key at once. The worker's lease is
   the limiter's, at most one day, and its refresh period is below it.
10. **No call pays the timeout of a limiter that keeps failing.** Each worker
    runs a circuit breaker in front of its limiter. Closed, it counts
    consecutive admits that got no usable answer: a timeout, a transport error
    (a name that does not resolve within the admit's budget included), any
    non-200 status (a 4xx included), a bad body. Any answered admit, a refusal
    included, ends the run, and only admits count. A run of 3 opens it. Open,
    an admit sends nothing and answers at once: the call owes no release by
    that admit and stays as it was (an initial admit leaves it uncounted for
    its life, a reroute admit leaves a counted call counted on its old set).
    The release queue and the refresh batch are held: nothing is sent, and
    every refresh due stays due (decision 11). A background probe asks the
    limiter's health answer, which reads its store, every second under the
    admit budget; answers of admits sent before the breaker opened change
    nothing. The first answer closes the breaker and resumes the queue and the
    batch, which send at once: a counted call whose set lapsed during the
    outage is re-registered at the close (decision 4), not one refresh period
    later, and each answer reaches its call. No call is a probe. A limiter client without a
    health answer runs without a breaker, and a guarded limiter is never
    guarded twice. The limiter's `host:port` is a socket address used as it
    is, or a name resolved on the request path: one lookup at a time, every
    request meanwhile waiting for it within its own budget, and a lookup
    outlives the requests that gave up on it. The runner looks the name up
    once at boot, waiting at most one probe period. A limiter whose address
    is not known starts its breaker open: admits fail open at once, counted,
    no call pays a lookup or a timeout, and the probe looks the name up every
    period. From process start, calls can so stay uncounted for up to the
    boot wait plus one probe period after the name resolves. The address
    found is kept until the breaker opens or a probe fails; both forget it,
    so the probe reaches a limiter whose name now leads elsewhere. A forget
    leaves the lookup in flight to land and be kept, so a lookup slower than
    the probe's budget still closes the breaker at the next probe. No
    `LIMITER_URL` runs without a limiter; a configured one is never replaced
    by none.
11. **A refresh is one request per worker per tick, and no call waits on
    it.** A counted call's `LimiterRefresh` turn marks its key, ids and
    generation due on the worker's refresh batch and goes on. One tick
    (1 s) after a key falls due, the batch sends every key due, at most
    `LIMITER_REFRESH_BATCH_MAX` keys per request, one request at a time,
    under the admit budget: the request rate is one per tick whatever the
    number of counted calls, and the calls one node reclaims at once, whose
    refreshes are all past due, leave in `ceil(calls / max)` requests. A key
    marked again while due is sent once with its latest ids. A request with
    no usable answer keeps its keys for the next tick. An entry is given up
    one lease after its first mark, at the release queue's cap (the oldest),
    or when its call's release is queued, so an ended call's set is never
    refreshed after its release; all three are counted. The batch is not
    replicated. Each answer but `Extended` goes back to its call as an
    internal event and is applied on the call's own turn, to the resident
    call only (none is materialised for it), and only under the generation
    it was marked with: an answer for an ended call, another key, an
    uncounted call or an older generation (a fold restated the set since)
    changes nothing. `Dropped` makes the call uncounted, still owing its
    release; `Released` and `Reregistered` leave it as it is. The breaker
    drives the batch like the release queue (decision 10).

## Lease, refresh and the replica TTL

The lease (120 s) is refreshed every 40 s: two refreshes may be missed. The
replica TTL (`reboot_budget_sec`, 600 s) is longer than the lease, and a
takeover is reactive (a call moves to its backup only when an in-dialog
request reaches it). So a call whose primary crashed and that nobody has taken
over stops refreshing and loses its set one lease after its last refresh, and
the reap of a deferred terminal at the replica TTL releases a set the lease
already dropped. Decision 4 is what closes this: the node that materialises
the call re-arms its timers; the restored past-due refresh, or the refresh the
invariant arms on the materialising turn, re-registers the set within one
refresh period of the takeover or the reclaim. A takeover copy that reaches
its end defers the release to the primary; when no primary reclaims it inside
the replica TTL, the backup's reap releases the key. The reap is the one
eviction site of an expired replica body and no read evicts one first, so
every expired deferred terminal is released once, whichever partition holds
it. Under the deployed relation the lease has freed the re-registered set by
then and that release is a no-op. The HA cells
that prove a release (the reap's, the reclaim's) run a lease longer than the
replica TTL on purpose, so the release they name is what frees the call; the
cells that prove re-registration run the deployed relation.

## Consequences

- Counters: `limiter_lease_expired_{calls,holds}_total`,
  `limiter_reregistered_calls_total`, `limiter_admit_released_total`, the
  gauges `limiter_calls`, `limiter_fences`, `limiter_current_total`,
  `limiter_admission_max` (the largest live count of one id: what an admit
  compares with its cap). The b2bua counts admits refused on a fence per site
  (`b2bua_limiter_admit_released_{initial,fold}_total`) and refreshes that
  re-registered, were refused by a release fence
  (`b2bua_limiter_refresh_released_total`) or learnt their set was dropped
  (`b2bua_limiter_refresh_dropped_total`).
- Config: `LIMITER_LEASE_SECONDS` on the limiter and on the workers (the
  same value), `LIMITER_REFRESH_SECONDS` on the workers, the refresh below
  the lease by more than one period; `LIMITER_REFRESH_BATCH_MS` (1000, below
  the refresh period) and `LIMITER_REFRESH_BATCH_MAX` (1000),
  `LIMITER_RELEASE_TIMEOUT_MS`, `LIMITER_RELEASE_QUEUE_CAP`,
  `LIMITER_BREAKER_FAILURES` (3), `LIMITER_BREAKER_PROBE_MS` (1000) and the
  exit's release flush bound `B2BUA_DRAIN_RELEASE_FLUSH_MS` (3000) on the
  workers.
- The breaker trades counts for latency: the calls a worker starts while its
  breaker is open stay uncounted for their life, so after the limiter comes
  back its counts read low by those calls until they end, and a cap can be
  passed by them. The worker counts the admits it did not send
  (`b2bua_limiter_breaker_admits_not_sent_total`: each owes no release by
  that admit; an initial admit leaves its call uncounted, a reroute admit
  leaves the call as it was). A gauge of the live calls left uncounted is
  future work. The breaker's state is `b2bua_limiter_breaker_open`, its
  transitions `b2bua_limiter_breaker_transitions_total{to=open|closed}`, its
  probes
  `b2bua_limiter_breaker_probe_failures_total` and
  `b2bua_limiter_breaker_probe_restarts_total`; a failed probe also counts in
  the limiter's fail-open episode, which so lasts as long as the outage. The
  limiter's health answer is `GET /v1/health`; `/healthz` answers the process
  alone. A malformed `LIMITER_URL` (another scheme, no port, a path, an
  unbracketed IPv6 address) refuses boot; a well-formed name that does not
  resolve at boot starts the breaker open.
- A refresh request carries every key due with its ids: one request per
  worker per tick. The limiter counts requests (`limiter_refresh_total`) and
  the calls they name (`limiter_refresh_calls_total`); the worker counts
  `b2bua_limiter_refresh_requests_total{result=answered|unavailable}`, the
  keys sent (`b2bua_limiter_refresh_keys_sent_total`, over the requests the
  mean batch size), the keys put back (`b2bua_limiter_refresh_retries_total`),
  the keys due (`b2bua_limiter_refresh_due`, held while the breaker is
  open), the entries given up
  (`b2bua_limiter_refresh_forgotten_total{reason=lease_expired|cap|released}`),
  the answers applied to their call
  (`b2bua_limiter_refresh_answers_applied_total{outcome}`) or discarded
  (`b2bua_limiter_refresh_answers_discarded_total{reason=call_gone|stale}`)
  and sender restarts (`b2bua_limiter_refresh_sender_restarts_total`). A
  refresh is sent within one tick of falling due, so the lease must outlast
  the refresh period plus a tick.
- Re-registration knows no cap: a stale counted copy materialised after its
  release's fence lapsed (a primary that released, crashed before the
  flush and reboots later than one lease) re-registers a set until the
  keepalive reaps that zombie; and after a limiter restart the sets
  re-registered beside admits made in the gap can read above the cap on
  `limiter_admission_max` for the life of the calls that outlived the restart.
- `b2bua_limiter_refresh_released_total` is not always a fault. A
  partitioned backup's reap of a primary it believes dead fences the key of a
  call the primary still serves; that call's refreshes are refused for one
  lease, then re-register. A refresh sent before a fold's refusal dropped the
  call's set lands on the drop fence and answers `dropped`; the answer and
  the fold reach the call in either order, and either way the call ends
  uncounted and only its terminal release is sent.
- No call waits on its release: a stalled or unreachable limiter delays
  no CDR and no removal, those of calls whose admit failed open included.
  What waits is the release queue, at most one lease of ending calls per
  worker and never more than its cap, read on
  `b2bua_limiter_release_queue_depth`, `b2bua_limiter_release_retries_total`,
  `b2bua_limiter_release_dropped_total{reason=lease_expired|cap}` and
  `b2bua_limiter_release_drainer_restarts_total`. A release lost with its
  worker or given up by the queue is freed by the lease, so the limiter's
  counts read high by those calls for up to one lease.
- `LimiterRefresh` entries are not cohort-smoothed on a bulk reclaim: the
  calls one node reclaims fall due together and the batch absorbs them.
- Replica bodies decode strictly: a body without the limiter key is dropped at
  reclaim and at the reap (no upgrade compatibility, by policy).
- Out of scope here: per-leg keys for a transfer, which extend the key
  without changing this contract.
