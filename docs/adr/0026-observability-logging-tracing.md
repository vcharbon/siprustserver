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
  an explicit close — the recovery that ended it — or on an idle window.
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
no other crate extracts SIP headers.

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
a live parent that will never close.

### 6. Tests

- **No OTel machinery in tests.** The harness installs a thread-scoped in-memory
  buffer subscriber: no background task, no real IO, no wall-clock signal
  (docs/testing/test-clock.md — a paused test must never ride one).
- The buffer is dumped on panic alongside `PanicDump`, otherwise discarded.
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
  traffic-independence guarantee that makes the log stream usable at all.
