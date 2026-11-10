# SIP Rust server

The project is not in production: a design ignores upgrade compatibility.
To show the user an html file or a URL: `xdg-open <path or URL>`.

## Coding rules

- Doc comments state present-tense contracts and invariants. History (dates, commit hashes,
  ticket IDs, "previously", "replaces", "no longer") lives in git and ADRs. A comment may cite
  ADR-00xx in one line, never retell it.
- A comment that has to justify wrong behavior is never written: report the defect. A comment
  that needs more than ~5 lines to justify a behavior belongs in an ADR, with a one-line pointer.
- Implement correct behavior unless asked to delay a named corner case.
- Each file has one concern.
- SIP header and message extraction lives in `sip-message` and nowhere else.
- A crate a production binary links declares its tier and never panics on input, and a pure one
  does no effects ([ADR-0051](docs/adr/0051-production-crates-never-panic-on-input.md)).
  `just purity` and `just panic-sites` check it. `panic-sites.allow` only goes down, except by
  sites a rule the change adds reveals. The same count covers the loop and recursion sites of
  [ADR-0057](docs/adr/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md) §5: a loop in
  a pure crate is a `for` over a finite collection, a range to a constant or a `len()`, or
  `bounds::bounded`; a recursion descends through a `bounds::Depth`.
- A branch that touches `Cargo.lock` or `.cargo/audit.toml` passes `just audit-gate`. Every
  `[advisories] ignore` entry states who links the crate and why it is acceptable.
- The flow viewer's UI follows [ts/viewer-app/UI-STYLE.md](ts/viewer-app/UI-STYLE.md).
- Formatting is `cargo fmt` under the root `rustfmt.toml` (stable options only). Run `just fmt` before a commit and
  never hand-format around the pre-commit hook (`just hooks` installs it). An amend on `develop`
  is refused: commit on a topic branch.

## Agent models

Delegated work runs on Opus, B2BUA changes included. Each review pass is one adversarial
reviewer on Opus. Fable reviews a change only when the user names it for that change, once. The complexity of a
change does not call for Fable.

## Read before the matching work

- A timed test, written or modified: [docs/testing/test-clock.md](docs/testing/test-clock.md).
  Most flaky tests here break one of its rules.
- Choosing a harness, what `finish()` gates, writing a b2bua or failover test:
  [docs/testing/harness-layers.md](docs/testing/harness-layers.md).
- Lanes, building, adding an integration test, build concurrency:
  [docs/testing/lanes-and-build.md](docs/testing/lanes-and-build.md).
- HA, chaos or endurance triage (SUT bug, declared window or infra artifact):
  [docs/testing/ha-acceptance.md](docs/testing/ha-acceptance.md).
- A change to a reader: [docs/testing/fuzzing.md](docs/testing/fuzzing.md).
- Any new `info!` or per-call trace emission: [docs/observability.md](docs/observability.md).
- Decisions: [docs/adr/](docs/adr/). An ADR over 8 KB keeps its decision in the numbered file
  and its scenarios, tables and evidence in `docs/adr/appendix/`. Most used:
  [ADR-0055](docs/adr/0055-services-are-ha-blind.md) (services are HA-blind),
  [ADR-0014](docs/adr/0014-reactive-only-takeover-version-vector.md) (reactive takeover),
  [ADR-0022](docs/adr/0022-initial-invite-final-response-guarantee.md) (the initial INVITE gets
  a final).
- A term: grep [CONTEXT.md](CONTEXT.md), one line per term, full definitions in `docs/glossary/`.

## Every test, no exceptions

- **RFC-compliant on the wire.** The RFC 3261/3262/3264 audit of the recorded trace gates at
  `finish()`, and at Drop without it. `allow_violation(rule, justification)` is only for a test
  whose purpose is interworking with a broken or lossy peer, and the deviation sits on the peer
  side. The SUT's own output stays compliant: a finding the SUT causes is a bug to fix.
- **Properly terminated.** Every call reaches a terminal state before the test ends. When the
  scenario is a lost teardown, advance the paused clock past the SUT's own dead-call detection
  (keepalive timeout, the 32 s terminating safety timer, `GlobalDuration`) so the B2BUA reaps
  the call.
- **Assert release, not silence.** `B2buaScene::finish()` runs `assert_fully_reaped()` (calls,
  locks, reaper stamps, setup-CANCEL marks, limiter holds); `finish_leaving(leak)` declares
  limiter holds left on purpose. `Harness::finish()` alone does not catch a leaked call: on a
  bare `Harness` and `B2buaSut`, drain with `settle_until(|| sut.is_reaped())` and assert
  `assert_fully_reaped()` (`assert_call_fully_over` in failover tests).
- **The setup dance is not hand-rolled.** `B2buaScene` and `scenario_harness::callflow` hold
  it. Hand-roll only a handshake that is the subject of the test.
- **Paused clock.** `#[tokio::test(start_paused = true)]` and `Harness::advance`, called
  between protocol steps, exactly to the deadline being tripped. Never leap two deadlines in
  one advance, never feed a paused test a wall-clock signal, transit delay at least 1 ms. A
  panicking scenario dumps its wire trace to stderr: read it before instrumenting.
- **Lanes.** A real-clock test of 1 s or more carries
  `#[ignore = "slow lane: real clock >= 1 s"]`, every `loadgen` test
  `#[ignore = "slow lane: loadgen"]`. A fake-clock test stays in the default lane whatever its
  length.
- **One integration binary per crate.** A new test file goes in the crate's `tests/it/` with
  its `mod` line in `tests/it/main.rs`. Only a test that installs process-global state is a
  `tests/*.rs` target of its own.

## Build

`just` lists the lanes. Two agents compile or run tests at once by default; work light on
disk, CPU and memory has no limit. Never set `RUSTFLAGS` (it drops `tokio_unstable` and mold),
never change `profile.dev`'s codegen backend, and keep profile settings in `Cargo.toml`.

## HA

Services are HA-blind (ADR-0055 §1): HA lives only in the aware crates. What an incident may
cost is ADR-0055 §11 and nothing else. A deviation no window declares is a SUT defect. Never
reintroduce: a time-based settle or handback, smoothing or skew re-anchoring inside the timer
driver, a discharge of an already-terminal body answering the caller, a non-pristine reboot.

## Logging and tracing

`info!` is lifecycle only and traffic-independent: a 5000-call failover prints about 5 lines.
A per-call `info!` line is a review must-fix. A per-call event class goes through
`observe::WaveSet`, per-call diagnostics in the per-call trace or at `debug!`. Every per-call
trace emission sits behind `if call.sampled { … }`. New code adds a trace only for per-call
external-service I/O, as a child span.

## The initial INVITE gets a final

Read ADR-0022 before touching the decision seam, `invariants::enforce` or the proxy's response
path. Once sip-txn sent 100 Trying, the caller gets a final: accept and forward, or 503. Keep
both mechanisms: `DeadlineDecisionEngine` bounds `new_call` and `call_failure`, and
`invariants::enforce` synthesizes the 503 for an a-leg terminated unanswered. Only a
live-serving funnel answers an unanswered caller (`UnansweredCaller::Answer`); the HA discharge
helpers pass `Leave` and never touch the wire. The LB proxy is transaction-less by design: no
100, no Timer C.
