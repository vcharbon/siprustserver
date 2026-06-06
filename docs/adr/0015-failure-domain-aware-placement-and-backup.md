# 0015 — Failure-domain-aware placement & backup selection: zone-pinned workers, single-picker distinct-zone backup, unified exclusion key

**Status:** accepted (2026-06-05)

**Source:** this codebase (no sipjsserver counterpart — the source's 2-worker
deployment never scaled past a pair, so "primary and backup on distinct nodes"
held by arithmetic, not by rule). Triggered by the move to **>2 B2BUA** on target
infra where the count of B2BUA exceeds the count of worker nodes, so naive
anti-affinity ("no two on a node") is unsatisfiable, and the proxy's zone-blind
`w_bak` selection could place a backup in the **same failure domain** as its
primary — defeating failover.

## Context

Two deployment-time facts drive everything here:

1. **More B2BUA than worker nodes** ⇒ multiple workers per node, so **full
   exclusion is impossible**. A *hard* "every worker on its own node" rule cannot
   schedule.
2. **Failover only protects a call if its primary and backup occupy different
   failure domains.** Today nothing enforces this: the proxy's `w_bak` is the
   HRW-2nd-best among *all* alive workers `!= primary`
   (`sip-proxy/.../load_balancer.rs::encode_stickiness`), completely zone-blind,
   and a `Peer`/`WorkerEntry` carries no zone at all.

The asked-for "generic way to write exclusion rules" is **not** a rule DSL — it is
**one configurable topology key** reused at every enforcement point. See the
**Failure domain** / **Distinct-zone backup** glossary entries in `CONTEXT.md`.

## Decision D1 — failure domain = one configurable topology key, read from EndpointSlice-native fields

A **failure domain** is the blast radius across which a primary and its backup
must be split. It is named by a single configurable k8s topology key:

- **`topology.kubernetes.io/zone`** by default — a *rack*, chosen for **LAN /
  network-partition isolation**, not power/cooling. (Cross-rack within one DC
  leaf-spine fabric is <1 ms — LAN-like — and SIP correctness tolerates the 1–2 ms
  even a cloud-AZ split would add, since T1 is 500 ms and replication is async. The
  domain choice is therefore driven by *blast radius*, not latency.)
- **`kubernetes.io/hostname`** as an override for degenerate clusters with no zone
  label.

Each worker's domain is read off its **`EndpointSlice` endpoint's native `zone`
(or `nodeName`) field** by the *one* `topology::K8sMembership` informer the proxy
and b2bua already share (ADR-0011 X7 / ADR-0012 D4). **No new RBAC, no node-`get`,
no downward-API hack** — the informer already watches the slices. Carried as a new
`Peer.failure_domain` → `WorkerEntry`. The selection *code* is **zone-count-
agnostic** ("exclude same domain"); the number of zones is a deployment fact, so
the same binary runs on kind-with-3-zones and any future N-zone infra.

## Decision D2 — distinct-zone backup is enforced in exactly ONE place: the proxy's `w_bak` selection

There is a single backup picker. The b2bua **does not** recompute HRW — it
**echoes `w_bak`** onto `topology.bak` at INVITE
(`b2bua/.../store/mod.rs::backup_of`), and forward-replication puts the **Element**
wherever `w_bak` named. So the distinct-zone constraint goes in **one** place —
the proxy's `encode_stickiness`, filtering HRW-2nd-best candidates to a **foreign
domain** — and the stored Element lands in a foreign domain *by construction*. No
b2bua/repl change, no divergence possible.

The choice is **frozen at INVITE** for the call's life. This is **safe** only
because of D3 (zone-pinned workers): a rebooted primary always returns to its own
domain, so it can never drift into its backup's.

**Degraded fallback.** When no alive foreign-domain worker exists, fall back to a
**same-domain** backup and emit a `b2bua_degraded_backup` metric. A same-domain
backup still survives the common case (process crash / pod restart / OOM); only a
domain partition is unprotected. With ≥3 zones this fallback triggers only if
*every other zone is dead*. A periodic b2bua self-scan (own zone vs
`topology.bak`'s zone) re-emits the metric as a safety net even though D3 makes
reschedule-drift impossible.

## Decision D3 — zone-pinned worker grouping: one StatefulSet per zone, shared headless Service

Workers have **no PVC** (replication is in-memory/TCP), so under a single
StatefulSet a rebooted pod is **zone-mobile** — during a rolling node upgrade the
scheduler could place a rebooted primary into its frozen backup's zone, silently
violating D2 for that call's remaining life (co-location *drift*). A soft
`topologySpreadConstraint` does not fix this: it balances *counts per zone*, not
*this pair*.

So zone becomes part of **identity**: **one StatefulSet per zone**
(`b2bua-worker-z1` / `-z2` / `-z3`), each with **hard `nodeAffinity`** to that
zone's nodes. All carry `app=b2bua-worker` and the **same `serviceName`**, so the
one headless Service's EndpointSlices enumerate them as a single flat pool — the
informer, proxy HRW, callRef routing, and repl addressing are unchanged (they see
more ordinals, nothing else). Pod names (`b2bua-worker-z1-0`, …) remain the
ordinals = `B2BUA_ORDINAL` = callRef primary = membership identity.

Consequence: a rebooted primary is pinned back into its own zone ⇒ **drift is
impossible by construction**, and the soft `topologySpreadConstraint` is **not
needed** (it was only compensating for mobility we no longer have).

## Decision D4 — voluntary disruption bounded by a SINGLE global PDB, `maxUnavailable: 1`

A cluster upgrade rebooting nodes in quick succession could take down a primary
**and** its backup before reclaim completes (acute simultaneous loss) — especially
under ADR-0014 reactive-only takeover, where a quiescent failed-over dialog is
**not** made live on a survivor but waits for the rebooting primary's reclaim, so
the Element-holding backup staying up is the *only* thing protecting it.

The fix is **one `PodDisruptionBudget` selecting `app=b2bua-worker` across all
three StatefulSets, `maxUnavailable: 1`.** Because the worker readiness probe
gates on re-hydration, the upgrade evicts one worker, waits for it to be fully
Ready, then proceeds — k8s counts a draining/not-Ready worker against the budget,
so it **cannot** evict the backup while the primary is still reclaiming. Fully
serial, safe by construction; slower fleet upgrade is the accepted price.

**Rejected: a per-zone PDB** (one per StatefulSet, `maxUnavailable: 1` each). It
permits one pod down in *each* zone simultaneously, so a primary in z1 and its
backup in z2 can be co-evicted ⇒ reintroduces the acute loss. The intuition
inverts under per-zone StatefulSets: the safe PDB is **global**, not per-zone.

## Decision D5 — the front proxy is unified on the SAME failure-domain key; cross-rack VIP needs unicast VRRP

The "generic exclusion scheme" is one key reused everywhere:

| Tier | Mechanism | Key |
|---|---|---|
| Worker backup selection | `w_bak` distinct-domain filter (D2) | the key |
| Worker placement | zone-pinned StatefulSets (D3) | the key |
| Front proxy (2 replicas) | hard `podAntiAffinity` | **the key** (was `hostname`) |

The proxy's `podAntiAffinity.topologyKey` moves from `kubernetes.io/hostname` to
the failure-domain key, so the two proxies land in **2 distinct racks** (else a
rack failure takes out both — asymmetric with worker resiliency). The proxies run
on **2 of the existing worker-zone nodes** — *not* a separate edge tier — so the
zone count stays 3 ("don't multiply the zones").

**Hard consequence: this breaks multicast VRRP.** The keepalived VIP (ADR-0012 D7)
advertises over multicast `224.0.0.18`, which requires the two proxies to share an
**L2 broadcast domain**. Distinct racks are distinct L2 domains joined by L3
routing (the same leaf-spine fabric), so multicast adverts do not cross. Switch
keepalived to **unicast VRRP** (`unicast_peer` targeting each proxy's node IP,
learnable from the same EndpointSlice informer or env). Adverts become L3-routable
unicast; the VIP / Record-Route design (ADR-0012 D7) is otherwise unchanged.

## Decision D6 — N+1 capacity: each zone sized to absorb a sustained full-zone loss

On a sustained full-zone outage, the dead zone's calls' backups are spread across
the other two zones (HRW over both foreign zones), so each survivor sustains
~**1.5×** its steady-state. Size each zone to **≤ ~66 % steady-state** so a
survivor at 1.5× stays ≤ 100 % — a full-zone loss is absorbed with **zero new-call
rejection**. Cost: ~50 % capacity provisioned idle. (ADR-0014 reactive-only
takeover bounds the *transient* spike to only the dialogs that receive an in-dialog
request; the 1.5× is the sustained-outage figure.)

## Consequences

**Code:**
- `topology`: `Peer.failure_domain` field; `k8s.rs` reads `endpoint.zone` (fallback
  `endpoint.nodeName`); the configurable key selects which.
- `sip-proxy`: `WorkerEntry.failure_domain`; `encode_stickiness` filters backup
  candidates to a foreign domain with the same-domain degraded fallback +
  `b2bua_degraded_backup` metric.
- `b2bua`: periodic self-scan emitting the degraded metric (safety net for D2).
- No change to HRW, callRef routing, repl addressing, or the single-picker
  invariant.

**Manifests:**
- `20-worker.yaml` → three zone-pinned StatefulSets sharing one headless Service,
  each with hard `nodeAffinity` to its zone; soft spread removed.
- New `PodDisruptionBudget` (`maxUnavailable: 1`, selector `app=b2bua-worker`).
- `30-proxy.yaml`: anti-affinity `topologyKey` → failure-domain key; `nodeSelector`
  → 2 worker-zone nodes (no dedicated `tier=edge`); keepalived.conf → unicast
  `unicast_peer`.
- `cluster.yaml` (kind): 6 app nodes labelled into 3 zones (2 each) with real
  `topology.kubernetes.io/zone` labels — exercises the **same** `endpoint.zone`
  path as prod; proxies co-locate on 2 of them; unicast peer IPs configured.

## Deferred (with justification)

- **Mid-call backup re-pin** — unnecessary: D3 makes zone identity-stable, so a
  primary never drifts into its backup's zone. Would also break the single-picker
  invariant (D2).
- **BGP-advertised VIP (MetalLB/cloud LB)** — the "real" rack/AZ-agnostic answer,
  but a much larger infra change (new component, BGP peering); unicast VRRP (D5) is
  the smallest change that works cross-rack and runs on kind.
- **>3 zones** — no code change needed; selection is zone-count-agnostic and
  placement is one StatefulSet per zone. Purely a deployment scale-out.
- **Per-zone capacity autoscaling** — D6 is static N+1; dynamic right-sizing is a
  later concern.

## Test

- The failover test matrix (ADR-0013) gains a **kill-whole-zone** injection
  (drain/kill every worker in one zone) asserting transparency for distinct-zone
  calls and the degraded metric for any same-zone fallback.
- kind endurance/chaos runs against the 3-zone topology validate D3/D4 (zone-pinned
  reclaim, serial PDB upgrade) and D5 (unicast VIP failover across zones).

## References

- ADR-0009 (proxy HRW + stickiness cookie + fresh-pod guard / drain grace),
  ADR-0011 (HA replication peer-to-peer: X7 one-watcher-both-consumers),
  ADR-0012 (HA addressing & membership: D4 proxy-on-informer, D7 keepalived VRRP
  VIP — superseded here for the cross-rack case), ADR-0013 (failover test matrix),
  ADR-0014 (reactive-only takeover — D4 composition).
- `CONTEXT.md`: **Failure domain**, **Distinct-zone backup**.
- `crates/topology/src/{lib,k8s}.rs`, `crates/sip-proxy/src/strategies/load_balancer.rs`,
  `crates/b2bua/src/store/mod.rs`, `deploy/k8s/{cluster.yaml,manifests/}`.
