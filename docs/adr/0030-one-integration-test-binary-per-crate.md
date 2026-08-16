# A crate's integration tests compile into one binary

**Status:** accepted (2026-08-15, applied to `b2bua-harness`)

## Context

Cargo builds one binary per `tests/*.rs` file, and each of those binaries
statically links the entire dependency graph. This workspace had 188 such
files — 73 in `b2bua-harness` alone — so the dev tree paid for 188 copies of
tokio, the SIP stack and every harness crate.

Measured on `-p b2bua-harness --tests`, cold, before and after folding its 73
files into a single `tests/it/` target:

| layout               | cold build | `target/` |
| -------------------- | ---------- | --------- |
| 73 test binaries     | 19.3 s     | 5676 MB   |
| 1 test binary        | 13.2 s     | 1034 MB   |

The inner loop moves further than the cold build does: touching
`crates/b2bua/src/lib.rs` and rebuilding went from ~9 s to **1.9 s**, and
touching a single test file to **1.1 s** — 72 links stop happening.

This is the largest build lever in the workspace by an order of magnitude; the
codegen backend (ADR-0029) is worth ~13 % against it.

## Decision

### X1 — One `tests/it/` target per crate; `tests/*.rs` stays empty

Integration tests live in `tests/it/<name>.rs` with one `mod <name>;` line in
`tests/it/main.rs`. A file left directly in `tests/` is a second binary and a
second copy of the graph — the cost is invisible at review time, which is why
the rule is stated rather than left to judgement.

Shared helpers keep working unchanged: `tests/it/common/mod.rs` is declared
once in `main.rs`, and files that used to say `mod common;` say
`use crate::common;`.

### X2 — A test that owns process-global state keeps its own binary

One binary means one process, and libtest runs its tests concurrently in it.
A file that installs process-wide state is therefore excluded from `it/` and
stays a `tests/*.rs` target of its own — the process boundary IS its isolation
mechanism, and folding it in does not merely slow the suite, it changes what
the other tests observe.

In `b2bua-harness` that is the five trace tests (`per_call_trace`,
`trace_wire_fidelity`, `trace_force_enable`, `trace_failover_force_enable`,
`trace_enforced_transition`): each calls `install_process_traces` and
`observe::test_buffer`, and the trace registry is one root span per call **per
process**. Folded in, they turn sampling on underneath every other test and the
suite wedges — `per_call_trace.rs` already said so in its own module doc.

Other crates carry the same shape and are excluded for the same reason:
`sip-message`'s `alloc_budget`/`perf` tests read a global allocation counter
(`crates/alloc-counter`), `media`'s `rtp_media_live` binds real UDP, and
`scenario-harness`'s `artifact_on_drop` sets process environment.

The rule is not "consolidate everything". It is: consolidate the tests that do
not care what shares their process, and leave the rest alone.

### X3 — Test selection is module-qualified

`cargo test -p b2bua-harness --test <file>` becomes
`cargo test -p b2bua-harness --test it <module>::`, and a test's reported name
gains its module prefix. Filters that named a bare test function still match.

## Consequences

- A hard abort (not a panic — libtest unwinds those per test) takes the whole
  crate's suite down instead of one file's. No test in `b2bua-harness` aborts.
- Peak memory during a run is higher: ~24 concurrent tests from across all 73
  modules rather than from one file. Bound it with `--test-threads` if a
  constrained box struggles.
- Touching one test file recompiles the crate's whole test target. That is the
  1.1 s measured above — cheaper than the link it replaces.
