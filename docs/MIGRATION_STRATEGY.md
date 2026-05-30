# Migration strategy

Port of [sipjsserver](../portsource/sipjsserver) (TypeScript + Effect v4) to
Rust, for performance. The project is pre-production: no upgrade/rollout
compatibility constraints — we can redeploy from scratch each release.

This doc consolidates the **cross-cutting decisions**. Per-layer specifics
and progress live in [MIGRATION_STATUS.md](../MIGRATION_STATUS.md). Decisions
that are hard to reverse get an [ADR](./adr/).

## Per-layer migration ritual

For each **layer** (one row of MIGRATION_STATUS = one workspace **crate**):

1. Record the exact source release (submodule SHA) in MIGRATION_STATUS.
2. Port the **layer interface** (Rust `trait`) + the production implementation.
3. Port the test implementation, **including** the property/fuzz tests and
   the cross-impl comparison (the **compliance matrix** / parity check).
4. Port and pass every test of the layer. List any un-ported test with a
   precise justification (see the slice's "Un-ported with justification").

## Effect → Rust construct map

| Effect construct | Rust equivalent | Notes |
|---|---|---|
| `Effect.Clock` | `tokio::time` | All time goes through tokio; no `std::time` wall-clock in logic paths. Test time via tokio's paused clock. (Relevant from the network layer on — the message layer is clock-free.) |
| `Effect.Layer` / `ServiceMap` | a **trait** (the DI seam) + impls | Consumers depend on the trait. Impls are swapped at the boundary; decorator structs implementing the same trait carry the [test Layer philosophy](../portsource/sipjsserver/.claude/skills/effect-layer-test/SKILL.md). |
| typed error channel | `Result<T, E>` with a typed error enum | No catch-all; model the error set explicitly, mirroring the source's typed errors. |
| `Effect.gen` (sync pure fn) | a plain function / trait method | The message layer is pure + synchronous; no async runtime is introduced. |
| the 4 contract wrappers (`propertyTest`/`paranoidInputs`/`parity`/`scopedAudit`) | decorator structs implementing the same trait | A network-layer-onward concern (they wrap `SignalingNetwork` in the source), deferred accordingly. |

## Project layout

Cargo **workspace, one crate per layer** — see
[ADR-0002](./adr/0002-cargo-workspace-crate-per-layer.md). Slice 1 creates a
single `sip-message` crate (a pure leaf). Sibling crates are added as each
layer lands; a shared foundation/types crate is extracted only when the first
cross-layer dependency cycle forces it (Rust crates cannot be cyclic).

## Parser engine

Port the source's **custom** zero-regex parser whole; keep `rvoip-sip-core`
as a **dev-only parity oracle** — see
[ADR-0001](./adr/0001-port-custom-parser-rvoip-as-parity-oracle.md). The
ABNF fuzz suite and the ADR-0007 strict gate are both built around the custom
parser's per-header functions, so porting it whole keeps the test corpus a 1:1
port; rvoip cross-checks it in the compliance matrix.

## ABNF tests

The verbatim RFC grammars are vendored under
`crates/sip-message/tests/abnf/grammars/`. Inputs are generated **once** by
`abnfgen` into a **frozen corpus** (default N=1000/target), committed so CI is
deterministic and needs no external binary at test time. Regeneration is the
opt-in `cargo run -p xtask -- abnf-regen [N]`. The source driver's
accepted/policy/buggy/silent-misparse classification is ported; a clean run
has zero buggy rejections and zero silent misparses.

## Large structural changes from the source

- **Redis sidecar for call cache → dropped.** Its only role was freeing memory
  from long-term storage; in Rust an internal memory buffer + tokio cleanup is
  closer to the in-memory test layer and removes the dependency. Redis (or a
  small dedicated Rust service) may still back the **call limiter** /
  cross-worker shared state — decided when that layer is reached.
