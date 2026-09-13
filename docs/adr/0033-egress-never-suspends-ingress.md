# Egress never suspends an ingress loop: UDP sends are non-blocking, drop-and-count

**Status:** accepted (2026-09-13)

## Context

Every ingress loop in this stack sends inline: the proxy's recv-shard core
(`ProxyCore::run`), the `sip-net` receive pump (its pre-ingress reply), the
transaction owner (`sip-txn`'s `Owner`, the b2bua's one event loop) and the
b2bua router. Each reached the socket through `UdpEndpoint::send_to`, and the
real implementation awaited `tokio::net::UdpSocket::send_to`. That await
completes when the kernel has room for the datagram in the socket's send
buffer. Usually that is immediate. It is not immediate whenever the kernel
holds datagrams already sent, and it holds them in ways that have nothing to
do with the peer the loop is serving next.

### What the kernel does with a UDP datagram

A datagram is charged to its socket (`sk_wmem_alloc`, against `SO_SNDBUF`)
from `sendmsg` until the skb is freed. It is freed:

- on transmit completion by a physical interface (the NIC's TX ring);
- on delivery into another network namespace (a veth peer: the skb is
  orphaned when it crosses);
- on a queue-discipline drop.

It is **not** freed while it sits on a neighbour's unresolved queue. A next
hop with no ARP entry starts a solicit cycle (`mcast_solicit` × `retrans_time_ms`,
3 × 1 s by default); every datagram sent meanwhile is queued on that
neighbour, still charged to its socket, up to `unres_qlen_bytes`. The default
`unres_qlen_bytes` is `SK_WMEM_MAX` (212 992), which is also the default
`SO_SNDBUF` (`net.core.wmem_default`). One unresolvable peer therefore pins
exactly one default socket. When the cycle fails the queue is purged, the
socket becomes writable again (the kernel wakes writers at half the buffer),
the next send toward that peer starts the cycle over. A blocked sender sleeps
about 3 s, drains for a few ms, sleeps again, on a lattice anchored at the
first failure.

An unconnected UDP socket without `IP_RECVERR` never sees an ICMP error for a
dead peer. The peer surfaces as nothing at all: no error, only a full buffer.

A physical interface can hold datagrams charged to the socket too: a TX ring
that stalls (link flap, PAUSE frames from a congested switch, a virtual NIC
under host contention, a bond failing over) leaves the queue discipline's
backlog (`txqueuelen` 1000 × a datagram's true size) charged to the sockets
that sent it, which is several times a default send buffer. None of this
reproduces on a veth, so the neighbour case is the one the test lane can
show; the interface case is the one production adds.

### What that did to the loops

A proxy recv shard serves one flow-hashed share of every peer (SO_REUSEPORT).
Parked inside `send_to` toward one dead peer, it received nothing from
anyone for the length of the cycle: its inbound queue filled and tail-dropped
(counted), every peer hashed to it saw seconds of loss, and every liveness
signal read healthy. The OPTIONS health probe uses its own socket; `/readyz`
keys on worker health; the intake-age recorder and the ELU sampler are
activity meters, and a task parked on write readiness has no activity — a
stalled shard was indistinguishable from an idle one. The b2bua's single
owner loop would park the same way, taking the timer wheel with it.

Every other third-party wait in the stack was already bounded: logging goes
through a bounded queue on its own thread, the inbound queues tail-drop, the
HTTP callouts carry timeouts, replication is pull-based. The socket send was
the one await with no timeout, no queue and no drop policy.

## Decision

1. **`UdpEndpoint::send_to` never suspends on the transport.** The real
   endpoint makes one non-blocking `sendto` on the fd. A full send buffer
   (`EAGAIN`) is `SendErrorKind::WouldBlock`, counted on the endpoint as
   `send_would_block`, and exported (`sip_proxy_udp_send_would_block_total`
   summed over shards and faces, `b2bua_udp_send_would_block_total`). The
   datagram is dropped, newest first, which is what the kernel itself does
   when a queue is full; SIP retransmission covers the loss. The syscall is
   made directly rather than through tokio's `try_send_to`, which answers
   from the reactor's readiness cache and reads "not writable" on a fresh
   socket until its first poll.
2. **`SO_SNDBUF` is a stated bind option** (`BindUdpOpts::send_buffer_bytes`,
   runner knobs `PROXY_UDP_SNDBUF` and `B2BUA_UDP_SNDBUF`). The manifests
   request 4 MiB. The kernel clamps at `net.core.wmem_max`, which is a host
   sysctl (read-only in any other network namespace), so the host preflight
   requires it.
3. **The neighbour unresolved queue is bounded** to 16 KiB per neighbour in
   the proxy's network namespace (`net.ipv4.neigh.<if>.unres_qlen_bytes`,
   set by the proxy pod's init container). With 1 and 2, one dead L2 peer
   pins under 0.5 % of a socket instead of all of it; 256 simultaneous dead
   peers would be needed to reach the buffer.
4. **A recv shard states its liveness.** Each proxy shard stamps the instant
   it dequeued a packet and clears the stamp when it returns to waiting
   (`sip_proxy::liveness::ShardPulse`). A shard still on one packet past
   `PROXY_SHARD_STALL_MS` (2 000) flips `/readyz` to NotReady and the gauge
   `sip_proxy_recv_shards_stalled`. This is the only signal that tells a
   parked shard from an idle one; the activity meters cannot.
5. **The invariant, stated:** an ingress loop awaits nothing whose completion
   a third party controls. The `RoutingStrategy` trait is `async` and its
   three call sites sit on the recv loop; a strategy that consults a network
   service must resolve off-loop (the DNS resolver's spawned single-flight is
   the pattern).

## Consequences

- A pinned send buffer now costs egress for the pinned window (counted drops)
  instead of freezing ingress and egress together; with 2 and 3 the window
  does not occur for a single dead neighbour.
- The transaction owner's `send_errors` and the proxy's per-peer
  `send_failure` include `WouldBlock`; a rising `*_send_would_block_total` is
  the signal that the kernel is holding this socket's datagrams somewhere.
- `sip-net`'s slow lane reproduces the neighbour case in a network namespace
  (a veth pair with nobody at the far address): 62 of 64 sends refused in
  under 100 µs where the old path suspended 3 s.
- Readiness does not by itself move traffic away from a stalled proxy in a
  deployment whose data path is a VRRP VIP that tracks nothing of the proxy;
  what should track the pulse (a keepalived `track_file`, an exit for
  restart) is a deployment decision outside this ADR.
- The TCP peers (replication, the CDR broker) keep untimed writes on their
  own tasks; their blast radius is one task, not an ingress loop, and they
  are outside this ADR.

## Alternatives considered

- **A per-socket egress task with a bounded queue.** Rejected: the pin is
  per socket, so a queue only delays the same drops by its depth, and adds a
  copy and a task per socket. The counted drop at the syscall is the same
  policy with nothing in between.
- **`connect()` plus `IP_RECVERR`** to learn of dead peers. Rejected: the
  neighbour queue is released by the ARP cycle, not by an error report, and
  the signalling sockets must stay unconnected.
- **A timeout around the send.** Rejected: a timer per send, and the loop
  still parks for the timeout.
- **Only the sysctls, no code change.** Rejected: the send buffer can be held
  by an interface as well as a neighbour, and no sysctl bounds a NIC; the
  loop must not depend on the kernel never holding its datagrams.
