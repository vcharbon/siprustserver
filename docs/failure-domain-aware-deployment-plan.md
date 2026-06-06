# Implementation plan — Failure-domain-aware placement & backup selection

> **STATUS: NOT YET IMPLEMENTED — design only.**
> This plan turns **ADR-0015** into code + manifests. **Nothing described here
> exists in the tree yet.** Every file path is a *target*; every struct field,
> env var, and manifest is what will be *added*, not what is. Do not cite this as
> current behaviour. The current behaviour is: zone-blind `w_bak` selection, a
> single un-anti-affined worker StatefulSet on 2 `tier=app` nodes, multicast
> VRRP, no PodDisruptionBudget.

**Design source:** [`docs/adr/0015-failure-domain-aware-placement-and-backup.md`](adr/0015-failure-domain-aware-placement-and-backup.md)
**Glossary:** `CONTEXT.md` → **Failure domain**, **Distinct-zone backup**.

---

## Strong assumptions (must hold, else the plan is wrong)

These are load-bearing. If any is false on the target infra, revisit the ADR
before coding.

1. **Single backup picker.** The b2bua never recomputes the backup — it echoes
   the proxy's `w_bak` onto `topology.bak` (`b2bua/src/store/mod.rs::backup_of`).
   The entire distinct-zone constraint therefore lives in **one** function
   (`encode_stickiness`). *If a future change makes the b2bua re-derive HRW, this
   plan breaks silently.*
2. **Workers are stateless (no PVC).** Pods are zone-mobile, which is *why* D3
   pins zone via per-zone StatefulSets. If volumeClaimTemplates are ever added,
   zone pinning would come from PV affinity instead and D3 changes.
3. **EndpointSlice carries topology.** `discovery.k8s.io/v1` `Endpoint` exposes
   `zone` (populated by the EndpointSlice controller from the node's
   `topology.kubernetes.io/zone` label) and `node_name`. **Verify the
   `k8s-openapi` version in `Cargo.lock` exposes `Endpoint.zone` /
   `Endpoint.node_name`** before coding D1 — if not, bump the crate.
4. **Identity is the pod name, everywhere.** `B2BUA_ORDINAL` = StatefulSet pod
   name = EndpointSlice `targetRef.name` = proxy cookie `w_pri`/`w_bak` =
   membership `Peer.ordinal` = callRef primary. Per-zone StatefulSets change pod
   names to `b2bua-worker-z1-0` etc.; this invariant must continue to hold across
   all of them (it does — they still get their pod name as ordinal).
5. **Racks are L3-separated.** The whole reason for unicast VRRP (D5). If the two
   proxy racks happen to share an L2 broadcast domain, multicast would still work
   — but we standardise on unicast so the design is correct regardless.
6. **≥3 zones with N+1 headroom (D6).** Selection is zone-count-agnostic, but the
   degraded-fallback rarity and the 1.5×-survivor capacity math assume 3 zones at
   ≤66% steady-state.

---

## Impact summary

| Area | Impact | Risk |
|---|---|---|
| `topology` crate | New `Peer.failure_domain` field; `peers_from_slices` reads `zone`/`node_name`. Consumed by both proxy and b2bua. | **Low** — additive field; existing tests pass an explicit domain or `None`. |
| `sip-proxy` | `WorkerEntry.failure_domain`; `encode_stickiness` gains a foreign-domain filter + degraded fallback + metric. **Changes which worker becomes `w_bak`** for new dialogs. | **Medium** — alters backup placement; cookie *format* unchanged (still `w_pri`/`w_bak` ids), so in-flight dialogs are unaffected. |
| `b2bua` | Periodic self-scan emitting the degraded metric. No selection change. | **Low** — read-only observability. |
| Manifests | `20-worker.yaml` split into 3 zoned StatefulSets; new PDB; proxy anti-affinity key + unicast keepalived + nodeSelector; `cluster.yaml` 6 zoned nodes. | **High (operational)** — deployment topology change; needs a fresh `run.sh up`. No data migration (early project). |
| Wire / storage | **None.** Cookie fields, callRef, replication frames, `topology.bak` semantics all unchanged. | — |

**Rollback:** revert the manifest split (back to one StatefulSet, hostname proxy
anti-affinity, multicast) and the `encode_stickiness` filter; the `Peer`/
`WorkerEntry` field is harmless if left (defaults to `None` → filter is a no-op,
i.e. today's behaviour). No persisted state to undo.

**Performance:** `encode_stickiness` gains one extra filter pass over the
(small) alive set per new dialog — negligible. No change to the in-dialog hot
path (`decode_stickiness` is untouched). Per-zone StatefulSets add no runtime
cost. Global PDB `maxUnavailable:1` makes cluster upgrades **slower** (serial,
readiness-gated) by design.

---

## Implementation slices (ordered; each independently testable)

### Slice 1 — `topology`: carry the failure domain

- `crates/topology/src/lib.rs`: add `pub failure_domain: Option<String>` to
  [`Peer`](../crates/topology/src/lib.rs#L51); add a `Peer::with_domain(...)`
  ctor; keep `Peer::new` (domain `None`) for the sim/test seams.
- `crates/topology/src/k8s.rs`: in
  [`peers_from_slices`](../crates/topology/src/k8s.rs#L110), read the domain per
  endpoint per a configured key:
  - key = `topology.kubernetes.io/zone` → `ep.zone.clone()`
  - key = `kubernetes.io/hostname` → `ep.node_name.clone()`
  - fall back `zone` → `node_name` → `None`.
  Thread the key in from the informer config (env `FAILURE_DOMAIN_KEY`, default
  `topology.kubernetes.io/zone`).
- **Impact:** additive. `MemberDelta::AddressChanged` should also fire on a
  domain change (rare, but keeps consumers correct).
- **Tests:** extend the synthetic-slice unit tests — a slice with `zone` set
  yields `Peer.failure_domain = Some("z1")`; hostname-key mode reads `node_name`;
  missing both → `None`.

### Slice 2 — `sip-proxy`: distinct-zone `w_bak` + degraded fallback

- `crates/sip-proxy/src/registry/mod.rs`: add `failure_domain: Option<String>` to
  [`WorkerEntry`](../crates/sip-proxy/src/registry/mod.rs#L44); map it from
  `Peer.failure_domain` in `K8sWorkerRegistry` (ADR-0012 D4).
- `crates/sip-proxy/src/strategies/load_balancer.rs::encode_stickiness`
  ([line ~268](../crates/sip-proxy/src/strategies/load_balancer.rs#L268)):
  - partition alive-`!= primary` candidates into **foreign-domain** and
    **same-domain**.
  - HRW over foreign-domain first; if empty, HRW over same-domain and increment a
    `sip_proxy_degraded_backup_total{reason="no_foreign_domain"}` counter.
  - if the primary's domain is `None` (degenerate cluster), behave as today
    (no filter) — never *worse* than current.
- **Impact:** changes `w_bak` for *new* dialogs only. `decode_stickiness`,
  ACK/CANCEL exemption, fresh-pod guard, drain grace all unchanged.
- **Tests:** unit — primary in z1 with alive z2/z3 ⇒ `w_bak` is z2 or z3, never
  z1; only-z1-alive ⇒ same-zone `w_bak` + counter bumped; `None`-domain ⇒
  identical to the pre-change golden.

### Slice 3 — `b2bua`: drift safety-net metric

- A periodic task (reuse the keepalive/reaper cadence) scanning calls this node is
  primary of: resolve `topology.bak`'s `Peer.failure_domain` from the shared
  membership; if it equals self's domain, emit
  `b2bua_degraded_backup{reason="drift"}`. (D3 makes reschedule-drift impossible,
  so this should read 0 — it's a watchdog for an unpinned/mislabelled cluster.)
- **Impact:** read-only. **Tests:** inject a same-domain `topology.bak` →
  gauge = 1.

### Slice 4 — manifests: zoned StatefulSets + PDB + proxy

- `deploy/k8s/manifests/20-worker.yaml` → **three** StatefulSets
  `b2bua-worker-z{1,2,3}`, each:
  - `nodeAffinity: requiredDuringScheduling` on
    `topology.kubernetes.io/zone In [zN]`.
  - same `serviceName: b2bua-worker`, label `app=b2bua-worker`, identical env
    except none zone-specific (zone comes from the node, read via EndpointSlice).
  - keep the headless Service as-is (selects all three).
- New `deploy/k8s/manifests/22-worker-pdb.yaml`:
  `PodDisruptionBudget{ selector app=b2bua-worker, maxUnavailable: 1 }`.
  **Single, global — not per-zone** (per-zone reintroduces acute pair loss, D4).
- `deploy/k8s/manifests/30-proxy.yaml`:
  - `podAntiAffinity.topologyKey` → `topology.kubernetes.io/zone`.
  - `nodeSelector` → land on 2 worker-zone nodes (drop dedicated `tier=edge`).
  - keepalived ConfigMap → **unicast**: `unicast_src_ip` = own node IP,
    `unicast_peer` = the other proxy's node IP (inject via env/downward API, or a
    small init that resolves the peer from the proxy EndpointSlice).
- **Impact:** operational — requires `run.sh up` on the new `cluster.yaml`.
- `FAILURE_DOMAIN_KEY` env wired into both proxy and worker runners (default
  `topology.kubernetes.io/zone`).

### Slice 5 — failover test matrix: kill-whole-zone

- Add a `kill_zone` / `drain_zone` injection to the failover harness (ADR-0013):
  drain or kill every worker in one zone at a safe-point; assert **transparency**
  for distinct-zone calls and `degraded_backup` only for any same-zone fallback.
- New callflows get zone coverage for free by declaring safe-points (ADR-0013).

---

## Target kind implementation

The kind cluster exercises the **same** `endpoint.zone` code path as prod by
labelling nodes with real `topology.kubernetes.io/zone` values (kind has no cloud
zones, so we assign them). The `hostname` override is **not** used here — it
stays a config knob for degenerate clusters.

### `deploy/k8s/cluster.yaml` (target)

- **control-plane** ×1.
- **app** ×6 — `tier: app` **and** `topology.kubernetes.io/zone: z1|z2|z3`,
  **2 nodes per zone**. Hosts the three zoned StatefulSets (2 workers/zone).
- **proxies** — co-located on 2 of the app nodes in 2 distinct zones (no separate
  `tier=edge` nodes). The proxy `nodeSelector` + zone anti-affinity place them.
- **load** ×2 — `tier: load` (unchanged; keeps SIPp generators off the SUT
  nodes; one carries the `30060→5060` port map).

> **WSL2 memory caveat.** 6 app + 2 load + control-plane ≈ 9 kind nodes is
> heavier than today's cluster — see the endurance OOM history
> (`deploy/observability/.../cap-kind-memory.sh`, MEMORY: *endurance+chaos
> suite*). Run `cap-kind-memory.sh` first; consider 1 load node if memory is
> tight. A smaller "mechanism-only" variant (3 zones × 1 node = 3 app nodes) can
> validate distinct-zone selection + zone-kill without the 2-per-zone density.

### Validation on kind

1. `deploy/k8s/run.sh up` on the new cluster.yaml; confirm 3 zoned StatefulSets
   Ready and `kubectl get pods -o wide` shows 2 workers per zone.
2. **Selection:** place calls; inspect the stickiness cookie / proxy metrics —
   `w_bak` is always in a different zone than `w_pri`;
   `sip_proxy_degraded_backup_total` stays 0.
3. **Zone kill (D2/D3):** `chaos.sh` kills both workers in z1; assert distinct-zone
   calls survive (transparent failover), z1 workers reboot **into z1** (pinned),
   reclaim, handback; degraded counter stays 0.
4. **Serial upgrade (D4):** trigger a rolling restart; assert the PDB serialises it
   (one worker NotReady at a time) and no call is lost.
5. **Cross-zone VIP (D5):** kill the VIP-master proxy; assert the standby (in the
   other zone) claims the VIP via **unicast** adverts within <2s and the 90%
   success bar holds.

---

## Open items to confirm during implementation

- `k8s-openapi` exposes `Endpoint.zone` / `Endpoint.node_name` (assumption 3).
- Proxy peer-IP discovery for `unicast_peer` — env vs init-container resolve vs
  reuse the proxy EndpointSlice informer. (Smallest: downward-API node IP + the
  other proxy's node IP from the 2-endpoint proxy slice.)
- Whether `MemberDelta::AddressChanged` is the right delta for a pure
  domain-label change, or a new `DomainChanged` variant is cleaner.
