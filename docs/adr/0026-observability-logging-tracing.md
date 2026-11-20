# Observability: lifecycle logging and per-call tracing

**Status:** accepted (2026-08-02)

The rationale of each rule, the endpoint resolution, the correlation ids and the deployment
compositions in full: [appendix](appendix/0026-observability-logging-tracing.md).

## Context

Production diagnosis has two irreconcilable needs. Fleet health must be legible at any traffic
level, so anything emitted once per call is unusable: a 5000-call failover would print 5000
lines, starve the log pipeline during the incident, and with a blocking writer stall the SIP
tasks under investigation. Single-call forensics needs the opposite: every wire message, rule
transition and decision round trip of one call, with original timestamps and correlation
across proxy, worker and takeover backup.

## Decision

Two independent planes. `tracing` is the single emission facade. `crates/observe` owns
subscriber composition, the admission chain and the OpenTelemetry dependency tree. Domain
crates depend on `tracing` and the thin helpers only.

### 1. Lifecycle logs: traffic-independent, lossy, non-blocking

- `info` level, single-line `key=value` (fmt compact), always to stdout, whether tracing is on
  or not.
- A bounded lossy channel drained by one writer thread. A full queue drops the line and bumps
  `log_lines_dropped_total`. A SIP task never blocks on stdout.
- No per-call info line, ever. State transitions and rare events get one line with full
  context. Per-call event classes get a rising-edge line, a ~5 s periodic summary on
  `tokio::time`, and a falling-edge totals line. `observe::WaveSet` holds one episode per key
  over a bounded key space. An episode ends on quiet only: a recovery arms the falling edge,
  and a failure inside the idle window revives the same episode silently.
- Every line carries node identity, and peer, epoch and `(p,b)` where relevant.

Detail: [appendix §1](appendix/0026-observability-logging-tracing.md#1-lifecycle-logs-traffic-independent-lossy-non-blocking).

### 2. Per-call traces: sampled, OTLP, explicit-guard

- One root span per call per process, closed at terminal state or reap. Detail is span events
  with their original timestamps: SIP messages in and out (raw wire bytes), rule-engine
  transitions, context transitions, limiter admit/refresh/release. Child spans exist only for
  decision-engine and limiter HTTP round trips.
- Every span carries `sip.call_id` plus the From/To tags.
- A 16 KiB per-attribute cap on the emitted attribute, with a `truncated=true` marker. A
  base64-encoded remainder is capped at 12 KiB at its source.
- Every per-call span and event uses the target `sip::trace`. The stdout fmt layer filters
  that target out per layer; the OTLP layer does not.
- Every per-call emission site is wrapped in `if call.sampled { … }`. An unsampled call
  constructs no span and allocates nothing. Subscriber-side filtering is never the mechanism.
- Only `OTEL_EXPORTER_OTLP_ENDPOINT` enables the exporter. Unset, or unusable at
  exporter-build time, the sampling machinery is inert: an activation attempt bumps
  `trace_dropped_no_exporter_total` and returns. The var is an OTLP/HTTP base url, and
  `observe` resolves `<base>/v1/traces` itself.

Detail: [appendix §2](appendix/0026-observability-logging-tracing.md#2-per-call-traces-sampled-otlp-explicit-guard).

### 3. Sampling and activation: monotonic, decided once

Sampling is enable-only: nothing revokes it mid-call. Entry paths:

1. Default draw: Bernoulli at the configured rate, default `1e-4`.
2. Header override: `X-Trace-Sample: <float 0..=1>` overrides the rate for that call, honored
   only when the process started with `SIP_TRACE_HEADER=1`. Unset, the header is not looked
   up. A malformed value falls back to the configured rate and counts
   `trace_header_malformed_total`. `sip-message` alone reads it.
3. Engine force-enable: `"trace": bool` (serde-default `false`) on `RouteDecision`
   (ADR-0017). On activation at `apply_route` the call is backfilled from facts it already
   carries, with original timestamps.

All three pass one admission chain:

> positive draw → token bucket (burst 10, refill 1/s) → `trace_max_active`
> concurrent cap (default 200)

A denial bumps a counter and is never a log line. `observe` mints W3C-shaped ids and every
span carries them as `trace_id` / `span_id` attributes. Every span event carries `at_ms`, when
the fact happened. Detail: [appendix §3](appendix/0026-observability-logging-tracing.md#3-sampling-and-activation-monotonic-decided-once).

### 4. No wire propagation

The proxy samples independently of the worker. No trace context rides the SIP wire.
Correlation is by the `sip.call_id` attribute alone. The proxy keeps a `Call-ID -> span
context` TTL map bounded by the active cap, behind an atomic "anything sampled?" flag. Detail:
[appendix §4](appendix/0026-observability-logging-tracing.md#4-no-wire-propagation).

### 5. HA

`Call.trace_id`, `Call.root_span_id` and `Call.sampled` ride the replicated `Call`. On takeover
the backup opens its own root span with the replicated `trace_id` and a link (the
`link.trace_id` / `link.span_id` attributes) to the nominal's `root_span_id`, never a parent.
Root spans are runtime state, never replicated. Adoption runs at each hydration site on the copy
the store keeps, under the residency check. Detail:
[appendix §5](appendix/0026-observability-logging-tracing.md#5-ha).

### 6. Deployment compositions

The two env vars place a process in its composition, so one binary serves all. Dev/lab and
endurance set both (`deploy/helm/siprust/values-kind.yaml`). Production sets both empty. Traces
are stored in VictoriaTraces. Operating detail: [docs/observability.md](../observability.md),
and [appendix §6](appendix/0026-observability-logging-tracing.md#6-deployment-compositions).

### 7. Tests

- No OTel machinery in tests. The harness installs a thread-scoped in-memory buffer subscriber
  that captures `info` and above, with no background task, IO or wall-clock signal.
- `scenario-harness`'s `Harness` installs it for every run and dumps the captured tail on
  panic. Installation is nested-safe.
- Scenario tests never assert on log content: the `Recorder` stays the oracle. Tests of the
  trace machinery may assert on the buffer.

Detail: [appendix §7](appendix/0026-observability-logging-tracing.md#7-tests).

## Consequences

- `crates/observe` is the only crate that links OpenTelemetry, behind its `otlp` feature. No
  domain crate requests the export tree.
- Log output is lossy under extreme pressure, measured by `log_lines_dropped_total`.
- A collector outage degrades to "no traces". Nothing on the SIP path awaits the exporter.
- A per-call `info!` anywhere is a review must-fix. The directives for new code live in
  [docs/observability.md](../observability.md).

The rest: [appendix, consequences](appendix/0026-observability-logging-tracing.md#consequences).
