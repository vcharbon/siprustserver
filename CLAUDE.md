# SIP Rust server

Project is ultra early, not in production, do not worry about upgrade
compatibility when designing solutions.
To show a html file or a URL to the user, `xdg-open ./path/to/file/index.html` / `xdg-open "http://localhost:.../"`.

## Coding rules

Doc comments state present-tense contracts and invariants. History (dates, commit hashes, ticket IDs, "previously/replaces/no longer") lives in git and ADRs — a comment may cite ADR-00xx in one line, never retell it. If a comment needs more than ~5 lines to justify a behavior rather than describe it, either the behavior is wrong — write FIXME(scope): <one-line defect + one-line fix direction> — or the rationale is architectural and belongs in an ADR with a one-line pointer. By default however do not write FIXME, implement correct behavior unless specifically asked to delay specific corner cases.
Each file must have it own concern. No not mix concerns.
Never implement SIP header or message extraction in crates other than sip-message.
Formatting is `cargo fmt` under the root `rustfmt.toml` (stable options only, shared byte-for-byte with newkahsip). Run `just fmt` before a commit; `.githooks/pre-commit` (installed by `just hooks`) refuses an unformatted stage — never hand-format around it.


## Where the details live (progressive disclosure)

Read the linked doc BEFORE the matching kind of work; the sections below are
summaries + hard directives only.

- [docs/testing/test-clock.md](docs/testing/test-clock.md) — **driving time in
  paused-runtime tests. Read before writing or modifying ANY timed test** —
  most flaky/broken tests here come from breaking one of its rules.
- [docs/testing/harness-layers.md](docs/testing/harness-layers.md) — the
  test-infrastructure stack: which harness for which test, what `finish()`
  gates automatically vs what you must assert.
- [docs/testing/ha-acceptance.md](docs/testing/ha-acceptance.md) — HA/chaos/
  endurance triage: SUT bug vs accepted collateral vs infra artifact, and the
  HA design invariants that must never be reintroduced.
- [docs/observability.md](docs/observability.md) — **read before adding any
  `info!` or any per-call trace emission**: operating the lab trace stack, plus
  the directive guidelines for new code.
- [docs/adr/](docs/adr/) — decisions. Load-bearing day-to-day:
  [ADR-0014](docs/adr/0014-reactive-only-takeover-version-vector.md) (reactive
  takeover, `(p,b)` reconciliation),
  [ADR-0022](docs/adr/0022-initial-invite-final-response-guarantee.md)
  (initial-INVITE final-response guarantee).

## Default test requirements (every test — no exceptions)

Every test models a complete, functioning callflow:

- **RFC-compliant on the wire.** The recorded-trace RFC 3261/3262/3264 audit
  gates at `finish()` (and at Drop if you forget `finish()`). A deliberate
  violation is sanctioned ONLY via `allow_violation(rule, justification)`, ONLY
  when the test's *purpose* is interworking with a buggy/lossy peer, and the
  delta must sit on the **Alice/Bob (peer) side — the SUT's own output stays
  compliant**. Never waive a rule to mute a finding caused by the SUT; that is
  a bug to fix.
- **Properly terminated — no exceptions.** Every call reaches a terminal state
  before the test ends: BYE/CANCEL/error-final in the flow. If the scenario
  itself is a lost or withheld teardown (dead peer, dropped 200), the worst
  case is still termination — advance the paused clock past the SUT's own
  dead-call detection (keepalive timeout, the 32 s terminating safety timer,
  `GlobalDuration`) so the B2BUA detects the dead call and reaps it. Never
  leave a call up at `finish()`.
- **Assert release, not just silence.** `finish()` does NOT catch a leaked
  call (structural leak anomalies deliberately don't gate). After any
  timeout-path termination, drain with `settle_until` then assert
  `B2buaSut::assert_fully_reaped()` (or `assert_call_fully_over` in failover
  tests).

## Writing a new b2bua / failover test

Do NOT hand-roll the INVITE/180/200/ACK dance — it lives once in
`scenario_harness::callflow`. Single-SUT b2bua test: use `B2buaScene::new(name)`
(alice :5060 / bob :5070 / b2bua :5080, routes to bob) then `scene.establish()`
→ interesting part → `scene.hangup(&mut dialog)` → `scene.finish()`; for a
non-default decision use `B2buaScene::with_b2bua(name, |bob_port| …builder…)`.
HA failover test: `scenario_harness::callflow::establish(&alice,&bob,proxy.addr())`
(or `Call::new(..).no_ring()` for the 200-only variant) and `hangup` for teardown.
ONLY for the uninterrupted happy-path setup — any dance that asserts on the 18x,
reads the relayed cookie/SDP, or injects a crash/partition mid-handshake stays
hand-rolled (those are the subject of the test). When unsure, hand-roll it.
Full stack map: [docs/testing/harness-layers.md](docs/testing/harness-layers.md).

## Test clock — summary ([full guide](docs/testing/test-clock.md))

Behaviour rides `tokio::time` directly; `Clock::test_at(0)` reads the same
timeline, so one `advance` moves behaviour timers *and* report timestamps —
there is **no separate fake-clock counter** (deliberately simpler than the TS
`TestClock` pump; keep it that way). Tests use
`#[tokio::test(start_paused = true)]` + `Harness::advance` (100 ms chunks),
called *between* protocol steps, exactly to the deadline being tripped. The
non-negotiables (details + smell table in the guide): never leap two deadlines
in one advance; never feed a paused test a real wall-clock signal; transit
delay ≥ 1 ms, never 0; don't hand-roll timer drivers — copy the epoch +
lockstep-`Key` shape in `crates/b2bua/src/timers.rs` (module doc) if you must.
A panicking scenario auto-dumps its wire trace to stderr (`PanicDump`) — read
that before instrumenting.

## Test-runtime policy (default vs slow lane)

**An integration test that takes >60 s of wall-clock on the REAL clock must not
run by default.** Mark it `#[ignore = "real-clock >60s — slow lane (just
test-slow)"]` and keep a fake-clock (`start_paused`) equivalent of the scenario
in the default lane — writing one if missing is the point of the rule. Lanes
live in the `justfile`: `just test` (default), `just test-slow`
(`cargo test --release -- --ignored`). Paused-clock tests are exempt from the
60 s rule but not free — before `#[ignore]`-ing a slow one, cut the timer churn
at its source (see the clock guide, rule 5).

## Compiling ([ADR-0029](docs/adr/0029-dev-build-cost.md))

`just` is the entry point — run it bare to list the lanes. `just check` for the
fast signal (no codegen), `just test [filter]` / `just test-slow`, `just lint`,
`just image` for the k8s image, `just doctor` when a machine looks broken,
`just disk` / `just clean-incremental` under disk pressure. Every recipe is a
plain cargo call, so a hand-typed `cargo test` behaves identically.

Prerequisites: mold and gcc >= 12. Do not:

- set `RUSTFLAGS` — it replaces `[build] rustflags` wholesale, silently
  dropping `tokio_unstable` *and* mold; anything that must set it re-states
  both flags;
- change `profile.dev`'s codegen backend — Cranelift is rejected (ADR-0029 X3),
  and re-proposing it means re-running the three panic tests named there;
- put profile settings in `.cargo/config.toml` — profile shape lives in
  `Cargo.toml`, toolchain/linker wiring in `.cargo/config.toml`.

## Adding an integration test ([ADR-0030](docs/adr/0030-one-integration-test-binary-per-crate.md))

A crate's integration tests compile into ONE binary. In a crate that has a
`tests/it/` directory, a new test file goes **inside it**, with a `mod` line
added to `tests/it/main.rs`; a stray `tests/foo.rs` links a second copy of the
whole dependency graph. Select one with
`cargo test -p <crate> --test it <module>::<name>`.

A test that installs process-global state (a trace registry, the allocation
counter, a real socket, process env) stays a `tests/*.rs` target of its own —
folded in, it does not fail the suite, it wedges it.

## Agent & build concurrency

**Never run more than ONE agent (subagent / workflow stage) at a time that
compiles or runs tests.** Builds serialise on the target directory and a build
racing a running SUT or loadgen skews what the run measures. On a host short on
memory, cap a heavy command in a scope:
`systemd-run --user --scope -q -p MemoryMax=12G cargo build … --jobs 6`; a
long-lived test process (SUT, loadgen, e2e-web) then gets its own small scope.

## HA / chaos — summary ([acceptance guide](docs/testing/ha-acceptance.md))

A failover/kill failure is NOT automatically a SUT bug. Protected: ringing and
established calls — a confirmed call dropping is always genuine. Accepted
collateral: a dialog whose state changed within the acceptance window (default
200 ms) of a kill (the confirm-race); the loadgen auto-buckets `chaos="near"`
vs `chaos="clear"` — triage `clear`. A takeover keepalive firing ~one interval
early on an endurance run is a host clock-STEP artifact (fix is infra; the SUT
re-anchors restored timers to bound the residual). Design invariants — never
reintroduce: time-based settle/handback (reconciliation is `(p,b)`-causal
only), smoothing or skew re-anchoring inside the timer driver, reclaim
discharge touching the SIP wire, a non-pristine reboot. Details + references
in the guide.

## Logging & tracing — summary ([guide](docs/observability.md), [ADR-0026](docs/adr/0026-observability-logging-tracing.md))

Two planes, never mixed. **`info!` is lifecycle ONLY** — state transitions, HA,
startup, readiness, drain, long-term peer state — and MUST be
**traffic-independent**: a 5000-call failover prints ~5 lines. A per-call event
class goes through `observe::WaveSet` (rising edge / ~5 s summary /
falling-edge totals); **a per-call `info!` line is a review must-fix**. Per-call
diagnostics go in the per-call trace or at `debug!`. Every line carries node
identity (+ peer / epoch / `(p,b)` where relevant).
Per-call **traces**: the choke points (SIP messages, rule/context transitions,
limiter) are already hooked — most work adds nothing. The ONLY class new code
adds is per-call external-service I/O (a new per-call HTTP/remote dependency) as
a child span with its request/response. **Every emission site sits behind
`if call.sampled { … }`** — an unsampled call evaluates no format arg and
allocates nothing; subscriber-side filtering is never the mechanism. Tracing is
inert unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set; `X-Trace-Sample` is honored
only under `SIP_TRACE_HEADER=1` (lab/endurance) and, like every SIP header, is
extracted ONLY in `sip-message` / `sip_message::sniff`.

## Initial-INVITE final-response guarantee — summary ([ADR-0022](docs/adr/0022-initial-invite-final-response-guarantee.md))

Read the ADR before touching the decision seam, `invariants::enforce`, or the
proxy's response path. Once sip-txn auto-sends 100 Trying, the caller MUST get
a final: accept + forward, or 503. Two mechanisms — do not remove either:
`DeadlineDecisionEngine` bounds `new_call`/`call_failure` (NOT `call_refer`;
X1), and `invariants::enforce` synthesizes the 503 for any a-leg terminated
unanswered (X2). The `answer_unanswered_a_leg` flag is a **correctness knob**:
`true` only on live-serving funnels; `false` on the two HA discharge helpers —
reclaim discharge never touches the wire (X5, and
[ha-acceptance.md](docs/testing/ha-acceptance.md)). The LB proxy is
transaction-less BY DESIGN: never emits/relays a 100, no Timer C, the caller
owns downstream-blackhole give-up (X4, pinned by
`stateless_final_response_contract.rs`).
