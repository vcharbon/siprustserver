# Tuning the deployment

How to change a deployment variable safely: its role, its default and unit,
the relations it keeps with other variables and the check that enforces them,
what to watch, and the trade-off either way. Where a value comes from for a
given deployment is that deployment's own sizing page; this page does not
repeat it. A product built on this stack adds its own rows in its own guide.

Every relation below is checked, never only documented: a worker or proxy
refuses to start on its final configuration and names every variable involved
(`b2bua_runner_kit::ConfigCheck`, the proxy's `ProxyConfig`), and the `siprust`
chart refuses to render (`deploy/helm/siprust/templates/checks.yaml`). A value
set through a component's `extraEnv` escapes the render checks.

## Worked example: the emergency call ceiling

`B2BUA_MAX_CALLS_EMERGENCY` is the live-call count past which every new call,
emergency included, is refused; `B2BUA_MAX_CALLS`, below it, refuses only
normal calls (ADR-0037 item 2). The pair is a protection: by default the
emergency ceiling sits a small margin above the normal one, enough for the
emergency share of the load while normal calls are refused.

- **Relations.** Emergency strictly above normal (worker start). Every peer's
  `B2BUA_REPL_BACKUP_MAX_CALLS` at least this ceiling, so every call a worker
  admits can be backed (worker start, ADR-0037 item 4). The worker's memory
  limit holds a takeover: its own calls up to this ceiling and every backup
  turned live, `limit · (1 − s) ≥ base + (C + B) · call + (N + reserve) · txn`
  (ADR-0037 item 7; checked at render by a product chart that states its
  measured costs).
- **After a worker dies.** Copies turned live are never refused; only new
  calls are. A survivor holding more live calls than its ceilings refuses new
  calls until its count falls under them. With mean holding time `T` and `L`
  calls live after the takeover, normal calls are refused for `T · ln(L / C_n)`
  and every call for `T · ln(L / C_e)`; the survivor then admits `C_n / T`
  normal calls a second.
- **Raising it** gives emergency calls priority through a takeover: set it at
  the count a survivor holds at peak, plus margin. Raise the peers' backup
  ceilings to it, the emergency RSS ceiling to what it implies, and the memory
  limit to the item 7 line with the raised `C`: a takeover can then hold
  `C + B` live calls.
- **Lowering it** toward the normal ceiling narrows the emergency band; it can
  never meet it.
- **Watch** `b2bua_new_calls_total{outcome="rejected"}` by `reason`,
  `b2bua_capacity_level`, `b2bua_active_calls`, and after a takeover
  `b2bua_repl_takeover_hydrated_total`.

## Worker (`b2bua-runner-kit`)

| variable | default | role | relations (check) | watch | raise / lower |
|---|---|---|---|---|---|
| `B2BUA_MAX_CALLS` | off | live calls past which new normal calls are refused (ADR-0037) | < `_EMERGENCY` (start) | `b2bua_new_calls_total{reason}`, `b2bua_active_calls` | raise: more admitted per worker, more memory per item 7; lower: refusals earlier |
| `B2BUA_MAX_CALLS_EMERGENCY` | off | the worked example above | > normal; ≤ every peer's backup ceiling (start) | as above | as above |
| `B2BUA_MAX_TXNS`, `_EMERGENCY` | off | live transactions past which new calls are refused | emergency > normal (start) | `b2bua_capacity_level` | raise with the call ceilings: a lone worker holds about the transactions of the whole rate |
| `B2BUA_MAX_RSS`, `_EMERGENCY` | off | process RSS past which new calls are refused | emergency > normal (start); < `resources.limits.memory` (render); item 7's RSS line | `b2bua_capacity_rss_bytes`, `b2bua_capacity_level` | set at the RSS the count ceilings imply, so counts bind first; raising above item 7's line risks an OOM kill on a takeover |
| `B2BUA_REPL_BACKUP_MAX_CALLS` | off | backups held for peers | ≥ the emergency call ceiling (start) | `b2bua_repl_backup_shed_total`, `b2bua_repl_calls_unbacked` | lower and calls go unbacked (admitted, lost with their worker); raise and item 7 needs more memory |
| `B2BUA_REPL_BACKUP_MAX_RSS` | off | RSS past which a new backup is not stored | ≥ the emergency RSS ceiling (start); < the memory limit (render) | `b2bua_repl_backup_shed_total` | as above |
| `B2BUA_INVITE_TXN_TIMEOUT_SEC` | 158 | the INVITE transaction bound, both call halves | 33..=600; > setup deadline; < `B2BUA_REBOOT_BUDGET_SEC` (start); = the proxy's `PROXY_INVITE_TXN_TIMEOUT_SEC` (render, `inviteTxnTimeoutSec`) | `b2bua_new_calls_total`, CDR causes | raise for long rings, with the proxy's memo cap; lower cuts rings |
| `B2BUA_SETUP_TIMEOUT_MS` | 150000 | the caller's total setup deadline | < the INVITE bound (start) | setup-timeout CDR causes | raise for long rings, under the bound |
| `B2BUA_CALL_CONTROL_TIMEOUT_MS` | 5000 | one decision round trip's deadline (ADR-0022) | < `B2BUA_LOOP_STALL_MS` − 2 s (start); > the proxy's dead verdict `HEALTH_THRESHOLD` × `HEALTH_INTERVAL_MS` + `HEALTH_TIMEOUT_MS` (render) | decision failures on `b2bua_new_calls_total` | lower refuses calls on a slow backend sooner and can undercut a takeover; raise holds callers longer |
| `B2BUA_KEEPALIVE_SEC` | 300 | in-dialog OPTIONS cadence | ≥ 120 (above the probe's Timer F); ≤ `B2BUA_REBOOT_BUDGET_SEC` − Timer F − 10 s (start) | keepalive failures in CDRs | lower detects a dead peer sooner, more traffic and memo entries; raise needs a longer replica TTL |
| `B2BUA_REBOOT_BUDGET_SEC` | 600 | the replica TTL (not a reboot budget, see HA_GUARANTEES.md) | ≥ 60; ≥ keepalive + Timer F + 10 s; > the INVITE bound (start) | `b2bua_store_bodies` | raise keeps quiet calls' replicas longer, more memory after a long outage |
| `B2BUA_CPS_BUCKET_RATE`, `_SIZE` | 500, 1000 | the per-worker new-call rate rung | rate > 0 when size > 0 (start) | `b2bua_new_calls_total{reason="bucket"}` | set to what a lone worker carries |
| `B2BUA_QUEUE` | 8192 | inbound UDP queue, packets | ≥ 256 (start) | `b2bua_udp_queue_depth`, `b2bua_udp_tail_dropped_total` | raise absorbs bursts, longer queueing delay |
| `B2BUA_UDP_INGRESS_BRAKE_PCT` | 70 | queue depth share at which new INVITEs are shed statelessly | 1..=100 (start) | `b2bua_udp_*` brake counters | lower sheds earlier |
| `B2BUA_EVENT_QUEUE_*` | 512 / 64 / 300 / 150 / 50 / 3000 | the event-queue gate (ADR-0050 §9) | accept bounds under refusal bounds, refusal depth ≤ the queue, interval < hold (start) | `b2bua_event_queue_refusing`, `_refusals_total` | see `runner_env::event_queue_env` |
| `B2BUA_LOOP_STALL_MS` | max(10000, decision + 5000) | a loop turn longer than this reads stalled | > `B2BUA_CALL_CONTROL_TIMEOUT_MS` + 2 s (start) | `b2bua_loop_stalled` | lower restarts a slow worker sooner, false stalls on a starved node |
| `B2BUA_DRAIN_GRACE_MS`, `_MIN_MS`, `_RELEASE_FLUSH_MS`, `_CDR_FLUSH_MS` | 5000, 1000, 3000, 3000 | the planned exit (ADR-0031) | floor ≤ grace (start); pod grace > grace + 2 × flush + CDR flush + 2 s (render) | `b2bua_drain_exits_total`, `b2bua_drain_seconds` | raise with `terminationGracePeriodSeconds` |
| `B2BUA_REPL_*_MS` (link) | see `replication/link_env.rs` | replication link liveness (ADR-0045) | idle ≥ 2 × Noop; dead-peer TTL > idle + backoff + connect (start) | `b2bua_repl_serving_flows` | lower idle detects a dead peer sooner, false downs on a loaded link |
| `B2BUA_REPL_OPEN_TIMEOUT_MS` | 4000 | an accepted replication connection that sends no opening request within it is closed (ADR-0045) | ≥ `B2BUA_REPL_IDLE_TIMEOUT_MS` (start): a live node's close then never comes before a confirming peer's own idle end, which would confirm a dead verdict on it | `b2bua_repl_inbound_dropped_total{reason="open_timeout"}` (0 on a healthy cluster) | lower frees a slot a silent client holds sooner, never under the idle timeout; raise costs nothing to the peers |
| `B2BUA_REPL_MAX_CONNECTIONS` | 64 | inbound replication connections served at once; one above it is closed | ≥ 4 (start); ≥ 4 × (`worker.replicas` − 1) (render): every peer's two flows and a reconnect of each | `b2bua_repl_inbound_dropped_total{reason="cap"}` (0 on a healthy cluster), `b2bua_repl_serving_flows` | a connection closed at the cap confirms a dead verdict on this live worker for the peer making it (ADR-0047 D1): the cap is safe only while the workers' NetworkPolicy confines port 9092 to the workers. Lower bounds the tasks a runaway client holds |
| `B2BUA_ADMIN` | 127.0.0.1:9094 | the operator routes' listener (ADR-0050 §10), reached through `kubectl port-forward` | a loopback `ip:port` (start) | — | not a tuning value; a port clash leaves the worker without its operator routes (logged) |
| `B2BUA_REPL_SYNC`, `_SYNC_CAP_MS`, `_ACK_BOUND_MS`, `_CANCEL_BOUND_MS` | applied, 100, 200, 200 | the write path (ADR-0055 §3) | 0 < cap < T1 and the §17.2.1 bound; cap < ACK, CANCEL bounds < T1 (start) | `b2bua_repl_outbox_released_total{reason}` | a longer cap holds more effects for the backup, slower answers |
| `B2BUA_REPL_BOOTSTRAP_HARD_TIMEOUT_MS` | 10000 | a cold flow's bound on reaching its peer: past it a peer never reached stops holding readiness (ADR-0011 X5, ADR-0014 §11); a serving node reads a peer's bootstrap in flight as no slow consumer for as long (ADR-0055 §3); the same value on every worker | > 2 × `B2BUA_REPL_CONNECT_TIMEOUT_MS` + `B2BUA_REPL_BACKOFF_MAX_MS`, a second connect before the peer is read unreachable (start). Readiness after a return waits, per ready peer, `t_reclaim` + `t_backup`: a peer never reached, about T + one connect bound; a peer lost between the two flows, up to 2 × T (the Backup flow opens after a Reclaim flow that took T, then runs its own T); a peer reached whose stream then stalls, as long as the peer stays listed ready (the strict X6 gate) | the worker's readiness transition line, `peer bootstrap complete` lines with `reason` | lower: a slow peer is read unreachable sooner and a worker reads ready without the calls it backs up; raise: a returning worker, and a rollout behind its PodDisruptionBudget, waits longer for a dead peer |
| `B2BUA_DEAD_NOTICE_FRESH_MS`, `B2BUA_DEAD_VERDICT_HOLD_MS` | 10000, 42000 | dead-verdict freshness and hold (ADR-0050 §3) | hold ≥ fresh + Timer F; hold > 2 × idle + lookup + connect of the link (start) | `b2bua_dead_verdicts_taken_total`, `_unconfirmed_total`, `b2bua_dead_notices_refused_total` | a shorter window refuses replays sooner, needs synchronised clocks |
| `LIMITER_REFRESH_SECONDS`, `LIMITER_REFRESH_BATCH_MS` | 40, 1000 | lease refresh cadence and tick | tick < period (start); 2 × period + tick < the limiter lease (render) | `b2bua_limiter_refresh_*`, `limiter_lease_expired_calls_total` | lower: more refresh traffic; raise: needs a longer lease |
| `LIMITER_TIMEOUT_MS`, `_REFRESH_TIMEOUT_MS`, `_RELEASE_TIMEOUT_MS` | 150, 2000, 2000 | request budgets | > 0 (start) | `b2bua_limiter_breaker_open` | a longer admit budget delays setup on a slow limiter, which fails open |
| `SIP_TRACE_HEADER` | off | honour `X-Trace-Sample` | on/off only (start) | — | a lab setting: the header is untrusted at a serving edge |

## Front proxy (`sip-proxy-runner`)

| variable | default | role | relations (check) | watch | raise / lower |
|---|---|---|---|---|---|
| `HEALTH_INTERVAL_MS`, `HEALTH_TIMEOUT_MS`, `HEALTH_THRESHOLD` | 1000, 1600, 2 | the worker probe; the dead verdict falls one cycle to `THRESHOLD × INTERVAL + TIMEOUT` (3.6 s) after a death | payload staleness ≥ 2 probe cycles; AIMD cooldown ≥ one cycle (start); the workers' decision deadline > the verdict bound (render) | `sip_proxy_worker_pool_empty`, worker health gauges | shorter: faster takeover, more false verdicts under load |
| `PROXY_RECV_SHARDS` | 1 | serial recv loops | 1..1024 (start) | `sip_proxy_recv_shards_stalled`, `sip_proxy_udp_queue_depth` | one per ~150 calls/s of a heavy call mix, one core each |
| `PROXY_SELF_GATE_CPS_RATE`, `_SIZE` | 100, 50 | the proxy's own intake rate | the branch memo holds the rate's working set (start) | `sip_proxy_self_external_invites_rejected_total`, `sip_proxy_self_cps_bucket_level` | raise with the memo; lower sheds at the proxy |
| `PROXY_BRANCH_MEMO_MAX_BYTES`, `_MAX_ENTRIES`, `_MEAN_RING_MS` | 192 MiB, 78 643, 30000 | the full CANCEL/ACK/retransmission memo (ADR-0009 X7) | holds `SELF_GATE_CPS_RATE` (start); bytes < the pod's memory limit (render) | `sip_proxy_branch_memo_bytes`, `sip_proxy_branch_memo_evictions_total` | an eviction at nominal load is undersizing; raise with the memory limit |
| `PROXY_INVITE_TXN_TIMEOUT_SEC` | 158 | the workers' INVITE bound; an INVITE entry lives it + Timer H at most | 33..=600 (start); = the workers' bound (render) | — | set by `inviteTxnTimeoutSec`, never alone |
| `LB_CAP_INITIAL_CPS`, `_CEILING_CPS`, `_FLOOR_CPS`, `LB_AIMD_INCREASE_STEP_CPS`, `LB_AIMD_COOLDOWN_TICKS` | 30, 200, 1, 2, 5 | the per-worker AIMD (ADR-0046) | floor ≤ initial ≤ ceiling; cooldown × 1 s ≥ one probe cycle (start) | `sip_proxy_lb_rate_cap_spilled_total`, `sip_proxy_worker_stale_decrease_total` | the ceiling must let a lone worker take the whole rate, or calls spill at nominal load |
| `PROXY_DRAIN_GRACE_MS`, `PROXY_BIND_RETRY_DEADLINE_MS` | 15000, 20000 | drain on SIGTERM: the proxy keeps forwarding while its node's routes are withdrawn (ADR-0056); a recreated pod's wait for its predecessor's ports | grace > `readinessProbe.periodSeconds` × `failureThreshold` + `announce.propagationMs` (render); deadline > grace (start); pod grace > drain grace + 2 s (render) | the `proxy_rollout` incident's `max_drain_to_withdrawn_ms`; calls failed during a rollout | provisional until the lab measures the withdrawal: then set to the measured drain-to-withdrawal plus margin. Shorter: a proxy exits while still routed to; longer: slower rollouts |
| `PROXY_EXT_INTERFACE`, `PROXY_EXT_NEXT_HOPS` | none, none (the chart: `network.externalInterface`, `network.externalNextHops`) | the external face `/readyz` requires usable: the interface up, one next hop not `FAILED` in the neighbour table (ADR-0056) | a name of 1 to 15 bytes; next hops need the interface (start) | `sip_proxy_external_face_usable` | list every router of the external plane: one live next hop keeps the face usable |
| `PROXY_QUEUE`, `PROXY_INTAKE_SHED_PCT` | 8192, 50 | inbound queue and its shed watermark | ≥ 1; 0..100 (start) | `sip_proxy_intake_shed_total`, `sip_proxy_udp_tail_dropped_total` | lower sheds earlier |

## Call limiter, CDR chain, chart

| variable | default | role | relations (check) | watch | raise / lower |
|---|---|---|---|---|---|
| `callLimiter.leaseSeconds` (`LIMITER_LEASE_SECONDS`) | 120 | the lease every answer states (ADR-0040) | > 2 × the workers' refresh + tick (render) | `limiter_lease_expired_holds_total` | longer: a dead worker's holds linger; shorter: more refresh traffic |
| `cdr.maxLength` | 100000 | the CDR queue's `x-max-length`, drop-head | producer and consumer declare it equal (render) | the queue depth, `cdr_consumed_total` | the CDRs a consumer outage may hold |
| `B2BUA_CDR_RABBITMQ_URL`, `CDR_AMQP_URL` | — | the broker | `heartbeat=0` refused (start); none proposes 30 s | `cdr_amqp_consuming`, `b2bua_cdr_dropped_total` | keep a heartbeat: a silent broker is detected only by it |
| `B2BUA_CDR_RABBITMQ_*_TIMEOUT_MS`, `_WINDOW` | 3000 / 1000 / 5000, 256 | delivery bounds | each in its range; backoff max ≥ backoff (start) | `b2bua_cdr_dropped_total` | longer waits hold more CDRs in memory |
| `inviteTxnTimeoutSec` | null | the one INVITE bound of workers and proxies | 33..=600 (schema) | — | see the worker's bound |
| `worker.terminationGracePeriodSeconds`, `proxy.terminationGracePeriodSeconds` | 30, 30 | the kubelet's SIGKILL | above the drains (render) | killed drains in pod events | raise with the drain bounds |
| `worker.podDisruptionBudget`, `proxy.podDisruptionBudget` | enabled, `maxUnavailable: 1` | a voluntary disruption (node drain, eviction) takes one worker and one proxy at a time, and the next waits until the one taken reads ready; a returned worker reads ready once its own calls and the calls it backs up are held (ADR-0014 §11), or once `B2BUA_REPL_BOOTSTRAP_HARD_TIMEOUT_MS` releases a peer never reached | ≥ 1 (schema) | evictions pending in `kubectl get pdb`; the worker's readiness transition line | 2 or more lets a drain take a primary and its backup together; disabled leaves node drains and evictions unserialised (a rolling update stays serialised by readiness) |
| `worker.affinity`, `worker.topologySpreadConstraints` | none, spread over nodes when possible (`ScheduleAnyway`) | where workers land; a call's two copies are on two workers, so two workers on one node share its fault | — | pods per node | a product with two workers sets a hard node anti-affinity (ADR-0015 status) |
| `proxy.readinessProbe.periodSeconds`, `failureThreshold` | 1, 2 | how fast a failing proxy starts its node's withdrawal (ADR-0056) | detection + propagation < `PROXY_DRAIN_GRACE_MS`; period + propagation < `minReadySeconds` × 1000 (render) | the withdrawal facts of `kill_proxy`, `cut_external_plane` | shorter: faster withdrawal, a slow `/readyz` answer withdraws a healthy node |
| `proxy.announce.propagationMs` | 8000 | the latency from a readiness change to the routers' tables: EndpointSlice, speaker, frr-k8s reload, BGP UPDATE, router FIB | bounds the drain grace and `minReadySeconds` (render) | the `proxy_rollout` incident's drain-to-withdrawal less the detection | provisional until measured on the lab; set it to the measurement, never below |
| `proxy.minReadySeconds` | 15 | a replaced proxy counts as available only this long after Ready, so a rollout stops the next one once it is announced | > one probe period + `propagationMs` (render) | the `proxy_rollout` incident's `absent_ms` | longer: slower rollouts; shorter: both proxies withdrawn at once |
| `proxy.announce.port` | 9 | the one TCP port of the announce-only Services; nothing listens on it on a proxy node | ≠ 9090 and ≠ `network.sipPort` (render) | — | any port free on every proxy node |
| `proxy.announce.metallb.bfd.*` | 300 ms, 300 ms, 3 | BFD on every peer session: a dead node's routes go after multiplier × receive interval | 10..60000 ms, multiplier 2..255 (schema); the routers' BFD to match | the `node_stop` incident's `withdrawn_ms` | shorter: faster detection, a loaded node flaps |
| `proxy.announce.metallb.enabled`, `.peers`, `.myASN` | off, none, 64513 | the MetalLB objects rendered into MetalLB's namespace; off leaves them to the operator | peers of both faces required, MetalLB's APIs present (render) | — | — |
| `worker.replicas`, `worker.replication.enabled` | 2, off | the pool and HA | replication ⇒ ≥ 2 workers and a cluster key (render) | — | — |
| `worker.networkPolicy.enabled`, `proxyNodeCidrs`, `monitoringNamespace` | true, [], observability | the workers' ingress: SIP from the internal VIP and the proxy nodes, replication from the workers, the probe port from the scraper's namespace | `proxyNodeCidrs` stated while the proxy is enabled (render) | `sip_proxy_worker_pool_empty` and the proxy's worker health gauges | `proxyNodeCidrs` is the source address the proxy nodes use toward pod IPs: `ip route get <worker pod IP>` on each proxy node (`src`); the tunnel address on an overlay CNI, not the node subnet. Wrong, and every proxy reads every worker dead. Off only where another layer filters the workers' ingress |
| `proxy.hmac.existingSecret`, `drawInCluster` | "", false | the cookie key's Secret | one of them (render) | `sip_proxy_hmac_failures_total` | a changed key 403s calls in progress |
