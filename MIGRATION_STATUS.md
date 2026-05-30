# Migration status

Port of [sipjsserver](./portsource/sipjsserver) → Rust. One **layer** per row;
each maps to a workspace **crate**. Status legend:

- ✅ **ported** — code + tests ported and passing
- 🟡 **scaffolded** — directories, manifests, trait/types in place; bodies are `todo!()`
- ⬜ **pending** — not started

| Layer | Crate | Source | Status |
|---|---|---|---|
| **SIP message** (parse/serialize/validate) | `crates/sip-message` | `src/sip/` (message core) | ✅ parser + serializer + sdp + generators + message-helpers/sipfrag ported; full test corpus green (159 tests). rvoip oracle behind `rvoip-oracle`; ABNF corpus pending `abnfgen` |
| **Network / UDP** (transport, SignalingNetwork) | `sip-net` *(slice 2)* | `src/sip/{UdpTransport,SignalingNetwork,BufferedUdpEndpoint}.ts` | ⬜ pending |
| **Transaction / dispatch** | `sip-net` or own crate | `src/sip/{TransactionLayer,SipRouter,PerCallDispatcher}.ts` | ⬜ pending |
| **CallContext data model** | `call` | `src/call/` | ⬜ pending |
| **Rule engine** | `rules` | `src/b2bua/rules/` | ⬜ pending |
| **Call limiter** | `limiter` | `src/call/CallLimiter*.ts` | ⬜ pending |

---

## Slice 1 — SIP message layer (`crates/sip-message`)

**Source release:** sipjsserver @ submodule commit pinned in `.gitmodules`
(record exact SHA here once the port begins).
**Scope (confirmed):** pure, synchronous message layer only — no tokio, no
I/O, no clock. UDP/transport/transaction deferred to slice 2.

### Decisions (see docs/MIGRATION_STRATEGY.md + ADRs)
- Port the **custom** zero-regex parser whole; `rvoip-sip-core` kept as a
  dev-only **parity oracle** ([ADR-0001](docs/adr/0001-port-custom-parser-rvoip-as-parity-oracle.md)).
- Cargo **workspace, crate-per-layer** ([ADR-0002](docs/adr/0002-cargo-workspace-crate-per-layer.md)).
- The `SipParser` **trait is the DI seam**; errors as `Result<_, SipParseError>`;
  no Effect Tag at this layer.
- ABNF tests run from a **frozen corpus** (N=1000/target), regen via xtask.

### Source modules → Rust (port checklist)
| Source | Rust module | Status |
|---|---|---|
| `parsers/errors.ts` | `error.rs` | 🟡 type defined |
| `types.ts` + `header-registry.ts` | `types.rs` | 🟡 type model defined ([ADR-0003](docs/adr/0003-typed-header-model.md)): eager mandatory fields, `NonEmpty<Via>`, refined views, `TypedHeader` trait. Field-parsing wired by the parser port. |
| `parsers/interface.ts` + `Parser.ts` | `parser/mod.rs` | 🟡 trait + limits defined (no Effect Tag) |
| `parsers/custom/scanner.ts` | `parser/custom/scanner.rs` | ✅ ported |
| `parsers/custom/start-line.ts` | `parser/custom/start_line.rs` | ✅ ported |
| `parsers/custom/compact-forms.ts` | `parser/custom/compact_forms.rs` | ✅ ported |
| `parsers/custom/headers.ts` | `parser/custom/headers.rs` | ✅ ported |
| `parsers/custom/structured-headers.ts` | `parser/custom/structured_headers.rs` | ✅ ported |
| `parsers/extract-fields.ts` (ADR-0007 gates) | `parser/custom/extract_fields.rs` | ✅ ported (wire+hydrate; finalize/hydrate wrappers fold into `CustomParser`) |
| `parsers/custom/lazy-parsers.ts` | `parser/custom/optional_headers.rs` | ✅ ported as **eager + non-fatal** (`OptionalHeaders`) + `SipMessage::validate_strict()` ([ADR-0003 amendment](docs/adr/0003-typed-header-model.md)) |
| `parsers/custom/index.ts` | `parser/custom/mod.rs` (`CustomParser`) | ✅ ported (full parse pipeline) |
| `parsers/native-adapter.ts` (role) | `parser/rvoip.rs` (`RvoipParser`, feature-gated) | ✅ ported — wraps `rvoip-sip-core` 0.1.26 behind `rvoip-oracle`; thin accept/reject parity shell (matrix never reads its fields) |
| `Serializer.ts` | `serializer.rs` | ✅ ported (`serialize`/`sip_summary`/`message_summary`, Content-Length safety net) |
| `SdpUtils.ts` + `SdpAnswerFromOffer.ts` | `sdp.rs` | ✅ ported (codec-profile extract, held-SDP, answer-from-offer, strict `validate_sdp_body`) |
| `generators.ts` | `generators.rs` | ✅ ported (all 8 generators; `StackDialog`/`InviteClientTransactionHandle` are minimal local input shapes pending slice-2 `Dialog`/`TransactionLayer`) |
| `SipFragUtils.ts`, `MessageHelpers.ts` | `sipfrag.rs`, `message_helpers.rs` | ✅ ported — sipfrag whole; MessageHelpers **pure half** only (header accessors + structured readers). RNG identifier generators + byte-level overload/dispatcher helpers deferred to slice 2 (see un-ported list) |

### Tests to port
| Source test | Rust home | Status |
|---|---|---|
| `parser-compliance.test.ts` (matrix, `custom` column) | `tests/compliance_matrix.rs` | ✅ 63 fixtures (RFC4475 valid/invalid, CVE, param-gaps, RFC5118 IPv6, strict-valid); byte-exact fixtures dumped from TS; invalid corpus now uses full `parse + validate_strict` (only the 2 TS-lenient start-line cases remain lenient) |
| `Parser.test.ts` (RFC 4475 field extraction asserts) | `tests/parser.rs` | ✅ 11 (field-content + eager-leniency split) |
| `parser-extraction.test.ts` | `tests/parser_extraction.rs` | ✅ 17 (jssip cross-parser block dropped — no jssip) |
| `parser-response-totag.test.ts` | `tests/parser_response_totag.rs` | ✅ 3 |
| `parser-x-api-call-fold.test.ts` | `tests/parser_fold.rs` | ✅ 3 (jssip/native oracle columns dropped) |
| `Serializer.test.ts` | `tests/serializer.rs` | ✅ 7 (warn-spy adapted to output bytes) |
| `sipfrag-utils.test.ts` | `tests/sipfrag.rs` | ✅ 4 |
| `contact-set.test.ts` | `tests/contact_set.rs` | ✅ 6 (added `contacts` field to `SipResponse` for 3xx redirects) |
| `lazy-headers.test.ts` | `tests/lazy_headers.rs` | ✅ 24 (adapted to eager `OptionalHeaders`; memoization → eager same-ref) |
| `header-cardinality.test.ts` | `tests/header_cardinality.rs` | ✅ 3 |
| `header-registry-extension.test.ts` | `tests/header_registry_extension.rs` | ✅ 3 (adapted to `TypedHeader` trait; memoization/collision N/A) |
| `header-registry-typing.test.ts` | `tests/header_registry_typing.rs` | ✅ 5 (compile-time field-type guarantees) |
| `sdp-utils.test.ts`, `sdp-answer-from-offer.test.ts` | `tests/sdp_utils.rs`, `tests/sdp_answer.rs` | ✅ 11 + 12 |
| `generators.test.ts` | `tests/generators.rs` | ✅ 24 |
| (pure MessageHelpers smoke) | `tests/message_helpers.rs` | ✅ 8 (no TS counterpart; locks the accessors generators depend on) |
| ABNF fuzz suite (`scripts/abnf-fuzz`) | `tests/abnf_fuzz.rs` + `xtask abnf-regen` | 🟡 driver + classifier ported & `xtask` regen implemented; **corpus generation pending `abnfgen`** (not installed). Test skips-with-log until the corpus is generated |
| Parser micro-benchmark (`bench/sip-parser-bench.ts`) | `benches/sip_parser.rs` | ✅ measuring (INVITE ~5.3µs, 200 OK ~4.9µs) |
| (smoke) walking-skeleton parse + refined views + 5 ADR-0007 rejections | `tests/parser_smoke.rs` | ✅ 9 passing |

### Un-ported with justification
- `parsers/jssip-adapter.ts` (`jssipParser`) — JsSIP JS-library adapter; no
  Rust equivalent. Its role in the compliance matrix is taken by the
  `rvoip` oracle.
- `parsers/sip-parser-adapter.ts` (`sipParserNpm`) — the `sip` npm package
  adapter; same reasoning.
- (none for the parser layer — `lazy-parsers.ts` is now ported as
  `optional_headers.rs`; the previously-noted 3.1.2.12/.13/.15 gap is closed by
  `validate_strict()`.)
- `MessageHelpers-random.test.ts` (seeded-RNG identifier generators) — the
  `newTag`/`newBranch`/`newCallId`/`currentRng` helpers read a fiber-local
  Effect `Random` reference. That RNG seam belongs to the **network slice**
  (where determinism is plumbed); the Rust port will inject an RNG at that
  boundary. Deferred with the helpers themselves.
- The byte-level overload/dispatcher helpers in `MessageHelpers.ts`
  (`buildStatelessReject503Buffer`, `isInviteRequestBuffer`,
  `bufferHasEmergencyMarker`, `bufferHasToTag`, `jitteredRetryAfter`) — Tier-1
  UDP / dispatcher concerns, deferred to **slice 2** with their consumers.
- The `parser-extraction.test.ts` "custom vs JsSIP" cross-parser equivalence
  block and the `jssip`/`native` oracle columns in `parser-*.test.ts` — there
  is no JsSIP in the Rust stack ([ADR-0001](docs/adr/0001-port-custom-parser-rvoip-as-parity-oracle.md));
  the `rvoip` oracle (compliance matrix, `--features rvoip-oracle`) is the
  surviving second column.
- Memoization assertions in `lazy-headers.test.ts` / `header-registry-extension.test.ts`
  ("parse called once", "same Result reference") — not applicable to the eager
  model (the value is parsed once at parse time and stored; `typed::<H>()` is a
  re-parse by design, [ADR-0003](docs/adr/0003-typed-header-model.md)). The
  runtime "re-registering a built-in throws" guard is likewise N/A: built-ins
  are concrete fields, not `TypedHeader` impls, so shadowing is impossible at
  compile time.

> The effect-layer-test **4 wrappers** (`propertyTest`/`paranoidInputs`/
> `parity`/`scopedAudit`) are **not** a message-layer concern — in the source
> they wrap `SignalingNetwork`. They are deferred to the network-layer slice.
