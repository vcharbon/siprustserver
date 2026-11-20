# Appendix: Replication link liveness

Decision part: [ADR-0045](../0045-replication-link-liveness.md).

## Owner

`crates/b2bua/src/repl/puller.rs` (connect, idle and send bounds,
the backoff), `repl/server.rs` (the `Noop` interval, the blocked-send bound,
failed accepts),
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
task). An informer that could not list kept the worker NotReady forever. One
failed accept (descriptors exhausted) ended the replication server for the
life of the process, silently.

### Settings and their env keys

Read at boot (`LinkSettings::from_lookup`). Unset takes the default; a value
that is not a positive integer refuses the boot, and so does a backoff
ceiling under the 100 ms floor, a keepalive time
that is not a whole number of seconds (1 s to 32 767 s), more than 127
keepalive probes, or a user timeout above `u32::MAX` ms. The idle timeout must
be at least twice the `Noop` interval, and the dead-peer TTL above the idle
timeout plus the backoff ceiling plus the connect timeout. The two span nodes — a node's pullers
read its peers' `Noop`s — so a change raises the idle timeout first and the
interval after.

| key | meaning | default |
|---|---|---|
| `B2BUA_REPL_CONNECT_TIMEOUT_MS` | one connect to a peer; a dead verdict meeting the Backup flow's stream down is confirmed by one connect at once within it (ADR-0047 D1), so a node lost whole is taken over at most idle + lookup + connect timeouts after the verdict, plus the idle timeout again for a frozen process whose kernel takes the connect (6–8 s by default) | 2000 |
| `B2BUA_REPL_BACKOFF_MAX_MS` | the reconnect backoff's ceiling (its floor is 100 ms) | 2000 |
| `B2BUA_REPL_IDLE_TIMEOUT_MS` | a stream delivering nothing is left; a dead verdict on a silent peer meets its stream down this long after its last frame at most, then the one connect confirms it (ADR-0047 D1); at least twice the `Noop` interval, far below a pod's force-delete | 2000 |
| `B2BUA_REPL_NOOP_INTERVAL_MS` | the longest a served stream goes without a `Noop`, on every stream, each flow alike: the heartbeat; a position claim vouches for twice it | 500 |
| `B2BUA_REPL_SEND_STALL_MS` | a send blocked this long leaves the stream, either end | 15000 |
| `B2BUA_REPL_OPEN_TIMEOUT_MS` | an accepted connection with no opening request this long is closed; at least the idle timeout (worker start) | 4000 |
| `B2BUA_REPL_MAX_CONNECTIONS` | inbound connections served at once; one above is closed; at least 4 (worker start) and 4 × (workers − 1) (render) | 64 |
| `B2BUA_REPL_KEEPALIVE_IDLE_MS` / `_INTERVAL_MS` / `_RETRIES` | TCP keepalive | 5000 / 2000 / 3 |
| `B2BUA_REPL_USER_TIMEOUT_MS` | `TCP_USER_TIMEOUT` | 15000 |
| `B2BUA_REPL_LOOKUP_TIMEOUT_MS` | one peer name lookup | 2000 |
| `B2BUA_REPL_DEAD_PEER_TTL_MS` | an HA timer of the serving side: a peer's change log no flow has served this long is dropped, writes or not (ADR-0055 §3); it decides only whether a peer coming back with the same identity resumes warm from its watermark or re-bootstraps cold; above the idle timeout plus the backoff ceiling plus one connect, so a short flow blip resumes warm | 60000 |

## Consequences

- Pinned by: `b2bua` `repl::link_liveness_tests` (a silent peer, a connect
  nobody answers, the backoff after a short and after a lasting stream, a
  blocked puller send, a peer that stops reading, a quiet live stream kept
  across 120 s, the `Noop` cadence, failed accepts),
  `repl::membership_sync_tests` (an unsynced view and the hard timer),
  `repl::heal_reach_tests` (a backup behind a long partition catches up within
  one ceiling and one connect of the heal);
  `repl-net` `transport::real` (`both_ends_carry_keepalive_and_a_user_timeout`,
  on the streams `connect` and `accept` wrap) and `simulated` (the black-holed
  address, failed accepts); `b2bua-runner-kit`
  `replication::tests` (malformed peers, bind failure, kube client retry),
  `replication::link_env::tests`, `replication::repl_resolver::tests` (the
  lookup bound); `failover-harness` `silent_replication_stream` (calls across a
  silent stream, fully released).
