# Appendix: The front proxy fails loud

The decision part is [ADR-0046](../0046-the-front-proxy-fails-loud.md).

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

## Settings and their ranges

| key | range | default |
|---|---|---|
| `PROXY_LISTEN`, `PROXY_LISTEN_EXT`, `PROXY_METRICS` | IP literal `ip:port` | `0.0.0.0:5060`, unset, `0.0.0.0:9090` |
| `PROXY_ADVERTISE`, `PROXY_ADVERTISE_EXT` | `host:port`; required when the listen IP is unspecified | the listen address |
| `PROXY_WORKERS` | `id@ip:port,..` | unset: discovery |
| `PROXY_WORKER_SERVICE` | non-empty | `b2bua-worker` |
| `PROXY_WORKER_PORT` | 1..65535 | 5060 |
| `PROXY_NAMESPACE`, `POD_NAMESPACE` | non-empty | the pod's own |
| `PROXY_QUEUE` | ≥ 1 packets | 8192 |
| `PROXY_UDP_SNDBUF`, `PROXY_UDP_RCVBUF` | ≥ 1 bytes; empty = kernel default | 4 MiB |
| `PROXY_RECV_SHARDS` | 1..1024 | 1 |
| `PROXY_INTAKE_SHED_PCT` | 0..100 | 50 |
| `PROXY_SHARD_STALL_MS`, `PROXY_BIND_RETRY_INTERVAL_MS`, `PROXY_BIND_RETRY_DEADLINE_MS`, `PROXY_DRAIN_GRACE_MS` | ≥ 1 ms | 2000, 200, 10000, 5000 |
| `HEALTH_INTERVAL_MS`, `HEALTH_TIMEOUT_MS` | ≥ 1 ms | 1000, 1600 |
| `HEALTH_THRESHOLD` | ≥ 1 | 2 |
| `PROXY_HMAC_KID` | non-empty | `k0` |
| `PROXY_HMAC_KEY` | ≥ 16 bytes | required |
| `LB_ELU_SOFT`, `LB_ELU_HARD`, `LB_ELU_CRITICAL` | (0, 1], soft < hard < critical | 0.4, 0.5, 0.65 |
| `LB_BAND_HYSTERESIS` | [0, 1), below the narrower band gap | 0.05 |
| `LB_AIMD_DECREASE_FACTOR` | (0, 1) | 0.5 |
| `LB_AIMD_INCREASE_STEP_CPS`, `LB_CAP_INITIAL_CPS`, `LB_CAP_CEILING_CPS`, `LB_CAP_FLOOR_CPS` | > 0, floor ≤ initial ≤ ceiling | 2, 30, 200, 1 |
| `LB_AIMD_COOLDOWN_TICKS` | > 0, × the 1000 ms tick ≥ one probe cycle (`HEALTH_INTERVAL_MS + HEALTH_TIMEOUT_MS`) | 5 |
| `PROXY_SELF_GATE` | one of `1`, `true`, `yes`, `on`, `0`, `false`, `no`, `off` | on |
| `PROXY_SELF_GATE_ELU_CRITICAL` | (0, 1] | 0.8 |
| `PROXY_SELF_GATE_CPS_SIZE`, `PROXY_SELF_GATE_CPS_RATE` | ≥ 1 | 50, 100 |
| `PROXY_RETRY_AFTER_JITTER_SEC` | 0..3600 s | 5 |
| `PROXY_BRANCH_MEMO_MAX_ENTRIES`, `PROXY_BRANCH_MEMO_MAX_BYTES` | ≥ 1; with the mean ring, they hold the working set of `PROXY_SELF_GATE_CPS_RATE` calls/s with a quarter of headroom (refused at boot otherwise) | 78 643, 192 MiB accounted (3/8 of the 512 MiB pod limit; that many entries of the heaviest shape, 109 calls/s at a 30 s mean ring and the 158 s INVITE bound) |
| `PROXY_BRANCH_MEMO_MEAN_RING_MS` | ≥ 1 ms | 30000: the mean ring the memo's two pools are split on |
| `PROXY_INVITE_TXN_TIMEOUT_SEC` | 33..=600 s, the workers' INVITE transaction bound | 158: an INVITE entry lives the bound plus Timer H at most |
| `PROXY_RESOLVER_POSITIVE_TTL_MS`, `PROXY_RESOLVER_NEGATIVE_TTL_MS`, `PROXY_RESOLVER_LOOKUP_TIMEOUT_MS` | ≥ 1 ms | 60000, 5000, 2000 |
| `PROXY_RESOLVER_PREWARM` | `host:port` list | none |

## Tests that pin it

`sip-proxy-runner` `config::tests` (one refusal per class: a malformed, empty and out-of-range
value, an unparsable `LB_*` override, an empty namespace, a missing advertise, a name as a bind
address, a missing key), `discovery::tests` (a kube client that fails then builds: NotReady with
no stand-in worker, then the pool fills; a watch that restarts quietly ends its broken episode),
`readiness::tests` (a stalled shard fails `/healthz` and `/readyz`, a proxy with none passes
them; before the first worker view `/readyz` fails); `sip-proxy` `resolver::tests` (a lookup past
its bound, no candidate past the deadline, the address family and a dual-stack bind).
