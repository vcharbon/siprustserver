# Slice — CallContext data model (`crates/call`)

## Context

Next layer in the port (MIGRATION_STATUS row 18: **CallContext data model**,
`call`, source `src/call/`, currently ⬜ pending). The source `src/call/` is
large and splits cleanly:

- **Pure data + serialization** — `CallModel.ts` (Call→Leg→Dialog hierarchy +
  all lens/index helpers), `timer-helpers.ts`, and the codec family
  (`CallCodec.ts`, `codec/`). No I/O, no runtime, no Effect on the hot path.
- **Stateful service** — `CallState.ts` (per-call semaphores, in-memory
  indexes, Redis persistence, orphan sweep, HA topology) and `TimerService.ts`
  (fiber scheduling). These depend on infrastructure that is **not ported yet**:
  Redis/cache (`PartitionedRelayStorage`, `BufferedTerminateWriter`),
  `AppConfig`, `CdrWriter`, `MetricsRegistry`, `CallLimiter`, `PerCallDispatcher`.

**Scope confirmed with user:** port the **data model + codec only**
(round-trip, **no parity**). Defer the stateful `CallState` + `TimerService` to
a later slice once their infra lands. The `CallLimiter*.ts` files are a separate
layer (row 20) and are out of scope here.

### Two decisions the user asked about (folded into this plan)

**Timers in this slice.** None are needed. The data model carries only the
*pure serializable* `TimerEntry` (id / type / fireAt / legId) plus
`replaceTimerById` + `TERMINATING_TIMEOUT_MS` constants. No live scheduling.
When `CallState` is ported later and must actually *fire* timers, it will reuse
**`sip-txn`'s existing `DelayQueue` timer driver** (one queue per owner task,
docs/adr/0007) rather than a new wheel — matching the sip-clock ADR's
"don't reimplement a timer wheel" stance. So: yes, reuse the sip-txn timer
infrastructure — but at the CallState slice, not now.

**msgpack encoding choice (size vs CPU).** Via `rmp-serde`:
- *Positional / array encoding* (rmp-serde default): structs serialize as arrays
  with **no field-name strings on the wire** → smallest payload and least CPU
  (nothing to write or hash for keys). This captures the same win the source's
  msgpackr "records / shared-structures" mode gives in JS (~60% smaller than
  self-describing maps).
- *Map encoding* (`with_struct_map`): embeds field names every message → larger
  and slower (≈ the source's "standard" mode).
- Cost of positional: it is **schema-coupled** — field order *is* the contract,
  so reordering/removing a field breaks previously-encoded bytes. That is a
  non-issue here: the project redeploys from scratch each release (no persisted
  format compatibility constraint — CLAUDE.md).
- **Decision:** a single positional msgpack codec. Best on both axes; the
  schema-coupling cost does not apply to this project.

## Approach

New pure leaf crate **`crates/call`**, added to the workspace. Depends only on
the serde stack (`serde`, `rmp-serde`, `serde_json`, `thiserror`); **no**
`sip-message`/`sip-net`/`tokio` (the model stays a synchronous leaf). Mirrors
the slice-1 `sip-message` shape: typed structs are the schema, a trait is the
DI seam, errors are `Result<_, E>`.

### Module layout (`crates/call/src/`)

- `model.rs` — the Call→Leg→Dialog structs + all enums + `TimerEntry`/`CdrEvent`/
  `TagMapping`/`RemoteInfo`/`PendingRequest`/`CallTopology`/`ActiveRule`/
  `ALegInviteSnapshot`/`InviteTxnHandle`/`CallLimiterState`. `#[derive(Serialize,
  Deserialize, Clone, PartialEq, Debug)]` throughout. Enums (`LegState`,
  `LegDisposition`, `ByeDisposition`, `LegKind`, `CallModelState`, `TimerType`,
  `CdrEventType`) as Rust enums with serde rename to the source string literals.
- `features.rs` — `FeatureActivations` (the closed union from
  `src/decision/schemas/features.ts`); embedded in `call` for now (the decision
  layer will own the canonical version later — ADR-0002 "extract a shared crate
  only when a cycle forces it").
- `helpers.rs` — the pure lens/index functions: `update_leg`, `update_dialog`,
  `set_leg_state`/`set_leg_disposition`/`set_bye_disposition`, `is_fully_resolved`,
  `add_cdr_event`, `add_b_leg`/`find_b_leg`/`find_leg`/`find_b_leg_by_call_id`,
  CSeq ops (`bump_local_cseq`/`update_remote_cseq`/`relay_cseq_delta`), pending-
  request ops, tag accessors + `add_tag_mapping`/`find_by_a_tag`/`find_by_b_tag`,
  peering (`get_peer`/`merge_leg`/`split_leg`/`all_peered_legs`), ext/rule
  helpers, `leg_kind`/`is_adopted`/`find_dialog_by_to_tag`/`confirmed_dialog`,
  `make_empty_dialog`/`make_dialog_from_incoming`, `replace_timer_by_id` +
  `TERMINATING_TIMEOUT_MS`.
- `callref.rs` — `derive_call_ref` / `parse_call_ref` (pure string codec) +
  `call_index_keys` / `call_index_keys_from_unknown` (Redis index-key
  derivation; pure, ported now so the later CallState slice can reuse it).
- `codec.rs` — `CallBodyCodec` **trait** (`encode(&Call) -> Vec<u8>`,
  `decode(&[u8]) -> Result<Call, CallDecodeError>`) = the DI seam, plus the
  `MsgpackCodec` impl (rmp-serde positional). `CallDecodeError` via `thiserror`.
- `lib.rs` — re-exports + crate docs.

### Decisions on opaque/seam fields

- **`ext` slices** (`Call.ext`, `Leg.ext`, `B2buaDialogExt.cachedSdp` opaque
  parts) — per-service opaque maps in the source. Model as
  `serde_json::Value` (or `BTreeMap<String, serde_json::Value>`) so they
  round-trip losslessly without coupling to the consuming layers.
- **`InviteTxnHandle.originalInvite` / `ALegInviteSnapshot`** — store the SIP
  payload as raw owned bytes (`Vec<u8>`) + primitive fields, not a typed
  `sip_message::SipRequest`. Keeps `call` a pure leaf (no `sip-message` dep) and
  matches the source's best-effort JSON serialization of these handles.
- **`random_initial_cseq` (RNG seam)** — the source reads a fiber-local Effect
  `Random`. Keep the data model pure: `make_empty_dialog` takes the initial CSeq
  as a parameter (caller supplies it). When CallState lands, the CSeq is drawn
  from `sip-txn`'s `IdGen`/`rng`. Mirrors the message-slice decision to defer the
  RNG identifier generators to where determinism is plumbed.

### Tests (`crates/call/tests/`)

- `codec_roundtrip.rs` — **proptest** suite over generated `Call`s, porting the
  meaningful codec properties: **P1** round-trip equality, **P2** encode
  determinism, **P3** decode determinism, **P5/P6** Option/None preservation,
  **P7** binary integrity (`Vec<u8>` bodies at sizes 0/1/1k/64k), **P8** empty-
  collection preservation, **P14** non-empty output. A `fixtures` module ports
  the source's representative `Call` + a proptest `Strategy` (the
  `tests/bench/call-codec/fixture.ts` + `fixtureMix.ts` mixer).
- `model_helpers.rs` — unit tests locking the pure lens/index helpers
  (no direct TS counterpart — they were exercised via the deferred CallState
  tests; analogous to slice-1's `message_helpers.rs` smoke tests).
- `callref.rs` — `derive_call_ref`→`parse_call_ref` round-trip + legacy-ref
  (`None`) handling + `call_index_keys` shape.

### Docs

- **ADR-0008** (`docs/adr/0008-call-context-data-model.md`) — records: pure-leaf
  crate split (data model now / stateful CallState+TimerService deferred);
  single positional msgpack codec (size/CPU rationale above); parity dropped for
  this slice; opaque-`ext`→`serde_json::Value`; raw-bytes INVITE handles; RNG
  CSeq seam deferred; CallState's eventual timers ride sip-txn's DelayQueue.
- **MIGRATION_STATUS.md** — flip row 18 to 🟡/✅ for the data-model+codec slice,
  add a "Slice — CallContext data model" section with the source→Rust checklist,
  the ported-tests table, and the **un-ported-with-justification** list:
  `CallState.ts`, `TimerService.ts`, protobuf codec + `call.proto`, the contract
  decorator wrappers (`paranoidInputs`/`parity`/`scopedAudit` + Recorder typed
  channel), and the CallState/TimerService/limiter test files.
- Record the exact source submodule SHA (per the migration ritual).

### Un-ported, with justification (carried into MIGRATION_STATUS)

- **`CallState.ts`** — stateful; depends on unported Redis/cache, `AppConfig`,
  `CdrWriter`, `MetricsRegistry`, `CallLimiter`, `PerCallDispatcher`. Later slice.
- **`TimerService.ts`** — fiber scheduling + per-handler timeout + metrics; when
  ported it rides `sip-txn`'s `DelayQueue`, not a new wheel. Later slice.
- **protobuf codec + `call.proto`** — needs a prost/build.rs toolchain + field-
  mapping shims (topology/featuresJson/extJson/isNull flags). The `CallBodyCodec`
  trait keeps the slot; deferred.
- **Contract decorator wrappers** (`paranoidInputs`/`parity`/`scopedAudit` +
  Recorder typed-channel recording) — **parity** dropped by user for this slice;
  **paranoidInputs** collapses into the Rust type system + the `decode` `Result`
  (PA1/PA5 are compile-time/range-trivial; PA2/PA4 are the decode error path);
  **scopedAudit** size-budget/alias-check deferred. Property checks are ported as
  a plain proptest suite instead of a decorator — consistent with how `sip-net`
  deferred its `propertyTest`/`parity` decorators (ADR-0005).
- **P10/P11/P13** codec properties — collapse into compile-time guarantees in
  Rust (`encode(&Call)` cannot mutate its input; `decode` returns the typed
  `Call`, so "schema conformance" is the type). Noted, not separately tested.
- **CallState/TimerService/limiter test files** (`callstate-arms-safety…`,
  `TimerService-*`, `limiter-*`, `forcepurge-*`) — defer with their subjects.

## Verification

- `cargo build -p call` and `cargo test -p call` green (proptest round-trip +
  helper/callref unit tests).
- `cargo build` (whole workspace) still green — new crate is additive.
- `cargo clippy -p call` clean.
- Spot-check: encode the representative fixture, confirm decode == input and a
  non-empty positional payload (sanity on the size claim vs a map-encoded round).
