# Replication link liveness

**Status:** accepted. Amended by ADR-0055 §3 (the hold of the ACK and the first
provisional reads the applied position). The `Noop` is a 500 ms heartbeat and the idle bound
2 s, so the stream confirms a dead verdict on a silent peer within 2 s (ADR-0047 D1).

Owner modules, the full settings table and the test pins are in the
[appendix](appendix/0045-replication-link-liveness.md).

## Context

Workers replicate over raw TCP, each pulling its peers (ADR-0011, ADR-0014). An unbounded
link waits on the kernel's SYN retries, on a peer that vanished without a reset, on a peer
that stopped reading, or on a name lookup. A malformed peer list, no kube client at boot or a
listener that failed to bind leaves a worker without HA while it reports healthy, and one
failed accept can end the replication server. [Detail](appendix/0045-replication-link-liveness.md#context).

## Decision

1. **Connect bound and backoff ceiling.** A puller gives up a connect after
   `connect_timeout_ms` (2 s) in every phase, bootstrap or not, and backs off
   from 100 ms doubling to `backoff_max_ms` (2 s). One attempt costs at most
   one connect bound, so the low ceiling is cheap, and it bounds how long a
   healed link waits for its next reconnect.
2. **Idle bound.** The server sends a `Noop` at least every `noop_interval`
   (500 ms, the heartbeat) on a caught-up stream, every stream alike. A
   puller that receives nothing for `idle_timeout_ms` (2 s) leaves the stream
   and pulls again from its
   retained watermark through the ordinary backoff. A stream that stayed
   connected for the idle bound resets the backoff to its floor; a shorter
   one does not, so a peer that closes every stream at once is redialled with
   a growing backoff.
3. **Blocked send.** Every send is bounded by `send_stall` (15 s), on both
   ends. A server send still blocked then drops the stream, which ends the
   serve task and releases the changelog its guard pinned; a puller send
   (its `PullRequest`, its `Position`s) leaves the stream like the idle bound.
4. **Socket options.** Both ends set TCP keepalive (5 s idle, 2 s interval, 3
   probes) and a 15 s `TCP_USER_TIMEOUT`, so the kernel closes a dead
   connection on an idle and on a busy socket alike. An option the kernel
   refuses is logged; a value it would round or refuse is refused at boot.
5. **Failed accepts.** A failed accept never stops the replication server: it
   logs a warning once per run of failures, pauses 100 ms and accepts again.
   Only a closed listener ends it.
   An accepted connection that sends no opening `PullRequest` within the open
   timeout is closed, and one accepted while the cap on concurrent
   connections is served is closed at once; both are counted
   (`b2bua_repl_inbound_dropped_total{reason}`). A peer reads either close as
   its stream ending unheard, so the open timeout is at least the idle
   timeout and the cap holds every peer's two flows and a reconnect of each.
6. **Bounded lookup.** Each peer name lookup is bounded at 2 s, single-flight
   per name, with the stuck-lookup cap of `ResolvedTarget` (ADR-0043, the
   outbound backend client policy). A lookup past its bound reads as
   unresolvable; a discovered peer then falls back to its pod IP.
7. **Boot refusals.** With replication on, a malformed peer list, a malformed
   link bound, an idle bound under twice the `Noop` interval, and a listen
   address that cannot be bound refuse the boot naming the variable. The host
   binds the listener before the core spawns (`ReplicationSetup::listener`).
   A puller announcing another protocol version is refused at connect with a
   `StreamRefused` naming the server's, counted
   (`b2bua_repl_streams_refused_total`), and pulls again after its backoff.
8. **Kube client retry.** A kube client that fails to build is built again in
   the background (1 s doubling to 30 s). The worker stays wired; its view is
   empty and unsynced until the informer lists.
9. **Readiness over an unsynced view.** `membership_settled` holds readiness
   while the view is unsynced, for at most the bootstrap hard timer from the
   supervisor's start. Past it the worker goes Ready with the view it has and
   peers join as the informer fills it: the risk an unreachable peer already
   carries under the same timer (a rebooting worker may serve before its
   reclaim pullers exist).
10. **Peer errors are expected.** A DNS or connect error toward a peer is the
   ordinary state after a reboot and is never a configuration error. It never
   gates readiness: peers stay excused by the bootstrap hard timer, and
   replication never gates readiness after the latch.

### Settings and their env keys

Read at boot (`LinkSettings::from_lookup`). Unset takes the default. A value that is not a
positive integer refuses the boot. The idle timeout is at least twice the `Noop` interval,
and since a node's pullers read its peers' `Noop`s, a change raises the idle timeout first
and the interval after.

| key | default |
|---|---|
| `B2BUA_REPL_CONNECT_TIMEOUT_MS` | 2000 |
| `B2BUA_REPL_BACKOFF_MAX_MS` (floor 100 ms) | 2000 |
| `B2BUA_REPL_IDLE_TIMEOUT_MS` | 2000 |
| `B2BUA_REPL_NOOP_INTERVAL_MS` | 500 |
| `B2BUA_REPL_SEND_STALL_MS` | 15000 |
| `B2BUA_REPL_OPEN_TIMEOUT_MS` | 4000 |
| `B2BUA_REPL_MAX_CONNECTIONS` | 64 |
| `B2BUA_REPL_KEEPALIVE_IDLE_MS` / `_INTERVAL_MS` / `_RETRIES` | 5000 / 2000 / 3 |
| `B2BUA_REPL_USER_TIMEOUT_MS` | 15000 |
| `B2BUA_REPL_LOOKUP_TIMEOUT_MS` | 2000 |
| `B2BUA_REPL_DEAD_PEER_TTL_MS` | 60000 |

Meanings and bounds: [appendix](appendix/0045-replication-link-liveness.md#settings-and-their-env-keys).

## Consequences

- A partition longer than the idle bound ends its streams: on heal the pullers
  reconnect at their next backoff step and read the compacted log from their
  watermarks, not the frames the partition held. A backup is behind a healed
  link for at most one backoff ceiling plus one connect bound (4 s by
  default).
- A blocked send is bounded per send call: a batch of the bootstrap scan
  counts as one send.
- Test pins: [appendix](appendix/0045-replication-link-liveness.md#consequences).
