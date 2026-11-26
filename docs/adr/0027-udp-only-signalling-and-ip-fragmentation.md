# UDP-only signalling, and oversize messages fragment at IP

**Status:** accepted

## Context

This stack speaks SIP over UDP only. The only TCP the runners open is HA
replication (`repl-net`) and the HTTP decision/limiter seams. Every signalling socket is a
`bind_udp` on the one network layer (`sip-net`), simulated or real.

SIP messages routinely outgrow a 1 500-byte Ethernet path. Measured over a
production SIP corpus: 279 messages exceed 1 472 bytes (the largest UDP payload
that crosses a 1 500-MTU path unfragmented), the largest single message is
1 975 bytes, and 392 datagrams had to be reassembled from more than one IP
fragment. Long Route sets, identity and charging headers and a full SDP get
there in ordinary calls.

How the tree handles it:

- **Offline**, `sip-pcap`'s `reassembly` module reassembles IPv4/IPv6 fragments
  out of captures, and is tested against fragment loss and reordering.
- **Live**, the receiving kernel reassembles before the datagram is delivered,
  and `sip-net`'s receive pump reads into a 65 536-byte buffer, larger than
  `MAX_UDP_PAYLOAD` (65 507), so `recv_from` never truncates.
- **On the send side**, the Linux default for a UDP socket is `IP_PMTUDISC_WANT`, which sets DF only once a
  path MTU has been learned; a socket that never learned one fragments and the
  send succeeds. Measured on a 1 500-MTU link: under the default and under
  `IP_PMTUDISC_DONT` a 4 000-byte send arrives reassembled; under
  `IP_PMTUDISC_DO` it fails with `EMSGSIZE` (errno 90), a failed call.

## Decision

### X1. Signalling is UDP-only, deliberately, with no TCP fallback

**This stack deviates from RFC 3261 §18.1.1 deliberately:** signalling is UDP-only; a datagram exceeding the path MTU fragments at the IP
layer (`IP_PMTUDISC_DONT`, DF clear) and is reassembled by the receiving
kernel; **no TCP fallback exists or is planned**. §18.1.1's "if the request is within 200 bytes of the path
MTU, or larger than 1300 bytes and the path MTU is unknown, the request MUST be
sent over a congestion-controlled transport" is the clause being declined.

Why: the corpus shows fragmented SIP carried and reassembled in production,
and a second transport is a second transaction layer,
a second connection lifecycle and a second failure surface for a back-to-back UA
whose whole state model is datagram-shaped. **The precondition a deploying
system must satisfy:** every signalling peer sits on a path that carries IP
fragments end to end: a carrier SBC or gateway on a controlled path, not an
arbitrary internet endpoint behind a fragment-dropping middlebox.

What it costs: a lost fragment loses the whole
message (recovery is the transaction layer's retransmission, not IP's);
fragmented traffic is more exposed to middleboxes that drop non-first fragments;
and reassembly buffers are a known resource-exhaustion surface on the
receiving host.

### X2. Every signalling socket pins the fragmenting mode

`bind_udp` builds every socket through socket2 and pins `IP_MTU_DISCOVER` to
`IP_PMTUDISC_DONT` (and `IPV6_MTU_DISCOVER` to `IPV6_PMTUDISC_DONT`) before
binding, the `reuse_port` shards included. The pin states the choice instead
of inheriting it, and forecloses `IP_PMTUDISC_DO`, under which an oversize
INVITE fails to leave at all.

Accepted: pinning `DONT` gives up ICMP-learned path-MTU discovery on the
signalling sockets. The cost differs by address family, and the IPv6 arm is the
sharper one:

- **IPv4.** `DONT` clears DF, so the datagram fragments at the local interface
  and any downstream router with a smaller MTU re-fragments it. Under the
  kernel default that discovery does not happen either (a socket that sets no
  DF learns nothing), so nothing is lost in practice.
- **IPv6.** Routers do not fragment; only the sender may. `DONT` fragments at
  the LOCAL interface MTU and the socket ignores ICMPv6 Packet Too Big, so a
  smaller downstream MTU (an IPsec, GRE or VXLAN tunnel on a carrier
  interconnect) **silently blackholes every oversize signalling datagram**,
  with no learning path and no TCP to fail over to. This stack chooses not to
  detect that; the deployment precondition in X1 is what keeps it out of reach.

### X3. A failed send says *why*, structurally

`SendError` carries a `SendErrorKind` (`MessageTooLong` for `EMSGSIZE`,
`Unreachable` for `ENETUNREACH`/`EHOSTUNREACH`/`ECONNREFUSED`, `Other`),
classified from `raw_os_error()` at the single construction site. The proxy
counts an oversize datagram under its own per-peer bucket
(`kind="message_too_long"`) instead of blaming the peer for the message's size,
and no caller string-matches an OS message to tell the two apart.

### X4. The reception bound is an assertion, not a coincidence

The receive buffer is a named `RECV_BUF_LEN` with
`const _: () = assert!(RECV_BUF_LEN > MAX_UDP_PAYLOAD);`. Linux truncates a
datagram that does not fit and reports the truncated length, so this inequality
alone keeps a reassembled datagram from becoming a torn SIP message.

### X5. The proxy node keeps an incomplete datagram briefly, in a large pool

The kernel holds an incomplete datagram's fragments for `ipfrag_time` (30 s,
IPv6 60 s) in a per-namespace pool of `ipfrag_high_thresh` (4 MiB). Over the
cap it drops every incoming fragment, a sound peer's included, until queues
expire. One peer whose path drops non-first fragments fills the default pool
at about 45 datagrams a second, retransmissions included. The proxy node, where every external datagram is
reassembled, sets a timeout of 2 s and a pool of 64 MiB (chart values
`proxy.reassembly.*`) for IPv4, and for IPv6 and its conntrack defrag pool where
present. The fragments of one datagram leave back to back; a tail later than
2 s is dropped, and the retransmission resends the whole message under a new IP
ID. The pool then fills at about 11 000 stuck datagrams a second, a flood
rather than a misbehaving path. A pool must exceed the node's
`ipfrag_low_thresh`, or the init container fails and names the sysctl.

## Consequences

- `sip-net` pins `unsafe_code = "deny"` instead of the workspace `forbid`: the
  two MTU-discovery options have no safe wrapper, so `fragmentation.rs` carries
  two narrow `#[allow(unsafe_code)]` raw `setsockopt`/`getsockopt` calls with
  SAFETY comments. Every other crate's `forbid` still holds.
- The default and slow lanes pin X1 to X4, the chart's render tests X5: [appendix, the
  tests](appendix/0027-udp-only-signalling-and-ip-fragmentation.md#the-tests).

## Alternatives considered

- **Implement a SIP TCP transport.** Rejected, and the rejection is the
  point of X1: it is a second transaction layer and connection lifecycle for a
  stack whose peers are carrier gateways on controlled paths. A deployment
  that needs it supersedes this ADR.
- **Leave the socket at the kernel default (`WANT`).** Rejected as the *stated*
  design although it behaves the same: it depends on a default that a sysctl,
  container runtime or library can change under the stack, and the failure mode
  is a call that never leaves.
- **`IPV6_PMTUDISC_WANT` on the IPv6 socket only** (fragment at the LEARNED path
  MTU, so a tunnel below the local MTU is honoured instead of blackholed). It is
  the answer to X2's IPv6 cost and it keeps the property the pin exists for:
  `WANT` is not `DO`, and the man page has it fragment rather than return
  `EMSGSIZE`. Not adopted: the measurement behind X2 covers IPv4 only, and the
  modes diverge by family only after the same veth-namespace measurement on an
  IPv6 path.
- **Reject oversize messages at the application layer** (refuse to send past
  1 300 bytes, per §18.1.1's threshold). Rejected: with no second transport, a
  refusal is a dropped call where fragmentation is a delivered one.
- **Trim messages to fit** (drop optional headers past a size threshold).
  Rejected: it makes the stack lossy exactly where the corpus shows the payload
  is load-bearing (identity, charging correlation, full SDP).
