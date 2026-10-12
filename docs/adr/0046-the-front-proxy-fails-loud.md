# The front proxy fails loud

**Status:** accepted (2026-10-12)

**Owner:** `crates/sip-proxy-runner/src/config.rs` and `config/reader.rs` (the
env grammar and its ranges), `discovery.rs` (the worker pool),
`readiness.rs`, `main.rs` (the SIGTERM handler), `crates/topology/src/k8s.rs`
and `k8s/watch_health.rs` (the kube client retry, the watch's own health),
`crates/sip-proxy/src/resolver.rs` (the bounded lookup, the address family),
`deploy/k8s/manifests/30-proxy.yaml` (the VRRP health check, the cookie key
Secret), `deploy/k8s/run.sh` (`proxy_hmac_secret`).

## Context

The front proxy degraded silently where the B2BUA worker refuses
(ADR-0045). A kube client that failed at boot left the proxy routing to a
loopback stand-in worker for the life of the process: the stand-in started
`Alive`, so `/readyz` read Ready for the first probe cycles, INVITEs were
forwarded to nothing, and every call after that got a 503. The VRRP instance
tracked only interfaces, so a crashed or crash-looping proxy on the address
holder dropped every call unanswered. About 25 variables kept their default
when set malformed or empty, some ranges were never checked (an AIMD decrease
factor above 1 raised the cap), the worker namespace and the advertise address
fell back to fixed values, and a SIGTERM handler that failed to install only
warned. The cookie signing key defaulted to a key published in this
repository. A named-target lookup had no bound: under `ndots:5` with DNS
black-holed one lookup could take about a minute, the packet was sent
whenever it ended (an INVITE after its sender's Timer B), 64 hung lookups
starved every other name of a forwarder, and the first address won whatever
its family.

## Decision

1. **Discovery never fakes a worker.** Without `PROXY_WORKERS` the pool is
   discovered from the EndpointSlices of the worker Service
   (`K8sMembership::spawn_connecting`). It starts empty; a kube client that
   fails to build is built again in the background, 1 s doubling to 30 s, for
   the life of the process. There is no stand-in pool.
2. **Readiness is "at least one worker `Alive`"**, with no hard timer. The
   proxy is NotReady until a listed worker answers OPTIONS. A broken watch
   never changes readiness: the last listed pool stays, and the OPTIONS probe
   keeps its health current.
3. **Discovery reports its own health.** `sip_proxy_discovery_no_client` is 1
   once a kube client build has failed and while none is built (0 before the
   first attempt ends); `sip_proxy_discovery_watch_broken_since_seconds` is the
   Unix time the watch broke, 0 while it delivers. A watch that restarts on a
   valid resource version delivers no event until the next change, so a broken
   watch ends on a delivered event or after 90 s without an error (the
   watcher's 30 s backoff ceiling plus the 30 s connect timeout, with margin).
   Each failure is an episode: one warning naming the cause when it starts,
   one line when it ends.
4. **The virtual address follows proxy health.** The VRRP instance tracks a
   script that reads the proxy's `/healthz`, wrapped in `/bin/sh -c` with its
   exit code clamped to 0/1: 1 s timeout, `interval 1`, `fall 3`, `rise 2`,
   `weight -50`, `init_fail`. A failing check lowers the node's priority and
   never puts the instance in FAULT; a starting node sits at the lowered
   priority until its proxy passes `rise 2`, so it never takes the address
   ahead of its own proxy. Both nodes keep the same base priority and preempt (no
   `nopreempt`). VRRP preempts only on a strictly higher priority, so:
   - a dead proxy loses the address in 4.6–6.6 s (below);
   - when both checks fail the priorities are equal again and the address stays
     where it is, never lost;
   - a proxy that recovers does not take the address back.

   The instance runs VRRPv2 with `advert_int 1` (VRRPv2 adverts are whole
   seconds). A backup that preempts discards the lowered node's adverts and
   moves only when its master-down timer expires, so a move takes

   ```
   move = δ + 2·interval + master_down − ε        = 4.6 to 6.6 s
   δ ∈ (0, 1]   the check's phase after the proxy dies
   master_down  = 3·advert_int + (256 − 100)/256 = 3.609 s
   ε ∈ [0, 1)   how long before the drop the last full-priority advert arrived
   ```

   A kubelet restart of the dead proxy can answer `/healthz` for a moment and
   reset the fall count, which adds up to 2 s (worst about 8.6 s).
5. **Strict configuration.** Every variable is read once, before the first
   bind. Unset takes the default. Set empty, malformed or out of its range
   refuses the boot, naming every problem at once; only `PROXY_UDP_SNDBUF` /
   `PROXY_UDP_RCVBUF` give empty a meaning (the kernel default). The
   cross-component checks between the probe timings and the `LB_*` set stay.
   `PROXY_SELF_GATE` takes only `1|true|yes|on|0|false|no|off`.
6. **No fixed fallbacks for identity.** The worker namespace is
   `PROXY_NAMESPACE`, else `POD_NAMESPACE`, else the pod's own (its service
   account namespace file); none known refuses a discovery boot. An
   unspecified listen address requires its advertise (`PROXY_ADVERTISE`,
   `PROXY_ADVERTISE_EXT`). Bind addresses (`PROXY_LISTEN`, `PROXY_LISTEN_EXT`,
   `PROXY_METRICS`) are IP literals: the boot looks no name up. A SIGTERM
   handler that cannot be installed is fatal: the proxy would be killed
   undrained.
7. **The cookie signing key is required.** `PROXY_HMAC_KEY` has no default.
   The lab deploy creates a Secret once per cluster when it is missing (32
   random bytes, base64 text, kid `k1`); both proxies read it through
   `secretKeyRef` and every redeploy reuses it. Rotation through env is not
   supported: changing the key fails the cookie check (403) for the in-dialog
   requests of every call in progress.
8. **Named-target lookups are bounded.** Each lookup is bounded at
   `PROXY_RESOLVER_LOOKUP_TIMEOUT_MS` (2 s). At the bound the packet is
   dropped and the negative entry (5 s) written: a packet is sent at once or
   never. A lookup past its bound starts no further attempt (no second name
   form); its in-flight slot stays taken until the attempt already running
   returns, so stuck lookup threads stay at most `max_in_flight` (64) per recv
   shard, and a name is looked up again within about one resolver timeout
   after DNS recovers. Of a name's addresses the first the egress socket can
   reach is used — the socket of the face the packet leaves on; an unspecified
   IPv6 bind is dual-stack and reaches both families — and kept for the cache
   lifetime.
9. **NAPTR/SRV is a deliberate gap.** Names resolve through A/AAAA only; RFC
   3263 §4.1-4.2 server selection is not implemented. A portless URI goes to
   port 5060.

### Settings and their ranges

| key | range | default |
|---|---|---|
| `PROXY_LISTEN`, `PROXY_LISTEN_EXT`, `PROXY_METRICS` | IP literal `ip:port` | `0.0.0.0:5060`, unset, `0.0.0.0:9090` |
| `PROXY_ADVERTISE`, `PROXY_ADVERTISE_EXT` | `host:port`; required when the listen IP is unspecified | the listen address |
| `PROXY_FACE_INT_CIDRS` | IPv4 CIDR list; required with `PROXY_LISTEN_EXT` | — |
| `PROXY_WORKERS` | `id@ip:port,..` | unset: discovery |
| `PROXY_WORKER_SERVICE` | non-empty | `b2bua-worker` |
| `PROXY_WORKER_PORT` | 1..65535 | 5060 |
| `PROXY_NAMESPACE`, `POD_NAMESPACE` | non-empty | the pod's own |
| `PROXY_QUEUE` | ≥ 1 packets | 8192 |
| `PROXY_UDP_SNDBUF`, `PROXY_UDP_RCVBUF` | ≥ 1 bytes; empty = kernel default | 4 MiB |
| `PROXY_RECV_SHARDS` | 1..1024 | 1 |
| `PROXY_INTAKE_SHED_PCT` | 0..100 | 50 |
| `PROXY_SHARD_STALL_MS`, `PROXY_BIND_RETRY_INTERVAL_MS`, `PROXY_BIND_RETRY_DEADLINE_MS`, `PROXY_DRAIN_GRACE_MS` | ≥ 1 ms | 2000, 200, 10000, 5000 |
| `HEALTH_INTERVAL_MS`, `HEALTH_TIMEOUT_MS` | ≥ 1 ms | 1000, 1500 |
| `HEALTH_THRESHOLD` | ≥ 1 | 2 |
| `PROXY_HMAC_KID` | non-empty | `k0` |
| `PROXY_HMAC_KEY` | ≥ 16 bytes | required |
| `LB_ELU_SOFT`, `LB_ELU_HARD`, `LB_ELU_CRITICAL` | (0, 1], soft < hard < critical | 0.4, 0.5, 0.65 |
| `LB_BAND_HYSTERESIS` | [0, 1), below the narrower band gap | 0.05 |
| `LB_AIMD_DECREASE_FACTOR` | (0, 1) | 0.5 |
| `LB_AIMD_INCREASE_STEP_CPS`, `LB_CAP_CEILING_CPS`, `LB_CAP_FLOOR_CPS` | > 0, floor ≤ initial cap ≤ ceiling | 2, 200, 1 |
| `PROXY_SELF_GATE` | one of `1`, `true`, `yes`, `on`, `0`, `false`, `no`, `off` | on |
| `PROXY_SELF_GATE_ELU_CRITICAL` | (0, 1] | 0.8 |
| `PROXY_SELF_GATE_CPS_SIZE`, `PROXY_SELF_GATE_CPS_RATE` | ≥ 1 | 50, 100 |
| `PROXY_RESOLVER_POSITIVE_TTL_MS`, `PROXY_RESOLVER_NEGATIVE_TTL_MS`, `PROXY_RESOLVER_LOOKUP_TIMEOUT_MS` | ≥ 1 ms | 60000, 5000, 2000 |
| `PROXY_RESOLVER_PREWARM` | `host:port` list | none |

## Consequences

- A proxy that cannot discover workers is NotReady and says why on
  `/metrics`; it never takes calls into a pool that does not exist.
- A deploy that forgets the cookie key, the namespace or an advertise address
  crash-loops with the variable named, instead of running on a guess.
- A name that resolves slower than the bound never resolves: the bound is the
  contract, and the operator raises it or fixes DNS.
- The VRRP check needs an HTTP client in the keepalived image (busybox
  `wget`).
- Pinned by: `sip-proxy-runner` `config::tests` (one refusal per class: a
  malformed, empty and out-of-range value, an unparsable `LB_*` override, an
  empty namespace, a missing advertise, a name as a bind address, a missing
  key), `discovery::tests` (a kube client that fails then builds: NotReady
  with no stand-in worker, then the pool fills; a watch that restarts quietly
  ends its broken episode); `sip-proxy` `resolver::tests` (a lookup past its
  bound, no candidate past the deadline, the address family and a dual-stack
  bind). The VRRP behaviour is checked on a lab cluster: staggered starts, a
  crash-looping proxy on the address holder, established calls across the
  move, no takeback, no FAULT.
