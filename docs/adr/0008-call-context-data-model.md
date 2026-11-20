# 0008 — CallContext data model: pure-leaf split + positional msgpack codec

**Status:** accepted (2026-05-31)

## Context

The "CallContext data model" layer (`crates/call`) is the call's
Call→Leg→Dialog data tree plus its persistence codec. A call's state splits
cleanly into a **pure** half (the schema + lens helpers, the timer list, and
the pluggable body codec) and a **stateful** half (per-call serialisation, the
in-memory call map, replication, the live timers), which depends on the
store, the configuration, the CDR writer, the metrics, the call limiter and
the per-call dispatcher.

## Decision D1 — the pure half is its own crate

`crates/call` is a **pure synchronous leaf**: it depends only on the serde
stack (`serde`, `rmp-serde`, `serde_json`, `serde_bytes`, `thiserror`) — no
`sip-message`, no `sip-net`, no `tokio`. It carries the data model, the lens /
accessor / timer-list helpers, `callRef` + index-key derivation, and the body
codec behind a trait seam.

The stateful half lives in `b2bua` (its call store and timer driver), on top
of this crate.

## Decision D2 — one positional msgpack codec; no parity check

`rmp-serde` has two struct encodings:

- **positional / array** (its default): no field-name strings on the wire.
- **map** (`to_vec_named`): field names embedded every message.

Measured on the representative fixture (a confirmed 2-leg call with ~1.5 KB SDP):

| encoding | size | encode |
|---|---|---|
| positional (msgpack array) | **2445 B** | ~18 µs |
| named (msgpack map) | 4241 B | ~31 µs |
| JSON | 5759 B | — |

Positional is **57.7 % of named** (≈42 % smaller) and ~1.75× faster, reached
structurally with no shared-structure registry to keep in cluster-wide
lock-step.

**Chosen:** a single positional `MsgpackCodec` behind the `CallBodyCodec` trait.
The only cost of positional encoding is **schema-coupling** — field order *is*
the contract, so reordering/removing a field breaks previously-encoded bytes.
That is a non-issue: the project redeploys from scratch each release (no
persisted-format compatibility constraint). With one codec there is no second
implementation to compare, so no parity check; another codec has its slot
behind the trait.

## Decision D3 — optional vs null; opaque ext; byte fields; the RNG seam

- **Absent and null are one `Option<T>`.** msgpack collapses absent/nil, and a
  nested `Option<Option<T>>` does not round-trip (both encode as nil). The one
  *behaviourally load-bearing* three-way absent/null/value distinction —
  `Call.policy_update_body` (absent = no override, null = force empty body,
  bytes = substitute) — is `Option<PolicyUpdateBody>` where
  `PolicyUpdateBody = Empty | Bytes(..)`. `billing_context` is not load-bearing
  on absence-vs-null and is an `Option<String>`.
- **Opaque `ext` slices** (per-service carry) → `BTreeMap<String,
  serde_json::Value>`. `BTreeMap` (not `HashMap`) keeps encode deterministic —
  the codec's P2 property.
- **Byte fields** (`aLegInvite.body`, `cachedSdp`, the INVITE handle) use
  `serde_bytes` so msgpack stores them as `bin`. The INVITE handle keeps the
  original INVITE as raw **bytes**, so the data model takes no `sip-message`
  dependency.
- **No random value is drawn here.** A dialog's initial CSeq is stated by the
  code that opens it, where identifier derivation and its determinism are
  plumbed (`sip-ids`).
- **Timers here are data.** The crate holds only the pure serializable
  `TimerEntry` list and its constants (`TERMINATING_TIMEOUT_MS`); firing rides
  the single-`DelayQueue` driver (ADR-0007), not a new wheel.

## Decision D4 — codec tests as a proptest suite, not contract decorators

The meaningful **property** checks of the codec (round-trip P1, encode/decode
determinism P2/P3, binary integrity P7, Option/empty-collection preservation
P5/P6/P8, non-empty output P14) are a plain `proptest` suite over a generated
`Call`, with no contract decorator. Paranoid inputs collapse into the type
system + `decode`'s `Result` (PA1/PA5 are compile-time / range-trivial; PA2/PA4
are the decode error path). P10/P11/P13 hold by construction
(`encode(&Call)` cannot mutate; `decode` returns the typed `Call`).

## Consequences

- `crates/call` is a reusable pure leaf: the rule engine, the limiter and the
  call store build on it without dragging in async infra.
- Bodies are ~42 % smaller and ~1.75× faster to encode than a named/self-
  describing layout, at the cost of strict field-order stability — acceptable
  given redeploy-from-scratch.
- The `CallBodyCodec` trait is the seam for another codec and for contract
  decorators if a consumer needs them.
