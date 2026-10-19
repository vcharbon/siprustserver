# The B2BUA's decision core is a pure crate; a turn is a function of its inputs

**Status:** proposed (2026-10-19)

**Owner:** `crates/b2bua-core` (the core: rules, the turn, the folds, the effect
vocabulary), `crates/b2bua` (the shell: router, interpreter, store, replication,
timers, reaper, limiter worker, decision engines, CDR writer, overload, metrics,
the trace registry), `crates/sip-ids` (the id source), `crates/b2bua-sdk` (the
rule SDK the core builds on), `crates/call` (the duration conversions).

Applies ADR-0051 (Consequences: "the seam has its own ADR"). Amends ADR-0010 X5
(the action executor draws its identifiers from the turn), ADR-0026 (a rule's
trace facts are returned, not emitted) and ADR-0020 X6 (the unrecoverable call).

## Context

The rule engine, the action vocabulary, the invariants and the obligation set
are synchronous and read only the call, the event and configuration; no rule
reads a lock, a socket, the clock or the store. They live in the crate that
owns every effect, and five things reach the effectful world from inside them:

- identifiers are drawn from the process's shared generator, an atomic counter
  in the transaction layer's crate;
- the rule trace is written into a process-global registry behind a lock;
- the wire-fault seam is an `Arc<AtomicBool>` a test flips while the node runs;
- the per-turn framework bumps two counters on the metrics handle (a repeat
  ladder's give-up, a message-cap teardown) and reads its inputs through the
  router's handle bag;
- a model defect or an undeclared rule transition, effect or lifecycle action
  met mid-turn panics in debug builds, a defect through a process-global
  record.

Constants and small pure functions the rules need sit in shell files (the
reaper's verdict names, the operator release's topic, the decision request's
standard headers, the refused-decision guards, the IP-literal reader, the
duration conversions). The caller's INVITE is rebuilt from the call's snapshot
of it at about 25 places, and the rebuild panics on a snapshot that does not
serialize, which a replicated snapshot from a peer can be. The initial
INVITE's turn awaits the decision engine, the call limiter and the failover
consult inside the turn, with clock reads around each await.

ADR-0051 makes a pure crate's output depend on its inputs alone. A call's
decisions can only be held to that rule, replayed, compared and driven by a
property lane once they live in such a crate.

## Decision

### 1. Core and shell

**`b2bua-core`** (tier pure) holds what decides:

- the rule engine and every rule family: selection, the action executor and
  vocabulary, the lifecycle close, the default rules, the services' composition
  and seeding (with the configured seed hook), the registry renderer;
- the per-turn framework (`rule_chain_turn`), its pre-rule screens (the timer
  screen, the answer-deadline screen, the admit report's application, the
  decision log's fold, the refused routes' record, the superseded relay's drop)
  and its framework steps (ladder repeat, give-up and settle, reliable
  provisionals, response facts, the message cap's teardown, the quiet-turn
  candidate);
- invariants (`finalize`, `enforce`) and the obligation set with its kinds;
- relay construction and SDP restatement, stack identity, the stated headers,
  the message ring, attempts, timers' arming, ring and answer deadlines;
- the call's admission set (ADR-0040): the admit report and its result event,
  the refresh arming, the release obligation;
- the decision DTOs, the decision error, and the route, reject and redirect
  folds;
- the effect vocabulary (`HandlerEffects`, `HandlerResult`);
- the landing pass a turn's result takes before it is persisted (§6);
- the constants the rules read: the reaper's verdict names, the operator
  release's topic and reason, the decision request's standard headers, the
  refused-decision guards and reason, the failover chain bound, the admit slack.

**`b2bua`** (tier shell) holds what acts: router ingress, resolve, dispatch and
the per-call FIFO; the per-turn gates (store lock, reaper verdict, incarnation,
store fault, materialisation and reclaim, the dialog gate, the re-offer, the
stray CANCEL); the effect interpreter; store and replication; the timer driver;
the reaper and sweep loops; the limiter worker, its batches and its HTTP client;
the decision engine trait and its engines; the CDR writer; overload and
capacity; the ingress admission ladder (ADR-0037); metrics; the trace registry
and intake; the model invariant oracle (§8). The shell re-exports the core's
public modules under the paths it exposed, so its dependents keep their
imports.

The services a deployment composes into the core's rule list come from pure
crates.

### 2. A turn and its inputs

A turn is one run of the core over one call under the call's lock. The shell
opens it before the per-turn gates, since their refusals draw identifiers, and
every shell call into the core's `enforce` runs inside one: the per-call
handler body, the refresh answer, the reaper's discharge, the reclaim
discharges and the folded terminal, an undecided call's settle.

The core takes:

- the per-process values, borrowed: the configuration, the composed rule list,
  the obligation set;
- the call, by value, and the event, by reference;
- the event's resolution as plain values: the source leg's id, the direction,
  the dialog it resolved to, the `callRef`;
- `now_ms`, the turn's one clock reading;
- the wire faults armed at the turn's start, as a `Copy` set (§5);
- `&mut TurnIds`, the turn's identifier source (§3);
- `&mut TurnNotes`, where the turn writes what it reports (§4).

Values the shell computes from its handles enter as values: the answer budgets
(adaptation port timeout, admit budget, failure and release chains), the
limiter's refresh period, and, on the initial INVITE, the instant the b-leg's
INVITE leaves. The core never sees a handle, a lock, a channel or a clock.

The action executor holds the configuration, `now_ms`, the fault set, the
`&mut TurnIds` and the `&mut TurnNotes` of its turn; every action that draws or
reports takes it by `&mut`.

### 3. Identifiers

Every identifier a turn mints (From/To tags, Via branches, Call-IDs, CSeq and
RSeq starts, `Retry-After` picks, the call's incarnation mark, the per-turn
gates' refusal tags) is a draw of the turn's `TurnIds`: HMAC(key, `turn_seq` ‖
`n`).

- The key is drawn from the OS entropy once per process incarnation. The
  shell's counter stream (the transaction layer's branches, stateless refusals
  at ingress, the OPTIONS tagger, the answer owed for a request a queue
  discarded) draws under the same key: a counter draw hashes 8 bytes and a turn
  draw 16, so the two never share an input.
- `turn_seq` is allocated by the shell from one counter per process, never
  reused under a key. A counter per call would not do: a call reclaimed or
  materialised again on a node holding the same key restarts it and draws the
  identifiers it already drew. The initial INVITE's turn keeps one `TurnIds`
  across its awaits.
- Nothing about it is replicated. A takeover copy draws under its own node's
  key.
- `TurnIds` is neither `Clone` nor `Copy`; the fixed-key constructor is behind
  `sip-ids`'s test feature.

A turn is reproducible from its inputs under a fixed test key: the same key and
`turn_seq` draw the same identifiers. A production key is never known outside
its process.

### 4. What a turn returns

A turn returns the `HandlerResult` (the call as the turn leaves it, its effects)
and writes `TurnNotes`, the data that were side effects inside the turn:

- the trace facts of a sampled call, in the order the turn met them: rule fired,
  cursor moved, call transition, lifecycle delivery, timer fire and drop, admit
  report applied, each with the timestamp of its fact (ADR-0026);
- the counts the turn owes the metrics: a repeat ladder's give-up with its
  obligation kind, a message-cap teardown;
- the defects met mid-turn, each with its name and detail: a model defect, a
  rule's undeclared machine transition or side effect, a lifecycle handler's
  action outside what a lifecycle handler may do.

The shell drains the notes as each core call returns, before it records or
sends anything else of the turn: trace facts go into the registry, counts onto
the metrics, defects to the oracle (§8), and the turn lands. Trace facts of an unsampled
call are never built: the core reads the call's sampled flag first, so an
unsampled call allocates nothing for them.

### 5. Wire faults

The fault seam stays a shell handle a test arms and disarms. The shell reads it
once at the turn's start into a `WireFaultSet`, a plain `Copy` value the core
reads at each guarded emission.

### 6. The landing pass

Every result the shell persists first takes the core's landing pass, with its
inputs as values: the answer deadlines of the requests the turn sent off its
turn (the budgets of §2), the refresh arming (the refresh period), the stated
headers on every outbound message, the message ring's record of what the turn
sends and the turn's seal. The interpreter calls it; the model invariant check,
persistence and effects follow.

### 7. The initial INVITE

The initial INVITE keeps its awaiting orchestration in the shell: the trace
intake, the decision consult, the limiter admit, the failover consult, and the
clock readings around them. Each step between two awaits is a core fold:

- the call built from the INVITE, its incarnation mark drawn from the turn;
- the decision request built from the INVITE;
- the hop budget: a request whose budget is spent takes its 483;
- the route applied up to its admit (the destination allow-list, features,
  subscriptions, service slices), returning either the call's final or the
  admit to send;
- the admit report applied, returning either the route to dial or the refusal
  to fail over from;
- the b-leg dialled, with the instant its INVITE leaves as an input;
- the reject, redirect and stack-final folds, and the services' seeding.

Each engine force-enable of the trace, the initial consult's and the failover
consult's, is the shell's: it runs as that consult's answer arrives, before the
route's folds. The admit's trace fact is recorded by the shell after the admit's await,
stamped with the turn's `now_ms`. Consulting the decision engine as an effect
re-entering the call is not part of this decision.

A decided final the stack refuses (a reject that is no non-2xx final, a
redirect that is not a 3xx or whose target does not read) draws the plain
server error `500`. The refusal keeps the decision's headers, its updates and
its adds, as its decided final would have, and carries none of the failing
peer's lines; it is the same on the initial consult's path and the failover
consult's.

### 8. The model invariant oracle

The model invariant check and the defect report stay in the shell, in every
build. The check runs behind the `model_invariants` configuration flag (on by
default in debug builds); a defect note is reported whatever the flag, as it
costs nothing until a turn meets one. Neither panics. Each violation, an
invariant the landing record breaks
or a defect note the core returned, is kept in the process's record (the first
since the last mark with the call's dump, later ones one line each), logs one
error line, and counts in `b2bua_model_defects_total{invariant}`. The record's
lock is a named lock that recovers from poison (ADR-0051 §7). The turn lands.

A harness that runs the B2BUA with the flag on reads the record at the end of
every run and fails the run on a violation; a test that provokes one on purpose
says so and reads it itself.

### 9. What moves out of the shell crates the core needs

- `CallEvent::from_txn` leaves the rule SDK for the shell's router, so the SDK
  no longer reaches the transaction layer.
- The IP-literal readers (`ip_host`, `ip_socket_addr`) move to `sip-message`;
  `sip-net` re-exports them.
- The `Duration` to milliseconds conversions move to `call`, beside its
  deadline type; `sip-clock` re-exports them.
- `TxnKind` and `TxnTimeoutKind` are in `call`.
- The shell's identifier generator is built from a key the shell also hands to
  each turn's `TurnIds` (§3).

### 10. Dependencies

`b2bua-core` depends on `call`, `sip-message`, `b2bua-sdk`, `sip-ids`,
`sip-retransmit` and `lookup-map`, and on `serde`, `serde_json`, `thiserror` and
`tracing`. `tracing` brings `tracing-core`, `once_cell` and `pin-project-lite`
into the pure dependency allowlist. A `LookupMap` / `LookupSet` serves a
function's local lookups only and never appears in `Call` or in an effect;
where order reaches output the core uses a `BTreeMap`.

### 11. The caller's INVITE, and an unrecoverable call

The core rebuilds the caller's INVITE from the call's snapshot only in a step
that reads it, through one fallible accessor (`relay::caller_invite`; a rule
reads it through `RuleContext::caller_invite`). A turn that reads nothing of it
rebuilds nothing. The rebuilt request is not kept beside the resident call: it
would double the memory every live call holds for its INVITE.

Every entry that makes a body resident checks that its snapshot rebuilds:

- **At creation** the shell rebuilds it from the snapshot it just took. When
  that fails, the caller is refused with a final built from the live INVITE
  still in hand, and no call is born (new-call refusal `unrecoverable`).
- **At materialisation** (a takeover, a reclaim) a copy whose snapshot does
  not rebuild is unrecoverable: the shell discharges it silently, as the
  reaper's second strike does (ADR-0020 X6). Its legs are abandoned, its CDR
  and its limiter release go through the one enforcement path, the call is
  removed, and the discharge is counted (`b2bua_unrecoverable_calls_total`).
  Nothing is sent on the wire for the call: the enforcement skips the
  unanswered caller's final (`UnansweredCaller::Leave`), which would need the
  INVITE the snapshot could not give. The request that asked for the copy is
  answered as one for a vanished call.
- **At a fold** the snapshot is written: a whole fold lands the other owner's
  body, its snapshot with it. A fold whose body does not rebuild is refused
  and counted (`b2bua_unrecoverable_folds_total`); the live copy, which
  rebuilds, keeps serving, and the version the body names is still seen. A
  rounds fold keeps the live copy's snapshot.

Nothing else writes the snapshot, so a resident call rebuilds its caller's
INVITE at every step that reads it. A step that fails all the same is a defect:
the core notes it (`caller-invite-rebuilds`) and the step does nothing; the
shell drops the turn's result, discharges the call as above, and answers the
request that opened the turn as one for a vanished call.

### 12. The panic set

The core's panic set reaches `deny` in a step of its own after the move. Every
panic site of `b2bua` is in the code that moves, so `b2bua`'s own crate root
takes the deny with the move itself: the panic-sites ratchet holds a crate at
zero to its zero attribute.

## Consequences

- A call's decisions can be replayed and compared from their inputs under a
  fixed test key: the same call, event, configuration, `now_ms`, key and
  `turn_seq` give the same result, effects and notes.
- The property lane drives `rule_chain_turn` over generated event sequences with
  no runtime, and the decision JSON through the folds.
- Each action that draws an identifier takes the executor by `&mut`.
- What differs from the code before the extraction:
  - generated tags, branches and Call-IDs change once, at the switch to
    per-turn draws, and the tests that pin a drawn identifier follow;
  - a fault armed during a turn applies from the next turn;
  - a route's trace activation is attempted once, as its consult answers, and
    not again after the hop-budget check;
  - a repeat give-up is counted when the turn returns, so a turn that panics
    after its give-up step does not count it;
  - a violation in a debug build no longer panics its turn or tears its call
    down: the turn lands, and the test fails at its end from the record;
  - production may turn the oracle on and read defects as a metric;
  - the failover consult's reject is held to the initial path's guard: a code
    that is no non-2xx final draws the plain server error;
  - a copy whose INVITE snapshot does not rebuild is discharged instead of
    panicking its handler; its live legs are left to the peers' own timers.
- The 136 synchronous rule tests move with the core, with their builders; the 22
  of the model invariant oracle stay with it. The synchronous tests of the
  other moved code move too (82: the answer deadline 20, destination allow-list
  15, ring deadline 12, stack identity 9, admit report 6, initial call and decision
  request 11 across three files, timer screen 3, no-answer deadline 2, superseded relay 2,
  attempts 1, decision schemas 1). Tests that need a runtime, the router or the
  fabric stay on the shell. The dependents that build a call with a seeded
  generator follow the per-turn source.

## Implementation order

Each step is one commit and keeps the suites green.

1. In the shell, one step each, with no move: the oracle that records instead
   of panicking (§8), with a scenario whose violation fails its run from the
   record; the turn's identifiers (§3); the
   notes (§4), checked by running the old emission and the drained notes side
   by side over the scenario harness corpus and comparing the registry's and
   the metrics' contents; the fault snapshot (§5); the INVITE rebuilt at the
   boundaries (§11); the `from_txn`, IP-literal and duration moves (§9).
2. The move, byte-identical, checked by the unchanged test suites. A shadow
   run of the old and the new turn, if one is used, is test-only.
3. The failover consult's reject guard, with its scenario, outside any
   comparison window.
4. The panic set's flip (§12).
5. The property lane over `rule_chain_turn`, the folds and the limiter DTOs.
