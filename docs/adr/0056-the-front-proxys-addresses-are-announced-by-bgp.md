# The front proxy's addresses are announced by BGP

**Status:** accepted (2026-11-03). Supersedes ADR-0012 D7, ADR-0015 D5, ADR-0046 D4 and the
virtual-address check of ADR-0050 §11; amends ADR-0046 (a total backend outage), ADR-0009,
ADR-0033 and ADR-0048.

**Owner:** `deploy/helm/siprust/templates/proxy-announce.yaml` (the announce-only Services),
`templates/proxy-metallb.yaml` (the MetalLB objects), `templates/proxy.yaml` (readiness probe,
`minReadySeconds`, anti-affinity), `files/vip-loopback.sh` (the VIPs on `lo`, the ICMP drop),
`templates/checks.yaml` (the timing relations), `crates/sip-proxy-runner/src/readiness.rs` and
`external_face.rs` (what `/readyz` reads), `deploy/k8s/tor/` and `deploy/helm/lib/tor.sh` (the
lab router).

## Context

The two front proxies were an active/passive pair behind one VRRP address. VRRP needs both
proxies in one layer-2 domain, so they cannot sit in different racks, which is the proxy's own
failure-domain separation. Its health coupling also left gaps: the address holder ignored
worker reachability, the address-release hook was not ordered before the proxy's own SIGTERM,
and the passive proxy needed a notice path of its own. Production networks route to top-of-rack
routers that speak BGP, and the proxy is stateless by default with a memo for the exceptions
(ADR-0009 X7), so either proxy can carry any flow.

## Decision

1. **Each node announces the VIPs while its proxy is Ready.** The proxy runs on the host
   network and binds both VIPs, which its node holds on `lo` for good (`vip-loopback.sh`, strict
   ARP): Via, Record-Route and every source address stay the VIP, and ADR-0048's trust by
   ingress face is unchanged. Per VIP the chart renders one `LoadBalancer` Service that carries
   no SIP port: one TCP port with no listener on a proxy node (never the metrics port, refused
   at render), `externalTrafficPolicy: Local`, `allocateLoadBalancerNodePorts: false`, the VIP
   pinned by `metallb.io/loadBalancerIPs`, the proxy pods as endpoints. MetalLB in BGP mode
   (frr-k8s backend) announces a Local Service only from nodes with an endpoint that can serve
   (Ready, or Serving while it terminates), so each VIP's route follows `/readyz`. The routers
   spread flows over the announcing nodes (ECMP): the pair is active-active.
2. **kube-proxy runs in iptables or nftables mode.** Both match a LoadBalancer address per
   protocol and port, so VIP:5060/UDP reaches the proxy's socket untouched. IPVS binds every
   LoadBalancer address to every node and its filter refuses the ports a Service does not list:
   a worker's INVITE to the internal VIP would be refused on its own node. IPVS is unsupported; a
   kube-proxy replacement is checked for the same property before use. kube-proxy serves the
   Services' `healthCheckNodePort` on every node.
3. **`/readyz` is the one signal.** Ready means: no drain latched, no recv shard stalled, the
   first worker view in, at least one worker `Alive`, and the external face usable: the external
   interface present and up and at least one of its configured next hops resolved (`REACHABLE`,
   `STALE`, `DELAY`, `PROBE`, `PERMANENT`) in the kernel's neighbour table (`PROXY_EXT_INTERFACE`,
   `PROXY_EXT_NEXT_HOPS`). A next hop read `FAILED` or `INCOMPLETE` latches the face unusable until
   one reads resolved again, so an entry flapping between `INCOMPLETE` and gone never reads usable.
   The next hops are the external plane's BGP peers (checked at render when the chart renders
   MetalLB's objects), whose BFD and keepalive traffic keeps them resolved. That is a read of local
   kernel state, every `PROXY_EXT_FACE_CHECK_MS` off the async runtime; nothing is probed on the
   wire. It ties the two faces together: a node whose
   caller-facing uplink is gone stops announcing the internal VIP as well. No component detects
   the reachability of a peer beyond the first router: a fault in the routed network hits both
   proxies alike and is the network's to converge. Consequences:
   - a proxy with no live worker answers a new call `503` with a `Retry-After` until its route is
     withdrawn, so the caller may retry elsewhere at once; a short loss is answered rather than
     left to time out;
   - a proxy that loses every worker, or its external uplink, while its peer does not, withdraws
     both VIPs and the peer carries all traffic;
   - **a total backend outage withdraws both routes**: once both are withdrawn the callers see
     an unreachable address. This amends ADR-0046's fail-loud rule for that one case;
   - **with the API server down** EndpointSlices cannot change: a crashed proxy is not withdrawn,
     a proxy restarted then gets no first worker view yet its node stays announced for the whole
     outage, and a speaker restarted then withdraws every route it announced. BFD still covers a
     node's death.
4. **The drain outlasts the withdrawal.** On SIGTERM the proxy turns `/readyz` Draining and keeps
   forwarding for `PROXY_DRAIN_GRACE_MS`. The withdrawal is readiness detection
   (`readinessProbe.periodSeconds` × `failureThreshold`, 1 s × 2) then the propagation to the
   routers' tables (EndpointSlice, speaker, frr-k8s reload, BGP UPDATE, router FIB:
   `proxy.announce.propagationMs`). The Deployment's `minReadySeconds` covers the announcement of
   a replaced pod, so a rollout stops the next proxy only once the previous one is back in the
   routers' tables. `templates/checks.yaml` refuses a drain grace at or below detection plus
   propagation, and a `minReadySeconds` at or below one probe period plus propagation. The lab
   measures them with the `proxy_rollout` incident: with kube-proxy in iptables mode the
   drain-to-withdrawal reached 7.29 s and the Ready-to-announced 6.7 s, so the propagation is
   7 s, the drain grace 13 s and `minReadySeconds` 12 s, pending the nftables run
   (`docs/TUNING.md`).
5. **No ICMP port unreachable from a VIP.** Until its route is withdrawn a node still receives a
   VIP's traffic while no proxy socket is bound (a crash, the end of a drain); its kernel would
   answer port unreachable, which a SIP client takes as a transport error (RFC 3261 §18.4)
   instead of retransmitting into the rehashed path. The proxy nodes drop outbound ICMP port
   unreachable sourced from the VIPs.
6. **BFD, no BGP graceful restart.** Each peer session runs BFD (300 ms × 3 by default), so a
   node's death is withdrawn within about a second. Graceful restart would keep a dead node's
   routes after its BFD session went down; it stays off. A restart of the speaker or of frr-k8s
   on a node ends its sessions, withdraws the node and rehashes its flows.
7. **No worker runs on a node that runs or ran a proxy.** The VIPs stay on a proxy node's `lo`
   whatever the routers say, after its proxy is gone too, so a worker there would reach that node
   instead of the announcing ones. The chart requires `proxy.nodeSelector` and gives the workers
   a required node affinity off every node it names (one `NotIn` term per label, ANDed into the
   workers' own terms); the proxy pods also carry a required anti-affinity to the worker pods.
8. **What ECMP does to flows.** One peer address and port is one flow, so one proxy carries all
   of a single-flow peer's traffic and each proxy is sized for all of it (`PROXY_*` budgets stay
   per proxy, ADR-0046, ADR-0009). Removing a next hop moves only its flows; adding one moves
   about half of them with hash-threshold multipath, few with resilient hashing. Production
   routers hash on layer 4 and preferably resiliently. The transactions a move splits across the
   proxies are ADR-0009 X7's.
9. **The MetalLB objects ship with the chart, behind a value.** With
   `proxy.announce.metallb.enabled` the chart renders, in MetalLB's namespace, one /32
   `IPAddressPool` per VIP (`autoAssign: false`), one `BGPAdvertisement` per pool to that face's
   peers only, one `BGPPeer` per router selected by `proxy.nodeSelector`, and one `BFDProfile`;
   a render without MetalLB's APIs fails naming them. Off, the operator keeps them.

## Rejected

- **SIP through the Service.** kube-proxy would source some outbound requests from the node's
  address and both faces would collapse onto one node address.
- **An FRR or GoBGP sidecar in the proxy pod.** A second session from the node's address to the
  same router collides with the cluster's own speaker.
- **Active/passive announcement** (one node preferred by AS-path or community). It needs routing
  policy on the routers, outside the chart, and the flow moves of ADR-0009 X7 happen at failover
  anyway; active-active also spreads load across the peers.
- **Keeping the layer-2 virtual address** as a fallback: no second mechanism is kept.

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
- A withdrawal's propagation bounds the window in which a lost node still draws traffic; the
  hakit budgets `proxy-failover` and `proxy-reroute` derive their allowances from it.
