# Observability — operating guide + code guidelines

Two independent planes ([ADR-0026](adr/0026-observability-logging-tracing.md)):
**lifecycle logs** on stdout, always on and traffic-independent; **per-call
traces** over OTLP, sampled and off unless a collector is configured.

## Part A — operating

### Where it lives

`deploy/observability/install.sh --bootstrap` brings up the host stack
(`deploy/observability/stack/docker-compose.yml`) once the kind lab's `up` has
made the `kind` network (deploy/README.md). Traces land in **VictoriaTraces** and are read in Grafana through the
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
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP/HTTP **base** url (`/v1/traces` is appended for you). Unset or empty = this process exports nothing and the whole sampling machinery is inert. A value that is set but unparseable warns on stdout once the subscriber is up, then behaves exactly like unset — inert sampling, no fallback to a default collector. |
| `SIP_TRACE_HEADER` | `1` makes the process honour `X-Trace-Sample`. Lab/endurance ONLY — the header is untrusted input on a serving edge. |
| `RUST_LOG` | Lifecycle log filter, default `info`. |

The SUT chart's kind values (`deploy/helm/siprust/values-kind.yaml`) are the
**dev/lab** composition: `tracing.otlpEndpoint` points the worker and the proxy
at the host VictoriaTraces through the kind bridge gateway, and
`tracing.traceHeader` sets `SIP_TRACE_HEADER=1`. **A production composition
leaves both empty** (the chart's defaults); sampling then costs one boolean per
call.

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
nominal and its takeover backup, and `node` names the node that served each
span. Every event carries `at_ms` (when the fact happened) — on the backfill
path that is NOT the emission timestamp, so read `at_ms` for the call's
timeline.

Why a call you expected is not there, in order of likelihood: no exporter
configured (`trace_dropped_no_exporter_total`), the burst bucket
(`trace_denied_rate_total`), the concurrent cap
(`trace_denied_active_cap_total`). Why a traced call goes quiet at a takeover:
the node that hydrated it was refused a span of its own by the same chain
(`trace_adoption_refused_total`) — the call stays sampled, this node just
records nothing for it. Denials are counted, never logged. All
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

**Node identity.** `node=` is `observe::node()`: the pod's identity, unless the
code runs under an `observe::NodeScope` — several nodes sharing one process, as
the in-process harnesses run them. A task a node spawns goes through
`observe::spawn_keeping_node` (or `keep_node` for a `JoinSet`), so it keeps that
node; a plain `tokio::spawn` names the process instead. A handle read from
outside its node's tasks (readiness, a harness proxy's registry) holds the
`NodeScope` it was built under and enters it to log.

**Traces.** Most implementation work adds NO trace emission: the choke points —
SIP messages in/out, rule and context transitions, limiter admit/refresh/release
— are already hooked, so new code flowing through them is traced for free. The
one class new code should add is **per-call external-service I/O**: a new
HTTP/remote dependency called once per call gets a child span carrying its
request and response as events. Every emission site sits behind
`if call.sampled { … }` — an unsampled call must evaluate no format argument and
allocate nothing; subscriber-side filtering is never the mechanism.

**Headers.** `X-Trace-Sample` is read in `sip-message`, off the parsed INVITE,
and nowhere else — the repo-wide rule that no other crate extracts SIP headers
has no exception here.
