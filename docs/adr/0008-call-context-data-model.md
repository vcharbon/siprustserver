# 0008 — CallContext data model: pure-leaf split + positional msgpack codec

**Status:** accepted (2026-05-31)

**Source:** sipjsserver @ `fffc4ac69c8aeef26cf48fe73469503145c9732b`,
`src/call/` (`CallModel.ts`, `timer-helpers.ts`, `codec/`).

## Context

The "CallContext data model" layer (`crates/call`, MIGRATION_STATUS row 18) is
the call's Call→Leg→Dialog data tree plus its persistence codec. The source
`src/call/` splits cleanly into a **pure** half (the `CallModel` schema + lens
helpers, `timer-helpers`, and the pluggable body codec) and a **stateful** half
(`CallState`: per-call semaphores, the in-memory call map, Redis persistence,
orphan sweep, HA topology; `TimerService`: live fiber scheduling). The stateful
half depends on infrastructure that is not ported yet — the cache
(`PartitionedRelayStorage`/`BufferedTerminateWriter`), `AppConfig`, `CdrWriter`,
`MetricsRegistry`, the call limiter, and the per-call dispatcher.

## Decision D1 — port the pure half now; defer the stateful half

`crates/call` is a **pure synchronous leaf**: it depends only on the serde
stack (`serde`, `rmp-serde`, `serde_json`, `serde_bytes`, `thiserror`) — no
`sip-message`, no `sip-net`, no `tokio`. It carries the data model, the lens /
accessor / timer-list helpers, `callRef` + index-key derivation, and the body
codec behind a trait seam.

`CallState` and `TimerService` are a **later slice**, blocked on their infra
deps. This mirrors how the network slice deferred its config/metrics-bound
facades (`UdpTransport`, ADR-0005). No premature stubs are introduced.

## Decision D2 — one positional msgpack codec; no parity this slice

The source ships three codec impls (msgpack standard, msgpack records, protobuf)
and a `parity` wrapper. For this slice the user dropped parity and asked for the
size/CPU trade-off behind the encoding choice.

`rmp-serde` has two struct encodings:

- **positional / array** (its default): no field-name strings on the wire.
- **map** (`to_vec_named`): field names embedded every message.

Measured on the representative fixture (a confirmed 2-leg call with ~1.5 KB SDP):

| encoding | size | encode |
|---|---|---|
| positional (msgpack array) | **2445 B** | ~18 µs |
| named (msgpack map) | 4241 B | ~31 µs |
| JSON | 5759 B | — |

Positional is **57.7 % of named** (≈42 % smaller) and ~1.75× faster — the same
win the source's msgpackr "records / shared-structures" mode buys in JS (it
quoted ~60 % of self-describing), reached here structurally with no shared-
structure registry to keep in cluster-wide lock-step.

**Chosen:** a single positional `MsgpackCodec` behind the `CallBodyCodec` trait.
The only cost of positional encoding is **schema-coupling** — field order *is*
the contract, so reordering/removing a field breaks previously-encoded bytes.
That is a non-issue: the project redeploys from scratch each release (no
persisted-format compatibility constraint). The protobuf codec keeps its slot
behind the trait; it and the `parity` comparison are deferred.

## Decision D3 — optional vs null; opaque ext; byte fields; the RNG seam

- **`Schema.optional` and `Schema.NullOr` → `Option<T>`.** msgpack collapses
  absent/nil, and a nested `Option<Option<T>>` does not round-trip (both encode
  as nil). The one place the source carries a *behaviourally load-bearing*
  three-way absent/null/value distinction — `Call.policyUpdateBody` (absent = no
  override, null = force empty body, bytes = substitute) — is preserved as
  `Option<PolicyUpdateBody>` where `PolicyUpdateBody = Empty | Bytes(..)`.
  `billingContext` (`optional(NullOr)`) is not load-bearing on absence-vs-null
  and collapses to `Option<String>`.
- **Opaque `ext` slices** (per-service carry) → `BTreeMap<String,
  serde_json::Value>`. `BTreeMap` (not `HashMap`) keeps encode deterministic —
  the codec's P2 property.
- **Byte fields** (`aLegInvite.body`, `cachedSdp`, the INVITE handle) use
  `serde_bytes` so msgpack stores them as `bin`. The `InviteTxnHandle` keeps the
  original INVITE as raw **bytes** (the source kept it `Schema.Unknown`,
  best-effort), so the data model takes no `sip-message` dependency.
- **RNG seam deferred.** `randomInitialCSeq` reads a fiber-local Effect
  `Random`; the dialog constructors here take the initial CSeq as a parameter
  instead. When `CallState` lands, the CSeq is drawn from `sip-txn`'s `IdGen` —
  the same place determinism is plumbed (mirrors the message slice deferring its
  RNG identifier generators).
- **CallState's eventual timers ride `sip-txn`'s `DelayQueue`.** This slice
  needs no live timer — only the pure serializable `TimerEntry` + `replaceTimerById`
  / `TERMINATING_TIMEOUT_MS`. When `TimerService` is ported, firing reuses the
  existing single-`DelayQueue` driver (ADR-0007), not a new wheel.

## Decision D4 — codec tests as a proptest suite, not contract decorators

The source's four codec contract wrappers (`propertyTest` / `paranoidInputs` /
`parity` / `scopedAudit`) are Effect-Layer decorators recording typed events.
Here the meaningful **property** checks (round-trip P1, encode/decode
determinism P2/P3, binary integrity P7, Option/empty-collection preservation
P5/P6/P8, non-empty output P14) are a plain `proptest` suite over a generated
`Call`. `paranoidInputs` collapses into the type system + `decode`'s `Result`
(PA1/PA5 are compile-time / range-trivial; PA2/PA4 are the decode error path);
`scopedAudit` aggregates and `parity` are deferred. P10/P11/P13 hold by
construction (`encode(&Call)` cannot mutate; `decode` returns the typed `Call`).
This matches how `sip-net` deferred its `propertyTest`/`parity` decorators.

## Consequences

- `crates/call` is a reusable pure leaf: the rule engine, limiter, and the
  eventual `CallState` build on it without dragging in async infra.
- Bodies are ~42 % smaller and ~1.75× faster to encode than a named/self-
  describing layout, at the cost of strict field-order stability — acceptable
  given redeploy-from-scratch.
- The `CallBodyCodec` trait is the seam for the deferred protobuf impl and for
  the contract decorators if a later slice needs them.
