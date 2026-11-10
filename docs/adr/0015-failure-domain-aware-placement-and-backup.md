# 0015: Failure-domain-aware placement and backup selection. Zone-pinned workers, single-picker distinct-zone backup, unified exclusion key

> **D5 is superseded by ADR-0056** (the front proxy's VIPs are announced by BGP).
>
> **Not implemented, a target design.** The SUT chart (`deploy/helm/siprust`) implements
> only D4's budget (one worker and one proxy PodDisruptionBudget, `maxUnavailable: 1`). It
> runs a single worker StatefulSet with `kubernetes.io/hostname` anti-affinity, no zone
> labels and no failure-domain key. Treat every decision below but D4 as planned, not live.

**Status:** accepted as design; deferred until more than two workers spread over zones. With
two workers a call's two copies sit on the two workers, and a hard node anti-affinity of the
workers (a deployment value) is the failure-domain separation. The worker and proxy
PodDisruptionBudgets are deployed (`templates/disruption-budgets.yaml`).

The rationale, the rejected per-zone PDB, the superseded D5 and the chart changes are in the
[appendix](appendix/0015-failure-domain-aware-placement-and-backup.md).

## Context

With more B2BUA than worker nodes, several workers share a node, so a hard "every worker on
its own node" rule cannot schedule. Failover protects a call only if its primary and backup
sit in different failure domains, and the proxy's `w_bak` (HRW second-best among all alive
workers but the primary) is zone-blind. The asked-for generic exclusion scheme is one
configurable topology key reused at every enforcement point, not a rule DSL. See
**Failure domain** and **Distinct-zone backup** in `CONTEXT.md`.
[Detail](appendix/0015-failure-domain-aware-placement-and-backup.md#context).

## Decision D1. Failure domain = one configurable topology key, read from EndpointSlice-native fields

A failure domain is the blast radius across which a primary and its backup must be split. One
configurable k8s topology key names it: `topology.kubernetes.io/zone` by default (a rack,
chosen for LAN and network-partition isolation), `kubernetes.io/hostname` for a cluster with
no zone label. The shared `topology::K8sMembership` informer (ADR-0011 X7, ADR-0012 D4)
reads each worker's domain off its EndpointSlice endpoint's `zone` (or `nodeName`) field into
`Peer.failure_domain`, then `WorkerEntry`. It needs no new RBAC. Selection is
zone-count-agnostic.
[Detail](appendix/0015-failure-domain-aware-placement-and-backup.md#decision-d1-failure-domain--one-configurable-topology-key-read-from-endpointslice-native-fields).

## Decision D2. Distinct-zone backup is enforced in exactly ONE place: the proxy's `w_bak` selection

The b2bua does not recompute HRW. It echoes `w_bak` onto `topology.bak` at INVITE, and
forward replication puts the Element where `w_bak` names. The proxy's `encode_stickiness`
filters HRW second-best candidates to a foreign domain, so the Element lands there by
construction. The choice is frozen at INVITE for the call's life, which D3 makes safe. With no
alive foreign-domain worker, the proxy falls back to a same-domain backup and emits
`b2bua_degraded_backup`.
[Detail](appendix/0015-failure-domain-aware-placement-and-backup.md#decision-d2-distinct-zone-backup-is-enforced-in-exactly-one-place-the-proxys-w_bak-selection).

## Decision D3. Zone-pinned worker grouping: one StatefulSet per zone, shared headless Service

Workers have no PVC, so under one StatefulSet a rebooted pod could land in its backup's zone.
Zone is part of identity: one StatefulSet per zone (`b2bua-worker-z1`, `-z2`, `-z3`), each
with hard `nodeAffinity` to its zone's nodes, all with `app=b2bua-worker` and the same
`serviceName`. The one headless Service enumerates them as a single flat pool. Pod names stay
the ordinals. A rebooted primary returns to its own zone.
[Detail](appendix/0015-failure-domain-aware-placement-and-backup.md#decision-d3-zone-pinned-worker-grouping-one-statefulset-per-zone-shared-headless-service).

## Decision D4. Voluntary disruption bounded by a SINGLE global PDB, `maxUnavailable: 1`

One `PodDisruptionBudget` selects `app=b2bua-worker` across every StatefulSet, with
`maxUnavailable: 1`. The readiness probe gates on re-hydration, so an upgrade evicts one
worker and waits for it to be Ready before the next. It cannot evict a backup while its
primary reclaims, which matters under ADR-0014 reactive-only takeover. A per-zone PDB is
rejected: it lets a primary and its backup in two zones be evicted together.
[Detail](appendix/0015-failure-domain-aware-placement-and-backup.md#decision-d4-voluntary-disruption-bounded-by-a-single-global-pdb-maxunavailable-1).

## Decision D5. The front proxy is unified on the SAME failure-domain key; cross-rack VIP needs unicast VRRP

Superseded by [ADR-0056](0056-the-front-proxys-addresses-are-announced-by-bgp.md): each
proxy's node announces the VIPs over BGP, so the proxies need no shared layer 2.
[Detail](appendix/0015-failure-domain-aware-placement-and-backup.md#decision-d5-the-front-proxy-is-unified-on-the-same-failure-domain-key-cross-rack-vip-needs-unicast-vrrp).

## Decision D6. N+1 capacity: each zone sized to absorb a sustained full-zone loss

A dead zone's backups spread over the other two zones, so each survivor carries about 1.5×
its steady state. Each zone is sized to at most about 66 % steady state, so a full-zone loss
causes no new-call rejection, at the cost of about 50 % idle capacity.
[Detail](appendix/0015-failure-domain-aware-placement-and-backup.md#decision-d6-n1-capacity-each-zone-sized-to-absorb-a-sustained-full-zone-loss).

## Consequences

`topology`, `sip-proxy` and `b2bua` carry the failure domain, the filtered backup pick and a
periodic self-scan that emits the degraded metric. HRW, callRef routing, repl addressing and
the single-picker invariant do not change. The chart gets zone-pinned StatefulSets and one
global PDB, and kind labels six app nodes into three zones.
[Detail](appendix/0015-failure-domain-aware-placement-and-backup.md#consequences).

## Deferred (with justification)

- Mid-call backup re-pin: unnecessary under D3, and it would break D2's single picker.
- More than three zones: a deployment scale-out, no code change.
- Per-zone capacity autoscaling: D6 is static N+1.

[Detail](appendix/0015-failure-domain-aware-placement-and-backup.md#deferred-with-justification).

## Test

- The failover test matrix (ADR-0013) gains a kill-whole-zone injection, asserting
  transparency for distinct-zone calls and the degraded metric for a same-zone fallback.
- kind endurance and chaos runs on the 3-zone topology validate D3 and D4 (zone-pinned
  reclaim, serial PDB upgrade).

## References

- ADR-0009, ADR-0011 X7, ADR-0012 D4, ADR-0013, ADR-0014, ADR-0056.
- `CONTEXT.md`: **Failure domain**, **Distinct-zone backup**.
- `crates/topology/src/{lib,k8s}.rs`, `crates/sip-proxy/src/strategies/load_balancer.rs`,
  `crates/b2bua/src/store/mod.rs`, `deploy/k8s/cluster.yaml`, `deploy/helm/siprust/`.
