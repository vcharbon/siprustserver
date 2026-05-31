# Test-only `layer-harness` foundation crate + contract decorators

**Status:** accepted (2026-05-31)

The network slice is the first to need the **effect-layer-test** machinery (the
source's `propertyTest` / `paranoidInputs` / `parity` / `scopedAudit` wrappers,
the `Recorder`, `RunContext`, and the three-tier severity model — see the
[effect-layer-test SKILL](../../portsource/sipjsserver/.claude/skills/effect-layer-test/SKILL.md)
and ADR-0013 in the source). That machinery is **shared by every layer from
here on** (cache, limiter, rules all wrap their Tags the same way). We extract
it now into a dedicated `crates/layer-harness` crate rather than duplicating it
per layer or parking it inside `sip-net`.

This does **not** contradict ADR-0002's "no premature shared types crate"
stance. ADR-0002 defers a shared *production* foundation until a production
cross-layer cycle forces it. `layer-harness` is **test-only**: it carries no
domain types (it is SIP-agnostic — it knows nothing about `SipMessage`,
sockets, or any service) and never appears in a production layer tree. It is
infrastructure, not a types crate.

## The Effect → Rust translation

| Source (Effect) | `layer-harness` |
|---|---|
| `RunContext` service (3-tier severity) | `RunContext` enum + `severity_for()` |
| `EventSequencer` | `EventSequencer` (atomic counter) |
| `Recorder` + typed `forTag` channels + projectors | `Recorder` + `Channel<E>` (type-erased by tag key) |
| `recordingHelpers.ts` | `recording` module (`record_call`, `ReleaseGuard`) |
| `RecordedAnomaly` union | flat `RecordedAnomaly` struct (`kind: &'static str`) |
| the 4 wrappers as same-`Tag` Layers | **decorator structs** implementing the same trait (live in each layer crate's `contracts` module, not here) |

Two shapes deviate deliberately:

- **`RecordedAnomaly` is a struct, not an enum.** A closed Rust enum would force
  every layer to edit this crate to add its variant — defeating "shared
  foundation". The `kind: &'static str` discriminant is layer-owned; extra
  per-variant fields are encoded into `detail`.
- **Recording channels are keyed by `&'static str`, downcast via `Any`.** The
  source keyed by the Effect `Tag` identity. Rust has no runtime `Tag`, so the
  channel stores `Arc<Mutex<Vec<Stamped<E>>>>` type-erased and `for_tag::<E>`
  recovers it by downcast. Re-opening a key with a different `E` panics (a
  programmer error, surfaced loudly).

## Scope-close: RAII + an explicit `close()`

Effect ran the wrappers' invariants in scope finalizers. Rust has no async
`Drop`, so the per-bind close (recording the release, the queue-leak check, the
per-peer rules) is RAII in the decorator endpoint's `Drop`, and the layer-close
finalizer (drain transit, structural invariants, the deferred-fail → violation
decision) is an explicit `async close()` the test calls. Drop endpoints before
`close()` — the RAII analogue of LIFO scope finalizers.

## Consequences

- `layer-harness` is a workspace member from slice 2; later layers add a
  `contracts` module that depends on it.
- The "wrappers are TEST-ONLY" rule (SKILL Rule 6) has no compiler guard here
  either; reviewers reject any `with_all_contracts` / decorator import from a
  production binary, exactly as the source instructs.
- `parity` has no rule trait — it is a direct deep-equal of two deterministic
  impls, written inline where two impls exist. (Not applicable to the network
  layer; see ADR-0005.)
