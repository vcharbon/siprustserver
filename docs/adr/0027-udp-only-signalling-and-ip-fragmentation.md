# UDP-only signalling, and oversize messages fragment at IP

**Status:** accepted (2026-08-05)

## Context

This stack speaks SIP over UDP and nothing else. There is no TCP or TLS SIP
transport in the tree: the only TCP the runners open is HA replication
(`repl-net`) and the HTTP decision/limiter seams. Every signalling socket is a
`bind_udp` on the one network layer (`sip-net`), simulated or real.

SIP messages routinely outgrow a 1 500-byte Ethernet path. Measured over a
production SIP corpus: 279 messages exceed 1 472 bytes (the largest UDP payload
that crosses a 1 500-MTU path unfragmented), the largest single message is
1 975 bytes, and 392 datagrams had to be reassembled from more than one IP
fragment. Long Route sets, identity/charging headers and a full SDP get there
without anything unusual happening.

What the tree already does with that:

- **Offline**, `sip-pcap`'s `reassembly` module reassembles IPv4/IPv6 fragments
  out of captures, and is tested against fragment loss and reordering.
- **Live**, the receiving kernel reassembles before the datagram is delivered,
  and `sip-net`'s receive pump reads into a 65 536-byte buffer — larger than
  `MAX_UDP_PAYLOAD` (65 507), so `recv_from` never truncates.
- **On the send side**, nothing stated a path-MTU-discovery mode. The Linux
  default for a UDP socket is `IP_PMTUDISC_WANT`, which sets DF only once a
  path MTU has been learned; a socket that never learned one fragments and the
  send succeeds. Measured on a genuine 1 500-MTU link: under the default and
  under `IP_PMTUDISC_DONT` a 4 000-byte send succeeds and arrives reassembled;
  under `IP_PMTUDISC_DO` the same send fails with `EMSGSIZE` (errno 90).

So the behaviour was correct by inheritance, and one `setsockopt` away from
silently becoming a call-failure mode.

## Decision

### X1 — Signalling is UDP-only, deliberately, with no TCP fallback

**This stack deviates from RFC 3261 §18.1.1 fully voluntarily and explicitly:**
signalling is UDP-only; a datagram exceeding the path MTU fragments at the IP
layer (`IP_PMTUDISC_DONT`, DF clear) and is reassembled by the receiving
kernel; **no TCP fallback exists or is planned**. This is a chosen design, not
an unimplemented one. §18.1.1's "if the request is within 200 bytes of the path
MTU, or larger than 1300 bytes and the path MTU is unknown, the request MUST be
sent over a congestion-controlled transport" is the clause being declined.

What buys the deviation: the corpus shows fragmented SIP being carried and
reassembled in production, and a second transport is a second transaction layer,
a second connection lifecycle and a second failure surface for a back-to-back UA
whose whole state model is datagram-shaped. **The precondition a deploying
system must satisfy:** every signalling peer sits on a path that carries IP
fragments end to end — a carrier SBC or gateway on a controlled path, not an
arbitrary internet endpoint behind a fragment-dropping middlebox.

What it costs, stated so it is never a surprise: a lost fragment loses the whole
message (recovery is the transaction layer's retransmission, not IP's);
fragmented traffic is more exposed to middleboxes that drop non-first fragments;
and reassembly buffers are a documented resource-exhaustion surface on the
receiving host.

### X2 — Every signalling socket pins the fragmenting mode

`bind_udp` builds every socket through socket2 and pins `IP_MTU_DISCOVER` to
`IP_PMTUDISC_DONT` (and `IPV6_MTU_DISCOVER` to `IPV6_PMTUDISC_DONT`) before
binding — not only the `reuse_port` shards, which is where the socket2 detour
used to be. The pin states the choice instead of inheriting it, and forecloses
`IP_PMTUDISC_DO`, under which an oversize INVITE would fail to leave at all.

Accepted: pinning `DONT` gives up ICMP-learned path-MTU discovery on the
signalling sockets. The cost differs by address family, and the IPv6 arm is the
sharper one:

- **IPv4** — `DONT` clears DF, so the datagram fragments at the local interface
  and any downstream router with a smaller MTU re-fragments it. Under the
  previous default that discovery never happened either (a socket that sets no
  DF learns nothing), so nothing is lost in practice.
- **IPv6** — routers do not fragment; only the sender may. `DONT` fragments at
  the LOCAL interface MTU and the socket ignores ICMPv6 Packet Too Big, so a
  smaller downstream MTU — an IPsec, GRE or VXLAN tunnel on a carrier
  interconnect — **silently blackholes every oversize signalling datagram**,
  with no learning path and no TCP to fail over to. This stack chooses not to
  detect that; the deployment precondition in X1 is what keeps it out of reach.

### X3 — A failed send says *why*, structurally

`SendError` carries a `SendErrorKind` — `MessageTooLong` (`EMSGSIZE`),
`Unreachable` (`ENETUNREACH`/`EHOSTUNREACH`/`ECONNREFUSED`), `Other` —
classified from `raw_os_error()` at the single construction site. The proxy
counts an oversize datagram under its own per-peer bucket
(`kind="message_too_long"`) instead of blaming the peer for the message's size,
and no caller string-matches an OS message to tell the two apart.

### X4 — The reception bound is an assertion, not a coincidence

The receive buffer is a named `RECV_BUF_LEN` with
`const _: () = assert!(RECV_BUF_LEN > MAX_UDP_PAYLOAD);`. Linux truncates a
datagram that does not fit and reports the truncated length, so this inequality
is the only thing standing between a reassembled datagram and a torn SIP
message.

## Consequences

- `sip-net` pins `unsafe_code = "deny"` instead of the workspace `forbid`: the
  two MTU-discovery options have no safe wrapper, so `fragmentation.rs` carries
  two narrow `#[allow(unsafe_code)]` raw `setsockopt`/`getsockopt` calls with
  SAFETY comments. Every other crate's `forbid` still holds.
- Default-lane tests pin the mode on both address families, a datagram past any
  Ethernet MTU round-tripping, a `MAX_UDP_PAYLOAD` datagram arriving at its
  exact length, and an oversize INVITE still parsing after the trip. The
  loopback tests state in their names that loopback does not prove
  fragmentation, and skip where the host's loopback cannot carry the size at
  all (WSL2 mirrored networking routes `127.0.0.1` over a 1 500-MTU device that
  drops fragments — a host property, not a stack one).
- The slow lane builds a `unshare --user --map-root-user --net` namespace whose
  loopback is pinned to MTU 1 500 and re-invokes the test binary inside it. The
  discriminating assertion is the pair — `IP_MTU_DISCOVER != IP_PMTUDISC_DO`
  AND a 4 000-byte send succeeding — never "the send returned `Ok`" alone. A
  second case drops non-first fragments (`ip frag-off & 0x1fff != 0`, at
  `prerouting priority raw`, because netfilter reassembles before the filter
  hooks and non-first fragments carry no UDP ports to match on) and pins that
  the receiver then sees *nothing*: a message missing a fragment is never
  delivered in part.

## Alternatives considered

- **Implement a SIP TCP transport.** Rejected for now, and the rejection is the
  point of X1: it is a second transaction layer and connection lifecycle for a
  stack whose peers are carrier gateways on controlled paths. If a deployment
  ever needs it, this ADR is what it supersedes.
- **Leave the socket at the kernel default (`WANT`).** Rejected as the *stated*
  design even though it behaves identically today: the behaviour then depends on
  a default that a future sysctl, container runtime or library can change under
  the stack, and the failure mode is a call that never leaves.
- **`IPV6_PMTUDISC_WANT` on the IPv6 socket only** (fragment at the LEARNED path
  MTU, so a tunnel below the local MTU is honoured instead of blackholed). It is
  the answer to X2's IPv6 cost and it keeps the property the pin exists for —
  `WANT` is not `DO`, and the man page has it fragment rather than return
  `EMSGSIZE`. Not adopted here because the measurement behind X2 covers IPv4
  only: the same veth-namespace measurement on an IPv6 path is what this needs
  before the modes diverge by family.
- **Reject oversize messages at the application layer** (refuse to send past
  1 300 bytes, per §18.1.1's threshold). Rejected: with no second transport, a
  refusal is a dropped call where fragmentation is a delivered one.
- **Trim messages to fit** (drop optional headers past a size threshold).
  Rejected: it makes the stack lossy exactly where the corpus shows the payload
  is load-bearing (identity, charging correlation, full SDP).
