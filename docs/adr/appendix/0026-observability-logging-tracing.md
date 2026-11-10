# Appendix: Observability: lifecycle logging and per-call tracing

The decision part: [ADR-0026](../0026-observability-logging-tracing.md).

## Decision

### 1. Lifecycle logs: traffic-independent, lossy, non-blocking

Dropped observability is strictly preferable to a stalled call path. Because the summary
ticker rides `tokio::time`, a paused-clock test drives it. With the aggregation discipline a
5000-call failover produces ~5 lines.

`observe::WaveSet` mechanises the discipline: an episode per key (dead peer, failing target,
shed reason) with named counters, one driver task per open episode, and a bounded key space
(keys can be wire-influenceable). A recovery arms the falling edge, which lands after the idle
window with no further failure. Without that hysteresis a source flapping at its cap (a token
bucket alternating reject/admit, a backend answering every other request) would emit a
rising/falling pair per call, the per-call line this discipline exists to forbid.

### 2. Per-call traces: sampled, OTLP, explicit-guard

Rule-engine transition events carry `rule_id`, `machine` and `from -> to`. The HTTP child
spans carry the bodies as events. The `truncated=true` marker keeps a prefix from being
mistaken for a whole value. 12 KiB encodes to exactly 16 KiB, so the cap governs the emitted
attribute rather than the pre-encoding value.

Without the target separation a traced call's wire bytes would render as stdout `info` lines,
per-call logging, the thing §1 forbids, and would crowd genuine lifecycle lines out of the
bounded lossy writer exactly when an operator needs them. A shared level/target filter cannot
do this: muting the trace plane for stdout would starve the exporter with it.

#### Explicit-guard discipline

Every per-call emission site is wrapped in `if call.sampled { … }`. An unsampled
call constructs no span object, evaluates no format argument and performs zero
additional allocations. Subscriber-side filtering is NEVER the mechanism: it
still pays argument evaluation and field construction on the hot path, which is
precisely the cost this design refuses.

#### Cost guarantee when no collector is configured

With the exporter unset, no draw is taken and no span is created. A process with no collector
pays one boolean check per call.

That env var is an OTLP/HTTP **base** url, exactly as every OpenTelemetry SDK
reads it: `observe` appends the trace-signal path and configures the exporter
with the resolved `<base>/v1/traces`. It resolves the url rather than leaving
the var to the exporter's own lookup because that lookup falls back to the SDK's
`localhost:4318` default whenever the value fails to parse — a typo'd endpoint
would then report as configured, open root spans, and post every batch into the
void. Resolved here, an unusable endpoint fails at exporter-build time and the
process enters the same "exports nothing" state as an unset var — the sampling
machinery goes inert, so no root span is opened for a collector that will never
see it. The cause is reported to the subscriber installer, which warns AFTER
installing the subscriber: resolution happens before any subscriber exists, and
a warning emitted there would reach the no-op dispatcher and be lost.

### 3. Sampling and activation: monotonic, decided once

A half-traced call is worse than an untraced one, hence enable-only. The force-enable is a
dedicated optional field in the decision-response schemas (ADR-0017 contract). The backfill
emits the INVITE received, the auto-100 sent, and the decision request/response as events with
their original timestamps. Nothing is buffered for unsampled calls: the backfill reads facts
the call already carries. A denial is never a log line because a refused sample must not
become the traffic-proportional output that sampling exists to avoid.

#### Env-gate trust model

`X-Trace-Sample` is attacker-controllable. It is honored ONLY when the process
was started with `SIP_TRACE_HEADER=1` (lab and endurance rigs). When that env is
unset the header is not even looked up. This is a deployment-level gate, not a
per-request authorization check: a serving edge simply cannot be steered into
tracing by a caller. A malformed value is ignored in favour of the configured
rate and counted (`trace_header_malformed_total`).

Header extraction lives ONLY in `sip-message`, on the parsed INVITE (the proxy
reads it off the message its receive path parsed), per the repo-wide rule that
no other crate extracts SIP headers. The reader keeps "absent" and "present but
unreadable" apart (`TraceSample::{Absent, Malformed, Rate}`) so a rig that
mistyped its rate does not look like a call that asked for nothing.

The engine force-enable is a `trace: bool` on `RouteDecision` — the treatment
that carries a call forward, and therefore the one that can turn its trace on.
`false` leaves the call to the sampler, so an engine that never sets it is unchanged.

#### Correlation ids

Ids are W3C-shaped (32-hex trace, 16-hex span) and are minted by `observe`, not
by the exporter, and ride every span as the `trace_id` / `span_id` attributes.
That is what lets a domain crate populate `Call.trace_id` / `Call.root_span_id`
and link a takeover span while the OpenTelemetry dependency tree stays inside
`observe`. For the same reason the HA link is recorded as the `link.trace_id` /
`link.span_id` attributes on the backup's root span rather than as an SDK link
object — the correlation is identical, and the emitting crate stays OTel-free.

Every span EVENT carries `at_ms`: when the fact HAPPENED. The subscriber's own
timestamp is the emission time, which on the backfill path is not the same
thing, so a reader that wants the call's timeline reads `at_ms`.

### 4. No wire propagation

There is no traceparent, no `X-B3-*`, no span context on the SIP wire, and therefore no trust
machinery to decide whether an inbound context may be believed.

Rationale: SIP peers here include third-party endpoints; propagating a span
context means either trusting attacker-supplied trace IDs (unbounded cardinality
and cross-tenant span grafting) or building the validation and signing machinery
to avoid it. `Call-ID` is already globally unique, already carried, and already
the key every operator triages by. The cost is that the two processes'
independent draws rarely both fire for the same call — accepted: a proxy-side
and a worker-side trace answer different questions, and the engine force-enable
covers the case where a specific call must be seen end to end.

Proxy internals: a `Call-ID -> span context` TTL map bounded by the same active
cap, with the per-packet check gated behind an atomic "anything sampled?" flag,
so an unsampled proxy pays one predicted branch. The span closes on an observed
BYE-final or on TTL.

### 5. HA

The nominal's span is closed (or lost) by definition at takeover; linking records the causal
relationship without claiming a live parent that will never close. A core's root spans die with
the core, so a survivor always opens its own.

Adoption runs at each hydration site **on the copy the store keeps, under the
residency check** — a node opens a span only for a call it goes on to serve.
Adopting a copy the store then discards would register a root span that no
stored call names, and the next takeover would link to a root no process ever
served.

### 6. Deployment compositions

Which composition a process belongs to is expressed entirely by the two env
vars, so the same binary serves all of them:

- **dev/lab and endurance** (the SUT chart's kind values,
  `deploy/helm/siprust/values-kind.yaml`): the worker and the proxy carry
  `OTEL_EXPORTER_OTLP_ENDPOINT` pointed at the host stack's VictoriaTraces
  through the kind bridge gateway, and `SIP_TRACE_HEADER=1`.
- **production**: both empty. Empty is the meaningful value — not a missing
  key — so a composition never has to remember to delete a line to stay inert.

Traces are stored in VictoriaTraces alongside the existing VictoriaMetrics /
VictoriaLogs, read in Grafana through a Jaeger-protocol datasource, and share
their lifecycle (`install.sh --bootstrap|--apply|--down|--status`). Operating
detail: [docs/observability.md](../../observability.md).

### 7. Tests

- **No OTel machinery in tests.** The harness installs a thread-scoped in-memory
  buffer subscriber: no background task, no real IO, no wall-clock signal
  (docs/testing/test-clock.md — a paused test must never ride one). It captures
  `info` and above, which is both planes.
- `scenario-harness`'s `Harness` installs it for every run — so `b2bua-harness`
  and `failover-harness`, which wrap it, are covered by construction — and dumps
  the captured tail to stderr on panic alongside `PanicDump`'s wire trace;
  `finish()` disarms both and a clean run discards the buffer.
- Installation is nested-safe: a test that already captures (a test OF the trace
  machinery) keeps its own buffer and the harness joins it rather than shadowing
  it, so the test still reads everything it asserts on.
- Scenario tests NEVER assert on log content — the `Recorder` stays the oracle.
  Dedicated unit/integration tests for the trace machinery itself MAY assert on
  the captured buffer.

## Consequences

- `crates/observe` is the only crate that links OpenTelemetry, and behind its
  `otlp` feature: the runners turn it on, the domain crates depend on `observe`
  for the lifecycle-aggregation helpers with default features, so a domain crate
  gains no transitive export tree. That holds per crate graph, not per build:
  Cargo unifies features across a workspace build, so any build that also
  selects a runner compiles ONE `observe` with `otlp` on, which `b2bua` and
  `sip-proxy` then link — the guarantee is that no domain crate REQUESTS the
  tree, not that a workspace `cargo build` can avoid compiling it.
- Log output is lossy under extreme pressure. That is deliberate and measurable:
  `log_lines_dropped_total` is scraped alongside the trace-denial counters.
- A collector outage degrades to "no traces": the batch exporter drops, and the
  SIP path is unaffected because nothing on it ever awaits the exporter.
- Adding a per-call `info!` anywhere is a review must-fix — it breaks the
  traffic-independence guarantee that makes the log stream usable at all. The
  standing directives for new code live in
  [docs/observability.md](../../observability.md) and CLAUDE.md.
- A failing scenario now self-documents twice: the wire trace and what the SUT
  logged and traced while producing it.
