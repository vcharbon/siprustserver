# Appendix: UDP-only signalling, and oversize messages fragment at IP

The decision part: [ADR-0027](../0027-udp-only-signalling-and-ip-fragmentation.md).

## The tests

- Default-lane tests pin the mode on both address families, a datagram past any
  Ethernet MTU round-tripping, a `MAX_UDP_PAYLOAD` datagram arriving at its
  exact length, and an oversize INVITE still parsing after the trip. The
  loopback tests state in their names that loopback does not prove
  fragmentation, and skip where the host's loopback cannot carry the size at
  all (WSL2 mirrored networking routes `127.0.0.1` over a 1 500-MTU device that
  drops fragments, a host property, not a stack one).
- The slow lane builds a `unshare --user --map-root-user --net` namespace whose
  loopback is pinned to MTU 1 500 and re-invokes the test binary inside it. The
  discriminating assertion is the pair, `IP_MTU_DISCOVER != IP_PMTUDISC_DO`
  AND a 4 000-byte send succeeding, never "the send returned `Ok`" alone. A
  second case drops non-first fragments (`ip frag-off & 0x1fff != 0`, at
  `prerouting priority raw`, because netfilter reassembles before the filter
  hooks and non-first fragments carry no UDP ports to match on) and pins that
  the receiver then sees *nothing*: a message missing a fragment is never
  delivered in part.
- The chart's render tests (`deploy/helm/check/test_siprust.py`) pin X5: the
  proxy node's init script writes both reassembly bounds for each family, and
  the schema refuses a pool below the kernel default.
