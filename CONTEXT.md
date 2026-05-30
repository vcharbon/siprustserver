# siprustserver

The Rust port of [sipjsserver](./portsource/sipjsserver). This glossary pins
the **migration vocabulary** — the words we use to talk about porting one
codebase to another. The **SIP domain glossary** (Worker, partition,
replication channel, leg model, overload, …) is not duplicated here; it lives
in [portsource/sipjsserver/CONTEXT.md](./portsource/sipjsserver/CONTEXT.md)
and carries over unchanged.

## Language

### Migration units

**Layer**:
One row of [MIGRATION_STATUS.md](./MIGRATION_STATUS.md) — a cohesive subsystem
ported as a unit (message layer, network layer, call model, rule engine,
limiter). In the Rust workspace, one layer maps to one **crate**.
_Avoid_: "Effect Layer" (a source-side DI construct — say **layer interface**
for that), "module" (a layer is a crate, not a Rust `mod`).

**Slice**:
One increment of work that advances one or more **layers**, recorded as a dated
entry in MIGRATION_STATUS. Slice 1 = the pure message layer. A layer may take
several slices.
_Avoid_: "phase" (overloaded — the source used "Phase 1/2" for the native
parser rollout).

**Layer interface** (a.k.a. **DI seam**):
The Rust `trait` a layer exposes so consumers depend on the abstraction, not a
concrete impl — the idiom that replaces the source's Effect `Layer`/`ServiceMap`
dependency injection. Multiple impls (and future decorator wrappers) implement
the same trait and are swapped at the boundary.
_Avoid_: "service" (Effect term), "interface" unqualified.

### Test porting

**Parity oracle**:
A second, independent implementation of a **layer interface** kept *only* for
tests, to cross-check the production impl. For the message layer the oracle is
`rvoip-sip-core`, checked against the ported `CustomParser`.
_Avoid_: "reference parser" (the production custom parser is the reference for
behaviour; the oracle is the looser cross-check).

**Compliance matrix**:
The test that runs every parser impl over a shared fixture corpus (RFC 4475
torture, CVE regressions, RFC 5118 IPv6, param-grammar gaps) and asserts each
impl's expected accept/reject. The Rust port of `parser-compliance.test.ts`;
it is how the **parity oracle** is exercised.
_Avoid_: "parity test" (the source's `parity` wrapper is a distinct
SignalingNetwork construct, deferred to a later layer).

**Frozen corpus**:
A committed set of grammar-valid inputs generated once by `abnfgen` from the
vendored ABNF grammars, replayed deterministically by the ABNF fuzz test.
Regenerated only on demand via `cargo run -p xtask -- abnf-regen`.
_Avoid_: "fuzz corpus" (implies live mutation each run — ours is frozen).

### Typed messages

**Refined view**:
A borrowed newtype wrapping a base `SipRequest`/`SipResponse` that encodes a
context guarantee proven once at a boundary — `InDialogRequest` (From/To tags
present), `InviteRequest` (single Contact), `SipResponseTagged` (To-tag
present). `Deref`s to the base, so it adds guarantees without hiding anything.
Built at the router/dispatch boundary; downstream code is never defensive.
_Avoid_: "subtype", "cast" (it is a validated projection, not a downcast).

**TypedHeader**:
The trait an integrator implements to add a typed, parsed custom header
(`msg.typed::<H>()`) — the open, compile-time replacement for the source's
declaration-merging + `SipHeaderRegistry.register`. Distinct from the raw
`get_header(name) -> Vec<&str>` escape hatch, which serves unknown headers only.
_Avoid_: "header registry" (there is no global mutable registry anymore).

**Policy rejection** / **Buggy rejection**:
The two classes a parser rejection of a grammar-valid input falls into.
*Policy* = a documented ADR-0007 strictness rule (expected; the grammar is
looser than the parser). *Buggy* = matches no known policy → a real parser
bug. A clean ABNF run has zero buggy rejections and zero silent misparses.
