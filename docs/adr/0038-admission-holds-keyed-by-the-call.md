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
3. **`release(key)` is idempotent.** It drops the set and fences the key
   for one lease: an admit or a refresh landing after the call ended re-creates
   nothing and is refused with its own reason, so every order of admit,
   refresh and release on one call is safe, and a second release from another
   node frees nothing. An admit that drops the set without replacing it (a cap
   refusal with `release_on_refusal`, an empty replacement) fences the key
   against refresh the same way, until the next admit of the key: a refresh
   that left before the drop and lands after it re-creates nothing.
4. **`refresh(key, ids)` extends the lease** of a known call. For a call the
   server does not know and has not fenced it **re-creates the set from
   the ids the refresh carries, with no cap check**: the call exists and was
   admitted; its set lapsed while nobody refreshed it, or the server restarted
   empty. A fenced call stays refused.
5. **A set whose lease lapses is dropped and counted.** The lease is the
   backstop for every release the server never received.
6. **A failed admit is never freed.** A technical failure (timeout, transport
   error, a bad or non-200 answer) leaves the call **uncounted**: no refresh,
   no release. A late admit that landed on the server lapses with its lease.
   Counts read low while such calls live; the counters say how many.
7. **Only a counted Active call refreshes**, and it always has its refresh
   armed: the invariant layer re-arms a missing or past-due `LimiterRefresh`
   on every turn of the call, so a timer fire the per-call queue dropped heals
   on the call's next turn. For an idle established call that turn is its
   keepalive, so the call may run uncounted for up to the keepalive interval
   plus one refresh period. A Terminating call stops refreshing: a teardown
   may outlast the lease (a release consult, then the sliding 32 s backstop),
   in which case the set lapses early and the terminal release is a no-op.
8. **The call state is `{key, counted, ids}`**, one replicated field. A route
   fold's dispatching task replaces the set before the fold reaches the call
   and the fold states the outcome (`SetLimiterState`): the admitted route, or
   uncounted when a refusal dropped the set and the consult resolves otherwise
   (a reject, a redirect, a relay, the local teardown). A fold landing on a
   gone call releases the call by the key it carries; a fold naming another
   key (an earlier call under the same `call_ref`) releases that key when
   counted and leaves the resident call alone.

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
its end defers the release to the primary. Known gap: that terminal, flushed
back to the primary's partition, is evicted at the replica TTL with no
release, so the lease is what frees its re-registered set. The HA cells
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
  re-registered or were refused.
- Config: `LIMITER_LEASE_SECONDS` on the limiter, `LIMITER_REFRESH_SECONDS`
  on the workers, the refresh below the lease by more than one period.
- A refresh carries the call's ids: one request per counted call per period.
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
  call's set lands on the drop fence; the fold then states the call uncounted
  and nothing more is sent.
- `LimiterRefresh` entries are not cohort-smoothed on a bulk reclaim: the
  calls one node reclaims refresh together (batching is follow-up work).
- Replica bodies decode strictly: a body without the limiter key is dropped at
  reclaim and at the reap (no upgrade compatibility, by policy).
- Out of scope here: what a batched per-node refresh and a release queue
  change about the request rate; per-leg keys for a transfer, which extend the
  key without changing this contract.
