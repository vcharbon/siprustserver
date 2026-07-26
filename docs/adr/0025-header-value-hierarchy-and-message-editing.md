# Header value hierarchy, immutable messages, and the editor/builder seam

**Status:** proposed (2026-07-26)

Supersedes the *access* half of ADR-0003 (the refined-view mechanisms A–D stay;
mechanism E's `TypedHeader` trait is absorbed; the `message_helpers` free-function
surface and the `generators` string-assembly path are replaced). Breaking changes
are in scope — the project is pre-production and call sites migrate crate by crate.

## Context

Two workspace-wide surveys (2026-07-26, read patterns + build/serialize paths)
established that the typed message model of ADR-0003 succeeded at *parse-side
guarantees* but never became the working currency of the codebase:

- **The raw `headers: Vec<SipHeader>` vec is the real API.** 366 consumer sites
  touch `.headers`; `get_header(&x.headers, "…")` has 269 call sites (+72
  `get_headers`) against 6 uses of `.optional` and 9 of `.contacts`. Consumers
  read the wire string and re-parse locally even when the same struct carries
  the typed field (`relay_request.rs` reads From/To/Via as strings off a parsed
  `SipRequest`).
- **There is no mutation vocabulary**, so every crate grew its own:
  `sip-proxy/src/headers.rs` (237 lines of prepend/upsert/pop/Record-Route
  composition), private `prepend_header` copies in scenario-harness and
  e2e-core, inline `req.headers.insert(0, …)` in b2bua.
- **Typed fields silently desync from the header vec.** `serialize_*_parts`
  renders only the vec, so after `let mut headers = req.headers.clone()` the
  typed fields are stale by convention. scenario-harness resyncs by
  mutate → serialize → **full re-parse** (`client_invite.rs:360-380`).
- **Extraction is re-rolled at scale** despite the "only sip-message extracts"
  rule: Via sent-by ×5, top-Via branch ×9, angle-bracket unwrap ×4, URI→host:port
  triple string-peel, option-tag splitting ×5 — mostly because the blessed
  helpers are too narrow (`ViaParams` carries only branch/cr/lg) or
  undiscoverable (three helpers had zero consumers).
- **Header identity is a string literal** with inconsistent casing ("To" 14× vs
  "to" 11×), 47 consumer-side `eq_ignore_ascii_case(h.name)` checks, and at
  least six hand-maintained "structural header" name arrays in b2bua alone,
  three of them the identical 9 strings — plus two more tables inside
  sip-message (`template.rs::REGENERATED_HEADERS`, `relay.rs::STRUCTURAL_HEADERS`).
- **The build path re-parses its own output.** Every generator assembles
  `format!` strings into `Vec<SipHeader>` (double allocation per value via
  `String → Arc<str>`), then calls `hydrate_request`, which runs the full
  mandatory extraction *plus ten optional-header scans* over headers the
  generator itself just wrote. Generated messages carry no shared image
  (`raw: Bytes::new()`), so they are second-class to parsed ones.
- **Parse is near-optimal on copies** (one `Arc<str>` image, `Bytes` body
  slice) but structurally allocation-heavy: a `BTreeMap` node block per
  From/To/Via/Contact param set, ten full header-list scans for the eager
  optional headers, several intermediate `Vec`s, ~20 allocs/msg measured. The
  alloc budgets are 4–5× stale and no build path is budgeted at all.

## Decision

Five moves, one theme: **the typed value is the only currency; strings exist
only on the wire.**

### 1. `HeaderName` — header identity is an enum, not a string

```rust
#[derive(Clone, Copy /* for known variants */, PartialEq, Eq, Hash)]
pub enum HeaderName {
    Via, From, To, CallId, CSeq, Contact, Route, RecordRoute, MaxForwards,
    ContentLength, ContentType, Require, Supported, RSeq, RAck, Event,
    Expires, ReferTo, /* … the ~30 names the workspace actually touches … */
    Other(SipStr),           // extension headers, original wire casing kept
}
```

- Constructed once at scan time (the header scanner already walks the name
  bytes; matching them into the enum is free) — compact forms (`v`, `f`, `t`,
  `k`…) normalize here, killing the "get_header is not compact-aware" footgun
  that three harness files carry warning comments about.
- `impl HeaderName { pub fn class(&self) -> HeaderClass }` is the **single**
  structural/end-to-end classification table. The three sip-message-internal
  tables and the six b2bua const arrays become queries against it; consumer
  policy lists that genuinely differ (b2bua's 6-name REFER passthrough set)
  stay policy, but as `&[HeaderName]` — typo-proof and greppable.
- Canonical wire rendering (`Via`, `Call-ID`, …) lives on the enum; consumers
  can no longer emit inconsistent casing.

### 2. The value hierarchy — name-addr + tag via marker-kind polymorphism

Every structured value implements one trait; the name-addr family shares its
core through a generic, not through copy-paste:

```rust
pub trait HeaderValue: Sized + Clone {
    fn name(&self) -> HeaderName;
    fn parse(raw: &SipStr) -> Result<Self, SipParseError>;
    fn render(&self, out: &mut Wire);          // appends to the single build buffer
}

pub struct NameAddr {                          // the shared core
    pub display: Option<SipStr>,
    pub uri: Uri,                              // STRUCTURED — no more raw SipStr
    pub params: Params,
}

pub struct NameAddrHeader<K: NameAddrKind> { addr: NameAddr, _k: PhantomData<K> }
pub type From    = NameAddrHeader<kind::From>;
pub type To      = NameAddrHeader<kind::To>;
pub type Contact = NameAddrHeader<kind::Contact>;
pub type RouteEntry       = NameAddrHeader<kind::Route>;
pub type RecordRouteEntry = NameAddrHeader<kind::RecordRoute>;

impl<K: NameAddrKind> NameAddrHeader<K> {       // shared name-addr API
    pub fn uri(&self) -> &Uri;
    pub fn display(&self) -> Option<&str>;
    pub fn param(&self, name: &str) -> Option<&ParamValue>;
}
impl<K: TaggedKind> NameAddrHeader<K> {         // tag API exists ONLY on From/To
    pub fn tag(&self) -> Option<&SipStr>;
    pub fn with_tag(self, tag: impl Into<SipStr>) -> Self;
    pub fn without_tag(self) -> Self;           // replaces the string-surgery strip_tag
}
```

- `kind::From`/`kind::To` implement `TaggedKind`; `Contact`/`Route` do not —
  "does this header take a tag" is now a compile-time fact, and the refined
  views (`InDialogRequest` etc., kept from ADR-0003) still upgrade
  `tag() -> Option` to infallible.
- **Every value type is its own builder.** Construction is functional update on
  the very struct the parser produces (`Via::udp(h, p, b).rport()`,
  `msg.to().clone().with_tag(t)` — cloning a parsed value is refcount bumps,
  because `SipStr` already unifies span/owned/static text). There is no
  parallel `FromBuilder` shape whose alignment with `From` could drift:
  `parse` and `render` sit on the same type, and a
  `parse(render(v)) == v` round-trip property test over the RFC-audit corpus
  pins them mechanically.
- Generic consumers write `fn identity<H>(h: &NameAddrHeader<H>) -> &Uri` or
  bound on `HeaderValue` — the trait-based polymorphism the P-headers need:
  `P-Asserted-Identity`, `Diversion`, `History-Info` etc. are
  `Vec<NameAddrHeader<kind::PAssertedIdentity>>` style aliases, not bespoke code.
- **`Uri` is parsed, always.** `uri.host_port() -> (host, u16 /*5060 default*/)`
  and `via.sent_by() -> HostPort` replace the five hand-rolled
  `split_whitespace().nth(1)` Via peels, the nine branch extractors
  (`via.branch()`), the four angle-unwraps, and the `dest_of(strip_uri(…))`
  triple-peel. `Via` grows the full param surface (`received`, `rport`,
  generic `param(name)`) so `ViaParams` (branch/cr/lg only) is deleted.
- **The kind axis extends beyond name-addr, and per-kind policy is part of
  it.** Kinds declare capabilities as marker traits: `TaggedKind` (From/To
  only); a params policy — `RichParams` for Contact (`q`, `expires`) and
  Route/Record-Route (`lr`), `NoParams` for P-Asserted-Identity and
  P-Preferred-Identity, whose RFC 3325 grammar is bare `name-addr / addr-spec`
  with **no** header params, so `tag()`/`param()` accessors simply do not
  exist on them (today they are `Vec<NameAddr>` with a dead `tag` field);
  and comma-foldability — PAI/Route/Contact are foldable lists, From/To are
  single-valued, and the Authorization family carries commas *inside* one
  value and must never be comma-split (a fact no current call site of
  `split_top_level_commas` can express). Further families ride the same
  pattern: `TokenListHeader<K>` (Require/Supported/Unsupported/Proxy-Require/
  Allow — set-like `contains("100rel")`, killing the five hand-rolled
  option-tag splitters), `TokenParamsHeader<K>` (Event, Subscription-State,
  Content-Type, Reason, Retry-After — leading token + `;`-params),
  `NumericHeader<K>` (Max-Forwards, Content-Length, Expires, RSeq), and the
  credentials family (WWW-Authenticate/Authorization/Proxy-\*) as its own
  non-foldable shape.
- **`Params` is an ordered small-vec** (`SmallVec<[(SipStr, ParamValue); 2]>`
  or equivalent), not `BTreeMap`: wire order round-trips faithfully, lookups
  are linear case-insensitive (params are short), the common 1–2-param case
  needs no node allocation, and the parser stops lowercase-copying mixed-case
  param names.
- The ADR-0003 `TypedHeader` extension trait folds into `HeaderValue` with
  `HeaderName::Other`; `msg.header::<H>()` is the unified typed accessor for
  built-ins and extensions alike.

### 3. The immutable message — one source of truth, private fields

```rust
pub struct SipRequest  { start: RequestLine, inner: MessageCore }
pub struct SipResponse { start: StatusLine,  inner: MessageCore }

struct MessageCore {              // everything requests and responses share — written ONCE
    headers: Headers,             // ordered; THE source of truth
    core: CoreHeaders,            // from: From, to: To, cseq, call_id, via: NonEmpty<Via>, contacts
    body: Bytes,
    image: MessageImage,          // SharedText + raw Bytes — present on parsed AND built messages
}

impl SipRequest {
    pub fn from(&self) -> &From;              // typed, infallible (parser is the gate)
    pub fn to(&self) -> &To;
    pub fn via(&self) -> &NonEmpty<Via>;
    pub fn header<H: HeaderValue>(&self) -> Option<Result<H, SipParseError>>;
    pub fn headers(&self) -> &Headers;        // ordered read-only walk
    pub fn raw(&self, name: HeaderName) -> impl Iterator<Item = &str>; // escape hatch
    pub fn thaw(&self) -> RequestDraft;       // the ONLY path to a modified message
}
```

- **Fields are private.** The typed accessors cannot desync from the header
  list because nothing can mutate either — the silent-staleness hazard the
  proxy avoids by convention becomes unrepresentable.
- Received messages stay exactly as cheap as today: one `Arc<str>` image,
  every field a span, clone = refcount bumps. This is the user-stated
  invariant — *received messages are immutable, so never copy* — promoted from
  convention to type system.
- `Headers` iteration yields `(HeaderName, &SipStr)` plus typed access; the
  269 `get_header` string lookups become `msg.raw(HeaderName::X)` only where
  the value genuinely stays opaque, typed `header::<H>()` everywhere else.
- **Request/response duality is mutualized, not duplicated.** The shared read
  surface (`headers()`, `header::<H>()`, `raw()`, `body()`, `from()`/`to()`/
  `via()`/`cseq()`) is implemented once on `MessageCore` and delegated; only
  the start line and the per-direction cardinality rules (response To-tag
  presence, 3xx multi-Contact) live on the outer types and their refined
  views. The `SipMessage` enum stays as the dispatch point — today it
  re-implements `get_header`/`has_header` by matching; tomorrow it forwards
  to the one core.

### 4. `Draft` — one construction seam: thaw / freeze

The parsed message and the message under construction form a persistent/
transient pair: `SipRequest` is the frozen, image-backed value; `RequestDraft`
is its thawed, editable twin. `freeze()` is the only way to make a message,
`thaw()` the only way to open one. Origination and relay are the **same type**
with different starting states — there is no separate editor.

```rust
pub struct Draft<S: StartKind> { start: S::Line, entries: Vec<Entry>, body: Bytes }
pub type RequestDraft  = Draft<kind::Request>;  // S contributes ONLY the start line
pub type ResponseDraft = Draft<kind::Response>; // and the freeze completeness check
enum Entry { Raw(HeaderName, SipStr), Typed(KnownHeader) }  // Raw = span into SOME image

impl RequestDraft {
    pub fn new(method: Method, uri: Uri) -> Self;   // blank — origination
    pub fn thaw(msg: &SipRequest) -> Self;          // seeded — every entry a span ref
    pub fn keep(msg: &SipRequest, keep: impl Fn(&HeaderName) -> bool) -> Self;
                                                    // filtered seed (b2bua passthrough)
    // Functional updates: consume self, return Self — construction phases
    // compose as plain functions over the draft (moves, not copies).
    pub fn from(self, f: From) -> Self;
    pub fn push(self, h: impl HeaderValue) -> Self;
    pub fn push_raw(self, name: HeaderName, value: impl Into<SipStr>) -> Self;
                                                    // value NOT parsed or validated
    pub fn update<H: HeaderValue>(self, f: impl FnOnce(H) -> H) -> Self; // parse-on-touch
    pub fn vias(self, f: impl FnOnce(HeaderList<Via>) -> HeaderList<Via>) -> Self;
    pub fn body(self, body: Bytes, ct: MediaType) -> Self;
    pub fn freeze(self) -> Result<SipRequest, IncompleteDraft>;
    pub fn render_unchecked(self) -> Bytes;         // bytes from ANY state — test lanes
}
```

- **Multi-phase construction stays immutable in style.** Consuming-`self`
  methods both chain and split across helpers —
  `fn copy_dialog_tags(d: RequestDraft, existing: &SipRequest) -> RequestDraft`
  is a plain function over the draft; a pipeline of such phases compiles to
  in-place mutation (each step is a move, never a copy).
- **Seeding from parsed state is free.** `thaw` copies no text — every entry
  is a span ref into the source image (refcount bumps). The proxy hop is
  `thaw` + touch the routing headers only; the b2bua passthrough case is
  `keep` with a `HeaderName` predicate instead of today's six string arrays.
  Entries pointing into *different* images coexist in one draft (spans of the
  a-leg INVITE next to spans of a stored 2xx); `freeze` memcpies each from
  wherever it lives.
- **Late edits are first-class.** `update::<H>` parses the entry on first
  touch (Raw → Typed in place), applies the functional update, and stores the
  typed value. Untouched entries never pay a parse — reading a thawed draft
  costs nothing beyond the original parse.
- **Multi-value headers: wire lines + one logical view.** Entries mirror the
  wire (one entry per line; a line may carry a comma-fold). Typed access
  flattens lines and folds into `HeaderList<H>` in wire order (`push_front`,
  `pop_front`, `iter`) — the §7.3.1-aware entry pop lives once, here. Whether
  a header may fold at all is per-kind policy (§2), so a credentials header
  can never be mis-split. Untouched lines keep their byte layout; values
  added through the view render line-per-value. `freeze` derives the
  `NonEmpty<Via>` core from the same view.
- **`freeze()` renders once and never re-parses**: one pre-sized buffer
  (entry lengths are known), spans recorded while writing, Raw entries
  memcpy'd, Typed entries rendered directly (no per-value
  `format!`-String→`Arc<str>` double allocation); the result carries its own
  image, with the typed core taken from the entries in hand. This deletes:
  scenario-harness's mutate→serialize→reparse loop, the proxy's
  parts-serializer + stale-fields convention, and the b2bua's
  clone-whole-message-to-serialize sites. `freeze` is fallible only on blank
  drafts (mandatory header missing); a thawed draft started valid and no
  operation can make it incomplete.
- **One draft engine for both directions.** `Draft<S: StartKind>` mirrors the
  header marker-kind pattern: the entry list, functional updates, list views,
  and the render engine are written once; `kind::Request` / `kind::Response`
  contribute only the start-line type and the mandatory-set check `freeze`
  runs. No request/response copy-paste in the construction layer.
- **Deliberately invalid wire output is a distinct, loudly-named exit** —
  `render_unchecked(self) -> Bytes`. It emits bytes from *any* draft state: a
  missing From, a `push_raw` garbage Via, a stray tag on a P-Asserted-Identity
  — no completeness check, no typed message produced. The core invariant
  survives precisely because of the split: invalidity can leave the stack
  only as bytes, never as a `SipRequest`/`SipResponse` value, so "a typed
  message is always valid" holds while the test lanes (peer-side
  `allow_violation` scenarios, the RFC compliance invalid corpus, the
  `deviation` module's CSeq mutations) get structured
  "valid-except-this-one-deviation" generation instead of hand-`format!`ed
  whole datagrams. SUT code paths use `freeze` only; `render_unchecked` is
  the reviewable grep target.
- The mutation vocabulary sip-proxy had to build locally (prepend, upsert,
  remove-first, comma-aware entry pop, received/rport stamping, Record-Route
  composition) moves here — `sip-proxy/src/headers.rs` shrinks to pure policy
  (which params go on *our* Record-Route), b2bua/e2e-core/scenario-harness
  delete their copies. `RouteSet` becomes first-class:
  `resp.record_route_set().reversed()` replaces the four copy-pasted
  get→comma-split→trim→reverse blocks.

### 5. Generators become recipes over the draft

The dialog/ACK/CANCEL *knowledge* in `generators` — CSeq stepping, route-set
application, tag placement — is the valuable part and stays; the string
assembly goes. The stringly `Generate*Opts` fields (`from: String`,
`vias: Vec<String>`, `cseq: String`) become typed (`From`, `Vec<Via>`,
`CSeq`), so the b2bua stops round-tripping parsed messages through strings to
relay them. Recipes take the mandatory fields as arguments, so they stay
infallible (`freeze` cannot miss). **`hydrate_request` disappears from the
build path** — and with it the self-re-parse and the ten optional-header scans
over generated headers; it survives only for genuinely raw input (snapshot
rehydration, test fixtures). Built messages carry a real image, so
capture/replay templating and any later re-read of a generated message is
uniform with the parsed case.

### Alignment between the shapes is confined and pinned

- **Per header value: nothing to align.** Parse and render live on one type
  (§2); a value is its own builder. Pinned by `parse(render(v)) == v`
  round-trip property tests over the RFC-audit corpus.
- **Message ↔ draft: exactly two functions know both shapes** (`thaw`,
  `freeze`). Pinned by `thaw(&m).freeze() == m` (image identity aside) over
  the same corpus, plus `parse(freeze(d).raw()) == freeze(d)` for built
  messages. No codegen or macro mirroring — the draft holds strictly *less*
  than the message (no image, no derived core), so alignment is one
  derivation, not two parallel definitions.

### Parse-side change: one dispatch pass

With `HeaderName` assigned at scan time, mandatory + optional extraction
becomes a single walk over the header list dispatching on the enum
(`Via → collect`, `PAssertedIdentity → parse into optional`, …) instead of
today's per-header-type full scans (5 mandatory probes + 10 optional scans +
intermediate `Vec<&SipStr>` per probe). Same eager + non-fatal semantics as
ADR-0003's amendment — laziness stays rejected — at O(N) instead of O(15·N),
and the `contact_list.clone()` / triple-Vec Via staging disappear into the
incremental fill.

## Considered and rejected

- **Keep `message_helpers` free functions as the API, just add more.** That is
  the current trajectory and it demonstrably fails discoverability: three
  helpers had zero consumers while five crates re-rolled Via sent-by. Methods
  on the type the caller is already holding are findable; free functions on
  `&str` are not.
- **Mutable `SipMessage` (make `headers` pub-mut and re-derive typed fields on
  demand).** Re-introduces the desync class of bug as a permanent hazard and
  forces either interior-mutability caching or re-parse-on-read. The
  immutable-plus-editor split matches the actual traffic pattern: messages are
  read many times, edited once, at a boundary.
- **Trait-object header list (`Vec<Box<dyn Header>>`, rsip-style).** Costs an
  allocation + vtable per header on the hot parse path and loses zero-copy
  spans. The enum-name + on-demand-typed-value model keeps parse lean and pays
  typed-parse cost only for headers actually touched.
- **Per-type structs for the name-addr family (`struct From`, `struct To`, …)
  instead of the marker generic.** Duplicates the shared API ~8× or hides it
  behind a delegation macro; the marker kind gives the same nominal typing
  (`From` ≠ `To` at compile time) with one implementation and lets `TaggedKind`
  express the tag capability as a bound.
- **Typestate completeness tracking on the draft
  (`Draft<HasFrom, HasTo, …>`).** Compile-time "all mandatory headers set" is
  attractive, but every phase helper
  (`fn apply_route_set(d: Draft<S1>) -> Draft<S2>`) would drag the full
  generic state through its signature — hostile to exactly the multi-phase,
  split-across-functions construction style the draft exists to serve. A
  fallible `freeze()` on blank drafts (thawed drafts cannot be incomplete)
  keeps signatures flat.
- **Patching raw bytes on relay (splice the changed headers into `raw`).**
  Tempting given `raw` is already shared, but Via-prepend/Route-pop shift
  offsets, making every span remap; the measured serializer cost (one buffer,
  `extend_from_slice` per header) is already low — render-once with span
  recording buys the same single-copy bound with none of the fragility.

## Performance invariants and guardrails

Hard invariants the implementation must keep (enforced by refreshed alloc
budgets + new benches):

1. Parse: exactly one text copy (the image); body/raw slicing stays
   refcount-only. Target ≤ ~10 allocs/msg (from measured 20–21) via
   small-vec `Params` + one-pass dispatch.
2. Thawed-draft hop (proxy Via/RR/MF/Route rewrite): one output buffer +
   O(edited) value allocations; untouched header bytes memcpy'd once, never
   re-parsed. Target ≤ ~12 allocs/hop (from measured 33 on a *simpler*
   synthetic hop).
3. Blank-draft build: one output buffer; no `hydrate`, no String→Arc double
   copies. First-ever build-path budget to be set from measurement.
4. Re-baseline `alloc_budget.rs` (current budgets are 4–5× stale on allocs) and
   extend it + the criterion bench to cover: blank-draft build, thawed-draft
   hop with the real proxy rewrite set, and `stamp_received_rport` equivalent.

## Consequences / migration

Breaking, migrated crate-by-crate; mechanics, ordering, and the live tracker
are in [docs/todos/header-model-migration.md](../todos/header-model-migration.md)
(additive coexistence: new namespace lands beside the old API, consumers port
one crate per commit, legacy is deleted last as the completion proof). Each
step deletes its local surgery module as it lands. Casualty list on completion:

- deleted: `message_helpers::{headers, name_addr, via}` free-function surface,
  `ViaParams`, `extract_tag`/`strip_tag`/`extract_name_addr_uri`,
  `set_header`-returns-new-Vec, `serialize_*_parts`, stringly `Generate*Opts`
  fields, `sip-proxy/src/headers.rs` mechanics, scenario-harness/e2e-core
  `prepend_header` copies, the six b2bua header-name arrays, the
  five Via-sent-by and nine branch hand-rolls, `template.rs`'s
  `REGENERATED_HEADERS` vs `relay.rs`'s `STRUCTURAL_HEADERS` split;
- unchanged in spirit: refined views (`InDialogRequest`, `SipResponseTagged`),
  `sniff` (lenient raw scanning is a different concern), `SipStr`/`SharedText`,
  the serializer's single-buffer core, the RFC-audit gate;
- out of scope, unblocked by this: `call::ALegInviteSnapshot`'s duplicate
  header type and the 19 `rebuild_a_leg_invite` re-hydrations (the `call`
  crate's no-sip-message-dep rule deserves its own decision once built
  messages carry an image and can be snapshotted as bytes + rebuilt cheaply);
  `template.rs` re-expression over `HeaderName::class()`.
