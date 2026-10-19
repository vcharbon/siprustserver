# Production crates never panic on input; pure crates do no effects

**Status:** accepted (2026-10-18). Its checks land crate by crate through the panic-sites
ratchet; a crate is held to all of it once it is at `deny`.

**Owner:** the tooling of `just verify` (tier, closure and ratchet checks), the root
`clippy.toml`, every crate linked by a production binary.

## Context

A SIP server takes bytes from any peer. A panic on them costs more than the packet:

- Datagrams are parsed inside loops every call shares (the transaction owner, the proxy's
  receive shards). A loop that ends makes the process exit, and a UDP peer retransmits the same
  datagram, so one bad packet becomes a crash loop of every node behind the proxy.
- A panic in a call's handler task is contained to that call, but only because panics unwind
  and the task boundary catches them.
- Stack overflow (recursion driven by input) and allocation failure abort the process; no panic
  boundary sees them.

Clippy sees only the crate's own syntax: indexing and `unwrap` are visible, a panic inside a
library call (`split_at`, `Vec::remove`, `DelayQueue::insert`, `Instant + Duration`) is not.

## Decision

### 1. Two tiers, declared per crate

Every crate linked by a production binary declares `[package.metadata.purity] tier = "pure"`
or `"shell"`. A crate of a production closure that declares no tier fails verify; test-support
crates declare none, so none of them can enter a production binary.

- **pure**: normal dependencies only on pure crates and an allowlist of effect-free libraries;
  no clock, randomness, filesystem, network, environment or sleep; no interior mutability
  (`Atomic*`, `Mutex`, `RwLock`, `RefCell`, `Cell`, `thread_local!`). Time and identifiers enter
  as parameters, so a pure function's output depends only on its inputs. `tracing` is allowed:
  it cannot change an outcome. A process-global registry is not. A std `HashMap` / `HashSet`
  serves lookups only: its random seed defends a peer-keyed map, and its iteration order is
  that seed, so a pure crate never iterates one; where order reaches output it uses a
  `BTreeMap` / `BTreeSet` or an `IndexMap` / `IndexSet` (insertion order).
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
  reason and the checked alternative.
- `arithmetic_side_effects` per module, where protocol numbers are computed (sequence numbers,
  versions, lengths and durations read from a message) and in the parser's cursor. Elsewhere
  offset arithmetic moves into cursor methods and checked access (`get`, `split_at_checked`,
  `strip_prefix`) instead of being checked operation by operation: the input is bounded by the
  datagram, release does not check overflow, and a wrapped offset meets a checked access and
  becomes a parse error. Rewriting every `i + 1` to checked or saturating form would add error
  branches no input reaches and invite silent defaults. Deadline arithmetic goes through one
  checked helper of the clock crate. Division by an input value is checked by hand.
- `#[expect(clippy::…, reason = "…")]` only where an access is provably safe (a constant table),
  on the statement or item of that one site.
- `#[no_panic]` is not used: allocation defeats it and its guard needs `unsafe`.

### 3. Mechanics

One root `clippy.toml`: clippy reads the nearest one and does not merge. Every member
inherits `[lints] workspace = true`; a crate with audited `unsafe` restates the workspace
table with `unsafe_code = "deny"` as its one difference, which the tier check verifies. A
ratchet pass runs clippy with the set force-warned on `--lib --bins` of every tiered crate and
holds per-file counts in `panic-sites.allow`, so no workspace `-D warnings` gate sees the set
early and no source attribute hides a site from it. An entry goes down, or up only by sites
git sees moved into its file from a counted site elsewhere and by sites a rule the base did
not count reveals (a lint or list entry the change adds); the total rises by the revealed
sites alone. At zero
the crate carries `#![cfg_attr(not(test), deny(…))]` with the eleven lints at its roots
(a pure crate adds `iter_over_hash_type`, which the pass counts in pure crates only),
which exempts all test code; the disallowed lists stay with the pass for every tiered crate,
since one list holds both tiers' entries, and a crate whose remaining sites are all of the
lists keeps its attribute.

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

### 6. Errors replacing panics

A function that can fail on input returns an error; a signature that was infallible changes
and its callers follow. A rewrite never introduces a default that changes a result
(`unwrap_or_default`, `unwrap_or(0)`). In the B2BUA, an unreadable SDP offer is refused as an
unacceptable offer (488); an unreadable answer fails the negotiation like an answer with no
common format.

### 7. Locks in the shell

A poisoned lock never stops the node. Every std lock goes through one helper that recovers the
guard, clears the poison, and logs and counts the recovery per named lock; a recovery alerts.
A critical section writes only values computed before the lock and writes its source of truth
last, so a panic leaves at most derived entries ahead of it. The store's periodic sweep
reconciles derived state with the source of truth on every pass (orphan entries dropped,
missing stamps added, a key owned by a live call never moved) and counts each correction by
kind.

### 8. Property lane and fuzzing

A no-panic property lane (stable toolchain, slow lane, committed regressions, fixed case count,
synthetic seeds, overflow checks and debug assertions on, small stack) runs over every entry
point that takes bytes or decoded peer data: message parse, typed header reads, SDP, multipart,
the transaction layer's packet handling, the proxy's per-datagram path, replication frames and
call snapshots, decision and limiter DTOs, timer arming from a snapshot, and the B2BUA's turn
over generated event sequences. Mutations follow the protocol's structure (truncation at every
offset, spliced and duplicated headers, extreme numeric values). Coverage-guided fuzzing runs
in its own workspace as discovery only; each crash becomes a committed regression case.

## Consequences

- The B2BUA's decision core becomes a pure crate split from its effectful shell; the seam has
  its own ADR.
- Test-support code compiled into production crates moves into crates of its own, or behind
  a `testkit` feature of its crate when that crate's own unit tests use it; the closure check
  refuses the feature in every production binary.
- Rare failure outcomes change where a panic becomes an error: a parse failure drops one
  packet, a call whose handler failed is torn down on the wire.
