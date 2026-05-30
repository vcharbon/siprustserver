# Cargo workspace with one crate per migration layer

**Status:** accepted (2026-05-30)

We lay the Rust port out as a Cargo **workspace with one crate per migration
layer** (`sip-message`, then `sip-net`, `call`, `rules`, `limiter`), rather
than a single crate with per-layer modules. At ~100K lines of source the usual
"workspace is ceremony" intuition inverts: crates compile in parallel and only
changed crates rebuild (a single crate is one serial compilation unit), and the
compiler's prohibition on cyclic crate dependencies turns the layer boundaries
that MIGRATION_STATUS draws into an enforced architectural guardrail instead of
a review-time convention.

## Considered and rejected

- **Single crate, modules per layer.** Lighter early refactoring, but serial
  compiles at scale and layer boundaries enforced only by module privacy.

## Consequences

- **Acyclicity is now a hard constraint.** The TypeScript source has type
  cycles (`SipMessage` ↔ `Leg`/`Call` ↔ `RuleContext`) that will not translate
  directly. We defer that cost: slice 1's `sip-message` is a pure leaf with no
  cycle risk; a shared `sip-foundation`/types crate is extracted only when the
  first cross-layer cycle actually forces it (expected around the call-model
  layer), not pre-emptively.
- Cross-crate refactors (moving a type between crates) are heavier than moving
  a file between modules — accepted, given the project is pre-production.
- The orphan rule may require newtypes if a decorator wrapper's trait and impl
  land in different crates — handled per-case as wrappers arrive.
- Per-crate dev-dependencies (e.g. `rvoip-sip-core` only in `sip-message`'s
  test deps) and feature flags fall out naturally.
