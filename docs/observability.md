# Observability — operating guide + code guidelines

Two independent planes ([ADR-0026](adr/0026-observability-logging-tracing.md)):
**lifecycle logs** on stdout, always on and traffic-independent; **per-call
traces** over OTLP, sampled and off unless a collector is configured.

## Part A — operating

### Where it lives

`deploy/observability/install.sh --bootstrap` brings up the host stack
(`deploy/observability/stack/docker-compose.yml`); `./deploy/k8s/run.sh up` calls
it. Traces land in **VictoriaTraces** and are read in Grafana through the
`VictoriaTraces` (Jaeger) datasource.

| | endpoint |
|---|---|
| Grafana | http://127.0.0.1:3333 (anonymous admin) |
| VictoriaTraces — OTLP ingest | `http://<host>:10428/insert/opentelemetry/v1/traces` |
| VictoriaTraces — query (Grafana) | `http://victoria-traces:10428/select/jaeger` |
| health / status | `install.sh --status [-v]` |

`install.sh --down` tears the stack down.

### Env knobs

| var | meaning |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP/HTTP **base** url (the exporter appends `/v1/traces`). Unset or empty = this process exports nothing and the whole sampling machinery is inert. |
| `SIP_TRACE_HEADER` | `1` makes the process honour `X-Trace-Sample`. Lab/endurance ONLY — the header is untrusted input on a serving edge. |
| `RUST_LOG` | Lifecycle log filter, default `info`. |

`deploy/k8s/run.sh` is the **dev/lab** composition: it stamps both into
`manifests/20-worker.yaml` and `manifests/30-proxy.yaml` (`OTLP_EXPORT_ENDPOINT`
pointed at the host VictoriaTraces, `SIP_TRACE_HEADER=1`) via `envsubst`.
`OBS_ENABLE=0` empties the endpoint. **A production composition leaves both
empty**; sampling then costs one boolean per call.

### Activating a trace on a call

Three entry paths, all through the same admission chain (draw → token bucket,
burst 10 refill 1/s → active cap, default 200). Sampling is enable-only: once a
call is sampled nothing revokes it.

1. **Default draw** — Bernoulli at the configured rate (default `1e-4`). Nothing
   to do; this is what a background trace sample looks like.
2. **Header** — send `X-Trace-Sample: 1.0` on the INVITE to force this call's
   draw. Needs `SIP_TRACE_HEADER=1` on the process; ignored otherwise, and a
   malformed value falls back to the configured rate against
   `trace_header_malformed_total`.
3. **Engine force-enable** — return `"trace": true` on the `RouteDecision`. This
   backfills: the INVITE, the auto-100 and the decision round trip are recorded
   with their ORIGINAL timestamps, so the span is complete despite activating
   late.

### Reading a trace

There is **no wire propagation** — proxy and worker sample independently and a
takeover backup opens its own root span linked to the nominal's. Correlate on
the `sip.call_id` attribute, which every span carries; `trace_id` correlates the
nominal and its takeover backup. Every event carries `at_ms` (when the fact
happened) — on the backfill path that is NOT the emission timestamp, so read
`at_ms` for the call's timeline.

Why a call you expected is not there, in order of likelihood: no exporter
configured (`trace_dropped_no_exporter_total`), the burst bucket
(`trace_denied_rate_total`), the concurrent cap
(`trace_denied_active_cap_total`). Denials are counted, never logged. All
counters are on each runner's `/metrics`, next to `log_lines_dropped_total`
(lifecycle lines the non-blocking writer dropped under pressure).

## Part B — guidelines for future code

**Logs.** `info!` is RESERVED for lifecycle and state transitions — HA
(takeover, reclaim, epoch, `(p,b)`), startup, readiness, drain, long-term peer
state — and MUST be traffic-independent. Any per-call event class goes through
`observe::WaveSet` (rising edge → ~5 s summary → falling-edge totals), never a
per-call `info!` line: a 5000-call failover prints ~5 lines, not 5000. A
per-call `info!` is a review must-fix. Per-call diagnostics belong in the
per-call trace, or at `debug!`. Every line carries node identity, plus peer /
epoch / `(p,b)` where relevant.

**Traces.** Most implementation work adds NO trace emission: the choke points —
SIP messages in/out, rule and context transitions, limiter admit/refresh/release
— are already hooked, so new code flowing through them is traced for free. The
one class new code should add is **per-call external-service I/O**: a new
HTTP/remote dependency called once per call gets a child span carrying its
request and response as events. Every emission site sits behind
`if call.sampled { … }` — an unsampled call must evaluate no format argument and
allocate nothing; subscriber-side filtering is never the mechanism.

**Headers.** `X-Trace-Sample` is read in `sip-message` (full parse) and
`sip_message::sniff` (proxy raw path) and nowhere else — the repo-wide rule that
no other crate extracts SIP headers has no exception here.
