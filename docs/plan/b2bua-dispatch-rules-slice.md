# Slice — B2BUA: dispatch / per-call FIFO + rule engine + memory layer (`crates/b2bua`)

## Context

MIGRATION_STATUS rows 18 (**Dispatch / per-call FIFO**) and 20 (**Rule engine**)
are the next slices. Together they are the heart of the B2BUA: the per-call FIFO
dispatcher + router that turns inbound SIP/timer events into ordered per-call
work, and the rule engine that decides what each event *does* (relay, respond,
confirm dialog, terminate, …). They sit on top of the already-ported
`crates/call` data model (✅ Call→Leg→Dialog + helpers + codec + features),
`crates/sip-txn` (✅ transaction FSMs + `IdGen`), `crates/sip-message`
(✅ parser/serializer/generators), `crates/sip-net` (✅ `SignalingNetwork`), and
`crates/sip-clock` (✅ `Clock`).

The source groups three concerns the user asked to land together because they
are mutually dependent: the dispatcher (`PerCallDispatcher`), the router
(`SipRouter`), the in-memory call store (`CallState`), and the rule engine
(`src/b2bua/rules/`). Per ADR-0007 §X3 these are **B2BUA-only** (the proxy does
not use them), so they land in one new crate.

**Goal:** a working in-process B2BUA that establishes and tears down a basic
bridged call (INVITE → 18x → 200 → ACK → BYE, plus CANCEL / failure / timers),
driven end-to-end through the existing `scenario-harness` (simulated
alice ↔ b2bua ↔ bob), emitting exactly one CDR per call.

**Source release:** sipjsserver @ submodule `fffc4ac69c8aeef26cf48fe73469503145c9732b`
(record exact SHA in MIGRATION_STATUS when the port begins).

## Confirmed decisions (from clarifying questions)

1. **One `crates/b2bua` crate** — dispatcher + router + rules + call-store + the
   decision-engine seam + CDR + timer service. Reuses `crates/call` for the data
   model. (Rust forbids crate cycles; these four are mutually dependent.)
2. **Per-call FIFO = per-call queues + worker tasks + a global concurrency
   semaphore** (faithful port of source ADR-0004/0005). A slow handler on one
   call must not block other calls; cap-drop / queue-drop / saturation counters
   ported as atomics.
3. **Memory seam = a replication-aware `CallStore` trait** whose method
   signatures already carry the HA parameters (partition `role`/`primary`,
   `peer`/`direction`, `call_gen`, index keys, `ttl`); the shipped in-memory impl
   ignores the HA params. A replicating impl drops in later with **zero changes**
   to rules / dispatcher / call-state. Mirrors `PartitionedRelayStorage`.
4. **Decision engine = port the adapter seam + a deterministic test impl.** Port
   the `CallDecisionEngine` trait (`new_call` / `call_failure` / `call_refer`) and
   the request/response JSON schemas. Ship a **test adapter that emulates the
   jssip backend by inspecting the request JSON** (R-URI, From/To, `X-*` headers,
   body) so the existing SIPp scripts keep producing the same call flows. The
   real HTTP adapter is stubbed behind the same trait (deferred).
5. **Tests via the scenario-harness**, simulated alice ↔ b2bua ↔ bob, in a
   **dedicated `crates/b2bua-harness` test crate** that depends on both
   `scenario-harness` and `b2bua` (keeps `scenario-harness` generic; avoids the
   dev-dependency cycle the proxy slice took with `with_proxy`). Rule-engine unit
   tests live in `b2bua` itself.
6. **OPTIONS = minimal 200 OK** for out-of-dialog keepalive; DrainingState /
   WorkerReadiness / OverloadController **deferred** with a distinct
   MIGRATION_STATUS line (drain / health-check).
7. **CDR = a real buffered CDR layer wired through the recording** so e2e tests
   assert exactly one CDR per terminated call. **CallLimiter = no-op seam**
   (row 20 stays its own future layer). **Metrics = atomic counters.**

## Reuse (do not reimplement)

- `crates/call` — `Call`/`Leg`/`Dialog`, all enums (`LegState`, `LegDisposition`,
  `ByeDisposition`, `TimerType`, `CdrEventType`, `CallModelState`, …), every lens
  helper (`update_leg`, `set_leg_state`, `confirm`-dialog primitives, CSeq ops,
  `add_tag_mapping`/`find_by_*_tag`, `merge_leg`/`split_leg`, `add_cdr_event`,
  `is_fully_resolved`, `make_*_dialog`, `replace_timer_by_id`,
  `TERMINATING_TIMEOUT_MS`), `callref::{derive_call_ref, parse_call_ref,
  call_index_keys}`, `CallBodyCodec`/`MsgpackCodec`, `FeatureActivations`.
- `crates/sip-message` — `generators` (`generate_response`, in-dialog request
  builders, CANCEL, ACK-for-2xx, out-of-dialog INVITE/OPTIONS), `message_helpers`
  (`get_header(s)`, `parse_sip_uri`, `parse_via_params`, tag/URI extraction),
  serializer, `SipRequest`/`SipResponse`/`SipMessage` types.
- `crates/sip-txn` — `TransactionLayer` (event stream + `send_*` + `cancel_txns_for_call`)
  and `IdGen` (tags/branches/Call-IDs). The dispatcher consumes its event stream.
- `crates/sip-clock` — `Clock` for `now_ms`; behavioural timers ride `tokio::time`.
- `crates/sip-net` — `SignalingNetwork` for the SUT binding in tests.
- `crates/scenario-harness` — `Harness::bind_sut` (already returns
  `(Box<dyn UdpEndpoint>, SocketAddr)` + registers a `Core` lane), `advance`,
  `finish`/`RunReport`.

## Crate module layout (`crates/b2bua/src/`)

```
lib.rs                 re-exports, crate docs, dependency-policy note
event.rs               CallEvent enum (sip|timer|cancelled|timeout|internal-event); txn-event → CallEvent
dispatch.rs            PerCallDispatcher: callRef→bounded queue map, per-call worker tasks,
                       global Semaphore, DispatchStats atomics (cap/queue/saturation drops)
router/mod.rs          SipRouter: owns txn-event consume loop; route_key() (sync callRef resolution);
                       dispatch to PerCallDispatcher or inline; OPTIONS-200 short-circuit
router/with_call.rs    withCall body: resolve → checkout → leg/dialog select → run handler → cap → processResult
router/process_result.rs   typed-effect interpreter (critical → outbound → soft → buffered → fire-and-forget)
store/mod.rs           CallState: callsMap + sipIndex + per-callRef lock; create/with_call/update/peek/
                       remove/resolve_from_sip_key(_sync)/flush/force_purge over a CallStore
store/call_store.rs    CallStore trait (replication-aware) + PartitionRole/PropagateDirection
store/memory.rs        InMemoryCallStore (HA params accepted, no-op'd) + index map
store/terminate_writer.rs  BufferedTerminateWriter: non-blocking put/delete queue + drainer task
timers.rs              TimerService: one tokio_util DelayQueue driver, schedule/cancel/cancel_all/restore;
                       fires a CallEvent::Timer back through the dispatcher
decision/mod.rs        CallDecisionEngine trait + NewCall/CallFailure/CallRefer request/response types + errors
decision/schemas.rs    serde structs: NewCallRequest, NewCallResponse {Route|Reject}, SipDestination,
                       SipHeaderUpdates, FeatureActivations (reuse call::features), CallLimiterEntry
decision/apply_route.rs    applyRoute: response → call state (features, ext seed, b-leg create, effects)
decision/test_adapter.rs   ScriptedDecisionEngine: emulates jssip by reading request JSON (ruri/from/to/X-*/body)
cdr/mod.rs             CdrWriter trait + CdrRecord/CdrEvent (reuse call::CdrEvent); build_record(call)
cdr/buffered.rs        BufferedCdrWriter (bounded queue + drainer, drop-on-overload, passthrough when max=0)
cdr/memory.rs          InMemoryCdrWriter (test buffer + read_all) — recording assertion target
limiter.rs             CallLimiter trait + NoopLimiter (always admits; decrement no-op)
initial_invite.rs      handle_initial_invite: build NewCallRequest, call engine, reject/route → applyRoute
rules/mod.rs           re-exports; CORE_LAYER/SERVICE_LAYER consts
rules/definition.rs    RuleDefinition, Match (request|response|timer|timeout|cancelled|internal-event),
                       RuleContext, RuleHandleResult, RuleAction enum (full vocabulary)
rules/matcher.rs       pick_ranked: column match + filter + overrides + layer/registration sort (first-match)
rules/executor.rs      execute_rules: collect active → match → run handle → actions → invariants → fallback
rules/actions.rs       execute_actions: RuleAction[] → HandlerResult (relay/respond/confirm/create/terminate/timer/cdr/ext)
rules/relay.rs         relay-to-peer/leg core: CSeq delta, dialog identity, route set, tag rewrite, pending-request tracking
rules/invariants.rs    InvariantEnforcer (termination cleanup) + ByeDispositionInvariant
rules/defaults/*.rs    the ported basic-B2BUA rule set (see below)
stack_identity.rs      leg_stack_identity: Via/Contact/branch stamping (cr=/lg= params) via IdGen
metrics.rs             atomic counters (dispatch + router + handler-timeout + force-purge + cdr-drop)
handlers.rs            HandlerRegistry { initial_invite, in_dialog } wiring rules + default fallback
b2bua_core.rs          B2buaCore: composes dispatcher + router + store + txn + timers + decision + cdr; run()
```

## Key designs

### Per-call FIFO (`dispatch.rs`)
- `PerCallDispatcher` owns `HashMap<callRef, PerCallQueue>` (behind a `Mutex` only
  for the map; the hot path is the per-call channel). Each entry = a bounded
  `tokio::mpsc` + a spawned worker task that loops `recv → acquire global
  semaphore permit → run handler → release`. `dispatch(call_ref, fut)` lazily
  creates the queue/worker (subject to `per_call_queue_cap`) and `try_send`s;
  full queue → `queue_drops`, over cap → `cap_drops`. `enqueue_poison` drains +
  exits the worker + removes the map entry. Global `Semaphore` =
  `event_dispatch_concurrency`. Handler panics are caught (`tokio::spawn` +
  result inspection) so one bad handler can't kill the worker. This is the Rust
  expression of the source's per-call queue + worker fiber + permit semaphore.
- The handler body is a boxed `FnOnce -> impl Future` capturing the router +
  services (the Rust analogue of the source's type-erased Effect body).

### Router (`router/`)
- `SipRouter::run()` consumes `TransactionLayer`'s event stream on one task,
  maps each txn event to a `CallEvent`, computes `route_key()` **synchronously**
  (timer/timeout carry callRef; initial INVITE → `derive_call_ref`; in-dialog →
  Via/URI `cr=`/`lg=` params → `sip_index` sync fallback), then either
  `dispatcher.dispatch(call_ref, body)` or runs inline (OPTIONS keepalive,
  unresolvable). Out-of-dialog OPTIONS short-circuits to a 200 OK before checkout.
- `with_call` checks the call out of `CallState` (per-callRef lock), selects
  leg/dialog, invokes the registry handler, applies the `max_messages_per_call`
  cap, and feeds the `HandlerResult` to `process_result`.
- `process_result` interprets `HandlerEffects` in fixed order: **persist call →
  critical** (schedule/cancel timers, flush, remove-call) **→ outbound** (ACK/CANCEL
  via `send_raw`, else via txn layer) **→ soft** (limiter decrement, bounded) **→
  buffered** (CDR write) **→ fire-and-forget**. State is persisted before effects.

### Call store + HA seam (`store/`)
- `CallStore` trait (async), shaped after `PartitionedRelayStorage`:
  `get_call/put_call/delete_call/refresh_call/get_index/scan_calls`, each taking
  `(role: PartitionRole, primary: &str, call_ref, …, indexes, ttl, call_gen,
  PutOpts{peer, direction})`. `InMemoryCallStore` keeps a `HashMap<key, Vec<u8>>`
  + index map and **ignores** `role/primary/peer/direction/call_gen/ttl` (HA is a
  later impl). `partition_of(call_ref)` uses `parse_call_ref` to pick role/primary
  (legacy → `pri:self`).
- `CallState` holds the live in-memory `callsMap: HashMap<callRef, Call>` +
  `sip_index: HashMap<String, callRef>` (keys from `call_index_keys`) +
  `locks: HashMap<callRef, Arc<Mutex<()>>>` (per-callRef serialization — the
  second FIFO layer) + a flush-dedup cache. `with_call` takes the lock, loads from
  memory (cache fallback deferred — no Redis), runs the body, releases. `update`
  installs the `terminating_timeout` safety timer atomically on entry to
  `terminating`. `remove` cancels timers + txns, poisons the dispatcher queue,
  submits a buffered delete. Encoding via `MsgpackCodec` for the store/replication
  path only (in-memory live calls stay typed).
- `BufferedTerminateWriter` = bounded queue + drainer task, so the router never
  blocks on the store (faithful to the source even though the in-memory store
  can't stall — keeps the seam identical for the future replicating store).

### Rule engine (`rules/`)
- `RuleDefinition { id, layer, name, composes_with, overrides, match, on_error,
  handle: fn(&RuleContext) -> Option<RuleHandleResult> }`. `Match` is the
  declarative discriminated enum (request/response/timer/timeout/cancelled/
  internal-event with method/status/state/direction columns + optional sync
  `filter`). `pick_ranked` filters by columns + filter, applies `overrides`, sorts
  by layer desc then registration order; `execute_rules` runs the first handler
  returning `Some`, executes its actions, finalizes termination, enforces
  invariants, else falls through to the default handler.
- `RuleAction` enum = the full vocabulary (relay-to-peer/leg, respond, ack-leg,
  send-provisional/-request/-prack-to-leg, send-raw, confirm-dialog,
  update-leg-state, stamp-dialog-to-tag, pin-a-tag, add-tag-mapping, create-leg,
  destroy-leg, cancel-leg, merge, split, schedule/cancel/cancel-all-timer,
  terminate-call, begin-termination, terminate-leg, add-cdr-event, set-call/leg-ext,
  deactivate-rule, send-notify/-reinvite/cache-sdp/set-policy-update-body/
  refer-async-http — the last cluster compiled but exercised only by deferred
  service rules). `actions.rs` translates them to `HandlerResult` using the
  `crates/call` helpers; `relay.rs` carries the CSeq-delta / dialog-identity /
  route-set / tag-rewrite relay core.
- `invariants.rs`: on `active|terminating → terminated`, guarantee
  `cancel-all-timers` + a `decrement-limiter` per live limiter entry + `write-cdr`
  + `remove-call` (last). `ByeDispositionInvariant`: a rule consuming a BYE-final
  or `terminating_timeout` without `terminate-leg` gets the leg force-resolved
  (`bye_confirmed`/`bye_timeout`) + a logged violation.

### Basic-B2BUA default rules (`rules/defaults/`) — ported now
Relay: `relay-bye`, `relay-ack`, `relay-reinvite`, `relay-prack`, `relay-options`,
`relay-info`, `relay-update`, `relay-message`. Dialog: `relay-provisional`,
`confirm-dialog`, `relay-non-invite-200`. Absorption: `absorb-bye-200`,
`absorb-options-200`, `absorb-notify-200`. Lifecycle: `handle-timeout`,
`handle-cancel`, `handle-481`. Terminating: `resolve-bye-response`,
`resolve-cross-bye`. Corner cases: `cancel-200-crossing`, `retransmit-200`,
`reinvite-glare`, `relay-reinvite-response`. Failure: `route-failure`,
`no-answer-failover`, `absorb-stale-failure`. Timers: `max-duration`, `keepalive`,
`keepalive-timeout`, `terminating-safety-timeout`. The `route-failure` /
`no-answer-failover` failover branch calls `decision.call_failure` (test adapter
returns terminate by default; failover path compiles and is unit-tested).

### Decision engine + scripted test adapter (`decision/`)
- `CallDecisionEngine` trait: `new_call(NewCallRequest) -> Result<NewCallResponse,
  CallDecisionError>` (+ `call_failure`, `call_refer`). Serde schemas mirror
  `src/decision/schemas/*` (route carries `destination`, `new_ruri`,
  `update_headers`, `update_body`, `no_answer_timeout_sec`, `call_limiter`,
  `callback_context`, `features`, `serviceExt`; reject carries `reject_code`/
  `reject_reason`/`update_headers`). `FeatureActivations` reuses `call::features`.
- `ScriptedDecisionEngine` (test): inspects the `NewCallRequest` JSON — R-URI /
  To user, `X-*` headers in `sip_headers`, presence of SDP — and returns a route
  to a configured destination with mandatory `platform` features (and reject /
  feature toggles keyed off markers), reproducing the jssip backend's decisions
  so the existing SIPp scenario scripts behave identically. The real HTTP adapter
  is a `todo!()`-bodied struct behind the same trait (deferred).
- `apply_route.rs` + `initial_invite.rs` port `applyRoute` + `handleInitialInvite`:
  validate `features` present (else 500), seed `call.ext`/policy headers, run
  limiter (no-op), create the b-leg via `stack_identity` + generators, return the
  outbound INVITE effect.

### CDR (`cdr/`)
- `CdrWriter` trait `write(&Call)` + `read_all()`. `build_record` maps a
  terminated `Call` → `CdrRecord { call_ref, created_at, terminated_at, a_leg,
  b_legs, events: call.cdr_events, billing_context }`. `BufferedCdrWriter` =
  bounded queue + drainer (drop-on-overload counter; passthrough when max=0).
  `InMemoryCdrWriter` keeps a shared `Vec<CdrRecord>` the test crate reads to
  assert one CDR per call. In the harness the writer is also surfaced through the
  recording so the report shows CDR emission.

### Timers (`timers.rs`)
- `TimerService` owns one `tokio_util::time::DelayQueue` driver task keyed by
  timer id, riding `tokio::time` (so `advance` drives it under the paused harness
  clock — the same approach as `sip-txn` ADR-0007, but a B2BUA-local driver since
  `sip-txn`'s queue is private to its actor). On expiry it builds a
  `CallEvent::Timer` and feeds it back through `SipRouter`/dispatcher.
  `restore_from_entries` re-arms persisted `TimerEntry`s on rehydrate (past-due
  fire immediately).

## Test crate (`crates/b2bua-harness`, dev/integration only)
- Depends on `scenario-harness` + `b2bua` (+ `sip-*`, `call`). Provides
  `B2buaSut`: calls `harness.bind_sut("b2bua", addr)`, wires a `B2buaCore` over
  the returned endpoint + a `Clock::test_at(0)` + `ScriptedDecisionEngine` +
  `InMemoryCdrWriter` + `NoopLimiter` + `InMemoryCallStore`, and spawns its
  recv→txn→router loop (`tokio::spawn`, aborted on drop). Exposes `addr`,
  `cdr_records()`, `metrics()`.
- Tests (`crates/b2bua-harness/tests/`): `basic_call.rs` (alice INVITE → b2bua →
  bob, 180, 200, ACK both legs, BYE teardown; assert recording trace shape +
  exactly one CDR with `answer`+`bye` events), `cancel.rs` (CANCEL during ringing
  → 487 + CDR cancel), `failure.rs` (bob 486 → relayed to alice + terminate),
  `timers.rs` (no-answer timeout via `advance`), `reinvite.rs` (in-dialog
  re-INVITE relayed). If `bind_sut` proves insufficient for the b2bua loop from an
  external crate, expose a minimal `Harness::signaling_network()`/`clock()` getter
  (additive, no behaviour change) — that is the only anticipated harness edit.
- Rule-engine unit tests live in `b2bua` (`rules/` `#[cfg(test)]`): matcher
  ranking/overrides, each default rule's action output, invariant enforcement,
  bye-disposition force-resolution — pinned at the rule seam without a full SUT.

## Cargo / deps
- New workspace member `crates/b2bua` + `crates/b2bua-harness`. `b2bua` deps:
  `call`, `sip-message`, `sip-net`, `sip-txn`, `sip-clock`, `tokio`, `tokio-util`
  (DelayQueue), `async-trait`, `serde`/`serde_json` (decision schemas + ext),
  `thiserror`. Dev-deps: none beyond the above (e2e lives in `b2bua-harness`).
  `b2bua-harness` deps: `b2bua`, `scenario-harness`, `call`, `sip-*`, `tokio`,
  `layer-harness` (for recording assertions).

## Docs to write (during implementation)
- **ADR-0010** `docs/adr/0010-b2bua-dispatch-rules-rust-shape.md` (0007 format):
  X1 one crate (mutual deps); X2 per-call queue+worker+global-semaphore FIFO
  (vs actor); X3 replication-aware `CallStore` seam, in-memory no-ops HA; X4
  decision-engine adapter seam + scripted jssip-emulating test impl; X5 rule
  engine first-match/layer-ranked + invariant enforcement; X6 B2BUA-local timer
  DelayQueue; X7 buffered CDR through recording; X8 OPTIONS-200 minimal, drain/
  health deferred; X9 `b2bua-harness` test crate (extends ADR-0006).
- Copy this plan into the repo's own `docs/plan/` (the repo tree, not the
  submodule) per the migration ritual.
- **MIGRATION_STATUS.md**: flip rows 18 + 20; add a "Slice — B2BUA dispatch +
  rules" section (source→Rust tables, ported-tests table, un-ported list);
  add a **distinct row/line** for *Draining / readiness / overload (health-check)*
  as ⬜ deferred. Record the source SHA.

## Implementation order (de-risk first)
1. `event.rs`, `metrics.rs`, `stack_identity.rs` (pure, unit-tested).
2. `store/` (CallStore trait + in-memory + CallState + terminate writer) — unit tests.
3. `dispatch.rs` (per-call FIFO) + `timers.rs` — unit tests (ordering, cap drops, timer fire under paused clock).
4. `decision/` (trait + schemas + scripted adapter + apply_route) + `initial_invite.rs` — unit tests.
5. `rules/` framework (definition/matcher/executor/actions/relay/invariants) — unit tests.
6. `rules/defaults/` the basic set — unit tests per rule.
7. `cdr/` + `limiter.rs`.
8. `router/` + `handlers.rs` + `b2bua_core.rs` wiring.
9. `crates/b2bua-harness` SUT + e2e tests; harness getter if needed.
10. ADR-0010 + MIGRATION_STATUS + plan copy.

## Verification
- `cargo test -p b2bua` green (dispatcher ordering/cap, store, timers, decision
  adapter, matcher, every default rule, invariants).
- `cargo test -p b2bua-harness` green: alice ↔ b2bua ↔ bob basic call establishes
  and tears down; CANCEL/failure/timer/re-INVITE flows pass; **exactly one CDR per
  call** with the expected event sequence; recording trace shape asserted.
- `cargo build` whole workspace + `cargo clippy -p b2bua -p b2bua-harness` clean.
- `cargo test -p scenario-harness` still green (any harness change is additive).
- Spot-check a basic-call HTML/SVG report: two-leg bridge, To-tag stability across
  180/200, in-dialog BYE routes back through the b2bua, CDR present.

## Un-ported, with justification (carried into MIGRATION_STATUS)
- **18x management strategies** (`relayFirst18xTo180`, `promote18xPemTo200`,
  PEM-to-200, fake-prack) — SERVICE_LAYER policy modules; user explicitly
  deferred. The action vocabulary they need (`send-prack-to-leg`,
  `cache-sdp-on-leg-dialog`, `set-policy-update-body`, `send-reinvite`) is defined
  but unused until they land.
- **REFER / blind transfer** (`referTransfer`, `TransferRules`, `/call/refer`) —
  large phase-gated state machine + HTTP callback; deferred (trait method
  `call_refer` + `refer-async-http` action compiled, dormant).
- **Real HTTP decision adapter** — the production `CallDecisionEngine` over HTTP;
  the scripted test adapter stands in. Stubbed behind the trait.
- **Draining / WorkerReadiness / OverloadController** — out-of-dialog OPTIONS
  answers a plain 200; the serving/draining/ready matrix + Tier-3 admission are a
  distinct deferred line.
- **Real CallLimiter** (row 20) — no-op admit/decrement; the limiter is its own
  migration layer.
- **CallState Redis/cache + replication transport (HA)** — the `CallStore` seam
  carries the HA params but the in-memory impl no-ops them; the replicating impl
  (propagate-set / topology-gen / `BufferedTerminateWriter` drain to a peer) is
  the HA slice. Orphan-sweep / `loadOwnedCalls` rehydrate ported only as far as
  the in-memory store allows.
- **Tracing / OTel span machinery** (`withProcessingSpan`/sampling/tombstones) —
  Effect-tracer artefacts with no tokio analogue this slice; `spanEvents` dropped.
- **Protobuf call codec** — `MsgpackCodec` is used for the store path (already
  ported).
```
