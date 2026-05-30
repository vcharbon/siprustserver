# Typed header access model (replacing the TS `getHeader` registry)

**Status:** accepted (2026-05-30); amended 2026-05-30 (optional headers: eager
non-fatal instead of lazy — see "Amendment" below)

The TypeScript message layer exposed headers through a `getHeader<K>` typed
registry built on three TS-only features — declaration merging, structural
subtyping for refinements (`InDialogRequest`, `TaggedNameAddrField`), and a
typed index map — none of which exist in Rust. We replace it with a layered,
"parse-don't-validate" model where the parser is the single gate and the type
carries the proof, so downstream code (which touches this everywhere) is never
defensive about presence:

- **A. Mandatory headers are eager non-`Option` fields** on `SipRequest` /
  `SipResponse` (From, To, Call-ID, CSeq, Via). The parser rejects messages
  that omit them, so consumers never check existence.
- **B. Mandatory non-empty lists use `NonEmpty<T>`** — `via.first()` is `&Via`,
  the port of `NonEmptyReadonlyArray`.
- **C. Grammar-mandatory sub-fields are non-`Option`** (`NameAddr.uri`).
  Context-dependent sub-fields (`tag`) stay `Option` on the base.
- **D. Context guarantees are flat, borrowed, refined newtype views** built
  once at a boundary (`InDialogRequest`, `InviteRequest`, `SipResponseTagged`),
  each `Deref`-ing to its base so no accessor is duplicated. Inside
  `fn handle(req: &InDialogRequest)` the `to_tag()` is an infallible `&str`;
  inside `fn on_invite(req: &InviteRequest)` `contact()` is the right
  (single) cardinality. Accessor names need not match across views.
- **E. Extensibility is preserved via a `TypedHeader` trait** — the open,
  compile-time registry replacement: an integrator crate implements
  `TypedHeader` for its own type and calls `msg.typed::<H>()`, with no change
  to this core and no global mutable state. A raw `get_header(name) -> Vec<&str>`
  escape hatch covers unknown headers.

## Considered and rejected

- **Raw `get_header(name) -> Vec<&str>` as the *only* API.** Loses the typed,
  registered-parser extensibility the source has (see
  `header-registry-extension.test.ts`). Kept only as the unknown-header hatch.
- **Keep `tag` / mandatory fields as `Option` everywhere and re-check at call
  sites.** Defeats the stated goal; pushes defensiveness into every consumer.
- **Phantom-typestate generics `Request<D, M>` to cross refinement axes.**
  Composes crossings for free but burdens every signature with bounds. We chose
  flat views combined on demand (a hand-written combined view only where a site
  genuinely needs both axes), since crossings are rare. Revisit if they prove
  common.

## Consequences

- ~4–6 refined view types plus their smart constructors; runtime cost is
  ~zero (borrowed pointer + one validation check; `Deref` avoids duplication).
- Refined views carry a lifetime; for owned storage use the owned `(SipRequest)`
  form. Handlers take `&`, so borrowed views are the norm.
- `typed::<H>()` does **not** memoize (unlike the TS registry). Deliberate:
  Rust parsing is cheap and a per-message type-erased cache fights the borrow
  checker. Bind to a `let` on hot paths. Tracked perf knob, not a silent drop.
- Crossing two refinement axes needs a hand-written combined view (no Rust
  intersection types).

## Amendment (2026-05-30) — optional structured headers are eager + non-fatal, not lazy

The original model described the optional structured headers (P-Asserted-Identity,
Diversion, History-Info, RAck, Refer-To, Geolocation, …) as **lazy** `Result`
accessors, mirroring the TS `getHeader`. We instead parse them **eagerly and
non-fatally** into `Result` fields on the message (`OptionalHeaders`), with a
separate opt-in `SipMessage::validate_strict()` for strict rejection.

**Why the TS laziness does not carry over:** its motivations were GC pressure
and skipping unread headers — both largely moot in Rust (no GC; a present
structured header is present because a rule reads it). The only motivation that
survives — *a malformed optional header must not reject a routable message* — is
the **non-fatal** axis, not the **lazy** axis, and is satisfied by storing a
`Result` field. Laziness would force per-message interior mutability
(`OnceCell`) to memoize on `&self`, making the message non-trivially `Sync`
(it crosses per-call worker fibers) and fighting the borrow checker.

**Decision:**
- Optional built-in headers: parsed eagerly at parse time into
  `OptionalHeaders { p_asserted_identity: Result<Vec<NameAddr>, _>, … }`.
  `parse()` stays tolerant — a malformed one is captured as `Err`, the message
  still parses.
- `SipMessage::validate_strict()` is the port of `runAllStrictLazyParsers`: it
  re-validates Date/From/To/Contact grammar + every optional header and returns
  the first violation. Security-sensitive callers (and the compliance matrix's
  invalid corpus) opt into it; production stays tolerant. Note it intentionally
  over-rejects some RFC-valid torture display names, so — as in the TS — it is
  applied to the invalid/canonical corpora, not the full valid torture set.
- Extension headers stay on-demand via `TypedHeader` (the core can't eagerly
  parse types it doesn't know); non-fatal by returning `Result`.

Net: immutable, `Send + Sync` messages; no `OnceCell`; same tolerance; the
strict surface the tests need. If profiling later shows a *specific* present-
but-unread header is hot, make that one lazy as a targeted optimization.
