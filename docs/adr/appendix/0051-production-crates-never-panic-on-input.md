# Appendix: Production crates never panic on input; pure crates do no effects

The decision part is [ADR-0051](../0051-production-crates-never-panic-on-input.md).

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

## 1. Two tiers, declared per crate

A std `HashMap` / `HashSet` serves lookups only: its random seed defends a peer-keyed map, and
its iteration order is that seed, so a pure crate never iterates one. The rule holds by type: a
pure crate names no std `HashMap` / `HashSet` (a pure-tier `disallowed-types` entry) and looks
up through `lookup-map`'s `LookupMap` / `LookupSet`, which keep the seed and offer no iteration,
no ordered `Debug` and no `Serialize`; the iterating methods stay listed as well. Where order
reaches output it uses a `BTreeMap` / `BTreeSet` or an `IndexMap` / `IndexSet` (insertion
order).

## 2. The panic set

The parser's reading-role entries (ADR-0053 §6) in the `disallowed-methods` list are counted
beside the panic set: a parser built with no reading role is a site in every member, tiered or
not, and the deployment's ingress parser one outside its composition root.

Offset arithmetic outside the `arithmetic_side_effects` modules moves into cursor methods and
checked access instead of being checked operation by operation: the input is bounded by the
datagram, release does not check overflow, and a wrapped offset meets a checked access and
becomes a parse error. Rewriting every `i + 1` to checked or saturating form would add error
branches no input reaches and invite silent defaults.

## 3. Mechanics

The ratchet pass runs clippy with the set force-warned on the lib and bin targets of the
workspace (resolved as the test build resolves them), counts the sites of every tiered crate and
holds per-file counts in `panic-sites.allow`, so no workspace `-D warnings` gate sees the set
early and no source attribute hides a site from it. An entry goes down, or up only by sites git
sees moved into its file from a counted site elsewhere and by sites a rule the base did not count
reveals (a lint or list entry the change adds); the total rises by the revealed sites alone.

At zero the crate's `#![cfg_attr(not(test), deny(…))]` exempts all test code, and the pass counts
`iter_over_hash_type` in pure crates only. The disallowed lists stay with the pass for every
tiered crate, since one list holds both tiers' entries, and a crate whose remaining sites are all
of the lists keeps its attribute. No attribute states the loop and recursion sites of ADR-0057 §5.

## 5. Panic strategy and backstops

A datagram is parsed once, by the parser only: a receive-path hook's parse travels with the
queued packet and the loop takes it.

A runner's boot returns its refusal to the binary's root, which logs it once through the running
log writer and exits with a failure code.

## 7. Locks in the shell

The store's sweep drops orphan entries, adds missing stamps, and never moves a key owned by a
live call.

## 8. Property lane and fuzzing

The no-panic property lane's entry points: message parse, typed header reads, SDP, multipart, the
transaction layer's packet handling, the proxy's per-datagram path, replication frames and call
snapshots, decision and limiter DTOs, timer arming from a snapshot, and the B2BUA's turn over
generated event sequences.
