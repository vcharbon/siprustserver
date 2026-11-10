# Language

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Language".

## Migration units

**Layer**:
One row of [MIGRATION_STATUS.md](../../MIGRATION_STATUS.md), a cohesive subsystem
ported as a unit (message layer, network layer, call model, rule engine,
limiter). In the Rust workspace, one layer maps to one **crate**.
_Avoid_: "Effect Layer" (a source-side DI construct; say **layer interface**
for that), "module" (a layer is a crate, not a Rust `mod`).

**Slice**:
One increment of work that advances one or more **layers**, recorded as a dated
entry in MIGRATION_STATUS. Slice 1 = the pure message layer. A layer may take
several slices.
_Avoid_: "phase" (overloaded; the source used "Phase 1/2" for the native
parser rollout).

**Layer interface** (a.k.a. **DI seam**):
The Rust `trait` a layer exposes so consumers depend on the abstraction, not a
concrete impl, the idiom that replaces the source's Effect `Layer`/`ServiceMap`
dependency injection. Multiple impls (and future decorator wrappers) implement
the same trait and are swapped at the boundary.
_Avoid_: "service" (Effect term), "interface" unqualified.

## Test porting

**Parity oracle**:
A second, independent implementation kept *only* for tests, to cross-check
the production impl; it need not implement the **layer interface**. For the
message layer the oracle is `rvoip-sip-core`, read into the same fields as a
`CustomParser` message and
compared seed by seed by a differential test that lists every expected
difference with its RFC section.
_Avoid_: "reference parser" (the production custom parser is the reference for
behaviour; the oracle is the looser cross-check).

**Compliance matrix**:
The test that runs the production parser over a shared fixture corpus (RFC
4475 torture, CVE regressions, RFC 5118 IPv6, param-grammar gaps) and asserts
its expected accept/reject. The Rust port of `parser-compliance.test.ts`; the
**parity oracle**'s differential test reads the same fixtures.
_Avoid_: "parity test" (the source's `parity` wrapper is a distinct
SignalingNetwork construct, deferred to a later layer).

**Frozen corpus**:
A committed set of grammar-valid inputs generated once by `abnfgen` from the
vendored ABNF grammars, replayed deterministically by the ABNF fuzz test.
Regenerated only on demand via `cargo run -p xtask -- abnf-regen`.
_Avoid_: "fuzz corpus" (implies live mutation each run; ours is frozen).

**Callflow choreography** (`scenario_harness::callflow`):
The imperative, inline helper a `#[tokio::test]` calls to run the canonical
INVITE/180/200/ACK dance (`establish`/`hangup`/`Call`), parameterised only on the
address the caller routes *through* (a `B2buaSut` addr single-SUT, a proxy VIP for
HA). It is the one home for that handshake in both harnesses. This
is **distinct** from the ADR-0018 **"Callflow shape"**, a registered,
parameterised *template* (declared, per ADR-0021, as a **Shape descriptor** in
the unified registry, shared with the load fleet) that the executor runs over
compatible Infra shapes. The choreography is code you call; the shape is a
declared artifact you register. (Both differ again from failover-harness's
ADR-0013 **CallScenario** safe-point DSL, and from the fallible
`scenario_harness::realcall` choreography the load fleet drives.)
_Avoid_: naming the choreography "CallScenario" or "Callflow shape".

## Typed messages

**Refined view**:
A borrowed newtype wrapping a base `SipRequest`/`SipResponse` that encodes a
context guarantee proven once at a boundary, such as `InDialogRequest` (From/To tags
present), `InviteRequest` (single Contact), `SipResponseTagged` (To-tag
present). `Deref`s to the base, so it adds guarantees without hiding anything.
Built at the router/dispatch boundary; downstream code is never defensive.
_Avoid_: "subtype", "cast" (it is a validated projection, not a downcast).

**TypedHeader**:
The trait an integrator implements to add a typed, parsed custom header
(`msg.typed::<H>()`), the open, compile-time replacement for the source's
declaration-merging + `SipHeaderRegistry.register`. Distinct from the raw
`get_header(name) -> Vec<&str>` escape hatch, which serves unknown headers only.
_Avoid_: "header registry" (there is no global mutable registry).

**Strict reading** / **Lenient reading** (ADR-0053 §6):
The two readings of the one reader of each grammar. *Strict* holds wire bytes to
RFC 3261 §25.1 whole and refuses anything else. *Lenient* is the reading of a
stored image, of a message the stack assembles from its own parts, and of a
recorded exchange an audit judges (`wire_grammar = false`, `read_stored`): a
value the strict reading takes reads the same way, and one it refuses keeps
what can be read, never a refusal.
_Avoid_: "relaxed parser", "second parser" (it is a mode, not an implementation).

**Reading role** (ADR-0053 §6):
What a parser is built to read, named by its constructor: a deployment's
`ingress`, a message `built` rather than received, a `tool`'s reading of
captured traffic, `read_stored`. Production code builds a parser only through a
role; `CustomParser::new` and `with_limits` are test support.
_Avoid_: "default parser" (no reading is the default one).

**Exemption** (ADR-0001, `SipParserLimits::exemptions`):
A shape a deployment accepts against the RFC, named in `SIP_EXEMPTIONS`: empty by
default, consulted only by the ingress gate. A tool's reading names its own set
(`sipflow --reading`). Only a shape with one reading can be
exempted, and every typed reader reads it on every deployment.
_Avoid_: "tolerance", "quirk mode".

**Single-valued header** (RFC 3261 §7.3.1, ADR-0053 §7):
A header a message carries on one line at most, compact forms counted, as
`header::SINGLE_VALUED` states. It reads as `msg.header::<H>()`, an `Option` of
its one value; every other header is a list, read as `msg.list::<H>()`. Every
reading refuses a second line and no draft freezes with one; a service stating
one on two lines has every line of it left out and the skip reported.
_Avoid_: "the first value" of a single-valued header (there is no second).

**Policy rejection** / **Buggy rejection**:
The two classes a parser rejection of a grammar-valid input falls into.
*Policy* = a documented strictness rule (the ABNF suite's policy patterns,
the gates of ADR-0001; expected, the grammar is looser than the parser).
*Buggy* = matches no known policy → a real parser bug. A clean ABNF run has zero buggy rejections and zero silent misparses.
