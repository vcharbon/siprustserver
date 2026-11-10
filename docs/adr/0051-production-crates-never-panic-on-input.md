# Production crates never panic on input; pure crates do no effects

**Status:** accepted. Its checks land crate by crate through the panic-sites
ratchet; a crate is held to all of it once it is at `deny`.

**Owner:** the tooling of `just verify` (tier, closure and ratchet checks), the root
`clippy.toml`, every crate linked by a production binary.

The [appendix](appendix/0051-production-crates-never-panic-on-input.md) holds the context and the detail of §1 to §3, §5, §7 and §8.

## Context

A SIP server takes bytes from any peer. A panic in a loop every call shares exits the process,
and a UDP peer's retransmission of the same datagram makes it a crash loop. Stack overflow and
allocation failure abort past any panic boundary. Clippy sees only the crate's own syntax, not a
panic inside a library call. Detail: [appendix, Context](appendix/0051-production-crates-never-panic-on-input.md#context).

## Decision

### 1. Two tiers, declared per crate

Every crate linked by a production binary declares `[package.metadata.purity] tier = "pure"`
or `"shell"`. A crate of a production closure that declares no tier fails verify; test-support
crates declare none, so none of them can enter a production binary.

- **pure**: normal dependencies only on pure crates and an allowlist of effect-free libraries;
  no clock, randomness, filesystem, network, environment or sleep; no interior mutability
  (`Atomic*`, `Mutex`, `RwLock`, `RefCell`, `Cell`, `thread_local!`). Time and identifiers enter
  as parameters, so a pure function's output depends only on its inputs. `tracing` is allowed:
  it cannot change an outcome. A process-global registry is not. A pure crate names no std
  `HashMap` / `HashSet` and looks up through `lookup-map`'s `LookupMap` / `LookupSet`, which
  offer no iteration. Where order reaches output it uses a `BTreeMap` / `BTreeSet` or an
  `IndexMap` / `IndexSet`. Detail: [appendix §1](appendix/0051-production-crates-never-panic-on-input.md#1-two-tiers-declared-per-crate).
- **shell**: effects allowed.
- **both**: the panic set of §2.

Each production binary is built by its own cargo command, and the closure check runs on that
command's resolved features: a `testkit` or `test-util` feature reaching a production binary
fails, whatever crate unified it in.

### 2. The panic set

- Lints, crate-wide: `indexing_slicing`, `string_slice`, `unwrap_used`, `expect_used`,
  `panic`, `unreachable`, `todo`, `unimplemented`, `cast_possible_truncation`,
  `cast_sign_loss`, `cast_possible_wrap`; the `assert!` family disallowed. `debug_assert!` is
  allowed: release compiles it out and the property lane runs it.
- A curated `disallowed-methods` list for panics inside library calls, each entry with its
  reason and the checked alternative. The same list holds the parser's reading-role entries
  (ADR-0053 §6), counted beside the panic set.
- `arithmetic_side_effects` per module, where protocol numbers are computed (sequence numbers,
  versions, lengths and durations read from a message) and in the parser's cursor. Elsewhere
  offset arithmetic moves into cursor methods and checked access (`get`, `split_at_checked`,
  `strip_prefix`). Deadline arithmetic goes through one checked helper of the clock crate.
  Division by an input value is checked by hand. Detail: [appendix §2](appendix/0051-production-crates-never-panic-on-input.md#2-the-panic-set).
- `#[expect(clippy::…, reason = "…")]` only where an access is provably safe (a constant table),
  on the statement or item of that one site.
- `#[no_panic]` is not used: allocation defeats it and its guard needs `unsafe`.

### 3. Mechanics

One root `clippy.toml`: clippy reads the nearest one and does not merge. Every member
inherits `[lints] workspace = true`; a crate with audited `unsafe` restates the workspace
table with `unsafe_code = "deny"` as its one difference, which the tier check verifies. A
ratchet pass counts the set's sites of every tiered crate and holds per-file counts in
`panic-sites.allow`. An entry only goes down, but for sites moved in from elsewhere or revealed
by a rule the change adds. At zero the crate carries `#![cfg_attr(not(test), deny(…))]` with the
eleven lints at its roots (a pure crate adds `iter_over_hash_type`). The same allowlist counts
the loop and recursion sites of ADR-0057 §5. Detail: [appendix §3](appendix/0051-production-crates-never-panic-on-input.md#3-mechanics).

### 4. Abort-class failures

No recursion driven by input; no allocation sized from an input value without a bound; parse
work linear in the input size (a review rule, no timing gate). Runtime thread stack sizes are
set explicitly. A `Drop` body never panics: a panic during unwinding aborts.

### 5. Panic strategy and backstops

`panic = "unwind"`. Two backstops, sound because neither holds partial state:

- an explicit `catch_unwind` around the datagram parse in each shared loop (owned bytes, pure
  parser): a panic drops the datagram and is counted;
- the per-call handler task: a panic is a `JoinError`; on an active call the reaper's first
  strike tears the call down on the wire through the existing teardown (BYE to confirmed legs,
  CANCEL to early legs, the final owed to the caller), the second strike discharges silently.

Every handler panic and every discharge is counted and alerted on: each one is a defect.

A long-lived task of the shell is supervised: one whose work survives in shared state restarts
after a panic (`task-restart`), counted per task in `*_task_restarts_total{task}`; one the
process cannot serve without ends the process, which exits non-zero.
Detail: [appendix §5](appendix/0051-production-crates-never-panic-on-input.md#5-panic-strategy-and-backstops).

### 6. Errors replacing panics

Any input yields a recoverable error; only the packet is refused, and a refused request is
answered with 400 when its parse error carries a partial (ADR-0054). A function that can fail
on input returns an error; a signature that was infallible changes and its callers follow. A
rewrite never introduces a default that changes a result (`unwrap_or_default`,
`unwrap_or(0)`). In the B2BUA, an unreadable SDP offer is refused as an unacceptable offer
(488); an unreadable answer fails the negotiation like an answer with no common format.

### 7. Locks in the shell

A poisoned lock never stops the node. Every std lock goes through one helper that recovers the
guard, clears the poison, and logs and counts the recovery per named lock; a recovery alerts.
A critical section writes only values computed before the lock and writes its source of truth
last, so a panic leaves at most derived entries ahead of it. The store's periodic sweep
reconciles derived state with the source of truth on every pass and counts each correction by
kind ([appendix §7](appendix/0051-production-crates-never-panic-on-input.md#7-locks-in-the-shell)).

### 8. Property lane and fuzzing

A no-panic property lane (stable toolchain, slow lane, committed regressions, fixed case count,
synthetic seeds, overflow checks and debug assertions on, small stack) runs over every entry
point that takes bytes or decoded peer data ([appendix §8](appendix/0051-production-crates-never-panic-on-input.md#8-property-lane-and-fuzzing)). Mutations follow the protocol's structure (truncation at every
offset, spliced and duplicated headers, extreme numeric values). Coverage-guided fuzzing runs
in its own workspace as discovery only; each crash becomes a committed regression case.

## Consequences

- The B2BUA's decision core is a pure crate split from its effectful shell; the seam has its own
  ADR (ADR-0052).
- Test-support code compiled into production crates lives in crates of its own, or behind
  a `testkit` feature of its crate when that crate's own unit tests use it; the closure check
  refuses the feature in every production binary.
