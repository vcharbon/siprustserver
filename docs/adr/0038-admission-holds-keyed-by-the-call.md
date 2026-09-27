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
   §8.1.3.5), which would meet the first call's tombstone. The key is
   replicated with the call, so a takeover, a reclaim and the lossy reap
   release the same call.
2. **`admit(key, entries, release_on_refusal)` replaces the call's whole
   set**, checked net of the set it already holds: an id the call keeps or
   reduces never refuses, an id it adds is checked against the cap the entry
   states; the same id twice takes two slots; all or none. On a cap refusal
   the old set stays, unless `release_on_refusal` drops it in the same step.
   The initial route admits with it off; every route fold with it on (a
   refused replacement frees the ended leg before the failure is consulted).
3. **`release(key)` is idempotent.** It drops the set and tombstones the key
   for one lease: an admit or a refresh landing after the call ended re-creates
   nothing and is refused with its own reason, so every order of admit,
   refresh and release on one call is safe, and a second release from another
   node frees nothing.
4. **`refresh(key, ids)` extends the lease** of a known call. For a call the
   server does not know and has not tombstoned it **re-creates the set from
   the ids the refresh carries, with no cap check**: the call exists and was
   admitted; its set lapsed while nobody refreshed it, or the server restarted
   empty. A tombstoned call stays refused.
5. **A set whose lease lapses is dropped and counted.** The lease is the
   backstop for every release the server never received.
6. **A failed admit is never freed.** A technical failure (timeout, transport
   error, a bad or non-200 answer) leaves the call **uncounted**: no refresh,
   no release. A late admit that landed on the server lapses with its lease.
   Counts read low while such calls live; the counters say how many.
7. **Only a counted Active call refreshes**, and it always has its refresh
   armed: the invariant layer re-arms a missing or past-due `LimiterRefresh`
   on every turn, so a timer fire the per-call queue dropped heals on the next
   turn. A Terminating call stops refreshing; its teardown is bounded by the
   32 s backstop, inside the lease.
8. **The call state is `{key, counted, ids}`**, one replicated field. A route
   fold's dispatching task replaces the set before the fold reaches the call
   and the fold states the outcome (`SetLimiterState`); a fold landing on a
   gone call releases the call by the key it carries.

## Lease, refresh and the replica TTL

The lease (120 s) is refreshed every 40 s: two refreshes may be missed. The
replica TTL (`reboot_budget_sec`, 600 s) is longer than the lease, and a
takeover is reactive (a call moves to its backup only when an in-dialog
request reaches it). So a call whose primary crashed and that nobody has taken
over stops refreshing and loses its set one lease after its last refresh, and
the reap of a deferred terminal at the replica TTL releases a set the lease
already dropped. Decision 4 is what closes this: the node that materialises
the call re-arms its timers, the past-due refresh fires at once, and the set is
re-registered within one refresh of the takeover or the reclaim. The HA cells
that prove a release (the reap's, the reclaim's) run a lease longer than the
replica TTL on purpose, so the release they name is what frees the call; the
cells that prove re-registration run the deployed relation.

## Consequences

- Counters: `limiter_lease_expired_{calls,holds}_total`,
  `limiter_reregistered_calls_total`, `limiter_admit_released_total`, the
  gauges `limiter_calls`, `limiter_current_total`, `limiter_admission_max`
  (the largest live count of one id: what an admit compares with its cap).
  The b2bua counts admits refused on a tombstone per site
  (`b2bua_limiter_admit_released_{initial,fold}_total`) and refreshes that
  re-registered or were refused.
- Config: `LIMITER_LEASE_SECONDS` on the limiter, `LIMITER_REFRESH_SECONDS`
  on the workers, the refresh below the lease by more than one period.
- A refresh carries the call's ids: one request per counted call per period.
- Out of scope here: what a batched per-node refresh and a release queue
  change about the request rate; per-leg keys for a transfer, which extend the
  key without changing this contract.
