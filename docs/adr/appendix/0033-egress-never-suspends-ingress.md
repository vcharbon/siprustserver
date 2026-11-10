# Appendix: Egress never suspends an ingress loop: UDP sends are non-blocking, drop-and-count

The decision part is [ADR-0033](../0033-egress-never-suspends-ingress.md).

## Context

Every ingress loop in this stack sends inline: the proxy's recv-shard core
(`ProxyCore::run`), the `sip-net` receive pump (its pre-ingress reply), the
transaction owner (`sip-txn`'s `Owner`, the b2bua's one event loop) and the
b2bua router. Each reaches the socket through `UdpEndpoint::send_to`. An implementation
that awaits `tokio::net::UdpSocket::send_to` completes when the kernel has room for the datagram in the socket's send
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

### What a blocking send does to the loops

A proxy recv shard serves one flow-hashed share of every peer (SO_REUSEPORT).
Parked inside `send_to` toward one dead peer, it receives nothing from
anyone for the length of the cycle: its inbound queue fills and tail-drops
(counted), every peer hashed to it sees seconds of loss, and every liveness
signal reads healthy. The OPTIONS health probe uses its own socket; `/readyz`
keys on worker health; the intake-age recorder and the ELU sampler are
activity meters, and a task parked on write readiness has no activity, so a
stalled shard reads as an idle one. The b2bua's single owner loop parks the
same way, taking the timer wheel with it.

Every other third-party wait in the stack is bounded: logging goes through a
bounded queue on its own thread, the inbound queues tail-drop, the HTTP
callouts carry timeouts, replication is pull-based. A blocking socket send is
the one await with no timeout, no queue and no drop policy.

## Decision

### 4. A recv shard states its liveness

**A recv shard states its liveness.** Each proxy shard stamps the instant
it dequeued a packet and clears the stamp when it returns to waiting
(`sip_proxy::liveness::ShardPulse`). A shard still on one packet past
`PROXY_SHARD_STALL_MS` (2 000) fails `/readyz` and `/healthz` (ADR-0046
D4) and raises the gauge `sip_proxy_recv_shards_stalled`. This is the only
signal that tells a parked shard from an idle one; the activity meters
cannot.
The b2bua worker's two loops, the transaction owner (recv, timer wheel,
sweep) and the router, state theirs the same way (`sip_txn::LoopPulse`,
`b2bua::loops`): one on a piece of work past `B2BUA_LOOP_STALL_MS` turns
`/ready` and `/healthz` to 503 and raises `b2bua_loop_stalled{loop}`. One
still on it past `B2BUA_LOOP_EXIT_MS`, read on two polls in a row, exits the
process non-zero, as a loop task that ends does: after the limiter release
flush and the CDR close, each on its own task and its own bound, the runtime
then letting go of a thread the loop holds in one poll within 1 s. The exit
threshold defaults to the larger of 15 s and the stall threshold plus 5 s,
and boot refuses one not above the stall threshold and the decision wait,
so a worker reads not ready before it exits. It sits under the
livenessProbe's 30 to 40 s, which stays the backstop for a runtime wedged
whole, whose watch never polls. A stall during a drain exits the same way;
a drain whose grace ends first returns by its own bound, which reads nothing
of the router. No router turn waits on a call's lock, whose holder may hold
it across its decision round trip (`B2BUA_CALL_CONTROL_TIMEOUT_MS`): a bulk
reclaim defers a call whose lock a turn holds to that call's own queue
(ADR-0010 X2). The stall threshold defaults to the larger of 10 s and that
timeout plus 5 s, and boot refuses one not above it by 2 s. A bulk reclaim
beats the router's pulse per call. Under the `cpu_starve` chaos the 0.3 CPU
quota is shared by every runnable thread: with N runtime threads busy, a
turn reads stalled only when its own work needs more than 3/N CPU-seconds,
and exits the worker only when it needs more than 4.5/N (one SIP event,
timer firing or sweep takes milliseconds); a process frozen past the
threshold wakes on one reading of a turn it then finishes, which exits
nothing. The pulse cannot see a loop that is waiting yet never scheduled
while input is queued for it: a starved runtime reads as idle, and only the
probe's own timeout catches it.

A stalled router still draws the proxy's dead verdict, which its backup
refutes on the heartbeats it still hears; the flip of its endpoint at the
stall threshold tells nothing, the proxy already reading it dead, and neither
does the exit (ADR-0047, "A refuted verdict is spent"). The exit is asked 16
to 17 s after the stall, on its second reading past the threshold; the process
ends then with quick sinks, and up to about 24 s with a silent limiter and a
slow CDR sink (flush 3 s, close 3 s, threads 1 s). Its calls wait for the
restart's reclaim, a message or the departure: about 22 s from the stall with
quick sinks and a boot of 3 s (`stalled_worker_exits.rs`). New INVITEs meanwhile
draw the refusal on arrival from the queue's age (ADR-0037 item 11).

### 5. The invariant, stated

**The invariant, stated:** an ingress loop awaits nothing whose completion
a third party controls. The `RoutingStrategy` trait is `async` and its
three call sites sit on the recv loop; a strategy that consults a network
service must resolve off-loop (the DNS resolver's spawned single-flight is
the pattern). A replicated flush that needs a call's lock is queued as a turn
of that call (`router/flush_turn.rs`), as a stand-down and a drop on a
backup's delete are, since a call's turn holds its lock across a decision
round trip: the loop goes on answering the proxy's probes, and a message
queued before the flush is served first (ADR-0010 X2). A flush offered behind
the call's queued release is lost with the other notices of that row
(`dispatch::class`): a refused copy, whose body rode the flush, is gone with
it, the call it would have met being released, and a stored one waits for a
later message, the next flush or the reap. A flush that finds no call releases
the queue only with nothing queued behind it, so an item that makes the call
live keeps the queue for the offers that follow. The bulk
reclaim's passes, run as a restarted node
returns, before a proxy reads it alive, still take each call's lock on the
loop.

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
