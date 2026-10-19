# The B2BUA's decision core is a pure crate; a turn is a function of its inputs

**Status:** proposed (2026-10-19)

**Owner:** `crates/b2bua-core` (the core: rules, the turn, the folds, the effect
vocabulary), `crates/b2bua` (the shell: router, interpreter, store, replication,
timers, reaper, limiter worker, decision engines, CDR writer, overload, metrics,
the trace registry), `crates/sip-ids` (the id source), `crates/b2bua-sdk` (the
rule SDK the core builds on).

Applies ADR-0051 (Consequences: "the seam has its own ADR"). Amends ADR-0010 X5
(the action executor draws its identifiers from the turn), ADR-0026 (a rule's
trace facts are returned, not emitted) and ADR-0020 X6 (the unrecoverable call).

## Context

The rule engine, the action vocabulary, the invariants and the obligation set
are synchronous and read only the call, the event and configuration; no rule
reads a lock, a socket, the clock or the store. They still live in the crate
that owns every effect, and five things reach the effectful world from inside
them:

- identifiers are drawn from the process's shared generator, an atomic counter
  in the transaction layer's crate;
- the rule trace is written into a process-global registry behind a lock;
- the wire-fault seam is an `Arc<AtomicBool>` a test flips while the node runs;
- the per-turn framework bumps two counters on the metrics handle (a repeat
  ladder's give-up, a message-cap teardown) and reads its inputs through the
  router's handle bag;
- a model defect met mid-turn is kept in a process-global record and, in debug
  builds, panics.

Constants and small pure functions the rules need also sit in shell files (the
reaper's verdict names, the operator release's topic, the decision request's
standard headers, the refused-decision guards, the IP-literal reader). The
initial INVITE's turn awaits the decision engine, the call limiter and the
failover consult inside the turn, with clock reads around each await.

ADR-0051 makes a pure crate's output depend on its inputs alone. A call's
decisions can only be held to that rule, replayed, compared and driven by a
property lane once they live in such a crate.

## Decision

### 1. Core and shell

**`b2bua-core`** (tier pure) holds what decides:

- the rule engine and every rule family: selection, the action executor and
  vocabulary, the lifecycle close, the default rules, the services' composition
  and seeding, the registry renderer;
- the per-turn framework (`rule_chain_turn`), its pre-rule screens (the timer
  screen, the answer-deadline screen, the admit report's application, the
  decision log's fold, the refused routes' record, the superseded relay's drop)
  and its framework steps (ladder repeat, give-up and settle, reliable
  provisionals, response facts, the message cap's teardown, the quiet-turn
  candidate);
- invariants (`finalize`, `enforce`) and the obligation set with its kinds;
- relay construction and SDP restatement, stack identity, the stated headers,
  the message ring, attempts, timers' arming, ring and answer deadlines;
- the call side of the admission set: the admit report and its result event,
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
the decision engine trait and its engines; the CDR writer; overload and capacity;
metrics; the trace registry and intake; the ingress admission ladder; the model
invariant oracle (§8). The shell re-exports the core's public modules under the
paths it exposed before, so its dependents keep their imports.

### 2. A turn's inputs

A turn is the shell's one run of the core over one call: the per-call handler
body under the call's lock, or a sweep's settle of one call. The core takes:

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
RSeq starts, `Retry-After` picks, the call's incarnation mark, the router's
refusal tags) is a draw of the turn's `TurnIds`: HMAC(key, `turn_seq` ‖ `n`).

- The key is drawn from the OS entropy once per process incarnation, as today.
  The shell's counter stream (the transaction layer's branches, stateless
  refusals at ingress, the OPTIONS tagger, the answer owed for a request a
  queue discarded) keeps its draws under the same key: a counter draw hashes
  8 bytes and a turn draw 16, so the two never share an input.
- `turn_seq` is allocated by the shell once per turn from a per-process counter
  and never reused under a key. The initial INVITE's turn keeps one `TurnIds`
  across its awaits.
- Nothing about it is replicated. A takeover copy draws under its own node's
  key.
- `TurnIds` is neither `Clone` nor `Copy`; the fixed-key constructor is behind
  `sip-ids`'s test feature.

A turn replayed with its key and `turn_seq` draws the same identifiers.

### 4. What a turn returns

A turn returns the `HandlerResult` (the call as the turn leaves it, its effects)
and writes `TurnNotes`, the data that were side effects inside the turn:

- the trace facts of a sampled call, in the order the turn met them: rule fired,
  cursor moved, call transition, lifecycle delivery, timer fire and drop, admit
  report applied, each with the timestamp of its fact (ADR-0026);
- the counts the turn owes the metrics: a repeat ladder's give-up with its
  obligation kind, a message-cap teardown;
- the model defects met mid-turn, each with its name and detail.

The shell drains the notes as each core call returns, before it records or sends
anything else of the turn: trace facts go into the registry, counts onto the
metrics, defects to the oracle (§8), or to the log where the oracle is not
built. Trace facts of an unsampled call are never built: the core reads the
call's sampled flag first, so an unsampled call allocates nothing for them.

### 5. Wire faults

The fault seam stays a shell handle a test arms and disarms. The shell reads it
once at the turn's start into a `WireFaultSet`, a plain `Copy` value the core
reads at each guarded emission. A fault armed during a turn applies from the
next turn.

### 6. The landing pass

Every result the shell persists first takes the core's landing pass, with its
inputs as values: the answer deadlines of the requests the turn sent off its
turn (the budgets of §2), the refresh arming (the refresh period), the stated
headers on every outbound message, the message ring's record of what the turn
sends and the turn's seal. The interpreter calls it; the model invariant check,
persistence and effects follow, unchanged.

### 7. The initial INVITE

The initial INVITE keeps its awaiting orchestration in the shell: the trace
intake, the decision consult, the limiter admit, the failover consult, and the
clock readings around them. Each step between two awaits is a core fold:

- the call built from the INVITE, its incarnation mark drawn from the turn;
- the decision request built from the INVITE;
- the route applied up to its admit (the hop budget, the destination
  allow-list, features, subscriptions, service slices), returning either the
  call's final or the admit to send;
- the admit report applied, returning either the route to dial or the
  refusal to fail over from;
- the b-leg dialled, with the instant its INVITE leaves as an input;
- the reject, redirect and stack-final folds, and the services' seeding.

An engine force-enable of the trace is the shell's, between the hop check and
the route's application, as it is now. Consulting the decision engine as an
effect re-entering the call is not part of this decision.

### 8. The model invariant oracle

The model invariant check stays in the shell as a test oracle, built only with
the shell's test feature. Its process record and its debug panics are the
oracle's; the core reports a defect as a note (§4) and goes on.

### 9. What moves out of the shell crates the core needs

- `CallEvent::from_txn` leaves the rule SDK for the shell's router, so the SDK
  no longer reaches the transaction layer.
- The IP-literal readers (`ip_host`, `ip_socket_addr`) move to `sip-message`;
  `sip-net` re-exports them.
- `TxnKind` and `TxnTimeoutKind` are already in `call`.
- The shell's identifier generator is built from a key the shell also hands to
  each turn's `TurnIds` (§3).

### 10. Dependencies

`b2bua-core` depends on `call`, `sip-message`, `b2bua-sdk`, `sip-ids`,
`sip-retransmit` and `lookup-map`, and on `serde`, `serde_json`, `thiserror` and
`tracing`. Its one hash collection is `lookup-map`'s, for lookups; where order
reaches output it uses a `BTreeMap`. `tracing` brings `tracing-core`,
`once_cell` and `pin-project-lite` into the pure dependency allowlist.

### 11. An unrecoverable call

A call whose replicated INVITE snapshot does not rebuild into a request cannot
answer its caller or relay anything: the core returns the turn as unrecoverable
instead of panicking, and the shell discharges the call as the reaper's second
strike does (ADR-0020 X6): its legs abandoned, its CDR and its limiter release
through the one enforcement path, the call removed, the discharge counted and
alerted on.

### 12. The move keeps behaviour

The extraction changes no outcome but identifiers:

- in the shell first, one step each: the turn's identifiers (§3), the notes
  (§4), the fault snapshot (§5), the `from_txn` and IP-literal moves (§9);
- then the move, during which the shell runs the old and the new turn over the
  same inputs (same call, event, `now_ms`, key and `turn_seq`) and compares the
  resulting call, every effect and every note, across the crate's tests, the
  scenario harness and the no-panic lane's sequences; the old turn goes once
  they agree;
- the core's panic set reaches `deny` after the move, in a step of its own.

## Consequences

- A call's decisions can be replayed and compared from their inputs: the same
  call, event, configuration, `now_ms`, key and `turn_seq` give the same result,
  effects and notes.
- The property lane drives `rule_chain_turn` over generated event sequences with
  no runtime, and the decision JSON through the folds.
- Generated tags, branches and Call-IDs change once, at the switch to per-turn
  draws; tests that pin a drawn identifier follow.
- Each action that draws an identifier takes the executor by `&mut`.
- The rule unit tests move with the core, with their builders; tests that need
  a runtime, the router or the fabric stay on the shell.
- A fault armed during a turn applies from the next one; a defect met mid-turn
  reaches the oracle as the turn returns, before anything of it lands.
