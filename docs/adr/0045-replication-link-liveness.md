# Replication link liveness

**Status:** accepted (2026-10-11)

**Owner:** `crates/b2bua/src/repl/puller.rs` (connect and idle bounds, the
backoff), `repl/server.rs` (the `Noop` interval, the blocked-send bound),
`repl/supervisor.rs` (`membership_settled`), `crates/repl-net/src/transport/socket_settings.rs`
(TCP keepalive and `TCP_USER_TIMEOUT`), `crates/topology/src/k8s.rs` (the kube
client retry), `crates/b2bua-runner-kit/src/replication.rs` and
`replication/{link_env,repl_resolver}.rs` (the env grammar, the bound listener,
the bounded peer lookup).

## Context

Workers replicate over raw TCP, each pulling its peers (ADR-0011, ADR-0014).
The links had no bound outside bootstrap: a connect to an address nobody
answers waited for the kernel's SYN retries; a peer that vanished without a
reset left its puller waiting forever, its `current` flag still set; a peer
that stopped reading parked the serve task on a full socket, and the serve
guard kept that peer's changelog from being reaped; a peer name lookup had no
bound. Three misconfigurations or outages left a worker running without HA
while it reported healthy: a malformed static peer list, no kube client at
boot, and a replication listener that failed to bind (swallowed in a spawned
task). An informer that could not list kept the worker NotReady forever.

## Decision

1. **Connect bound.** A puller gives up a connect after `connect_timeout_ms`
   (2 s) in every phase, bootstrap or not, and backs off.
2. **Idle bound.** The server sends a `Noop` at least every `noop_interval`
   (5 s) on a caught-up stream. A puller that receives nothing for
   `idle_timeout_ms` (15 s) leaves the stream and pulls again from its
   retained watermark through the ordinary backoff. A stream that delivered a
   frame resets the backoff to its floor.
3. **Blocked send.** Every server send is bounded by `send_stall` (15 s). A
   send still blocked then drops the stream, which ends the serve task and
   releases the changelog its guard pinned.
4. **Socket options.** Both ends set TCP keepalive (5 s idle, 2 s interval, 3
   probes) and a 15 s `TCP_USER_TIMEOUT`, so the kernel closes a dead
   connection on an idle and on a busy socket alike.
5. **Bounded lookup.** Each peer name lookup is bounded at 2 s, single-flight
   per name, with the stuck-lookup cap of `ResolvedTarget` (ADR-0043, the
   outbound backend client policy). A lookup past its bound reads as
   unresolvable; a discovered peer then falls back to its pod IP.
6. **Boot refusals.** With replication on, a malformed peer list, a malformed
   link bound, an idle bound not above the `Noop` interval, and a listen
   address that cannot be bound refuse the boot naming the variable. The host
   binds the listener before the core spawns (`ReplicationSetup::listener`).
7. **Kube client retry.** A kube client that fails to build is built again in
   the background (1 s doubling to 30 s). The worker stays wired; its view is
   empty and unsynced until the informer lists.
8. **Readiness over an unsynced view.** `membership_settled` holds readiness
   while the view is unsynced, for at most the bootstrap hard timer from the
   supervisor's start. Past it the worker goes Ready with the view it has and
   peers join as the informer fills it: the risk an unreachable peer already
   carries under the same timer (a rebooting worker may serve before its
   reclaim pullers exist).
9. **Peer errors are expected.** A DNS or connect error toward a peer is the
   ordinary state after a reboot and is never a configuration error. It never
   gates readiness: peers stay excused by the bootstrap hard timer, and
   replication never gates readiness after the latch.

### Settings and their env keys

Read at boot (`LinkSettings::from_lookup`). Unset takes the default; a value
that is not a positive integer refuses the boot.

| key | meaning | default |
|---|---|---|
| `B2BUA_REPL_CONNECT_TIMEOUT_MS` | one connect to a peer | 2000 |
| `B2BUA_REPL_IDLE_TIMEOUT_MS` | a stream delivering nothing is left | 15000 |
| `B2BUA_REPL_NOOP_INTERVAL_MS` | the longest a served stream goes without a `Noop` | 5000 |
| `B2BUA_REPL_SEND_STALL_MS` | a send blocked this long drops the stream | 15000 |
| `B2BUA_REPL_KEEPALIVE_IDLE_MS` / `_INTERVAL_MS` / `_RETRIES` | TCP keepalive | 5000 / 2000 / 3 |
| `B2BUA_REPL_USER_TIMEOUT_MS` | `TCP_USER_TIMEOUT` | 15000 |
| `B2BUA_REPL_LOOKUP_TIMEOUT_MS` | one peer name lookup | 2000 |

## Consequences

- A partition longer than the idle bound ends its streams: on heal the pullers
  reconnect at their next backoff step (up to the 30 s ceiling) and read the
  compacted log from their watermarks, not the frames the partition held.
- A blocked send is bounded per send call: a batch of the bootstrap scan
  counts as one send.
- Pinned by: `b2bua` `repl::link_liveness_tests` (a silent peer, a connect
  nobody answers, the backoff floor after a working stream, a peer that stops
  reading), `repl::membership_sync_tests` (an unsynced view and the hard
  timer); `repl-net` `transport::real` (`both_ends_carry_keepalive_and_a_user_timeout`)
  and `simulated` (the black-holed address); `b2bua-runner-kit`
  `replication::tests` (malformed peers, bind failure, kube client retry),
  `replication::link_env::tests`, `replication::repl_resolver::tests` (the
  lookup bound); `failover-harness` `silent_replication_stream` (calls across a
  silent stream, fully released).
