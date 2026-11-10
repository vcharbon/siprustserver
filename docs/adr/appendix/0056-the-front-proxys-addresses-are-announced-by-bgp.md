# Appendix: The front proxy's addresses are announced by BGP

The decision part is [ADR-0056](../0056-the-front-proxys-addresses-are-announced-by-bgp.md).

## Owner

`deploy/helm/siprust/templates/proxy-announce.yaml` (the announce-only Services),
`templates/proxy-metallb.yaml` (the MetalLB objects), `templates/proxy.yaml` (readiness probe,
`minReadySeconds`, anti-affinity), `files/vip-loopback.sh` (the VIPs on `lo`, the ICMP drop, the
external VIP's routing), `files/vip-egress-routes.sh` (its next hops),
`templates/checks.yaml` (the timing relations), `crates/sip-proxy-runner/src/readiness.rs` and
`external_face.rs` (what `/readyz` and `/fitz` read), `crates/sip-proxy/src/health/reach.rs`
(the isolation verdict), `crates/route-guard` (the route guard), `deploy/k8s/tor/` and
`deploy/helm/lib/tor.sh` (the lab router).

## 1. Each node announces the VIPs while its proxy is Ready

The proxy runs on the host
network and binds both VIPs, which its node holds on `lo` for good (`vip-loopback.sh`, strict
ARP): Via, Record-Route and every source address stay the VIP, and ADR-0048's trust by
ingress face is unchanged. Per VIP the chart renders one `LoadBalancer` Service that carries
no SIP port: one TCP port with no listener on a proxy node (never the metrics port, refused
at render), `externalTrafficPolicy: Local`, `allocateLoadBalancerNodePorts: false`, the VIP
pinned by `metallb.io/loadBalancerIPs`, the proxy pods as endpoints. MetalLB in BGP mode
(frr-k8s backend) announces a Local Service only from nodes with an endpoint that can serve
(Ready, or Serving while it terminates), so each VIP's route follows `/readyz` while the kubelet
reports it. A node whose heartbeats are lost has its pod marked not ready by the node controller,
whatever `/readyz` answers, and its speaker withdraws it: an open limit, that path has no answer
here. The routers spread flows over the announcing nodes (ECMP): the pair is active-active.

## 2. kube-proxy runs in iptables or nftables mode

Both match a LoadBalancer address per
protocol and port, so VIP:5060/UDP reaches the proxy's socket untouched. IPVS binds every
LoadBalancer address to every node and its filter refuses the ports a Service does not list:
a worker's INVITE to the internal VIP would be refused on its own node. IPVS is unsupported; a
kube-proxy replacement is checked for the same property before use. kube-proxy serves the
Services' `healthCheckNodePort` on every node.

## 3. `/readyz` is the announcement's signal

Ready means: no drain latched, no recv shard stalled, the
first worker view in, at least one worker `Alive`, and the external face usable: the external
interface present and up and at least one of its configured next hops resolved (`REACHABLE`,
`STALE`, `DELAY`, `PROBE`, `PERMANENT`) in the kernel's neighbour table (`PROXY_EXT_INTERFACE`,
`PROXY_EXT_NEXT_HOPS`). A next hop read `FAILED` or `INCOMPLETE` in two readings, none resolved
between them, latches the face unusable until one reads resolved again, so an entry flapping
between `INCOMPLETE` and gone never reads usable. The next hops are the external plane's BGP
peers (checked at render when the chart renders MetalLB's objects), whose BFD and keepalive
traffic keeps them resolved while the node is let out. The route guard drops that traffic at
output, before neighbour resolution, so while the face is latched each reading solicits every
next hop itself: one empty datagram to the discard port from a fresh UDP socket, which needs no
capability and which the guard's chain lets pass (§10). An answer clears the latch, so a
recovered next hop brings the node back with nothing sent from outside. A solicit the node's
own firewall refuses warns once per latch, and that latch then waits for outside traffic. The
interface read not up once, from a usable face, is a carrier flap and holds the face usable; the
second reading in a row is the face down; a removed interface is unusable at its first reading.
The flap also empties the neighbour table and the next packet leaves the entry `INCOMPLETE`
until an ARP retry is answered, past the flap: hence the two readings of a next hop, and an ARP
retry of at most half a reading period (render, §10). All of it is local kernel state read
every `PROXY_EXT_FACE_CHECK_MS` off the async runtime, two readings always a period apart. It
ties the two faces together: a node whose caller-facing uplink is gone stops announcing the
internal VIP as well. No component detects the reachability of a peer beyond the first router: a
fault in the routed network hits both proxies alike and is the network's to converge.
Consequences:
- a proxy with no live worker answers a new call `503` with a `Retry-After` until its route is
 withdrawn, so the caller may retry elsewhere at once; a short loss is answered rather than
 left to time out;
- a proxy that loses every worker, or its external uplink, while its peer does not, withdraws
 both VIPs and the peer carries all traffic;
- **a total backend outage withdraws both routes**: once both are withdrawn the callers see
 an unreachable address. This amends ADR-0046's fail-loud rule for that one case. The
 routes come back R after the workers do (a probe round 1 s, readiness up to 2 s, the
 propagation P, and the returning workers' backlog), so an in-dialog transaction first sent
 in the outage's first D + R − L gets no final, D the outage and L its sender's last
 retransmission (27.5 s for a non-INVITE, T1 500 ms, T2 4 s): an established call loses a
 BYE (stranded) or a re-INVITE (cut). That is the residual of a total outage longer than
 L − R, budgeted by the hakit budget `backend-outage`;
- **with the API server down** EndpointSlices cannot change, and a speaker restarted then
 withdraws every route it announced. The route guard (§10) still withdraws an unfit, hung or
 crashed proxy's node, and BFD a dead node.

`/readyz` is the slow path: the kubelet's probe, the EndpointSlice, the speaker and the
frr-k8s reload take 3.8 s on their own, so it drives planned rollouts and backs the route
guard up, which carries every fault (§10).

## 4. The drain outlasts the withdrawal

On SIGTERM the proxy turns `/readyz` Draining and keeps
forwarding for `PROXY_DRAIN_GRACE_MS`. The withdrawal is readiness detection
(`readinessProbe.periodSeconds` × `failureThreshold`, 1 s × 2) then the propagation to the
routers' tables (EndpointSlice, speaker, frr-k8s reload, BGP UPDATE, router FIB:
`proxy.announce.propagationMs`). The Deployment's `minReadySeconds` covers the announcement of
a replaced pod, so a rollout stops the next proxy only once the previous one is back in the
routers' tables. `templates/checks.yaml` refuses a drain grace at or below detection plus
propagation, and a `minReadySeconds` at or below one probe period plus propagation. The lab
measures them with the `proxy_rollout` incident, frr-k8s reloading FRR 500 ms after a
configuration change (its default 3 s adds 2.5 s to every withdrawal and announcement): the
drain-to-withdrawal reached 3.8 s and the Ready-to-announced 4.65 s in both kube-proxy modes,
so the propagation is 5 s, `minReadySeconds` 10 s and the drain grace 8.8 s, the measured
drain-to-withdrawal plus 5 s (`docs/TUNING.md`). The route guard does not start a planned
drain: the proxy stays fit while it drains, so readiness withdraws the node. When the
drained proxy exits, its guard exits too and the last refresh runs out, so the node's BFD
and BGP drop; the successor's init step writes the table withdrawing again, and its guard
lets the node out once the new proxy read fit for the hold. Each rollout step thus takes
its node's sessions down and back, inside the time `minReadySeconds` budgets (§10).

## 5. No ICMP port unreachable from a VIP

Until its route is withdrawn a node still receives a
VIP's traffic while no proxy socket is bound (a crash, the end of a drain); its kernel would
answer port unreachable, which a SIP client takes as a transport error (RFC 3261 §18.4)
instead of retransmitting into the rehashed path. The proxy nodes drop outbound ICMP port
unreachable sourced from the VIPs.

## 6. BFD, no BGP graceful restart

Each peer session runs BFD (300 ms × 3 by default), so a
node's death is withdrawn within about a second. Graceful restart would keep a dead node's
routes after its BFD session went down; it stays off. A restart of the speaker or of frr-k8s
on a node ends its sessions, withdraws the node and rehashes its flows.

## 7. No worker runs on a node that runs or ran a proxy

The VIPs stay on a proxy node's `lo`
whatever the routers say, after its proxy is gone too, so a worker there would reach that node
instead of the announcing ones. The chart requires `proxy.nodeSelector` and gives the workers
a required node affinity off every node it names (one `NotIn` term per label, ANDed into the
workers' own terms); the proxy pods also carry a required anti-affinity to the worker pods.

## 8. What ECMP does to flows

One peer address and port is one flow, so one proxy carries all
of a single-flow peer's traffic and each proxy is sized for all of it (`PROXY_*` budgets stay
per proxy, ADR-0046, ADR-0009). Removing a next hop moves only its flows; adding one moves
about half of them with hash-threshold multipath, few with resilient hashing. Production
routers hash on layer 4 and preferably resiliently. The transactions a move splits across the
proxies are ADR-0009 X7's.

## 9. The MetalLB objects ship with the chart, behind a value

With
`proxy.announce.metallb.enabled` the chart renders, in MetalLB's namespace, one /32
`IPAddressPool` per VIP (`autoAssign: false`), one `BGPAdvertisement` per pool to that face's
peers only, one `BGPPeer` per router selected by `proxy.nodeSelector`, and one `BFDProfile`;
a render without MetalLB's APIs fails naming them. Off, the operator keeps them.

## 10. A route guard withdraws an unfit node within 3.4 s

A caller retransmits an INVITE at
0.5, 1.5 and 3.5 s, so a node withdrawn within 3.4 s of a fault loses no setup a
retransmission can still answer. Readiness cannot get there (§3), so a container beside the
proxy, on the node's network with `NET_ADMIN` alone (the proxy keeps none), reads the proxy's
`/fitz` over loopback every `ROUTE_GUARD_POLL_MS` (100 ms) and owns one nftables table. Its
output chain drops the node's BFD control (UDP 3784) and BGP (TCP 179, either end) toward the
routers (`proxy.routeGuard.peers`, by default MetalLB's peers) unless the router is in a set
whose entries time out `ROUTE_GUARD_DEAD_MS` (1 s) after each refresh. While the proxy reads
fit the guard refreshes the set every quarter of that bound; the second unfit answer since
the last fit one empties it, so one reading of a passing blip does not; a proxy or a guard that stops refreshing (hung, crashed, killed with its pod) lets it
run out in the kernel, with no write. The routers miss the node's BFD for their detection
time (3 × 300 ms) and withdraw every route through it, as for a dead node. BGP is dropped
with BFD: without it the node's next connect retry re-establishes the session the BFD loss
ended and announces the routes again while BFD stays down.
- `/fitz` is fit when `/readyz` would be Ready, its drain latch aside, and some worker
  answered a health probe within `HEALTH_ISOLATION_MS` (2 s): the isolation verdict, the
  proxy's own reach, read before any per-worker dead verdict (2.6-3.6 s), which stays the
  routing's. A proxy is never fit before it has read a worker alive.
- Bounds, checked at render against `proxy.routeGuard.withdrawWithinMs` (3.4 s): a worker cut
  2 s + two readings 0.2 s + 0.9 s = 3.1 s; an external link lost, read down twice, 2 ×
  `PROXY_EXT_FACE_CHECK_MS` 1 s + 0.2 s + 0.9 s = 2.1 s (an interface removed reads unusable
  at once, 1.6 s); a hung or dead proxy 1 s + 0.9 s = 1.9 s.
- A next hop that dies with its carrier up is outside that bound and inside Timer H: its
  neighbour entry reads `FAILED` at the end of the kernel's STALE, DELAY and PROBE cycle. By
  phase that takes from `ucast_solicit` × `retrans_time_ms`, 3 s on an entry already entering
  PROBE, up to 1.5 × `base_reachable_time_ms` + `delay_first_probe_time` + 3 s, 10 s at the
  chart's `proxy.neighbour` (the init step sets them on every interface; the kernel's defaults
  give about 50 s), + up to 1 s for the next packet that takes the entry out of STALE once the
  session's BFD went down. Multipath egress leaves that next hop then
  (`fib_multipath_use_neigh`, set by the same step, over the table of §11), so a worker's retransmitted final reaches its caller inside Timer
  H. With no next hop left two face readings latch the face, up to 1 s: the proxy reads unfit
  from about 4 s to about 12 s after the death. The guard's two readings and the BFD detection
  then take the node out of the routers' ECMP, 13.1 s at most. The render checks that chain
  against 32 s, without the guard's terms when the guard is off. The timers rest on the solicit
  (§3): without it a shorter cycle reaches a latch nothing clears sooner. Measured on the lab
  (`next_hop_dead`, one next hop, kernel 7.2): the proxy unfit 6.0 to 8.0 s after the death, the
  internal VIP's route gone 6.8 to 8.9 s after it (up to 12.0 s over the earlier runs of these
  timers), the external VIP's 0.85 s after it through the BFD of the router that died, its node
  routed again 8.6 s after the next hop's return, with nothing sent from outside; the kernel's
  default timers read the death in 47 s.
- So a carrier-up silence of the next hops latches by phase from about 4 s, and always past
  the ceiling of the read, about 12 s. Next hops both proxies share then withdraw both VIPs,
  and the routes return about 9 s after the silence ends: the solicit's answer, the hold 3 s,
  the connect time 1 s, BFD up. That is the price of the shorter cycle.
- A carrier flap leaves the proxy fit: the face holds through it when the first ARP retry after
  it is answered (§3), and no hold applies. The node's external BGP session re-establishes
  after it: on the lab every flap of 0.1 to 0.4 s took the node's external routes away for about
  1 s, on both nodes for a flap they share.
- A withdrawn node is let out after the proxy read fit for `ROUTE_GUARD_HOLD_MS` (3 s), so a
  flapping fault does not rehash flows each time; its BGP sessions come back within the
  BGPPeer `connectTime` (`proxy.announce.metallb.connectTimeSeconds`, 1 s). A restarted guard
  rebuilds its table from its first reading, keeping a fit proxy's node out without a hold;
  the pod's init step writes the table withdrawing, so a node whose pod starts, after a node
  restart among others, announces nothing before its proxy reads fit for the hold.
- The guard's own `/readyz` is Ready exactly while it lets the node out, and the guard
  container has a readiness probe: the pod is Ready, and `minReadySeconds` counts, only from
  a node that may carry traffic, so a blip after Ready that restarts the hold holds the
  rollout too. The render requires `minReadySeconds` above one probe period + the connect
  time + 2 s, so a rollout also waits for the let-out node's sessions.
- A total backend outage withdraws both nodes within 3 s (§3) and brings them back once a
  worker answers, readiness and the hold allowing.
- The guard reports `route_guard_withdrawn`, `route_guard_withdrawals_total{cause}` and
  `route_guard_restores_total`, and logs one line per withdrawal and per return.

## 11. Traffic from the external VIP is routed only through the external interface

The proxy
crosses (ADR-0048 §10), so a request or a response a worker's peer named, whatever address it
holds, leaves from the external VIP, and the node keeps that on the external plane. Nothing here
reads a datagram's content.
- **Policy routing.** `ip rule priority 5060 from <external VIP> lookup 5060`, in table 5060 an
  `unreachable` default at the highest metric and the external next hops
  (`network.externalNextHops`) below it, so a miss never falls through to the main table and a
  send with no next hop fails at once. The init step writes the rule, the `unreachable` route
  and the chain, none of which reads the interface, so a pod starting during a cut comes up, and
  reads unfit (§3).
- **A drop on the node, a recommended hardening** (`network.externalEgressDrop`, default off).
  An owned chain, `SIP-EXT-EGRESS`, swapped whole by `iptables-restore --noflush`, jumped from
  `OUTPUT`: `-s <external VIP>/32 ! -o <external interface> -j DROP`, with no address list and no
  exemption for `lo`. The kernel consults the local table before any rule, so the routing above
  does not cover a local address: this chain is what covers it. It also refuses a send that falls
  through to the main table when the policy rule is lost (measured: the send fails with EPERM).
  Running without it, a send from the external socket to the internal VIP is delivered locally
  and reaches a socket trusted by its face, one to the external VIP re-enters on that face, and
  any local UDP listener on the port a target names gets a datagram sourced from the external
  VIP. A request reaching the proxy from its own address is answered 482 Loop Detected (RFC 3261
  §16.3). It does not cover what the table
  routes: a send from the external socket to a worker leaves through the external interface
  either way, so the routers' isolation stops it, then the workers' NetworkPolicy; on a CNI that
  does not enforce NetworkPolicy, the routers' isolation alone does.
- **Plane isolation at the routers**, a deployment requirement (`deploy/README.md`).

A container on the network tools image, `egress-routes`, with `NET_ADMIN` alone, keeps the next
hops in the table: the kernel drops a gateway route when its interface goes and they return
with it, with no pod restart. Each turn (`proxy.egressRoutes.periodSeconds`, 1 s) it reads the
table and writes only on a difference, so no route or cached entry churns. A write failing three
turns in a row with the interface up and addressed exits it. Its readiness probe reads the table
and passes while it holds a next hop, so a pod whose table is empty stays out of Ready across
the container's restarts: the node withdraws by the slow path (§3) and stays out until the routes
are in. It is not the route guard: the guard is optional, its image
has no `ip`, and a stuck route call there would delay its refresh and falsely withdraw the
node. The render requires next hops and a period below `routeGuard.holdMs`, so the table is
filled before the guard lets the node out. A pod stop removes nothing, as for the VIPs on `lo`
(§7); `deploy/helm/README.md` states the uninstall commands.

## Rejected

- **SIP through the Service.** kube-proxy would source some outbound requests from the node's
  address and both faces would collapse onto one node address.
- **An FRR or GoBGP sidecar in the proxy pod.** A second session from the node's address to the
  same router collides with the cluster's own speaker.
- **Active/passive announcement** (one node preferred by AS-path or community). It needs routing
  policy on the routers, outside the chart, and the flow moves of ADR-0009 X7 happen at failover
  anyway; active-active also spreads load across the peers.
- **Keeping the layer-2 virtual address** as a fallback: no second mechanism is kept. A VRRP
  pair needs both proxies in one layer-2 domain, so they cannot sit in different racks, which is
  the proxy's own failure-domain separation. Its health coupling leaves gaps: the address holder
  ignores worker reachability, the address-release hook is not ordered before the proxy's own
  SIGTERM, and the passive proxy needs a notice path of its own.
- **A faster readiness chain** (shorter probe period, a faster frr-k8s reload): the chain from
  the kubelet's probe to the routers' tables stays above 3.4 s whatever its tuning.
- **The guard inside the proxy process**: the proxy would need `NET_ADMIN`, and a hung proxy
  could not withdraw itself; the kernel-side timeout covers a hung proxy and a dead guard alike.
- **The routers' BFD strict mode** (no BGP session before BFD is up) in place of the BGP drop:
  it is a router setting outside the chart, not on every router.
- **The guard for planned drains**: readiness already withdraws a draining node within the drain
  grace, and tearing the node's sessions would add a reconnect to the successor's announcement.

## The lab

The kind lab simulates the top-of-rack router with FRR in a host-network container
(`deploy/k8s/tor/`, `--restart unless-stopped`) peering by eBGP with the edge nodes on both
bridges, BFD and multipath on, the internal VIP accepted from the internal bridge only and the
external VIP from the external bridge only. Both VIPs lie outside their bridge's subnet, so
every sender reaches them through its default route to the host, where the router's /32 routes
win. Docker's isolation between the bridges keeps each VIP off the other plane. A one-time host
setup (`deploy/k8s/lab-host-setup.sh`, with sudo) sets layer-4 multipath hashing, no ICMP
redirects and loose reverse-path filtering on the bridges, and the firewall rules BGP, BFD and
same-bridge forwarding need. `deploy/README.md` lists the production requirements.

## A rejoining proxy forgets its blind verdicts

A proxy cut off every worker reads them all `Dead`, verdicts about its own reach. When a
write reads one worker `Alive` again while no worker reads alive, every other worker read
`Dead` returns to `Unknown` and its probe judges it afresh
(`crates/sip-proxy/src/registry/projection.rs`). Otherwise the rehashed flows bring
responses for a live primary to a proxy that sends them to the call's backup, which takes the
call over beside its primary (failover harness `rejoining_proxy_stale_verdicts`). A worker
really dead reads `Dead` again at its next probe miss; until then a response for it follows its
Via.

## Residuals

- A flow move splits transactions across the proxies (ADR-0009 X7): a CANCEL or ACK reaching a
  proxy without the memo entry its call needed, an INVITE repeat placed like a new call.
- The two proxies can hold different views of one worker; each routes the traffic it receives by
  its own view. The failover harness cell `split_proxy_views` judges that case with real B2BUAs:
  no leak, full cleanup, one CDR, no copy left serving.
- An internal-plane blip both proxies see for about 2.1 s or more (the isolation window, two
  readings), or for as little as about 1.1 s when the last probe answer was one interval old at
  the cut, withdraws both nodes, the total-outage rule of §3. The VIP is unreachable from the
  withdrawal (the BFD detection, 0.9 s, after those 2.1 s) until the blip's end, the next probe
  round that answers (up to 1 s), the guard's hold (3 s), the pod's readiness (up to 2 s, as in
  §3) and the announcement's propagation (4.65 s measured): for a blip just past the threshold,
  the VIPs are routed again up to about 13 s after it began, about 10 s of them unreachable, where
  readiness alone rode out blips under about 2.6 s. The hakit budget `backend-outage` derives
  its recovery R from the same chain.
- The route guard's bound (3.4 s) caps the window in which a lost, hung or cut node still draws
  traffic; the hakit budgets `proxy-failover` and `proxy-cut` derive their allowances from it,
  `proxy-reroute` from the readiness path of a planned withdrawal.
- After a node restart its speaker may re-establish BGP before the proxy pod's init step writes
  the withdrawing table, while the EndpointSlice still names the old pod Ready: the node is
  announced for that gap (the kind lab measures it with `node_stop`).
- The guard drops BGP to its peers whatever speaker owns the session: a CNI peering with the same
  routers from the proxy nodes, or routes the nodes learn over those sessions, is unsupported
  (`deploy/README.md`).
- A guard removed from a node (the pod gone, no successor) leaves its table withdrawing; a pod
  without the guard deletes it at start (`vip-loopback.sh`), or an operator does
  (`nft delete table inet route_guard`).
- A worker target naming a VIP its proxy node holds, internal or external, leaves the external
  socket for a local address. With §11's chain on it is refused at the node with no exemption: the
  send fails, counted as a send failure, and its sender gives up on its own timers. With it off it
  is delivered locally and the proxy answers it 482 Loop Detected, its source being the proxy's own
  address: the attempt fails at once and each reroute to that target costs one attempt
  (`failover-harness`'s `dual_face::a_route_naming_the_proxy_*`). The guard compares the source
  with each face's bound address and its IP-literal advertise only: a face bound to `0.0.0.0` whose
  advertise is a name or a NAT address is not covered, and a loop through it runs until
  Max-Forwards is spent (483). A call looping back through the
  platform reaches it through a hop on the external plane, as `b2bua-harness`'s
  `spiral_through_the_front_proxy` states it.
- Silencing a next hop cuts the node's egress to callers through it (§10's chain of timers): the
  node's own traffic has no other way out.
- A next hop that answers ARP and forwards nothing reads alive to the face: its BFD session
  withdraws the routes through it on the routers, and the node's own egress keeps using it.
- An interface whose operstate stays down longer than the flap (a driver or the kernel's link
  event rate limit holding it) gives the second reading, and the node withdraws for that flap.
- A link down for under one reading period, again and again (a bad optic down 400 ms every second),
  never withdraws: the proxy stays fit, the external BGP session resets at each flap, and the
  internal VIP stays announced, so the node keeps drawing workers' finals that leave over that
  link.
- A carrier flap holds the face only when the first ARP retry after it is answered. A router port
  that forwards more than about 250 ms after the carrier returns (LACP, STP) leaves the entry
  `INCOMPLETE` across two readings: the face latches, the node withdraws, and the solicit brings it
  back.
- A bond keeps its carrier up while one member lives: the face reads the bond and sees a lost
  member only through the next hop it served.
