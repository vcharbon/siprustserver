# The front proxy's addresses are announced by BGP

**Status:** accepted. Supersedes ADR-0012 D7, ADR-0015 D5, ADR-0046 D4 and the virtual-address
check of ADR-0050 §11; amends ADR-0046 (a total backend outage), ADR-0009, ADR-0033 and
ADR-0048.

The [appendix](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md) holds the owning
files, the full rules, the timing bounds and measurements, the lab and every residual.

## Context

A layer-2 virtual address needs both front proxies in one layer-2 domain, so they cannot sit in
different racks. Production networks route to top-of-rack routers that speak BGP. The proxy is
stateless by default with a memo for the exceptions (ADR-0009 X7), so either proxy can carry any
flow.

## Decision

### 1. Each node announces the VIPs while its proxy is Ready

The proxy runs on the host network and binds both VIPs, which its node holds on `lo`. Via,
Record-Route and every source address stay the VIP, and ADR-0048's trust by ingress face holds.
Per VIP an announce-only `LoadBalancer` Service with `externalTrafficPolicy: Local` lets MetalLB in
BGP mode announce the VIP from each node whose proxy pod can serve. The routers spread flows over
the announcing nodes by ECMP, so the pair is active-active.
Detail: [appendix §1](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#1-each-node-announces-the-vips-while-its-proxy-is-ready).

### 2. kube-proxy runs in iptables or nftables mode

IPVS binds every LoadBalancer address on every node and refuses the ports a Service does not
list, so a worker's INVITE to the internal VIP would be refused on its own node. IPVS is
unsupported. A kube-proxy replacement is checked for the same property before use.

### 3. `/readyz` is the announcement's signal

Ready means no drain latched, no recv shard stalled, the first worker view in, at least one worker
`Alive`, and the external face usable (interface up, one next hop resolved). A proxy with no live
worker answers a new call `503` with a `Retry-After` until its route is withdrawn. A proxy that
loses every worker or its external uplink withdraws both VIPs. A total backend outage withdraws
both routes, which amends ADR-0046's fail-loud rule for that case. Readiness takes 3.8 s to reach
the routers, so it drives planned rollouts and the route guard carries faults (§10).
Detail: [appendix §3](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#3-readyz-is-the-announcements-signal).

### 4. The drain outlasts the withdrawal

On SIGTERM the proxy turns `/readyz` Draining and keeps forwarding for `PROXY_DRAIN_GRACE_MS`.
`templates/checks.yaml` refuses a drain grace at or below detection plus propagation, and a
`minReadySeconds` at or below one probe period plus propagation.
Detail: [appendix §4](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#4-the-drain-outlasts-the-withdrawal).

### 5. No ICMP port unreachable from a VIP

The proxy nodes drop outbound ICMP port unreachable sourced from the VIPs. A client whose datagram
reaches a node with no bound socket then retransmits instead of failing on a transport error
(RFC 3261 §18.4).

### 6. BFD, no BGP graceful restart

Each peer session runs BFD (300 ms × 3 by default), so a dead node is withdrawn within about a
second. Graceful restart stays off, since it keeps a dead node's routes.

### 7. No worker runs on a node that runs or ran a proxy

The VIPs stay on a proxy node's `lo` after its proxy is gone. The chart requires
`proxy.nodeSelector`, keeps the workers off those nodes by a required node affinity, and gives the
proxy pods a required anti-affinity to the worker pods.

### 8. What ECMP does to flows

One peer address and port is one flow, so each proxy is sized for all of a single-flow peer's
traffic. Production routers hash on layer 4, preferably resiliently. The transactions a flow move
splits are ADR-0009 X7's.
Detail: [appendix §8](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#8-what-ecmp-does-to-flows).

### 9. The MetalLB objects ship with the chart, behind a value

With `proxy.announce.metallb.enabled` the chart renders the address pools, advertisements, peers
and BFD profile. Off, the operator keeps them.
Detail: [appendix §9](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#9-the-metallb-objects-ship-with-the-chart-behind-a-value).

### 10. A route guard withdraws an unfit node within 3.4 s

A caller retransmits an INVITE at 0.5, 1.5 and 3.5 s, so a node withdrawn within 3.4 s of a fault
loses no setup a retransmission can still answer. A container beside the proxy, with `NET_ADMIN`
alone, reads the proxy's `/fitz` every 100 ms and owns one nftables table. The table drops the
node's BFD and BGP toward the routers unless the router is in a set whose entries time out 1 s
after each refresh. A fit proxy keeps the set refreshed. A second unfit answer empties it, and a
hung proxy or a dead guard lets it run out in the kernel. The routers then withdraw the node as
dead. A withdrawn node is let out after the proxy read fit for `ROUTE_GUARD_HOLD_MS` (3 s).
Detail: [appendix §10](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#10-a-route-guard-withdraws-an-unfit-node-within-34-s).

### 11. Traffic from the external VIP is routed only through the external interface

A policy rule sends traffic sourced from the external VIP to table 5060, which holds the external next hops
above an `unreachable` default. The optional chain `SIP-EXT-EGRESS` (`network.externalEgressDrop`,
recommended) drops that traffic on any other interface, local addresses included. Plane isolation
at the routers is a deployment requirement. The `egress-routes` container keeps the next hops in
the table.
Detail: [appendix §11](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#11-traffic-from-the-external-vip-is-routed-only-through-the-external-interface).

## Rejected

- SIP through the Service. kube-proxy would collapse both faces onto one node address.
- An FRR or GoBGP sidecar. Its session collides with the cluster's own speaker.
- Active/passive announcement. It needs routing policy on the routers, outside the chart.
- Keeping the layer-2 virtual address as a fallback. No second mechanism is kept.
- A faster readiness chain. It stays above 3.4 s whatever its tuning.
- The guard inside the proxy process. A hung proxy could not withdraw itself.
- The routers' BFD strict mode in place of the BGP drop. It is a router setting outside the chart.
- The guard for planned drains. Readiness already withdraws a draining node.

Detail: [appendix, Rejected](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#rejected).

## The lab

The kind lab simulates the top-of-rack router with FRR in a host-network container
(`deploy/k8s/tor/`). `deploy/README.md` lists the production requirements.
Detail: [appendix, The lab](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#the-lab).

## A rejoining proxy forgets its blind verdicts

When a proxy that read every worker `Dead` reads one `Alive` again, every other `Dead` worker
returns to `Unknown` and its probe judges it afresh (`crates/sip-proxy/src/registry/projection.rs`).
Detail: [appendix](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#a-rejoining-proxy-forgets-its-blind-verdicts).

## Residuals

- A flow move splits transactions across the proxies (ADR-0009 X7).
- The two proxies can hold different views of one worker, and each routes by its own.
- An internal-plane blip both proxies see for about 2.1 s leaves the VIPs unreachable about 10 s.
- The guard's bound (3.4 s) caps the window in which a lost, hung or cut node draws traffic.
- A next hop that dies with its carrier up withdraws the node within 13.1 s, inside Timer H.

Every residual, with its bounds: [appendix, Residuals](appendix/0056-the-front-proxys-addresses-are-announced-by-bgp.md#residuals).
