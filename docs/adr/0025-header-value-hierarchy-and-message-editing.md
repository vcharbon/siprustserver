# Header value hierarchy, immutable messages, and the editor/builder seam

**Status:** proposed

Supersedes the *access* half of ADR-0003. The refined-view mechanisms A–D stay, mechanism E's
`TypedHeader` trait is absorbed, and the `message_helpers` free-function surface and the
`generators` string-assembly path are replaced. Call sites migrate crate by crate.

The [appendix](appendix/0025-header-value-hierarchy-and-message-editing.md) holds the survey
behind the context, the type sketches, the long form of each rule and rejected alternative, the
measured performance figures and the migration's casualty list.

## Context

The typed message model of ADR-0003 holds the parse-side guarantees, but consumers work on the
raw `headers: Vec<SipHeader>` by string name. Each crate grew its own mutation helpers, typed
fields desync from the header vec after an edit, extraction is re-rolled outside sip-message,
header names are string literals of mixed casing, and the build path re-parses its own output.
Detail: [appendix, Context](appendix/0025-header-value-hierarchy-and-message-editing.md#context).

## Decision

The typed value is the only currency. Strings exist only on the wire.

### 1. `HeaderName`: header identity is an enum, not a string

The header scanner matches each name into `HeaderName` (known variants plus `Other(SipStr)`,
which keeps the wire casing). Compact forms (`v`, `f`, `t`, `k`…) normalize there.
`HeaderName::class()` is the single structural and end-to-end classification table, and a
consumer policy list is a `&[HeaderName]`. Canonical wire rendering lives on the enum.
Detail: [appendix §1](appendix/0025-header-value-hierarchy-and-message-editing.md#1-headername-header-identity-is-an-enum-not-a-string).

### 2. The value hierarchy: name-addr + tag via marker-kind polymorphism

Every structured value implements `HeaderValue` (`name`, `parse`, `render`). The name-addr family
is one generic, `NameAddrHeader<K>`, and the tag API exists only where `K: TaggedKind` (From,
To). Kinds also declare their params policy and whether the header may comma-fold, so a
credentials header is never comma-split. Every value type is its own builder by functional
update, and a `parse(render(v)) == v` property test pins the pair. `Uri` is always parsed.
`Params` is an ordered small-vec. `msg.header::<H>()` reads built-ins and extensions alike.
Detail: [appendix §2](appendix/0025-header-value-hierarchy-and-message-editing.md#2-the-value-hierarchy-name-addr--tag-via-marker-kind-polymorphism).

### 3. The immutable message: one source of truth, private fields

`SipRequest` and `SipResponse` each hold a start line and one shared `MessageCore` (ordered
headers, typed core headers, body, image). Fields are private, so typed accessors cannot desync
from the header list. A received message is one `Arc<str>` image with every field a span, and a
clone is refcount bumps. `thaw()` is the only path to a modified message. The read surface is
written once on `MessageCore`.
Detail: [appendix §3](appendix/0025-header-value-hierarchy-and-message-editing.md#3-the-immutable-message-one-source-of-truth-private-fields).

### 4. `Draft`: one construction seam, thaw / freeze

`Draft<S: StartKind>` is the editable twin of a message. `RequestDraft::new` starts a blank one,
`thaw` and `keep` seed one from a parsed message with every entry a span ref. Updates consume
`self` and return `Self`, so construction phases compose as plain functions. `update::<H>` parses
an entry on first touch. Multi-value headers keep their wire lines and expose one logical
`HeaderList<H>`. `freeze()` renders once into one buffer and never re-parses; it fails only on a
blank draft missing a mandatory header. `render_unchecked` is the only exit for deliberately
invalid output, as bytes, never as a typed message. SUT code uses `freeze` only. The proxy's
local mutation vocabulary and `RouteSet` move here.
Detail: [appendix §4](appendix/0025-header-value-hierarchy-and-message-editing.md#4-draft-one-construction-seam-thaw--freeze).

### 5. Generators become recipes over the draft

The dialog, ACK and CANCEL knowledge in `generators` stays and the string assembly goes.
`Generate*Opts` fields are typed values, or a draft `Entry` where RFC 3261 §8.2.6.2 makes the
header an echo. Recipes take the mandatory fields as arguments, so they cannot fail.
`hydrate_request` leaves the build path and serves only raw input. Built messages carry a real
image.
Detail: [appendix §5](appendix/0025-header-value-hierarchy-and-message-editing.md#5-generators-become-recipes-over-the-draft).

### Alignment between the shapes is confined and pinned

A header value has parse and render on one type. Only `thaw` and `freeze` know both the message
and the draft, pinned by `thaw(&m).freeze() == m` and `parse(freeze(d).raw()) == freeze(d)`.
Detail: [appendix](appendix/0025-header-value-hierarchy-and-message-editing.md#alignment-between-the-shapes-is-confined-and-pinned).

### Parse-side change: one dispatch pass

Mandatory and optional extraction run as one walk over the header list dispatching on
`HeaderName`, eager and non-fatal as in ADR-0003's amendment.
Detail: [appendix](appendix/0025-header-value-hierarchy-and-message-editing.md#parse-side-change-one-dispatch-pass).

## Considered and rejected

- More `message_helpers` free functions. Free functions on `&str` are not discoverable.
- A mutable `SipMessage`. It makes the desync bug a permanent hazard.
- A trait-object header list. It costs an allocation and a vtable per header and loses spans.
- Per-type structs for the name-addr family. They duplicate the shared API about 8 times.
- Typestate completeness tracking on the draft. It drags generic state through every phase helper.
- Patching raw bytes on relay. Every Via prepend or Route pop shifts every span.

Detail: [appendix, Considered and rejected](appendix/0025-header-value-hierarchy-and-message-editing.md#considered-and-rejected).

## Performance invariants and guardrails

1. Parse makes exactly one text copy (the image). Body and raw slicing stay refcount-only.
2. A thawed-draft hop uses one output buffer and O(edited) value allocations. Untouched header
   bytes are copied once and never re-parsed.
3. A blank-draft build uses one output buffer, with no `hydrate` and no double copy.
4. `alloc_budget.rs` and the criterion bench cover parse, blank-draft build, the thawed-draft hop
   with the proxy rewrite set, and the received/rport stamp, from one set of cases in
   `tests/perf/mod.rs`.

Targets and measured figures: [appendix](appendix/0025-header-value-hierarchy-and-message-editing.md#performance-invariants-and-guardrails).

## Consequences / migration

Breaking, migrated crate by crate beside the old API, legacy deleted last
([docs/todos/header-model-migration.md](../todos/header-model-migration.md)). Refined views,
`sniff`, `SipStr`/`SharedText`, the serializer's single-buffer core and the RFC-audit gate stay.
Casualty list: [appendix](appendix/0025-header-value-hierarchy-and-message-editing.md#consequences--migration).
