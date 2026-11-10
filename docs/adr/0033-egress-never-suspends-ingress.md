# Egress never suspends an ingress loop: UDP sends are non-blocking, drop-and-count

**Status:** accepted

The kernel's handling of a UDP datagram, the effect on the loops, and the long form of §4,
§5 and the alternatives are in the [appendix](appendix/0033-egress-never-suspends-ingress.md).

## Context

Every ingress loop in this stack sends inline: the proxy's recv-shard core
(`ProxyCore::run`), the `sip-net` receive pump, the transaction owner and the b2bua router.
An awaited `UdpSocket::send_to` completes only when the send buffer has room. The kernel keeps
datagrams charged to the socket while they wait on an unresolved neighbour (3 × 1 s ARP cycles
by default, up to `unres_qlen_bytes`, which equals the default `SO_SNDBUF`) or on a stalled TX
ring. One dead peer therefore parks the loop for seconds, and every peer it serves sees loss
while every liveness signal reads healthy.
Detail: [appendix, context](appendix/0033-egress-never-suspends-ingress.md#context).

## Decision

1. **`UdpEndpoint::send_to` never suspends on the transport.** The real endpoint makes one
   non-blocking `sendto` on the fd. A full send buffer (`EAGAIN`) is
   `SendErrorKind::WouldBlock`, counted on the endpoint as `send_would_block`, and exported
   (`sip_proxy_udp_send_would_block_total` summed over shards and faces,
   `b2bua_udp_send_would_block_total`). The datagram is dropped, newest first; SIP
   retransmission covers the loss. The syscall is made directly, not through tokio's
   `try_send_to`, which reads "not writable" on a fresh socket until its first poll.
2. **`SO_SNDBUF` is a stated bind option** (`BindUdpOpts::send_buffer_bytes`, runner knobs
   `PROXY_UDP_SNDBUF` and `B2BUA_UDP_SNDBUF`). The SUT chart's kind values request 4 MiB. The
   kernel clamps at `net.core.wmem_max`, a host sysctl, so the host preflight requires it.
3. **The neighbour unresolved queue is bounded** to 16 KiB per neighbour in the proxy's network
   namespace (`net.ipv4.neigh.<if>.unres_qlen_bytes`, set by the proxy pod's init container).
   With 1 and 2, one dead L2 peer pins under 0.5 % of a socket; 256 dead peers at once would be
   needed to reach the buffer.
4. **A recv shard states its liveness.** Each proxy shard stamps the instant it dequeued a
   packet and clears it on return to waiting (`sip_proxy::liveness::ShardPulse`). A shard on
   one packet past `PROXY_SHARD_STALL_MS` (2 000) fails `/readyz` and `/healthz` (ADR-0046 D4)
   and raises `sip_proxy_recv_shards_stalled`. The b2bua's transaction owner and router state
   theirs the same way (`sip_txn::LoopPulse`, `b2bua::loops`): past `B2BUA_LOOP_STALL_MS`
   `/ready` and `/healthz` answer 503 and `b2bua_loop_stalled{loop}` rises; past
   `B2BUA_LOOP_EXIT_MS`, read on two polls in a row, the process exits non-zero. No router turn
   waits on a call's lock.
   Detail: [appendix §4](appendix/0033-egress-never-suspends-ingress.md#4-a-recv-shard-states-its-liveness).
5. **The invariant, stated:** an ingress loop awaits nothing whose completion a third party
   controls. A `RoutingStrategy` that consults a network service resolves off-loop. A
   replicated flush that needs a call's lock is queued as a turn of that call
   (`router/flush_turn.rs`).
   Detail: [appendix §5](appendix/0033-egress-never-suspends-ingress.md#5-the-invariant-stated).

## Considered and rejected

- A per-socket egress task with a bounded queue: it only delays the same drops by its depth.
- `connect()` plus `IP_RECVERR`: the ARP cycle releases the queue, and the signalling sockets
  stay unconnected.
- A timeout around the send: a timer per send, and the loop still parks for the timeout.
- Only the sysctls: no sysctl bounds a NIC.

Detail: [appendix, alternatives](appendix/0033-egress-never-suspends-ingress.md#alternatives-considered).

## Consequences

- A pinned send buffer costs egress for the pinned window (counted drops), never ingress; with
  2 and 3 the window does not occur for a single dead neighbour.
- The transaction owner's `send_errors` and the proxy's per-peer `send_failure` include
  `WouldBlock`. A rising `*_send_would_block_total` means the kernel holds this socket's
  datagrams somewhere.
- `sip-net`'s slow lane reproduces the neighbour case in a network namespace: 62 of 64 sends
  refused in under 100 µs.
- A stalled proxy fails `/readyz` and `/healthz`: its node withdraws the VIPs (ADR-0056) and
  the livenessProbe restarts the proxy (ADR-0046 D4).
- The TCP peers (replication, the CDR broker) keep untimed writes on their own tasks; their
  blast radius is one task, and they are outside this ADR.
