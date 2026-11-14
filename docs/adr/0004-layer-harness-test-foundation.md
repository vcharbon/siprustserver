# Test-only `layer-harness` foundation crate + contract decorators

**Status:** accepted (2026-05-31)

The layers' tests share one test-contract machinery: the contract wrappers
(the property, paranoid-inputs and scoped-audit decorators, and the parity
check), the `Recorder`, `RunContext`, and the three-tier severity model. The
network layer (`sip-net-testkit/src/contracts.rs`) is the one layer that wraps
its seam in contract decorators; a layer that adds contracts builds on this
crate. The machinery lives in a dedicated `crates/layer-harness` crate rather
than duplicated per layer or parked inside `sip-net`.

This does **not** contradict ADR-0002's "no premature shared types crate"
stance. ADR-0002 defers a shared *production* foundation until a production
cross-layer cycle forces it. `layer-harness` is **test-only**: it carries no
domain types (it is SIP-agnostic — it knows nothing about `SipMessage`,
sockets, or any service) and never appears in a production layer tree. It is
infrastructure, not a types crate.

## What the crate holds

| Concern | `layer-harness` |
|---|---|
| 3-tier severity | `RunContext` enum + `severity_for()` |
| per-scenario ordering | `EventSequencer` (atomic counter) |
| typed recording channels + projectors | `Recorder` + `Channel<E>` (type-erased by tag key) |
| recording helpers | `recording` module (`record_call`, `ReleaseGuard`) |
| anomalies | flat `RecordedAnomaly` struct (`kind: &'static str`) |
| the contract wrappers | **decorator structs** implementing the wrapped trait (live in the layer's `contracts` module, not here) |

Two shapes are deliberate:

- **`RecordedAnomaly` is a struct, not an enum.** A closed Rust enum would force
  every layer to edit this crate to add its variant — defeating "shared
  foundation". The `kind: &'static str` discriminant is layer-owned; extra
  per-variant fields are encoded into `detail`.
- **Recording channels are keyed by `&'static str`, downcast via `Any`.** The
  channel stores `Arc<Mutex<Vec<Stamped<E>>>>` type-erased and `for_tag::<E>`
  recovers it by downcast. Re-opening a key with a different `E` panics (a
  programmer error, surfaced loudly).

## Scope-close: RAII + an explicit `close()`

Rust has no async `Drop`, so the per-bind close (recording the release, the
queue-leak check, the per-peer rules) is RAII in the decorator endpoint's
`Drop`, and the layer-close step (drain transit, structural invariants, the
deferred-fail → violation decision) is an explicit `async close()` the test
calls. Drop endpoints before `close()`.

## Consequences

- A layer's `contracts` module depends on `layer-harness`; the network
  layer's (`sip-net-testkit`) is the one that exists.
- The wrappers are test-only, and nothing in the compiler enforces it;
  reviewers reject any `with_all_contracts` / decorator import from a
  production binary.
- `parity` has no rule trait — it is a direct deep-equal of two deterministic
  impls, written inline where two impls exist. (Not applicable to the network
  layer; see ADR-0005.)
