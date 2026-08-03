# Observability: lifecycle logging and per-call tracing

**Status:** accepted (2026-08-02)

## Context

Production diagnosis of this stack has two irreconcilable needs.

**Fleet health** must be legible at any traffic level. Anything emitted once per
call is unusable: a 5000-call failover would print 5000 lines nobody reads,
starve the log pipeline exactly when the incident happens, and — with a
blocking writer — apply back pressure to the SIP tasks whose latency is the
thing under investigation.

**Single-call forensics** needs the opposite: the complete story of one call —
every wire message, every rule-engine transition, every decision-engine round
trip — with original timestamps and cross-process correlation across proxy,
worker and takeover backup.

The pre-existing state served neither: ~80 `eprintln!` sites, unstructured, all
on the blocking stderr path, with no per-call correlation and no export.

## Decision

Two independent planes. `tracing` is the single emission facade; a new
`crates/observe` owns subscriber composition, the admission chain and the
OpenTelemetry dependency tree. Domain crates depend on `tracing` and the thin
helpers only.

### 1. Lifecycle logs — traffic-independent, lossy, non-blocking

- `info` level, human-readable single-line `key=value` (fmt compact), always to
  stdout, never conditional on tracing being enabled.
- Written through a **bounded lossy** channel drained by one dedicated writer
  thread. A full queue DROPS the line and bumps `log_lines_dropped_total`. A SIP
  task never blocks on stdout — dropped observability is strictly preferable to
  a stalled call path.
- **Aggregation discipline** (the hard requirement): state transitions and rare
  events get an individual line with full context; per-call event classes get a
  rising-edge line, a ~5 s periodic summary, and a falling-edge totals line. The
  summary ticker rides `tokio::time`, so a paused-clock test drives it. **No
  per-call info line, ever** — a 5000-call failover produces ~5 lines.
  Mechanised by `observe::WaveSet`: an episode per key (dead peer, failing
  target, shed reason) with named counters, one driver task per open episode,
  and a bounded key space (keys can be wire-influenceable). An episode ends on
  quiet only: a recovery ARMS the falling edge, which lands after the idle
  window with no further failure, and a failure inside that window revives the
  same episode silently. Without that hysteresis a source flapping at its cap —
  a token bucket alternating reject/admit, a backend answering every other
  request — would emit a rising/falling pair per call, i.e. the per-call line
  this discipline exists to forbid.
- Every line carries node identity, and peer / epoch / `(p,b)` where relevant.

### 2. Per-call traces — sampled, OTLP, explicit-guard

- **One root span per call per process**, closed at terminal state or reap.
  Detail is span EVENTS with their original timestamps: SIP messages in and out
  (raw wire bytes), rule-engine transitions (`rule_id`, `machine`,
  `from -> to`), context transitions, limiter admit/refresh/release. Child spans
  exist ONLY for decision-engine and limiter HTTP round trips (bodies as
  events).
- Every span carries `sip.call_id` plus the From/To tags.
- **16 KiB per-attribute cap**, with a `truncated=true` marker so a prefix is
  never mistaken for a whole value.
- **The two planes are separated by `tracing` TARGET.** Every per-call span and
  event is emitted under the single target `sip::trace`; the stdout fmt layer
  carries a per-layer filter that excludes it, the OTLP layer does not. Without
  that separation a traced call's wire bytes would render as stdout `info`
  lines — per-call logging, the thing §1 forbids — and would crowd genuine
  lifecycle lines out of the bounded lossy writer exactly when an operator needs
  them. A shared level/target filter cannot do this: muting the trace plane for
  stdout would starve the exporter with it.

#### Explicit-guard discipline

Every per-call emission site is wrapped in `if call.sampled { … }`. An unsampled
call constructs no span object, evaluates no format argument and performs zero
additional allocations. Subscriber-side filtering is NEVER the mechanism: it
still pays argument evaluation and field construction on the hot path, which is
precisely the cost this design refuses.

#### Cost guarantee when no collector is configured

The exporter is enabled ONLY by `OTEL_EXPORTER_OTLP_ENDPOINT`. With it unset the
**entire sampling machinery is inert**: no draw is taken, no span is created,
and an activation attempt bumps `trace_dropped_no_exporter_total` and returns.
A process with no collector therefore pays one boolean check per call.

That env var is an OTLP/HTTP **base** url, exactly as every OpenTelemetry SDK
reads it — the exporter resolves `<base>/v1/traces` itself. `observe` reads the
var only to decide WHETHER to export and never passes an endpoint
programmatically, because a programmatic endpoint is taken verbatim and would
silently diverge from what an operator expects to configure.

### 3. Sampling and activation — monotonic, decided once

Sampling is enable-only: once a call is sampled it stays sampled; nothing
revokes it mid-call (a half-traced call is worse than an untraced one).

Entry paths:

1. **Default draw** — Bernoulli at the configured rate, default `1e-4`.
2. **Header override** — `X-Trace-Sample: <float 0..=1>` overrides the draw
   RATE for that call.
3. **Engine force-enable** — a dedicated optional `"trace": bool` field
   (serde-default `false`) in the decision-response schemas (ADR-0017
   contract). On activation at `apply_route`, the call is **backfilled**: the
   INVITE received, the auto-100 sent, and the decision request/response are
   emitted as events with their ORIGINAL timestamps. Nothing is buffered for
   unsampled calls — the backfill reads facts the call already carries.

All three pass the SAME admission chain, in order:

> positive draw → token bucket (burst 10, refill 1/s) → `trace_max_active`
> concurrent cap (default 200)

A denial bumps a counter and is **never a log line** — a refused sample must not
become the traffic-proportional output that sampling exists to avoid.

#### Env-gate trust model

`X-Trace-Sample` is attacker-controllable. It is honored ONLY when the process
was started with `SIP_TRACE_HEADER=1` (lab and endurance rigs). When that env is
unset the header is not even looked up. This is a deployment-level gate, not a
per-request authorization check: a serving edge simply cannot be steered into
tracing by a caller. A malformed value is ignored in favour of the configured
rate and counted (`trace_header_malformed_total`).

Header extraction lives ONLY in `sip-message` (full-parse path) and
`sip_message::sniff` (the proxy's raw INVITE path), per the repo-wide rule that
no other crate extracts SIP headers. The reader keeps "absent" and "present but
unreadable" apart (`TraceSample::{Absent, Malformed, Rate}`) so a rig that
mistyped its rate does not look like a call that asked for nothing.

The engine force-enable is a `trace: bool` on `RouteDecision` — the treatment
that carries a call forward, and therefore the one that can turn its trace on.
`#[serde(default)]`, so an engine that never heard of tracing is unchanged.

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

The proxy samples **independently** of the worker. There is no traceparent, no
`X-B3-*`, no span context on the SIP wire, and therefore no trust machinery to
decide whether an inbound context may be believed. Correlation is by the
`sip.call_id` attribute alone.

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

`Call.trace_id`, `Call.root_span_id` and `Call.sampled` already exist on the
replicated `Call` and are populated. On takeover the backup opens its OWN root
span with the replicated `trace_id` and a **span LINK** to the nominal's
`root_span_id` — not a parent. The nominal's span is closed (or lost) by
definition at takeover; linking records the causal relationship without claiming
a live parent that will never close. A core's root spans die with the core: they
are runtime state, never replicated, so a survivor always opens its own.

Adoption runs at each hydration site **on the copy the store keeps, under the
residency check** — a node opens a span only for a call it goes on to serve.
Adopting a copy the store then discards would register a root span that no
stored call names, and the next takeover would link to a root no process ever
served.

### 6. Deployment compositions

Which composition a process belongs to is expressed entirely by the two env
vars, so the same binary serves all of them:

- **dev/lab and endurance** (`deploy/k8s/run.sh`, which endurance drives): both
  the worker and the proxy manifests carry `OTEL_EXPORTER_OTLP_ENDPOINT` pointed
  at the host stack's VictoriaTraces and `SIP_TRACE_HEADER=1`. The values are
  stamped by `envsubst` at deploy time because the host's address from the
  cluster's point of view is the kind bridge gateway, which is not a fixed
  address on WSL2.
- **production**: both empty. Empty is the meaningful value — not a missing
  key — so a composition never has to remember to delete a line to stay inert.

Traces are stored in VictoriaTraces alongside the existing VictoriaMetrics /
VictoriaLogs, read in Grafana through a Jaeger-protocol datasource, and share
their lifecycle (`install.sh --bootstrap|--apply|--down|--status`). Operating
detail: [docs/observability.md](../observability.md).

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
  gains no transitive export tree.
- Log output is lossy under extreme pressure. That is deliberate and measurable:
  `log_lines_dropped_total` is scraped alongside the trace-denial counters.
- A collector outage degrades to "no traces": the batch exporter drops, and the
  SIP path is unaffected because nothing on it ever awaits the exporter.
- Adding a per-call `info!` anywhere is a review must-fix — it breaks the
  traffic-independence guarantee that makes the log stream usable at all. The
  standing directives for new code live in
  [docs/observability.md](../observability.md) and CLAUDE.md.
- A failing scenario now self-documents twice: the wire trace and what the SUT
  logged and traced while producing it.
