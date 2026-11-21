# The front proxy's receive path is sharded by flow, on one clock

**Status:** accepted

**Owner:** `crates/sip-proxy-runner/src/binds.rs`, `src/main.rs`, `src/shards/`;
`crates/sip-proxy/src/core/mod.rs`.

## Context

One receive loop per face caps the proxy at one core. Spreading its intake over several
loops must keep each flow's messages in order (an INVITE before its CANCEL, a retransmission
after its original), keep the self-gate's pressure reading meaningful across loops, and leave
no socket behind when a loop is aborted, since a takeover proxy binds the same address.

## Decision

### §1 One socket per shard per face, the kernel steering flows

With `PROXY_RECV_SHARDS` > 1 every signalling bind of both faces, the first included, sets
`SO_REUSEPORT`, and each shard runs its own receive loop on its own socket. The kernel hashes
on the 4-tuple, so every datagram from one source address and port lands on one shard of a
face: per-flow order holds with no cross-shard coordination, on the internal and the external
plane alike. Every signalling socket carries the selective intake-shed hook (ADR-0009 X4): a
blind tail-drop at the queue cap would drop emergency and in-dialog requests first come, first
served.

### §2 One process clock for arrival and dequeue

The intake age, the self-gate's pressure reading, is the dequeue time minus the arrival time
stamped on the packet. Both come from one process clock, built before the first bind and
shared by every endpoint and every shard's core. A clock per endpoint or per shard was
rejected: two clocks anchored at different instants make the difference meaningless. Every
shard folds its ages into one shared recorder, so the sampled window maximum is the whole
intake's saturation.

### §3 A core owns its helper tasks

A core publishes its endpoint's queue stats from a helper task, since a tail-dropping queue
would otherwise read as 100% forwarded. The helper sits under an abort-on-drop guard owned by
the receive loop's future. A detached helper was rejected: a supervisor or harness abort drops
the loop's future mid-receive, and a helper holding the endpoint would keep the socket (and on
the simulated fabric the bind address) alive past the core, so a takeover proxy could not bind
it. Each shard's core owns its own stats slot.

### §4 The allocator is jemalloc

The proxy runs on jemalloc, as the worker does. glibc malloc keeps freed arena chunks and
ratchets RSS under sustained churn; jemalloc's decay purging returns pages to the OS. The proxy
allocates far less than the worker: the gain is one memory model across tiers and the same
allocator metrics on both, which rules the proxy in or out of a soak's RSS growth. Decay is
tuned through `_RJEM_MALLOC_CONF` on the container.

## Consequences

- A client that varies its source port spreads one call over shards, and over instances
  (ADR-0009 X7's residual); nothing in the proxy reorders across shards.
- State shared by every shard (the trace registry, ADR-0026; the branch memo) is read on every
  shard's datagrams and is built for concurrent readers.
- `sip-proxy` `tests/it/shard_e2e` drives two shards over real sockets.
