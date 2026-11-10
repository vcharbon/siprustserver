# The B2BUA's decision core is a pure crate; a turn is a function of its inputs

**Status:** accepted

**Owner:** `crates/b2bua-core` (the core: rules, the turn, the folds, the effect vocabulary),
`crates/b2bua` (the shell: router, interpreter, store, replication, timers, reaper, limiter
worker, decision engines, CDR writer, overload, metrics, the trace registry), `crates/sip-ids`
(the id source), `crates/b2bua-sdk` (the rule SDK the core builds on), `crates/call` (the
duration conversions).

Applies ADR-0051 (Consequences: "the seam has its own ADR"). Amends ADR-0010 X5 (the action
executor draws its identifiers from the turn), ADR-0026 (a rule's trace facts are returned, not
emitted) and ADR-0020 X6 (the unrecoverable call).

The [appendix](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md) holds the full context,
the long form of sections 1 to 4, 7 to 9 and 11, the behaviour and tests that follow, and the
implementation order.

## Context

The rules, the action vocabulary, the invariants and the obligation set are synchronous and read
only the call, the event and configuration. ADR-0051 makes a pure crate's output depend on its
inputs alone. A call's decisions can be held to that rule, replayed, compared and driven by a
property lane only once they live in such a crate.
Detail: [appendix, Context](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#context).

## Decision

### 1. Core and shell

`b2bua-core` (tier pure) holds what decides: the rule engine and every rule family, the per-turn
framework `rule_chain_turn` with its screens and steps, the invariants and obligation set, relay
construction, the admission set (ADR-0040), the decision DTOs and folds, the effect vocabulary,
the landing pass (§6) and the constants the rules read. `b2bua` (tier shell) holds what acts and
re-exports the core's public modules under their old paths.
Detail: [appendix §1](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#1-core-and-shell).

### 2. A turn and its inputs

A turn is one run of the core over one call under the call's lock. The core takes the
configuration, rule list and obligation set by borrow, the call by value, the event by
reference, the event's resolution as plain values, `now_ms` (the turn's one clock reading), the
wire faults as a `Copy` set (§5), `&mut TurnIds` (§3) and `&mut TurnNotes` (§4). The core never
sees a handle, a lock, a channel or a clock.
Detail: [appendix §2](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#2-a-turn-and-its-inputs).

### 3. Identifiers

Every identifier a turn mints is a draw of its `TurnIds`, HMAC(key, `turn_seq` ‖ `n`). The key is
drawn from OS entropy once per process incarnation and is shared with the shell's counter
stream, whose inputs never collide with a turn's. The shell allocates `turn_seq` from one counter
per process. Nothing about it is replicated. Under a fixed test key a turn is reproducible.
Detail: [appendix §3](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#3-identifiers).

### 4. What a turn returns

A turn returns the `HandlerResult` and writes `TurnNotes`: the trace facts of a sampled call, the
counts owed to the metrics, and the defects met mid-turn. None of them panics, in any build. The
shell drains the notes as each core call returns, before it records or sends anything else.
Detail: [appendix §4](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#4-what-a-turn-returns).

### 5. Wire faults

The fault seam stays a shell handle a test arms and disarms. The shell reads it once at the
turn's start into a `WireFaultSet`, a plain `Copy` value the core reads at each guarded emission.

### 6. The landing pass

Every result the shell persists first takes the core's landing pass, with its inputs as values:
the answer deadlines of the requests the turn sent off its turn (the budgets of §2), the refresh
arming (the refresh period), the stated headers on every outbound message, the message ring's
record of what the turn sends and the turn's seal. The interpreter calls it. The model invariant
check, persistence and effects follow.

### 7. The initial INVITE

The initial INVITE keeps its awaiting orchestration in the shell (trace intake, decision consult,
limiter admit, failover consult, the clock readings). Each step between two awaits is a core
fold. A decided final the stack refuses, or an engine's stated refusal with a code that is no
non-2xx final, draws `500` with the decision's headers kept.
Detail: [appendix §7](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#7-the-initial-invite).

### 8. The model invariant oracle

The model invariant check and the defect report stay in the shell, in every build. Neither
panics. Each violation is recorded, logged and counted in `b2bua_model_defects_total{invariant}`,
and the turn lands. A harness with the flag on fails its run on a recorded violation.
Detail: [appendix §8](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#8-the-model-invariant-oracle).

### 9. What moves out of the shell crates the core needs

`CallEvent::from_txn` lives in the shell's router, the IP-literal readers in `sip-message`, the
duration conversions and `TxnKind` in `call`.
Detail: [appendix §9](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#9-what-moves-out-of-the-shell-crates-the-core-needs).

### 10. Dependencies

`b2bua-core` depends on `call`, `sip-message`, `b2bua-sdk`, `sip-ids`, `sip-retransmit` and
`lookup-map`, and on `serde`, `serde_json`, `thiserror` and `tracing`. `tracing` brings
`tracing-core`, `once_cell`, `pin-project-lite` and `log` (its facade) into the pure dependency
allowlist. A `LookupMap` / `LookupSet` serves a function's local lookups only and never appears
in `Call` or in an effect. Where order reaches output the core uses a `BTreeMap`.

### 11. The caller's INVITE, and an unrecoverable call

The core rebuilds the caller's INVITE from the call's snapshot only in a step that reads it,
through one fallible accessor (`relay::caller_invite`). Every entry that makes a body resident
checks that its snapshot rebuilds: at creation the caller is refused, at materialisation the copy
is discharged silently as unrecoverable, and at a fold the fold is refused. A step that fails to
rebuild all the same is a defect, and the shell discharges the call.
Detail: [appendix §11](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#11-the-callers-invite-and-an-unrecoverable-call).

### 12. The panic set

The core's panic set reaches `deny` in a step of its own after the move. Every panic site of
`b2bua` is in the code that moves, so `b2bua`'s own crate root takes the deny with the move
itself. The panic-sites ratchet holds a crate at zero to its zero attribute.

## Consequences

- A call's decisions replay from their inputs under a fixed test key: the same call, event,
  configuration, `now_ms`, key and `turn_seq` give the same result, effects and notes.
- The property lane drives `rule_chain_turn` over generated event sequences with no runtime, and
  the decision JSON through the folds.
- Each action that draws an identifier takes the executor by `&mut`.

Behaviour and tests that follow from the split:
[appendix, Consequences](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#consequences).
The steps of the move:
[appendix, Implementation order](appendix/0052-the-b2bua-decision-core-is-a-pure-crate.md#implementation-order).
