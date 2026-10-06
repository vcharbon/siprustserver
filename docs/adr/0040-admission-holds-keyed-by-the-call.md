# Admission holds are keyed by the call

**Status:** accepted (2026-09-27)

**Owner:** `crates/b2bua/src/limiter/` owns the admission set; its call side is `call::model::limiter`.

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

1. **The server keeps, per call, the entries the call holds (a multiset of
   ids, each with the cap it was admitted under), the change number of the
   last admit it answered for the call, and a lease.** The call is named by a **limiter key** minted once at the call's
   creation and unique over time: the `call_ref` plus a nonce. The `call_ref`
   alone is reused by a retried INVITE (same Call-ID and From tag, RFC 3261
   §8.1.3.5), which would meet the first call's fence. The key is
   replicated with the call, so a takeover, a reclaim and the lossy reap
   release the same call.
2. **`admit(key, change, entries, release_on_refusal)` replaces the call's
   whole set**, checked net of the set it already holds: an id the call keeps
   or reduces never refuses, an id it adds is checked against the cap the
   entry states; the same id twice takes two slots; all or none. On a cap
   refusal the old set stays, unless `release_on_refusal` drops it in the
   same step. The initial route and a service's replacement admit with it
   off; every route fold with it on (a refused replacement frees the ended
   leg before the failure is consulted). Every change to a call's set is one
   admit: no change is split into a reserve and a release that could half
   succeed.
   **Each admit carries the call's change number**, above every number the
   call sent before for the key. The server refuses an admit whose number is
   not above the one it knows for the key (`superseded`), changing nothing,
   so a late or repeated admit never overwrites a newer set. Every answered
   admit records its number, a cap refusal included: on the key's set, on
   its drop fence, or, for a key holding neither, on a change marker kept
   one lease that orders admits and fences nothing (a refresh still
   re-registers, under the marker's number). **Every admit answer but a
   release fence's states the set the server holds for the key** after it:
   its entries and its number.
   **Each admit also carries the call's held set** under its number
   (decision 8). For a key the server holds no set for and has not fenced
   (it restarted empty, or the set lapsed; a change marker is no fence), an
   admit that is not superseded first re-creates that set with no cap check,
   exactly as a refresh carrying it would (decision 4), then checks the change
   net of it: an id the call keeps is never checked against its cap, however
   many calls admitted or re-registered since hold it. The admit's number
   replaces the re-created set's in every outcome: admitted, the new set;
   refused, the re-created set kept under the refusal's number, or dropped
   behind a drop fence with `release_on_refusal`. A key fenced by a release
   or a drop re-creates nothing (the call ended, or its own admit dropped the
   set, so a held set carried past that answer is stale), and neither does a
   superseded admit: the call's refresh re-registers it. The held set is the
   call's as of the turn that sends the admit; when an answer the call lost
   stated a newer set the server then lost too, the server re-creates the
   older one, as a refresh would, and the change is checked net of it. A call
   that learnt no set carries none: every id it names is checked.
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
4. **`refresh` extends the lease** of every call it names, each `(key, held)`
   answered on its own terms, in order, and every answer but a release
   fence's states the set the server holds for the key. For a call the server
   knows it extends the lease. For a call the server does not know and has
   not fenced it **re-creates the set from the entries and number the refresh
   carries (the call's held set), with no cap check**: the call exists and
   was admitted; its set lapsed while nobody refreshed it, or the server
   restarted empty. A fenced call stays refused.
5. **A set whose lease lapses is dropped and counted.** The lease is the
   backstop for every release the server never received.
6. **A call that sent an admit request releases its key at its end**,
   whatever the answer: admitted, refused at a cap, refused by a release
   fence, a timeout, a transport error, a bad or non-200 answer. The request
   may have landed; by decision 3 the release frees exactly what the server
   holds for the call and nothing otherwise, and fences the key, so an admit
   landing after it re-creates nothing. The initial route's admit that sent
   no request (no limiter configured, a local guard refusing to send, an
   open breaker, decision 10) owes nothing by itself; an admit sent off the
   call's turn (a failure or release consult's, a service's) is owed from the
   turn that dispatches it, whatever becomes of it (decision 8), and a
   release of a key the server holds nothing for is a no-op. Refresh stays reserved to a set the
   server confirmed: a failed **initial** admit leaves the call uncounted (no
   refresh), so a set its late admit created lives until the call's release
   or its lease, whichever comes first. A counted call whose later admit (a
   reroute, a service's replacement) fails technically stays counted on its
   held set: its refresh extends whichever set the server holds for the key
   (the old one, or the new one when the admit landed) or re-registers the
   held set, and its release frees it. The refresh answer states the set the
   server holds, so a landed admit whose answer was lost becomes the call's
   held set at the next refresh at the latest. When the lost admit landed
   and dropped the set (an empty replacement, a cap refusal), the refresh
   answers `dropped` and the call goes uncounted, still owing its release: a
   set its own admit dropped is never re-registered. A refresh refused by a release
   fence leaves the call counted (see Consequences).
7. **Only a counted Active call refreshes**, never waiting on the limiter in
   its turn: its `LimiterRefresh` turn marks the key due on the worker's
   refresh batch (decision 11). It always has its refresh armed: every turn
   of the call re-arms a missing or past-due `LimiterRefresh`, or one due
   later than one refresh period, before its record lands, so a refresh that
   never fired heals on the call's next turn. For an idle established call
   that turn is its keepalive, so the call may run uncounted for up to the
   keepalive interval plus one refresh period. A Terminating call stops
   refreshing: a teardown may outlast the lease (a release consult, then the
   sliding 32 s backstop), in which case the set lapses early and the
   terminal release is a no-op.
8. **The call state is `{key, release_owed, held, held_change, target,
   target_change, change, runs_on}`**, one replicated field. `held` is the
   set the server last stated it holds for the key, under `held_change`, and every
   admit carries it (decision 2): an admit of a failover chain sent after a
   refusal carries the set the refusal stated; a statement
   (an admit answer, a refresh answer) becomes `held` only when its number is
   not below `held_change`, so an older answer never rolls the call back.
   `target` is the set the call's latest admit asked for, under
   `target_change`. A refused or superseded change is not taken: the target is
   then the held set, the newest statement of all. `change` is the last number
   the call gave an admit or learnt the server knows; the next admit is
   numbered above it. Every change is computed from `held`, never from an
   unconfirmed target, so the next change that succeeds repairs one that
   failed. The call is **counted** (refresh eligibility) iff `held` is not
   empty; it runs **fail open** iff `target` names an id (with its
   multiplicity) `held` lacks: its admit got no usable answer or was not sent,
   a release fence refused it, a refresh answered `dropped`, or a change asked
   on a turn adds an id, for its round trip. `runs_on` is the set the call
   runs on whatever the limiter answers: the set a service moved it onto
   (decision 13), from the turn that sends the move, and the set of the
   route a failover or a release reroute runs, from the fold that runs it
   whatever its admit's outcome (a superseded admit included). It is none
   from the call's creation and after a resolution that follows a refused
   route (a reject, a redirect, a termination, a release): the call runs on
   its target. The call **runs uncounted** iff it runs fail open or
   `runs_on` names an id (with its multiplicity) `held` lacks: a refused,
   superseded, unanswered or in-flight move, and every change after it that
   does not hold the set the call runs on. It is a function of `held`,
   restated on every write of the call, so it follows the newest statement
   in any report order, and the next statement that holds the set (an admit
   answer, a refresh answer) ends it. `release_owed` is set on the turn that
   sends the first admit request (the initial route's, or the dispatch of
   a failure consult, a release consult or a service's replacement, before any
   of them answers) and nothing clears it; every end path reads it to send the
   release: the terminal settle, the primary's discharge of a takeover copy's
   terminal, a reclaim, the lossy reap of a deferred terminal, an admit report
   landing on a gone call. An admit sent off the call's turn (a route fold's,
   a service's) comes back as an internal event carrying its report `{key,
   change, entries, outcome}`, and the router applies it to the call before
   any rule reads the event, whichever rule claims it. A report landing on a
   gone call releases its key when owed; one naming another key (an earlier
   call under the same `call_ref`) releases that key when owed and leaves the
   resident call alone. The turn that sends an admit reserves its number: one
   for a release reroute or a service's replacement, one per admit the
   failover chain may send (`MAX_LIMITER_FAILOVER + 1`) for a failure consult.
   A service's replacement is the call's `target` from that turn, so a change
   computed while it is in flight sees it; a consult's route is known only in
   its report. The reservation replicates with that turn's write, which may
   not reach the backup before the admit leaves: **a call materialised on
   another node** (a takeover copy, a reclaim) **moves its counter to its next
   epoch**, the next multiple of 2^32, so it never reuses a number its
   previous holder reserved and sent without replicating it. The bound: over
   its whole lifetime on the call a holder reserves far fewer numbers than an
   epoch holds. It fails only for a copy materialised from a write older than
   the previous holder's own materialisation, the write that moved it to its
   epoch (a takeover copy's first write still in the backup's buffered writer
   when a reboot reclaim or a second takeover reads the store): that copy
   moves to the previous holder's epoch again, and its admits under numbers
   the previous holder already sent are answered `superseded`, leaving the
   call on what it holds until its next change.
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
   held, and a resume sends every waiting key at once. The lease an entry
   waits is the limiter's as the worker last learnt it (decision 12).
10. **No call pays the timeout of a limiter that keeps failing.** Each worker
    runs a circuit breaker in front of its limiter. Closed, it counts
    consecutive admits that got no usable answer: a timeout, a transport error
    (a name that does not resolve within the admit's budget included), any
    non-200 status (a 4xx included), a bad body. Any answered admit, a refusal
    included, ends the run, and only admits count. A run of 3 opens it. Open,
    an admit sends nothing and answers at once: the call owes no release by
    that admit unless its turn owed it already (decision 6) and stays as it
    was (an initial admit leaves it uncounted for
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
    it.** A counted call's `LimiterRefresh` turn marks its key and held set due on
    the worker's refresh batch and goes on. One tick
    (1 s) after a key falls due, the batch sends every key due, at most
    `LIMITER_REFRESH_BATCH_MAX` keys per request, one request at a time,
    under its own budget (`LIMITER_REFRESH_TIMEOUT_MS`, 2 s, since no call
    waits on it): the request rate is one per tick whatever the
    number of counted calls, and the calls one node reclaims at once, whose
    refreshes are all past due, leave in `ceil(calls / max)` requests. A key
    marked again while due is sent once with its latest held set. A request with
    no usable answer keeps its keys, and the next round waits a backoff that
    doubles from 200 ms with each consecutive unanswered request, never
    below a tick nor above 5 s; an answer or the breaker's close ends it.
    Refreshes never trip the breaker, so a dead limiter behind a closed one
    draws a request every 5 s. An entry is given up one lease (decision 12)
    after its first mark, at the release queue's cap (the oldest), or when
    its call's release is queued, so an ended call's set is never
    refreshed after its release; all three are counted. The batch is not
    replicated. Each answer but an `Extended` one stating the set the
    refresh carried goes back to its call as an internal event and is
    applied on the call's own turn, to the resident call only (none is
    materialised for it): the set it states becomes the call's held set
    under decision 8's rule, and an answer for an ended call, another key or
    a set older than the call's held one changes nothing. `Dropped` makes the
    call uncounted, still owing its release; `Released` leaves it as it is;
    an `Extended` or `Reregistered` stating another set repairs an admit whose
    answer was lost. The breaker
    drives the batch like the release queue (decision 10). A drain does not
    flush the batch: a call live at the exit is refreshed by the timer its
    successor re-arms.
12. **The worker learns the lease from the limiter.** Every admit, refresh
    and health answer states the limiter's lease (`lease_ms`, at least 1 s);
    an answer without it, or with less, is a bad answer: handled as no
    answer and counted per request as its own cause. So the limiter ships
    before its workers: a worker newer than its limiter reads every answer
    as bad. The worker keeps the last lease it learnt, at most one day, and
    before any answer assumes the limiter's default (120 s); it configures
    none of its own. A worker whose breaker boots open learns it from the
    probe that closes it. The release queue and the refresh batch wake on
    each change, so an entry waiting, held or not, is given up at its
    deadline under the lease of the moment (a refresh entry never before two
    ticks, so it gets its round). A counted call refreshes every
    `LIMITER_REFRESH_SECONDS`, or every third of the lease when that is
    shorter: each armed refresh is brought within one period on the call's
    next turn, so a short lease never lets a set lapse between refreshes
    (only a lease below one and a half ticks does). The first lease stated
    and each change after it are checked: one the configured period plus a
    tick reaches is warned about and counted, and one that shortens the
    period is counted.

13. **A service replaces the call's admission set and is told the outcome.**
    A service's `ReplaceAdmissionSet { correlation_id, entries, moves_call }`
    sends one admit of the whole set under the call's
    next change number, off the call's turn and with `release_on_refusal`
    off, and the call re-enters with
    a `limiter-admit-result` internal event: its outcome is the admit's
    (`admitted`, `rejected`, `superseded`, `released`, `unavailable`,
    `not_sent`), its payload echoes `correlation_id` beside the admit's
    report, and the call's limiter state already states the outcome when the
    service's rule reads it (decision 8). A service reads the held set
    (`RuleCall::limiter_held`) to compute its next set, never an admit whose
    answer it has not seen. With `moves_call` the sending turn moves the call
    onto `entries` (its `runs_on`, decision 8): the call runs on them whatever
    the outcome, as a service that already connected the parties the set
    counts; without it the call keeps running on what it ran on, as a
    service that waits for the outcome before acting. A replacement with
    nothing to send (an empty set, for a call that holds nothing and owes no
    release) sends no admit: its move is recorded on its turn and its result
    re-enters as `not_sent`. The call
    owes its release from the turn that sends the admit; a result reaching
    no call releases the key once more.

## Lease, refresh and the replica TTL

The lease (120 s) is refreshed every 40 s: two refreshes may be missed. The
replica TTL (`reboot_budget_sec`, 600 s) is longer than the lease, and a
takeover is reactive (a call moves to its backup only when an in-dialog
request reaches it). So a call whose primary crashed and that nobody has taken
over stops refreshing and loses its set one lease after its last refresh, and
the reap of a deferred terminal at the replica TTL releases a set the lease
already dropped. Decision 4 is what closes this: the node that materialises
the call re-arms its timers; the restored past-due refresh, or the refresh the
materialising turn arms, re-registers the set within one
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

- Metrics, one list. Counters end in `_total` and carry one closed label
  set; only `b2bua_limiter_failures_total` carries two labels (`op`, `cause`).
  The limiter (`limiter_*`): `limiter_admits_total{outcome=admitted|rejected|released|superseded}`,
  `limiter_refresh_requests_total`,
  `limiter_refresh_calls_total{outcome=extended|reregistered|released|dropped}`,
  `limiter_admit_reregistered_calls_total` (the sets an admit re-created from
  the held set it carried, decision 2),
  `limiter_release_requests_total`, `limiter_release_calls_total`,
  `limiter_lease_expired_calls_total` and `limiter_lease_expired_holds_total`
  (the holds no release freed: a lane that loses no release treats any as a
  failure), and the gauges `limiter_calls`, `limiter_holds` (every live count),
  `limiter_fences`, `limiter_change_markers` and `limiter_admission_max`
  (the largest live count of one id: what an admit compares with its cap). Each worker (`b2bua_limiter_*`):
  `b2bua_limiter_requests_total{op=admit|refresh|release|health}`;
  `b2bua_limiter_failures_total{op,cause}`, the requests with no usable answer
  by cause (`timeout`, `transport`, `status`, `bad_answer`; `breaker_open` for
  an admit the open breaker answered without a request); an `op=admit` one
  on an initial admit, or on an uncounted call, runs the call uncounted, one
  on a reroute of a counted call leaves it counted;
  `b2bua_limiter_admit_released_total{site=initial|fold|service}`;
  the gauge `b2bua_limiter_uncounted_calls`, the resident calls running
  uncounted (decision 8): fail open — a call whose asked change adds an id
  among them for that change's round trip — or on a set a service moved it
  onto that the limiter does not hold all of — moved by
  every write of the worker's call map so it is exact on every node that
  holds the call, taken over or reclaimed (a fleet-wide sum counts a call
  twice while two nodes hold it);
  `b2bua_limiter_refresh_answers_total{outcome=extended|reregistered|released|dropped}`,
  `b2bua_limiter_refresh_answers_discarded_total{reason=call_gone|stale}`,
  `b2bua_limiter_refresh_keys_sent_total` (over the refresh requests, the mean
  batch size), `b2bua_limiter_refresh_retries_total`,
  `b2bua_limiter_refresh_given_up_total{reason=lease_expired|cap|released}`,
  the gauge `b2bua_limiter_refresh_due`;
  `b2bua_limiter_release_retries_total`,
  `b2bua_limiter_release_given_up_total{reason=lease_expired|cap|shutdown}`,
  the gauge `b2bua_limiter_release_queue_depth`,
  `b2bua_limiter_release_flushes_total{outcome=empty|sent|given_up}` and
  `b2bua_limiter_release_flush_seconds_total` (a planned exit's flush);
  `b2bua_limiter_task_restarts_total{task=release_sender|refresh_sender|breaker_probe}`;
  the gauge `b2bua_limiter_breaker_open` and
  `b2bua_limiter_breaker_transitions_total{to=open|closed}`; the gauges
  `b2bua_limiter_lease_seconds` and `b2bua_limiter_refresh_period_seconds`,
  `b2bua_limiter_lease_too_short_total` and
  `b2bua_limiter_refresh_period_clamped_total`.
- Config: `LIMITER_LEASE_SECONDS` on the limiter only (1 s to one day, boot
  refuses anything else), stated in its answers; `LIMITER_REFRESH_SECONDS`
  on the workers, the refresh below the lease by more than one period (a
  third of the lease rules when shorter);
  `LIMITER_REFRESH_BATCH_MS` (1000, below the refresh period, the refresh
  period plus a tick below the lease),
  `LIMITER_REFRESH_BATCH_MAX` (1000), `LIMITER_REFRESH_TIMEOUT_MS` (2000),
  `LIMITER_RELEASE_TIMEOUT_MS`, `LIMITER_RELEASE_QUEUE_CAP`,
  `LIMITER_BREAKER_FAILURES` (3), `LIMITER_BREAKER_PROBE_MS` (1000) and the
  exit's release flush bound `B2BUA_DRAIN_RELEASE_FLUSH_MS` (3000) on the
  workers.
- The breaker trades counts for latency: the calls a worker starts while its
  breaker is open stay uncounted for their life, so after the limiter comes
  back its counts read low by those calls until they end, and a cap can be
  passed by them: an admit the breaker did not send counts as a failure with
  `cause=breaker_open` and owes no release by that admit (a consult's turn
  owed it already, decision 6); an initial one
  leaves its call in `b2bua_limiter_uncounted_calls` until it ends, a reroute
  one leaves the call as it was. A failed probe counts as a health request
  with no usable answer, and in the limiter's fail-open episode, which so
  lasts as long as the outage. The
  limiter's health answer is `GET /v1/health`; `/healthz` answers the process
  alone. A malformed `LIMITER_URL` (another scheme, no port, a path, an
  unbracketed IPv6 address) refuses boot; a well-formed name that does not
  resolve at boot starts the breaker open.
- A refresh request carries every key due with its held set: one request per
  worker per tick, the keys due held while the breaker is open. A refresh is
  sent within one tick of falling due, so the lease must outlast the refresh
  period plus a tick; the worker counts each learnt lease where the
  configured period does not and each that shortens its period. A call's
  trace shows its refresh falling due and every answer but `Extended`: an
  extended lease leaves no per-call evidence.
- Re-registration knows no cap: a stale counted copy materialised after its
  release's fence lapsed (a primary that released, crashed before the
  flush and reboots later than one lease) re-registers a set until the
  keepalive reaps that zombie; and after a limiter restart the sets
  re-registered (by a refresh, or by an admit carrying them) beside admits
  made in the gap can read above the cap on `limiter_admission_max` for the
  life of the calls that outlived the restart.
- A refresh answered `released` is not always a fault. A
  partitioned backup's reap of a primary it believes dead fences the key of a
  call the primary still serves; that call's refreshes are refused for one
  lease, then re-register. A refresh sent before a fold's refusal dropped the
  call's set lands on the drop fence and answers `dropped`; the answer and
  the fold reach the call in either order, and either way the call ends
  uncounted and only its terminal release is sent.
- No call waits on its release: a stalled or unreachable limiter delays
  no CDR and no removal, those of calls whose admit failed open included.
  What waits is the release queue, at most one lease of ending calls per
  worker and never more than its cap. A release lost with its worker or
  given up by the queue is freed by the lease, so the limiter's counts read
  high by those calls for up to one lease, and each such hold is counted on
  `limiter_lease_expired_holds_total`.
- `LimiterRefresh` entries are not cohort-smoothed on a bulk reclaim: the
  calls one node reclaims fall due together and the batch absorbs them.
- Replica bodies decode strictly: a body without the limiter key is dropped at
  reclaim and at the reap (no upgrade compatibility, by policy).
- A call holds one set under one key whatever legs it creates: a service
  that creates legs states the call's whole set at each change, computed
  from the held set (decision 13), and needs no key of its own per leg.
