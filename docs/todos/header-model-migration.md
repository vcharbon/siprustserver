# ADR-0025 header-model migration — plan & tracker

Implements [ADR-0025](../adr/0025-header-value-hierarchy-and-message-editing.md).
Read the ADR first; this file is the *how*, the ADR is the *what/why*.

## Strategy: additive coexistence, green at every commit

The workspace must compile and pass the default test lane after **every**
phase commit. No long-lived red branch, no compat shims to unwind later:

1. New types land in a **new namespace** (`sip_message::header`, `sip_message::draft`)
   alongside the old API. Nothing existing changes shape until nobody uses it.
2. The parser populates both the old typed fields and the new `CoreHeaders`
   during the transition (temporary double cost, measured, accepted).
3. Consumer crates port **one crate per commit** to the new API. Field access
   (`req.from`) and accessor (`req.from()`) coexist legally in Rust, so
   porting order between crates is free of type-level coupling.
4. Only when the last consumer is ported: delete the legacy API, privatize
   message fields, restructure internals to `MessageCore`, and re-baseline
   perf budgets. Deletion is the proof the port is complete — `grep` finds
   stragglers, the compiler enforces the rest.

Branch: all work on `feat/adr0025-header-model`; merge to master when the
teardown phase is green.

## Hard rules for every step (workflow agents: these are not optional)

- ONE compile/test process at a time, capped (CLAUDE.md):
  `systemd-run --user --scope -q -p MemoryMax=12G -p CPUQuota=1200% nice -n 10 cargo test --workspace --jobs 6`
- One phase (or one crate) per commit:
  `refactor(sip-message): ADR-0025 <phase> — <one-line>` /
  `refactor(<crate>): port to ADR-0025 header model`.
- Never waive an RFC-audit rule to mute a finding caused by your change
  (CLAUDE.md test rules). If the audit flags your port, the port is wrong.
- Timed tests you touch: read docs/testing/test-clock.md first.
- Tick this file's tracker + append findings to the log in the same commit.
- SIP header/message extraction stays in sip-message — a port that moves
  parsing INTO a consumer crate is a regression, stop and reread the ADR.

## Phases

### M1 — `HeaderName` + one-pass parse dispatch (sip-message internal)

- `HeaderName` enum (known names + `Other(SipStr)`), compact-form aware,
  assigned at scan time in `parser/custom/headers.rs`; `SipHeader` grows the
  enum name alongside (or replacing) the raw-name `SipStr` — old string APIs
  keep working via `HeaderName::as_wire_str()`.
- `HeaderName::class()` — the single structural/end-to-end table.
- Rewrite `extract_request_fields`/`extract_common_fields`/`extract_optional`
  as ONE dispatch walk (kills the 5 mandatory probes + 10 optional scans +
  per-probe `Vec<&SipStr>`; kills `contact_list.clone()` at
  `extract_fields.rs:573`).
- Acceptance: workspace green; `decode/*` alloc counts strictly lower than
  the current measured 20–21 (record numbers in the log).

### M2 — value hierarchy + draft engine (new namespace, additive)

- `sip_message::header`: `Uri` (unifying `RequestUri`/`Uri`/`ParsedSipUri`),
  ordered small-vec `Params`, `NameAddr`, `NameAddrHeader<K>` + kind traits
  (`TaggedKind`, `RichParams`/`NoParams`, foldability), `Via` (full param
  surface incl. `received`/`rport`/generic), `TokenListHeader<K>`,
  `TokenParamsHeader<K>`, `NumericHeader<K>`, credentials shape. Every type:
  `parse` + `render` + corpus round-trip test.
- `sip_message::draft`: `Draft<S: StartKind>`, `Entry`, `HeaderList<H>`,
  `thaw`/`keep`/`freeze`/`render_unchecked`/`push_raw`, render-with-span
  recording (extend `serializer.rs`'s single-buffer core).
- New accessors on `SipRequest`/`SipResponse`/`SipMessage` (`from()`, `to()`,
  `via()`, `header::<H>()`, `raw(HeaderName)`, `thaw()`) computed from the
  parse; old pub fields untouched.
- Acceptance: workspace green; round-trip pins (`parse∘render` per value,
  `thaw→freeze` identity over the parser corpus + torture fixtures) in place.

### M3 — generators become recipes (sip-message internal)

- Reimplement `generators::*` bodies over `Draft` (public signatures kept for
  now). `hydrate_request` leaves the generator path; built messages carry an
  image. `Generate*Opts` gain typed twins; stringly fields marked deprecated.
- Acceptance: workspace green; `sip-message/tests/generators.rs` +
  scenario-harness lanes green; first build-path alloc measurement recorded.

### M4..M11 — consumer ports, one crate per commit, in this order

Order = dependency + risk (small first, audit last so it validates the rest):

| # | crate | known work (from the 2026-07-26 surveys) |
|---|---|---|
| M4 | `sip-txn` | already the cleanest typed consumer; port reads, drop string lookups |
| M5 | `sip-proxy` | `core/request/route.rs:181` clone→`thaw`; `core/response.rs:105` same; `headers.rs` (237 L) shrinks to Record-Route *policy* — prepend/upsert/pop/received-rport move to draft vocabulary; delete local `via_sent_by`, comma-split wrapper |
| M6 | `b2bua-sdk` + `b2bua` | one step — coupled via `MessageTransform` (`b2bua-sdk/src/model.rs:336`, becomes typed draft ops). The big one. Delete: `relay.rs:613 top_via_host_port`, `:621 strip_uri`, `:104 dest_of`, `respond.rs:322 top_via_dest`, `relay_response.rs:341 via_sent_by`, `relay_request.rs:209 rewrite_rack` (→ typed RAck), `dialog_track.rs:306 unwrap_angle`, `refer_transfer.rs:88 refer_target_uri`, `reliable_rseq` ×2 (`relay_first_18x.rs:56`, `promote_pem.rs:44` → TokenList+Numeric), the 6 header-name arrays (`initial_invite.rs:34,335`, `respond.rs:336`, `relay.rs:296,421,485`, `refer_transfer.rs:101` → `HeaderName` consts/`class()`), route-set reconstruction ×3 (`dialog_track.rs:59,148,274`), serialize-clone sites (`relay_request.rs:166`, `relay.rs:362`, `originate.rs:213`, `relay_response.rs:160`), `MessageTransform` application → draft ops. `rebuild_a_leg_invite` stays but returns a frozen message built via draft |
| M7 | `scenario-harness` | `client_invite.rs:360` mutate→serialize→reparse loop → `thaw`/`freeze`; `agent/proxy.rs:39-95` via/RR/prepend → draft; template `frozen_headers` → `push_raw` entries; `legpick.rs:172-197`, `callee_group.rs:349` extraction → typed; server_txn route-set ×1 |
| M8 | `e2e-core` + `e2e-model` + `announcement` | `registrar.rs` prepend_header/top_via_addr/via_sent_by_addr → draft + typed Via; sweep e2e-model + announcement reads |
| M9 | `sip-net` (rfc_audit) | read-side only: `top_via_branch` ×4, `cross_generic.rs:52 read_top_via_rport`, `dialog_model.rs route_is_loose`, `txn_correlation.rs:259 split_option_tags` → typed accessors. CARE: the audit reads deliberately-broken *peer* output — keep tolerant paths (`Result` typed access / `sniff`), do not let a port make the auditor reject what it must diagnose |
| M10 | `sip-pcap`, `loadgen` | pcap `query/project.rs:136` unwrap → NameAddr; loadgen is `sniff`-only — verify no port needed beyond type renames |
| M11 | harness/test crates | `b2bua-harness`, `failover-harness` (ha-acceptance danger zone — comment scrub rules apply), remaining `tests/` string lookups |

Coverage verified against the Cargo.toml dep graph (2026-07-26): the direct
sip-message dependents are exactly {sip-txn, sip-proxy, b2bua-sdk, b2bua,
b2bua-harness, scenario-harness, e2e-core, e2e-model, announcement, sip-net,
sip-pcap, loadgen, failover-harness} — all covered by M4–M11. Runner crates
(sip-proxy-runner, b2bua-runner, e2e-web, e2e-cli, callshapes, …) have no
direct dep and are covered transitively.

Excluded: `call` (no sip-message dep BY DESIGN — ADR-0008; its
`ALegInviteSnapshot` duplication is a separate decision, see ADR-0025
out-of-scope).

Per-crate procedure: map that crate's sip-message imports (`grep -rE
'sip_message|message_helpers|generators::' crates/<c>`), port reads → typed
accessors, writes → draft, delete the crate's local surgery/extraction
helpers listed above, run capped workspace test, tick tracker, commit.

### M12 — teardown (the deletion is the completion proof)

- Delete: `message_helpers::{headers,name_addr,via}` free-fn surface,
  `ViaParams`, `extract_tag`/`strip_tag`/`extract_name_addr_uri`,
  `set_header`/`remove_header`, `serialize_request_parts`/`_response_parts`,
  stringly `Generate*Opts` fields, old `NameAddr`/`Via`/`Contact`/`RequestUri`
  /`Uri` types, `TypedHeader` (absorbed), template header-class tables
  (re-express over `HeaderName::class()`).
- Privatize `SipRequest`/`SipResponse` fields; restructure to `MessageCore`;
  drop the transitional double-population.
- Acceptance: `grep -rE 'get_header\(|message_helpers::' crates
  --include='*.rs'` returns only sip-message internals; workspace green.

### M13 — perf re-baseline

- Re-measure `alloc_budget.rs`; set budgets at measured+20 % (per its own
  stale comment); add blank-draft build, thawed-draft hop (real proxy rewrite
  set), and received/rport-stamp cases; extend the criterion bench likewise.
- Targets (ADR): parse ≤ ~10 allocs/msg, hop ≤ ~12. Run `just test` +
  loadgen smoke; record before/after in the log.

## API mapping (old → new), for port agents

| current | target |
|---|---|
| `get_header(&m.headers, "x")` | `msg.raw(HeaderName::X).next()` — or better, `msg.header::<H>()` |
| `get_headers(&m.headers, "via")` + re-parse | `msg.via()` / `msg.list::<Via>()` |
| `req.from.tag` (field) | `req.from().tag()` |
| `extract_tag(value)` | never re-parse a value you got from a message — `msg.to().tag()` |
| `parse_via_params(v).branch` | `via.branch()` |
| `via_sent_by(v)` / hand-rolled ×5 | `via.sent_by() -> HostPort` |
| `unwrap_angle` / `extract_contact_uri` | `contact.uri()` (structured `Uri`) |
| `dest_of(strip_uri(x))` | `uri.host_port()` |
| RR reconstruction (get→split→trim→reverse) | `resp.record_route_set().reversed()` |
| option-tag `split(',')` checks | `msg.header::<Require>()?.contains("100rel")` |
| structural-name const arrays | `HeaderName::class()` / `&[HeaderName]` |
| `headers.clone()` + `set_header` + `serialize_*_parts` | `msg.thaw()…freeze()` |
| mutate → serialize → re-parse | `thaw…freeze` (typed core stays in sync) |
| `hydrate_request` (build) | `RequestDraft::new(…)…freeze()` / recipes |
| `format!`-assembled peer-violation fixtures | draft + `push_raw` + `render_unchecked` (opportunistic — don't block a port on it) |
| `stamp_received_rport_on_via` | draft `top_via` stamp op (typed params) |

## Tracker

- [x] M1 HeaderName + one-pass dispatch
- [x] M2 value hierarchy + draft engine
- [x] M3 generators → recipes
- [x] M1–M3 review findings addressed (fallible edits, Request-URI fidelity)
- [x] M4 sip-txn
- [x] M5 sip-proxy
- [x] M6 b2bua-sdk + b2bua
- [x] M7 scenario-harness
- [x] M8 e2e-core + e2e-model + announcement
- [x] M9 sip-net rfc_audit
- [x] M10 sip-pcap + loadgen
- [x] M11 harness/test crates
- [x] M12 teardown (delete legacy, privatize, MessageCore)
- [ ] M12b `Generate*Opts` stringly fields → typed (deferred out of M12, see log)
- [x] M13 perf re-baseline
- [ ] merge `feat/adr0025-header-model` → master

## Findings log

Append per-phase: measurements, surprises, suspicions (same rules as
docs/todos/large-file-cleanup-program.md — raise, don't silently fix).

### M1 — HeaderName + one-pass parse dispatch

**Measurements** (`cargo test -p sip-message --test alloc_budget --release`,
1000 ops/case, same box, before → after):

| case | allocs/msg | bytes/msg |
|---|---|---|
| decode/invite | 20 → **14** | 3601 → 3145 |
| decode/invite_sdp | 21 → **15** | 3937 → 3481 |
| decode/200_ok | 21 → **15** | 4068 → 3612 |
| proxy_hop/invite | 33 → **27** | 6355 → 5899 |
| proxy_hop/invite_sdp | 33 → **27** | 6995 → 6539 |

Removed per parse: the Via staging triple (`via_values` → `via_segments` →
`vias_parsed`), the Contact staging pair plus `contact_list.clone()`, and the
`Vec` per `split_top_level_commas` call on the Via/Contact/optional-list paths.
Workspace: 2028 tests passed, 0 failed.

**Deviation from the phase spec — `SipHeader` did NOT grow the enum.** Adding a
field breaks 78 struct-literal construction sites in 34 files across seven
consumer crates, i.e. it breaks the old API before teardown. `HeaderName` is
instead resolved once per header at the scan-time gates (`parse_headers`, which
now dispatches the numeric / quoted-string / Digest registries on the enum
instead of three candidate-list probes) and once per header in the single
dispatch walk (`HeaderIndex::build`). Storing the resolved name on the header
entry belongs to M2, where `Entry`/`Headers` own the storage and nothing has to
break to get it.

**Two `HeaderClass` types now coexist.** The new
`sip_message::header::HeaderClass` (Structural / EndToEnd) is the single
stack-ownership table; the root-exported `sip_message::HeaderClass`
(Regenerated / Frozen) is the template's narrower axis and now *delegates* to
it (`REGENERATED_HEADERS` deleted, and it matched Structural exactly). Merge the
two names at M12, per ADR-0025's "template.rs re-expression" note.

`generators::relay`'s `STRUCTURAL_HEADERS` is deleted the same way: the relay
set is `class() == Structural` **plus Content-Type** — the relay emits its own
body, so the media type describing it is the relay's to state. That delta is
now policy at the call site (`relay_owns`) instead of a duplicated table.

**Behaviour deltas, both strictly more correct, no test needed changing:**
compact forms (`v:`, `f:`, `m:`…) now resolve in the eager-field/optional-header
dispatch and in the relay's structural filter, which previously probed long
forms only. Parsed messages already carry expanded names, so nothing changes on
the wire; only a hydrate-built header list that uses a compact name behaves
differently (it is now classified correctly).

`CommonEager.contact` was dead (nothing read it) and is gone — it existed only
to hold `contact_list.first().cloned()`, which forced the `contact_list.clone()`
into `ContactSet`.

Error ordering is preserved exactly: Via and Contact each keep their original
two-pass validate-then-parse order by re-running the (allocation-free) segment
iterator rather than staging segments in a `Vec`.

**Suspicions raised, not fixed:**
- `tests/alloc_budget.rs` budgets are still the pre-zero-copy numbers (87/93/82
  /176/189 allocs) — 6× the measured cost, so today they gate nothing. M13 owns
  the re-baseline; until it lands this phase's win is unprotected.
- `hydrate_request`/`hydrate_response` still run the full eager extraction over
  headers a generator just wrote (now one pass instead of fifteen, but still).
  ADR-0025 §5 / M3 removes it from the build path.
- `flamegraph-util::capture_produces_an_svg` failed once inside the full
  workspace run and passed both standalone and on rerun. It samples CPU stacks,
  has no sip-message dependency, and starves under the capped parallel lane —
  infra flake, not a SUT finding.

### M2 — value hierarchy + draft engine

Landed as two commits (`sip_message::header` values, then `sip_message::draft`
+ message accessors); the workspace is green at both.

**Part 1 — `sip_message::header` value hierarchy.**

Shape as specified: `Wire` (the one build buffer), ordered small-vec `Params`,
unified `Uri`/`HostPort`, `NameAddr`, `NameAddrHeader<K>` with the kind axis
(`NameAddrKind` / `RichParams` / `TaggedKind`), full-surface `Via`,
`TokenListHeader<K>`, `TokenParamsHeader<K>`, `NumericHeader<K>`,
`Credentials<K>`, plus the scalar identity values `CallId` / `CSeq` / `RAck`.
`smallvec` joins the workspace dependencies for `Params`.

**Round-trip pin: 100 % of the frozen ABNF corpus, zero fixpoint failures.**
`tests/header_round_trip.rs` drives every generated line through
`parse(render(v)) == v`; acceptance was 1000/1000 inputs for From, CSeq, RAck,
Refer-To and SIP-URI, 1038 values for Contact/Route (comma folds), 1890 for Via
and 1989 for P-Asserted-Identity. Floors are pinned at 900 so a parser that
started rejecting could not pass vacuously.

**Three deliberate value-model choices, all fixpoint-forced:**
- `ParamValue` grows a `Quoted` variant beside `Flag`/`Token` (the old
  `types::ParamValue` has only `Flag`/`Value`). Without it, unescaping at parse
  and re-rendering bare loses a value whose text carries a separator, and the
  fixpoint fails on the corpus.
- `Params` keeps the wire spelling of a parameter name and compares
  case-insensitively; the old parser lowercase-*copied* mixed-case names.
- A `NameAddr` always renders `<uri>`, even when the wire had a bare addr-spec.
  Unconditional brackets are always legal and stop a URI parameter being read
  back as a header parameter.

**Ports are `u16` and out-of-range digits are a parse error**, not the old
saturating `u64`. A stack that rounds a port silently misroutes; the ABNF
corpus's `port=88161` lines are simply rejected (and skipped by the pin).

**Deviation — `PAssertedIdentity`/`PPreferredIdentity` keep parsed parameters.**
ADR-0025 §2 says the accessors must not exist on them, which is what the
`RichParams` bound does. Parsed parameters are still *stored* and re-rendered:
dropping them would make a thawed edit lose peer bytes, and the ADR asks for an
API-surface policy, not a lossy parse.

**Naming — `header::From`/`header::To` shadow the prelude's `From` trait inside
any module that imports them unqualified.** The ADR names are kept; the module
doc directs consumers to `header::From`. Port agents in M4–M11 that need
`impl From<…>` in the same file must qualify.

**Part 2 — `sip_message::draft` + the message read surface.**

`Draft<S: StartKind>` with `Entry`, `HeaderList<H>`, `thaw`/`keep`/`freeze`/
`render_unchecked`/`push_raw`, rendering into one pre-sized buffer with span
recording. `StartKind` contributes only the start line, the mandatory header set
and the frozen-message assembly, so request and response share one engine.
`SipRequest`/`SipResponse`/`SipMessage` grow the ADR read surface
(`from()`, `to()`, `call_id()`, `cseq()`, `via()`, `top_via()`, `header::<H>()`,
`list::<H>()`, `raw(HeaderName)`, `has()`, `route_set()`,
`record_route_set()`, `thaw()`) beside the untouched public fields.

**Round-trip pin: 31 of 31 parsed torture fixtures survive `thaw → freeze`**
(`tests/draft_round_trip.rs`, over rfc4475-valid/-invalid, strict-valid, ipv6,
param-gaps and cve). Per fixture the pin asserts header order/duplicates/values,
body, typed core, that the frozen bytes re-parse to the same header list (the
built message carries a real image), and that a second freeze is byte-identical.

**`Entry::Typed` is a boxed `dyn` value, not the closed `KnownHeader` enum** the
ADR sketches. A closed enum would have to name every value type and grow with
every new header; the trait object keeps `update::<H>`/`list::<H>` open to
extension headers, renders straight into the freeze buffer (no per-value
`String`), and costs one box per *edited* header — which is the ADR's stated
"O(edited) value allocations" budget either way.

**`freeze` re-runs the eager field extraction, but never re-lexes.** It renders
once, files the recorded spans into a `Vec<SipHeader>` over the new image, and
hands that to the existing one-pass `HeaderIndex` + `extract_*_fields`. Building
the typed core straight from the entries in hand is M12 work: until
`MessageCore` exists, the frozen message still has to populate the old eager
fields, and deriving them twice would be the real duplication. No re-parse of
text happens on the freeze path.

**Deviations and deliberate choices, all logged rather than silently taken:**
- `freeze` writes the CANONICAL spelling of every header name, so thawing a
  message that used a compact form or odd casing normalizes it. That is
  ADR-0025 §1 working as designed; the identity pin therefore compares
  canonicalized names, not raw bytes.
- `freeze` restates Content-Length from the body and adds one only when a body
  is present — the same contract `serializer::finish` has always had, so a
  bodiless message does not silently grow a header.
- Request `freeze` requires Max-Forwards (RFC 3261 §8.1.1), so a blank draft
  that forgets it fails loudly instead of emitting a non-compliant request.
- `Draft::list::<H>` replaces every line of the header with one line per value
  when the view is used, so a comma-folded line that is *edited* expands. The
  ADR's "untouched lines keep their byte layout" still holds for lines nothing
  touches; preserving the fold across an edit needs a per-item kept/edited flag
  on `HeaderList` and is not worth it until a consumer wants it.
- `list`/`update` are no-ops when a line does not read as `H`. Reading with an
  error is `Draft::values::<H>()`, which returns the `Result`.

**Suspicions raised, not fixed:**
- The mandatory read accessors (`from()`, `to()`, `via()`, …) convert the OLD
  typed fields into new values on every call, so each is a small allocation and
  the parameter list comes back in the old `BTreeMap`'s sorted order with
  lowercased names. Reads do not care, but nothing should round-trip a value
  obtained this way back onto the wire before M12 stores the real core —
  `msg.thaw()` (which seeds from raw spans) is the lossless path.
- `SipRequest::raw` is now both a field (`Bytes`) and a method
  (`raw(HeaderName)`). Legal, and the migration plan anticipated field/accessor
  coexistence, but it is a readability trap until M12 privatizes the field.
- `alloc_budget.rs` still has no build-path or freeze-path case, so this
  phase's "one buffer per freeze" claim is unmeasured. M13 owns it.

### M3 — generators → recipes

Every generator body is now a draft recipe; `hydrate_request`/`hydrate_response`
are gone from the build path (they survive for genuinely raw input), and a built
message carries its rendered image like a parsed one.

**Measurements — the first build-path budget** (`cargo test -p sip-message
--test alloc_budget --release`, 1000 ops/case, same box, hydrate path → draft
recipes). The three `build/*` cases are new; the options are built once outside
the measured region, so the number is construction cost, not the caller's own
bookkeeping.

| case | allocs/msg | bytes/msg |
|---|---|---|
| build/invite_sdp (blank draft, SDP offer) | 53 → **48** | 3892 → **6576** |
| build/bye (in-dialog, one Route) | 56 → **54** | 4045 → **5966** |
| build/response_200 (echo + stamp) | 46 → **37** | 3991 → **7316** |

Budgets are set at measured + ~20 %. Workspace: 2111 tests passed, 0 failed.

**Bytes are up because a built message now carries its image** — the rendered
datagram plus the shared text its header spans point into, which is exactly what
the ADR asks for. It is not yet a saving: every sender still calls `serialize()`
on the way out, so the render happens twice. The first consumer port that sends
`msg.raw` instead reclaims it.

**Allocs barely moved because `freeze` still runs the eager extraction.** Split
on the INVITE case: recipe ≈ 25 allocs, render 3, `assemble` ≈ 17 — the last is
the same one-pass extraction a parse runs, and it exists only to populate the
legacy typed fields (M2 note, M12 owns it). The old path's ~20 `format!`-String
→ `Arc<str>` double copies are gone; what replaced them is one box per typed
entry plus one owned copy per generated text value.

**Typed twins landed as one `values` field per opts struct**, not as per-field
twins: `opts.values.from = Some(header::From…)` supersedes `from_uri`/`from_tag`,
`values.hops` supersedes `vias`, and so on. Per-field twins would have to be
named around the stringly names they replace; a `values` group keeps the final
names free, so M12 deletes the stringly fields and flattens. Adding the field
broke 12 exhaustive struct literals in b2bua / scenario-harness / e2e-core —
patched with `..Default::default()`, the only edits outside sip-message.

**Stringly options ride `push_raw`, so no existing caller's wire bytes change.**
A Raw entry is memcpy'd at freeze, never parsed, which is what keeps CANCEL's
"Via copied verbatim" and the relay's transparent headers byte-exact. Three
deliberate deltas, all typed paths:
- the topmost echoed Via in `generate_response` is stamped as a value
  (`Via::stamped_from`) and re-rendered — but only when the stamp actually
  changes it, and never for a comma-folded line, which is echoed verbatim;
- `Via::stamped_from` no longer overwrites a `received` another hop recorded.
  M2 wrote it corrective; the string helper it replaces is idempotent, and
  idempotent is right — a hop records what it saw, it does not correct history;
- a To that reads cleanly is tagged typed (`to.with_tag`), so a bare
  `addr-spec` To gains angle brackets before the tag — which is what makes the
  tag a header parameter instead of a URI one. A To the reader rejects still
  leaves tagged, by text, so the peer is answered about the address it sent.

**`extra_headers` keep the caller's exact header-name spelling.** The
scenario-harness template lane replays a captured message byte for byte —
compact `c:`/`k:`, `p-AsSeRtEd-IdEnTiTy` — and canonicalizing those names broke
four of its tests. Caller lines are therefore `HeaderName::Other(verbatim)`, and
`HeaderName::same_header` (used by `Entry::is`) resolves casing and compact forms
so such a line still answers to the header it names: identity is the header, the
bytes are only its spelling. Consequence, strictly better: the stack-default
probes are now compact-aware for Content-Type too, so a caller who froze `c:`
no longer receives a second, canonical Content-Type line.

**Engine additions this phase needed:** `Draft::with_body` (carry a body without
touching the media type; `Draft::body` delegates), `SipRequest/SipResponse::
raw_text(HeaderName)` (the verbatim-echo seam — CANCEL, the non-2xx ACK and the
response echo copy no bytes, every echoed line is a span of the source image),
blank drafts pre-size their entry list, and `Uri::sip`/`sip_user` stop allocating
the scheme.

**Suspicions raised, not fixed:**
- `StackDialog` and its route set are still `String`s, so `build/bye` is the
  most allocation-heavy build case (a String clone per route, plus the remote
  target). The dialog shape is not a `Generate*Opts` field, so typing it was out
  of this phase; it is the next real build-path win.
- `route_for_in_dialog` still computes over text via `message_helpers::route`.
  It belongs on `RouteEntry`/`Uri` once the dialog is typed.
- Nothing reads the image a built message now carries, so the build path pays
  for it twice (see above). Worth a port-order note for M4–M11.
- `emit::name_addr_text` treats an empty tag as no tag on From as well as To.
  The old path emitted `;tag=` and let hydrate reject it (a panic); no caller
  passes an empty local tag, and neither outcome is defensible — the typed
  `values.from` is the way out.

### M1–M3 review findings — both fixed before any consumer ports

Two independent reviews of the foundation raised two majors; both were real and
both are fixed here, so M4–M11 port onto the corrected seam.

**1. `Draft::list`/`update`/`vias` returned the draft UNEDITED when a line did
not read.** The edits they exist for are the routing-critical ones — the hop's
own Via, the Max-Forwards decrement, popping the top Route — and a silent no-op
there forwards a request with no Via of its own (no path back for the response)
or with its loop bound intact. They now return
`Result<Self, SipParseError>`; the draft is never left half-edited and the
caller cannot ignore the outcome. No consumer existed yet, so the signature
change costs nothing.

Beside them, `Draft::prepend(value)` — insert a typed value on its own line
immediately above the lines the header already has, reading nothing. That is
the *correct* primitive for a router adding its own Via or Route (RFC 3261
§16.6 asks a proxy to add a hop, not to understand the ones below it), so a
hop is not forced to reject a message over a lower Via it has no business
parsing. `list` stays the way in when the existing values genuinely matter.

**2. The Request-URI was round-tripped through `Uri::parse` + `Uri::render` on
every thaw/freeze, and inside `generate_cancel` / `generate_ack_for_non_2xx`.**
Rendering from parsed parts is lossy — the scheme case-folds, a `?h` escaped
header with no `=` is dropped, bytes between the authority and the parameters
are discarded — while RFC 3261 §9.1 and §17.1.1.3 require the CANCEL's and the
non-2xx ACK's Request-URI to *equal* the INVITE's, and §16.6 forbids a proxy
rewriting one it is not retargeting.

`Uri` now keeps the text it was read from and renders those bytes until an
update touches it (every `with_*`/`without_*` drops the source; `normalized()`
drops it on request). Identity stays the parsed parts — `PartialEq` ignores the
source, so a built URI and a parsed one that mean the same thing compare equal.
The fix is at the value, not at the start line, so it covers the Request-URI,
the URI inside every name-addr, and any URI a converter hands back, uniformly:
an unedited URI is now as byte-faithful as an unedited header line.

Pins added: the thaw/freeze identity pin asserts `frozen.uri == request.uri`
and `frozen.version == request.version` (requests) and the version (responses)
— the two start-line fields it never covered; the URI fixpoint over the ABNF
corpus now asserts the verbatim property *and* drives `parse(render(v)) == v`
through `normalized()`, so preserving the source cannot make the corpus pin
vacuous; and `generators.rs` pins CANCEL and the non-2xx ACK echoing an
awkward INVITE Request-URI (`SIP:` scheme, `?X-Trace` with no value) octet for
octet.

**Cost** (`--test alloc_budget --release`, same box, M3 → now): allocs/msg
unchanged in every case; bytes/msg +32 on `build/invite_sdp` and +64 on
`build/response_200` — `Uri` grew one `Option<SipStr>`, so each boxed typed
entry carrying a URI is slightly larger. Budgets unchanged and still met.
Workspace: 2118 tests passed, 0 failed.

**Suspicion raised, not fixed:** `loadgen::smoke::
loadgen_actor_refer_recovers_loss_without_false_audit` failed once inside a full
workspace run ("SUT holds 1 live calls vs 0 failed — a RECOVERED call leaked")
and passed standalone and on a clean re-run of the same lane. No sip-message
surface is involved in that path; it matches the known loadgen-smoke contention
flake, but it is a leak assertion, so it is worth a second look if it recurs.

### M4 — sip-txn

The survey was right that sip-txn is the cleanest consumer: **zero `get_header`
call sites**. The port is therefore about the two things left — one genuine
string re-parse, and the double render M3 left on the build path.

**Reads now go through the typed surface.** Top-Via branch ×6
(`msg.via.first().branch` → `msg.top_via().branch()`), Via `cr`/`lg` custom
params (old `types::ParamValue::Value` match → `top_via().param(name)` +
`header::ParamValue::as_str`), From/To tags ×6 (`msg.from.tag` →
`msg.from().tag()`), CSeq ×2, Call-ID ×2. The one real deletion the survey
missed is `message_helpers::parse_uri_params(&req.uri)` in
`extract_ruri_call_ref` — a string re-parse of an already-parsed Request-URI
into a `BTreeMap<String, String>` — now `req.request_uri().param("callRef")`.
That trade is a strict win even transitionally: the old helper minted an owned
`String` pair per URI parameter, the typed read parses into a small-vec
`Params` and hands back a borrowed `&str`.

`decode_param` STAYS. It lives in `message_helpers::param_codec`, not in the
`{headers,name_addr,via}` surface M12 deletes: percent-decoding a param value a
peer encoded is a codec concern, orthogonal to the value model, and no typed
accessor should silently apply it (the wire spelling is what round-trips).

**Method comparisons are typed too** (`req.method == "ACK"` → `Method::Ack`,
`resp.cseq.method.as_str().eq_ignore_ascii_case("CANCEL")` →
`resp.cseq().method() == &Method::Cancel`). Equivalent by construction —
`Method::from_wire` already folds known methods case-insensitively, so
`Method::Other` can never hold a known spelling.

**The build path stops rendering everything twice.** M3's open item ("nothing
reads the image a built message now carries… the first consumer port that sends
`msg.raw` instead reclaims it") is reclaimed here for the five messages this
layer builds itself: the auto-100 Trying, the auto-ACK for a non-2xx final, and
the CANCEL trio (200, 487, and the unmatched-CANCEL 481). A recipe freezes into
its own image and that image IS the wire form, so `send_buffer(&msg.raw)`
replaces `serialize(&SipMessage::…(msg))`. Byte-identity holds by construction:
`freeze` and `serializer::finish` write the same `Name: value\r\n` block over
the same header list, and `freeze`'s `with_content_length` leaves nothing for
`finish`'s Content-Length correction to change.

`do_send_response` deliberately does NOT do this. The response comes from the
TU, which may have edited the header list of a message it parsed, so its image
is not authoritative — that path renders. It does drop its whole-`SipResponse`
clone, though: every field the layer needs (status, top-Via branch, To-tag) is
read first, then the response goes to the serializer by value.

**Cached datagrams are `Bytes`, not `Vec<u8>`** (`Transaction::last_response`,
`retransmit_buf`). That is what lets a built message's image be cached without a
copy, and it makes every replay a refcount bump instead of a memcpy of the whole
datagram: Timer A/E client retransmits, Timer G server final retransmits, and
the cached-response replay on a duplicate request. `retransmit_buf_bytes` still
censuses the retained bytes correctly — the layer holds the sole reference.

**Per-message copies removed** (structural count, no sip-txn alloc budget
exists — M13 owns adding one): one full datagram render per inbound INVITE (the
100), per received non-2xx INVITE final (the ACK), and two per CANCEL (three on
the unmatched path); one whole-`SipResponse` clone per outbound TU response; one
datagram memcpy per retransmit / duplicate replay. Workspace: 2118 tests passed,
0 failed.

**Suspicions raised, not fixed:**
- The transitional read accessors are not free here the way they will be after
  M12. `msg.from()`/`to()` run `Uri::parse_or_opaque` on the name-addr URI just
  to reach a tag, and `top_via()` copies the param list — so this layer, which
  touches every datagram, now pays a small per-message conversion where it used
  to read a field. It is bounded (1–2 conversions per message, no text copied —
  `SipStr` clones are refcount bumps) and M12's stored `MessageCore` deletes it
  outright, but a loadgen throughput run before M12 lands would be reading this
  cost, not a regression in the protocol path.
- `Owner::do_send_request` still clones the whole `SipRequest` to stash it as an
  INVITE client txn's `original_request` (for the auto-ACK), and again into the
  returned `ClientTransactionHandle`. Once messages are image-backed and
  immutable (M12) those are refcount bumps, not deep copies; not worth touching
  before then.
- `flamegraph-util::capture_produces_an_svg` failed once inside the full
  workspace run and passed standalone and on a clean re-run — the same infra
  flake M1 logged (CPU-stack sampling starving under the capped parallel lane).

### M5 — sip-proxy

`headers.rs` is down from 237 lines of byte surgery to 122 lines of
Record-Route **policy**: which params ride the entries this proxy records
(`record_route`, `record_route_flagged`), the cookie read back off one
(`cookie_params`), and the transport destination a Route names
(`route_target`). Deleted with the mechanics: `prepend_header`,
`upsert_header`, `first_header_value`, `remove_first_header`,
`remove_first_header_entry`, the `split_top_level_commas` wrapper,
`populate_received_rport_on_top_via`, `via_sent_by`/`via_sent_by_addr`,
`route_value_to_addr` and `uri_port_u16`. `sip-proxy` has **zero**
`get_header` / `message_helpers::{headers,name_addr,via,uri}` call sites left,
tests included; the one surviving helper is
`message_helpers::is_emergency_request` (the `emergency` module is not on the
M12 deletion list).

**Both hops are thaw → touch → freeze.** The request path thaws once, pops its
own Route entries, stamps received/rport on the top Via, states Max-Forwards,
pushes the double Record-Route and its own Via, and forwards
`freeze_bytes()`; the response path thaws, pops the top Via entry and forwards
the same way. `serialize_request_parts` / `serialize_response_parts` and the
`headers.clone()` that fed them are gone from this crate.

**Two draft primitives were missing and landed first** (own commit,
`refactor(sip-message): ADR-0025 draft top-line edits + checked byte exit`):

- `update_top` / `pop_top` — rewrite or drop the FIRST value of a header,
  reading only the line that carries it. `update`/`list` read every line, which
  is right when the values matter but wrong for a transit hop: an unreadable
  Via five hops down must not fail a relay that only ever touches the top one.
  `pop_top` is also the §7.3.1 entry pop the proxy used to hand-roll (a
  comma-folded line keeps the values below the one removed).
- `freeze_bytes` — `freeze`'s mandatory-header check and its single render,
  without assembling the typed message. The relay reads no field of what it
  forwards, so it must not pay the eager field extraction; ADR-0025 guardrail 2
  says a hop never re-parses, and `freeze` does. `render_unchecked` stays the
  only *unchecked* exit, so the "invalidity leaves only as bytes" property is
  untouched.

**Measurements — one forwarded in-dialog INVITE hop** (throwaway alloc-counter
harness over the same fixture: 1 Route, Via stamp, Max-Forwards, double
Record-Route, own Via; `--release`, 1000 ops):

| path | allocs/hop | bytes/hop |
|---|---|---|
| old (clone + string surgery + `serialize_request_parts`) | 29 | 2254 |
| new (thaw + draft ops + `freeze_bytes`), first cut | 33 | 10345 |
| new, after the four fixes below | **27** | 6737 |

The first cut was *worse* on both axes; four fixes in sip-message closed the
alloc gap and two thirds of the byte gap, and each is a plain defect:
`Draft::thaw` grew its entry list from zero (now sized to the header list,
3 allocs → 1); `freeze_bytes` recorded per-header spans nobody reads (`render`
now has a no-span path); `HeaderValue::parse_line` let a `Vec` of 432-byte
values grow from 4 for the one-value line the wire almost always carries; and
`Via::with_branch`/`with_received`/`with_rport` and `To::with_tag` allocated a
`SipStr` for the *parameter name* on every call (now `from_static`).

**Bytes/hop is still 3× the old path, and it is the value model, not the
draft.** `size_of` on this branch: `RouteEntry` 432 B, `Uri` 272 B, `Via`
232 B — so one boxed typed entry costs more than the whole rewritten datagram,
and reading a one-entry Route set costs 2160 B. Allocation *count* is what the
ADR budgets, and that is now below the old path; the byte figure is a
`Params`-inline-smallvec + `Uri`-source + escaped-header-`Vec` width problem
that M12's `MessageCore`/value-storage pass owns. Raised, not fixed here.

**Behaviour deltas, all deliberate:**
- A malformed Max-Forwards now defaults to 70 instead of being read as a signed
  integer, so `Max-Forwards: -1` is forwarded with 69 rather than answered 483.
  A negative hop count is not a hop count; §8.1.1.6's default is the honest
  reading of a value no reader accepts.
- Loose-route detection reads the first SURVIVING Route entry
  (`uri().is_loose_route()`) instead of scanning the whole first Route *line*
  for `;lr`, so a fold whose second entry is loose no longer makes the first
  one look loose.
- The 420's `Unsupported`, the 503's `Retry-After` and every proxy `Reason` are
  built as typed values (`TokenListHeader`, `TokenParamsHeader`) and rendered
  onto the generator's still-stringly `extra_headers` seam through one
  `extra_header` adapter in `reply.rs`. Byte-identical — the wire assertions in
  `self_gate_admission.rs` were left comparing exact text and still pass.
- `reply()` sends the generated response's own image (`resp.raw`) instead of
  re-serializing it — the M4 reclaim, applied to the proxy's self-generated
  finals.

**A Route the strict reader rejects stays tolerated.** `req.list::<RouteEntry>()`
failing means no pop, no loose-route next hop, and the lines ride through
byte-verbatim — which is what `oversized_route_port_does_not_alias_the_advertised_address`
pins (a `sip:vip:70596` Route must neither alias the proxy nor drive routing).
`Uri::parse` rejecting an out-of-range port is what replaced the old
`uri_port_u16` guard, so the "70596 must not wrap to 5060" contract is now a
property of the value type rather than a call-site check.

**Suspicions raised, not fixed:**
- The Route set is read twice per in-dialog request: once off the message to
  classify, once inside each `pop_top` to edit. `Draft::list` consumes `self`
  and cannot hand the draft back on a parse error, so a single read-and-edit
  call cannot also implement the tolerance above. A `try_list` that returns the
  draft alongside the error would collapse the two.
- `route_request` is 245 lines (clippy `too_many_lines` warns at 200). It was
  250 before this port, so the port did not cause it — but the ladder is now
  the only thing left in the file and wants splitting.
- `record_response` no longer uppercases the CSeq method for its metric label.
  `Method::as_str()` is canonical for every known method, so only an unknown
  method's label changes case; it lands in the bounded `other` slot either way.
- `cookie_params` returns `lr` (and `outbound`) alongside the stickiness
  fields, exactly as `parse_uri_params` did. The strategy names the fields it
  decodes, so nothing reads them — but a cookie signature computed over "all
  params" would.

### M6 — b2bua-sdk + b2bua

One step, as the plan called it: `MessageTransform` is the seam and both crates
had to move together. `b2bua` + `b2bua-sdk` have **zero**
`message_helpers::{headers,name_addr,via,uri}` call sites left, tests included;
the survivors are `is_emergency_request` (the `emergency` module, not on the
M12 list), the `param_codec` re-export in `stack_identity.rs` (same reason as
M4's `decode_param`) and the four buffer scanners the Tier-1 brake uses
(`sniff`-side, also off the list).

**`MessageTransform` is typed at both axes.** `remove_headers: Vec<&'static str>`
→ `Vec<HeaderName>` and `add_headers: Vec<(&'static str, String)>` →
`Vec<Entry>` — a draft entry names its own header, so a stamp cannot disagree
with the value it carries, and the four `eq_ignore_ascii_case(&h.name)` scans it
fed become `HeaderName::matches`. `RuleAction::SendReinvite`'s `add_headers`
follows. `Entry` and `HeaderName` join the `b2bua_sdk::rules` façade so an
out-of-tree service crate can build a transform without reaching past the SDK.

**Every deletion target the plan listed is gone**, plus what the sweep found:

| deleted | replaced by |
|---|---|
| `relay.rs` `top_via_host_port`, `respond.rs` `top_via_dest`, `relay_response.rs` `via_sent_by` | `msg.top_via().sent_by().pair()` / `Via::parse` on the pending-request snapshot |
| `relay.rs` `strip_uri` + `dest_of` (7 call sites) | `relay::target_dest` — one `NameAddr::parse` → `uri().host_port()` |
| `relay_request.rs` `rewrite_rack` | `req.header::<RAck>()` → `RAck::new(rseq, target_cseq, method)` on the typed opts seam |
| `dialog_track.rs` `unwrap_angle` | `msg.header::<Contact>()` → `uri()` |
| `refer_transfer.rs` `to_bare_uri` + the `contains("replaces")` probe | `ReferTo::parse(..).uri().without_escaped_headers()` / `uri().escaped_header("Replaces")` |
| `reliable_rseq` ×2 (`relay_first_18x.rs`, `promote_pem.rs`) | `header::<Require>()?.contains("100rel")` + `header::<RSeq>()` |
| route-set reconstruction ×3 (get→comma-split→trim→reverse) | `list::<RecordRouteEntry>()` → `uac_route_set` / `uas_route_set` |
| the six header-name arrays | `HeaderName::class()` where the set IS the structural set; `&[HeaderName]` where it is genuinely policy |
| four `serialize(&SipMessage::…(msg.clone()))` caches | `msg.raw.to_vec()` — a freeze-built message's image IS its wire form |
| `resolve.rs` `via_cr_lg` (hand-rolled `;`-split) + `parse_uri_params` | `via.param("cr"/"lg")`, `req.request_uri().param("callRef")` |
| `apply_route.rs` `invite.headers.retain/push` + `req.body =` | one `thaw` → body/`Supported` edits → `freeze` |
| `relay.rs` `to_msg_headers` (only caller was `rebuild_a_leg_invite`) | — |

**`rebuild_a_leg_invite` builds a draft instead of `hydrate_request`.** Every
snapshot header rides as a `push_raw` line, so the rebuild carries the caller's
bytes exactly as they arrived and the result carries a real image. It no longer
runs the eager extraction twice (hydrate parsed the header list it was handed);
it renders once.

**Behaviour deltas, all deliberate:**
- The two "structural headers a service may not set on a response" arrays
  (`respond.rs`, `initial_invite.rs` — the same nine names) are now
  `class() == Structural`, which additionally blocks `Route`. A Route header on
  a response is not a thing RFC 3261 defines; a service naming one was
  previously let through.
- `STANDARD_HEADERS` (the decision-request field list) and the REFER
  `/call/refer` skip list stay policy lists — as `&[HeaderName]`, not strings.
  Both genuinely differ from `class()` (neither excludes Route/Record-Route),
  and folding them in would silently drop headers the decision backend sees.
- `BodyUpdate::Drop` on the b-leg INVITE now drops `Content-Type` with the body
  (`Draft::without_body`). A bodiless message describing a media type is a
  §7.4.1 contradiction; the old path cleared only `body`.
- `rebuild_a_leg_invite` states `Max-Forwards: 70` when the snapshot has none.
  `freeze` requires it (RFC 3261 §8.1.1.6) and `hydrate_request` did not, so
  without this an INVITE from a peer that omitted the hop count would panic the
  worker. Nothing reads the hop count off the rebuild.
- The relay-header forbidden set is `class() == Structural || Content-Type`,
  identical to the eleven names it replaces.

**The bootstrap Route preload is a draft edit** (`thaw().prepend(route)
.freeze()`), so it lands below the Via rather than at wire position 0, and the
INVITE's image stays the message that goes out. `prepend` is the right
primitive here per the M1–M3 review: a hop adds its own Route without reading
routes below it.

Workspace: 2121 tests passed, 0 failed. Clippy clean on both crates (the only
warnings in the lane are pre-existing `sip-message` parser ones).

**Suspicions raised, not fixed:**
- `list::<RecordRouteEntry>()` failing yields an EMPTY route set
  (`unwrap_or_default`), where the old text path stored the unreadable line
  verbatim. An empty dialog route set sends in-dialog requests pod-direct — the
  long-call-loss class. Unreachable behind our own front proxy (it records what
  the reader accepts), but a `try_list`-style tolerant read — the same gap M5
  logged for `Draft::list` — would remove the cliff.
- `InviteTxnHandle::original_invite` is still snapshotted inside `build_b_leg`,
  i.e. BEFORE `apply_route` substitutes the body and rewrites `Supported`. The
  cached INVITE therefore differs from the one sent; it feeds CANCEL generation
  and `acked_invite_cseq`, which read only the CSeq and the Request-URI, so
  nothing is wrong today. Pre-existing, surfaced by this port.
- `interpret.rs` still calls `serialize(&SipMessage::…(msg.clone()))` on the two
  raw-send paths, and `send_request`/`send_response` render again inside
  sip-txn. Every b2bua-produced message now carries a faithful image, so those
  are the next `msg.raw` reclaims — deliberately left out of this step because
  the producers are spread across the rule set and a stale image there would
  send wrong bytes silently.
- `apply_b_leg_egress` swallows a `freeze` error by forwarding the unmodified
  request to the un-preloaded destination. A thawed draft cannot be incomplete,
  so the arm is unreachable; the alternative was a panic on the relay path.
- `PendingRequest` (Via/From/To/Call-ID) and `StackDialog` (route set, remote
  target) are still `String`s in the `call` crate, so the relay re-parses them
  on every response. That is the ADR's out-of-scope `call`-crate decision, and
  it is now the only string round-trip left on the b2bua's hot path.

### M7 — scenario-harness

`scenario-harness` has **zero** `message_helpers::*` / `get_header` /
`set_header` / `remove_header` / `serialize_*_parts` call sites left, `src` and
`tests` alike. One reader was missing and landed in its own sip-message commit:
`sniff::request_uri` (the second request-line token) — the leg-picker's demux
tier reads an unparsed datagram, which is `sniff`'s concern, not the parser's.

**Every deletion target the plan listed is gone**, plus what the sweep found:

| deleted | replaced by |
|---|---|
| `client_invite.rs` mutate → `serialize_request_parts` → re-parse (§22.2 auth resend) | `thaw` → `set(Via)` / `set(CSeq)` / `remove`+`push_raw(credential)` → `freeze` |
| `agent/proxy.rs` `prepend_header`, `strip_top_route_if_self`, `strip_top_via_if_self` | `thaw` → `pop_top::<RouteEntry>` / `push_front`+`prepend` / `pop_top::<Via>` → `freeze` |
| `addressing.rs` `top_via_branch` over a header vec, `via_addr(&str)`, `strip_route_uri_to_request_uri` + `extract_host_port` + `via_sent_by` + `parse_via_params` | `req.top_via().branch()`, `via.sent_by()`, `NameAddr::parse(..).uri().authority()` |
| `ua.rs` `via_header()` (the one hand-assembled Via string) | `ViaSpec::value()` — the typed hop the generators already build |
| route-set reconstruction ×3 (`client_invite` ×2 UAC-reversed, `server_txn` ×1 UAS-order) | `record_route_set()` / `.reversed()` |
| `extract_contact_uri` ×2 (dialog remote target) | `msg.header::<Contact>()` → `uri()` |
| `legpick.rs` `first_line` / `ruri` / `header_value` / `uri_user` byte scanners | `sniff::{first_line,request_uri,req_method,header_value}` + `Uri`/`NameAddr` for the user-part |
| `callee_group.rs` `to.contains(";tag=")` | `To::parse(..).tag()` |
| `realcall/env.rs` `refer_to` `split_once('@')` URI splice | `Uri::with_user` + `ReferTo::from_uri` |
| `realcall/auth.rs` challenge-header string lookup | `resp.raw(HeaderName::WwwAuthenticate / ProxyAuthenticate)` |
| `name_matches("Allow"/"Supported", …)` ×3, `eq_ignore_ascii_case` on header names ×3 | `HeaderName::{Allow,Supported,RecordRoute}.matches` / `HeaderName::from(name).matches` |
| RSeq / RAck / Expires / Content-Type string reads (×7 incl. the actor lane and the report renderer) | `header::<RSeq>()`, `RAck::new(..).to_wire()`, `header::<Expires>()`, `header::<MediaType>()` |
| `resp.cseq.method.eq_ignore_ascii_case("INVITE")`, `leg.method() == "INVITE"` | `Method::Invite` comparisons |

**The harness proxy hop stops rendering twice.** `Agent::try_send_wire` sends an
already-rendered datagram, and `try_send` is now its `serialize` veneer — so the
two messages this crate freezes itself (the §16 forwarded request/response and
the §22.2 retried INVITE) go out as their own image. The recorder's input is the
same bytes either way, so the trace and the RFC audit see no difference. Without
it the port would have made the proxy hop *slower* than the header-vec surgery
it replaces (thaw + freeze-render + serialize-render).

**Behaviour deltas, all deliberate:**
- **A comma-folded Record-Route line now teaches several routes, not one.** The
  old path stored raw header *values*, so a §7.3.1 fold entered the dialog route
  set as a single opaque string; `record_route_set()` splits it. This is the
  long-call-loss class, on the harness side — the fold the harness itself emits
  for `RecordRouteFold::Combined` UAs was previously mis-read by the harness's
  own UAC. Route-set entries are now rendered name-addrs (`<sip:…;lr>`), so an
  unbracketed captured entry gains brackets.
- The proxy's own Record-Route opens the header block when the request carries
  none (RFC 3261 §7.3: a proxy writes what it processes near the top), instead
  of riding at wire position 0 above the Via. Its Via is still the topmost Via.
- Loose-route self-detection reads the first Route ENTRY's URI authority rather
  than scanning the first Route LINE, so a fold whose second entry names the
  proxy no longer makes the first one look like ours.
- A Route or Record-Route the strict reader rejects yields no pop and no route
  (the same tolerant read M5/M6 took), where the old text path routed on it.
- `suppress_default_ct` is now compact-aware. M3 made the generator's own
  Content-Type probe compact-aware, so the "frozen `c:` plus a stamped
  `Content-Type`" case it guarded can no longer arise; the flag's live case is
  the one it names — a captured message with a body and no media type at all.
- `refer_to` keeps the policy-resolved host of a USERLESS target URI instead of
  falling back to the resolved socket address; the fallback now fires only for a
  URI no reader accepts.

**Suspicions raised, not fixed:**
- **`thaw`/`freeze` canonicalizes header-name spelling, which the template lane
  exists to preserve.** Three of the new freeze sites sit on template-capable
  paths (`suppress_default_ct` on the INVITE / in-dialog / response builders).
  It is invisible today because the flag only fires when the capture states no
  media type, and no fixture combines that with a compact spelling elsewhere —
  but a capture with a body, no Content-Type and a `k:`/`p-AsSeRtEd-IdEnTiTy`
  line would come out canonicalized. The same applies to any template-emitted
  message forwarded through the harness `Proxy`. The clean fix is a
  spelling-preserving seed (`thaw` keeping `HeaderName::Other(verbatim)` when
  the wire spelling is not canonical), which is M12 territory because it changes
  what every ported crate's relay emits.
- **`rr_fold::fold_record_routes` is the one header-vec surgery left**, and it
  stays because no draft primitive can place a comma-folded RAW line at a chosen
  position: `set`/`update_top`/`list` all take typed values and render one line
  per value, and `push_raw` only appends. Folding it through the draft would
  move the header to the end of the block AND canonicalize the response (see
  above). Its identity test is typed now (`HeaderName::RecordRoute.matches`).
  A `Draft::fold::<H>()` (or a `HeaderList` fold flag, which M2 already logged
  as "not worth it until a consumer wants it") is the primitive that would
  retire it — this is the consumer that wants it.
- `apply_name_forms` / `apply_remote_target_emits` still rewrite a cloned header
  vec after generation (the template lane's compact-name and Contact re-spelling
  passes). They live in sip-message and are not on the M12 deletion list, but
  they are the reason the wire copy and the retained canonical message diverge —
  the same divergence a spelling-preserving `thaw` would make unnecessary.
- `GenerateResponseOpts::extra_headers` and friends are still `Vec<SipHeader>`.
  The plan's "template `frozen_headers` → `push_raw` entries" is already true
  *inside* the generators (M3 lowers every extra header to a `push_raw` entry
  under its verbatim name); flipping the opts field to `Vec<Entry>` would break
  b2bua / e2e-core construction sites in this step, so it belongs to M12's
  `Generate*Opts` flattening, where every consumer moves at once.
- `StackDialog` is still stringly (route set, remote target, URIs), so the
  harness renders typed values back to text at every dialog boundary —
  `record_route_set()` → `to_wire()` → `NameAddr::parse` again in `next_hop`.
  That is the same `call`-crate-adjacent debt M6 logged for `PendingRequest`;
  typing the dialog is the next real win on this path.

Workspace: 2123 tests passed, 0 failed. Clippy on `scenario-harness`: 18
warnings, down from 23, all pre-existing categories (doc indentation, type
complexity, variant size).

### M8 — e2e-core + e2e-model + announcement

`e2e-core` and `e2e-model` have **zero** `message_helpers::*` / `get_header` /
`set_header` / `serialize_*_parts` call sites left, `src` and `tests` alike. No
reader was missing — the port needed nothing added to `sip-message`.

**`announcement` needed no port.** It builds `MessageTransform`s through the SDK
façade alone (`new_ruri`, still `Option<String>`); its only sip-message contact
is a `Cargo.toml` dependency nothing imports. See the suspicions below.

**Every deletion target the plan listed is gone**, plus what the sweep found:

| deleted | replaced by |
|---|---|
| `registrar.rs` `prepend_header`, `strip_top_route_if_self`, `strip_top_via_if_self`, `decrement_max_forwards` (header-vec surgery) | `thaw` → `set(MaxForwards)` / `pop_top::<RouteEntry>` / `push_front`+`prepend(RecordRouteEntry)` / `prepend(Via)` / `pop_top::<Via>` → `freeze_bytes` |
| `registrar.rs` `top_via_addr` + `via_sent_by_addr` (the `split_whitespace().nth(1)` peel) | `via.sent_by().pair()` |
| `registrar.rs` `uri_to_addr` (`parse_sip_uri` + host/port reassembly) ×4 | `uri.host_port()` |
| `registrar.rs` `extract_contact_uri` ×3, `parse_sip_uri` ×4 | `req.to().uri().user()`, `req.request_uri().user()`, `Contact::parse(..).uri()` |
| `registrar.rs` `effective_expires` header-level `;expires=` string scan (the `find('>')` + `split(';')` block) | `header::<Expires>()`, `contact.uri().param("expires")`, `contact.param("expires")` |
| `registrar.rs` `get_header("route")` next-hop read | `req.list::<RouteEntry>()` |
| `checks.rs` `msg.get_header(name)` | `msg.raw(HeaderName::from(name))` — and now compact-form aware |
| `checks.rs` `parse_sip_uri(&addr.uri)` re-parse of an already-parsed address | `addr.uri()` (structured `Uri`) |
| `checks.rs` `types::{ContactSet, NameAddr, ParamValue}` field reads (`r.from`, `r.contacts`, `optional().p_asserted_identity`) | `msg.from()`/`to()`, `msg.list::<PAssertedIdentity/PPreferredIdentity/Diversion/Contact>()` |
| `transfer_refer_media.rs` `format!("<{}>", target.uri)` name-addr assembly | `ReferTo::from_uri(..).to_wire()` |
| `registrar.rs` `serialize(&SipMessage::…)` on every forwarded message and every self-generated response | `freeze_bytes()` / `resp.raw` |

**The register front proxy stops rendering twice.** Both hops are
thaw → touch → `freeze_bytes` (no eager field extraction on a message this
proxy forwards without reading), and its own 200/4xx/483 go out as the
generator's image — the M4/M5 reclaim applied to this crate's proxy.

**Behaviour deltas, all deliberate:**
- **The granted Contact echo is a typed value**, so the lifetime rides as a
  HEADER parameter: an unbracketed `sip:bob@h:p` Contact is echoed
  `<sip:bob@h:p>;expires=N` instead of `sip:bob@h:p;expires=N`, where the old
  `format!` made `expires` a URI parameter of the binding (RFC 3261 §10.2.4
  puts it on the header).
- **A Contact no reader accepts can de-register but never register.** `*` (and
  any garbage) now yields 400 Bad Request when the effective Expires is
  non-zero — RFC 3261 §10.3 step 6 — where the old path stored the star as a
  binding whose lookup later answered 500.
- **A non-numeric `Max-Forwards` never reaches the forwarding decision**: the
  parser rejects the whole message, so the proxy's `70 - 1` default now covers
  only the request that states no count. The old local repair was reachable
  only through a hand-built header vec, which is what its test built.
- The proxy's own **Record-Route opens the header block** when the request
  carries none (§7.3), instead of riding at wire position 0 above the Via; its
  Via is still the topmost Via. Same delta M7 took in the harness proxy.
- **A Route the strict reader rejects yields no self-pop and no route hop**
  (the request falls back to the Request-URI), the same tolerant read M5/M6/M7
  took, where the old text path routed on it.
- `header(Name)` in the check grammar resolves compact forms and casing, so
  `header(Contact)` now also answers for a `m:` line.
- A `.port` subfield on a URI the strict reader rejects reads as `5060` rather
  than absent: `msg.from()`/`to()` keep an unreadable URI whole
  (`Uri::parse_or_opaque`) instead of dropping the whole address.

Workspace: 2124 tests passed, 0 failed. Clippy on the three crates: only the
pre-existing doc-indentation warnings.

**Suspicions raised, not fixed:**
- **`announcement` declares a `sip-message` dependency it never uses.** The
  crate's whole point is "depends ONLY on b2bua-sdk (+ call/sip-message)" as a
  no-path-to-internals proof, so the dep may be deliberate ballast — but an
  unused dependency is also what an out-of-tree service crate would NOT carry.
  M12 is the moment to decide, since it is the last chance to notice.
- **`Registrar` stores a `header::Uri` per binding, which is a 272-byte value
  for what routing reads as a host and a port.** That is the same
  `size_of`-of-the-value-model debt M5 logged; a binding store keyed on
  `SocketAddr` would be smaller but would stop the store being verbatim, which
  its sipjs parity contract states.
- `RegisterProxy::on_response` reads the whole Via list to find the relay
  target and then pops the top entry through the draft, so the header is read
  twice — the `try_list` gap M5 and M6 both logged, seen from the response
  side.
- `CalleeTarget::uri` is still a `String` the shapes wrap in angle brackets and
  the harness re-parses. Typing it is `e2e-model`'s own model decision, not a
  header-model one, and it is the last string round-trip on this crate's dial
  path.

### M9 — sip-net (rfc_audit)

`sip-net` has **zero** `message_helpers::*` / `get_header` / `get_headers` call
sites left, `src` and `tests` alike. No reader was missing — the port needed
nothing added to `sip-message`. The crate's only remaining raw-byte scans are
the SDP `o=`/`m=`/`c=` line reads (a body concern, not a header one) and the
`sniff` calls below.

**Every deletion target the plan listed is gone**, plus what the sweep found:

| deleted | replaced by |
|---|---|
| `dialog_model.rs` `route_is_loose` (the `;lr` substring walk), `split_header_list` / `split_header_values` (the angle/quote-aware comma splitter), `extract_route_uri` | `msg.list::<RouteEntry>()` → `uri().is_loose_route()` / `uri()` — one `route_entries` read stating the tolerance once |
| `dialog_model.rs` `msg_headers` (the raw header-list escape hatch every rule reached through) | typed reads; the few genuinely opaque ones take `msg.raw(HeaderName::X)` |
| `top_via_branch` ×4 (`dialog_model`, `cseq` ×2, `starter_peer::sent_top_branch`) | `msg.top_via().branch()` |
| `cross_generic.rs` `read_top_via_rport` (hand-rolled `;`-split) | `via.param("rport")` + `via.rport() -> Rport` |
| `cross_generic.rs` `route_host_port` over `message_helpers::extract_host_port` | `uri.host_port()` |
| `cross_generic.rs` `parse_sip_uri` wire-destination peel | `Uri::parse` on the Request-URI / the Route entry's `uri()` |
| `txn_correlation.rs` `split_option_tags` (+ `rfc3261_cross::collect_option_tags`, `header_values_owned`, `rfc3261_peer`/`rfc3262_*` `has_option_tag`) | `header::<Require/Supported/Unsupported/ProxyRequire>()` — `contains` is the set membership, `option_tags::<K>` the lower-cased list for the recognised-tag tables |
| `rfc3262_cross.rs` `parse_rack`/`ParsedRack` and the RSeq `parse::<u64>` rows | `header::<RAck>()` / `list::<RSeq>()` |
| `starter_peer.rs` `ToTagPresenceRule::scan` (hand-rolled line split + `extract_tag`) | `sniff::resp_status` + `sniff::to_tag` — the sanctioned raw scanners for the datagram this rule must see *before* it parses |
| `starter_peer.rs` Max-Forwards `parse::<i64>` | `header::<MaxForwards>()`, with `raw(HeaderName::MaxForwards)` kept only to quote the offending text |
| `rfc3264_peer.rs` `is_sdp_content_type` (length-15 split + `\b` boundary walk) | `list::<MediaType>()` → `ct.is("application/sdp")` |
| `cseq.rs` `get_header("call-id")` / `get_header("from") + extract_tag` | the parsed `call_id` field and `req.from().tag()` |
| `rfc3261_cross.rs` `cseq_of` (raw CSeq row, trimmed) | `msg.cseq().to_wire()` — both sides of the correlation key are now normalised |

**`DialogModel::route_set` is `Vec<RouteEntry>`**, not `Vec<String>`: the model
carries the route set *as in-dialog requests must reproduce it*, so the
Record-Route stack is retargeted (`RecordRouteEntry::retarget`) and reversed for
the UAC at the one place §12.1.1/§12.1.2 says it is.

**The tolerance the M9 plan warned about is stated once, in
`dialog_model::value_of` / `route_entries`:** a value no reader accepts leaves
the rule silent instead of throwing or guessing. A rule states one invariant, and
a message whose grammar is already wrong belongs to the grammar rules — the
auditor still sees every deliberately-broken peer message, because the audit
parses leniently (`lenient_parser`, unchanged) and the *rules* decide, which is
exactly the split ADR-0007 set up. The 260 audit unit tests (many of which are
hand-built malformed fixtures) pass unchanged.

**Behaviour deltas, all deliberate:**
- **A `;lr` inside the URI no longer needs a substring scan, so `;lrx` and a
  `;lr` in a display name can never read as loose routing.** The old
  `route_is_loose` walked the whole header value; the typed read asks the URI.
- **A comma-folded Route/Record-Route line is split by the value grammar**, not
  by an angle/quote-aware string splitter. Same result on every fixture, but a
  fold whose *second* entry is loose no longer makes the first look loose (the
  same delta M5/M7/M8 took on the routing side, now on the auditing side).
- **`rfc3261.midDialogWireDestination` skips a request whose only Route line no
  reader accepts** instead of resolving it as a destination. The old path fed
  the raw value to `parse_sip_uri`, which saturated an out-of-range port — a
  `sip:vip:70596` Route used to produce a wire-destination finding on a message
  the routing layer (post-M5) does not route on at all.
- **`rfc3261.via` stores the branch its sender minted, taken from `top_via()`**,
  instead of re-parsing the stored top Via *line* later. A comma-folded Via line
  now yields the first hop's branch rather than a parse of the whole line.
- **`rfc3262.reliable1xxHeaders` asks presence and readability separately**, so
  an `RSeq` above `u32` is reported as out of range (it is) rather than being
  read as a `u64` in range.
- Presence probes (`Contact`, `Content-Type`, `Route`, `Allow`, `Supported`,
  `Accept*`, `Retry-After`, `Unsupported`) are `HeaderName`-keyed and therefore
  compact-form aware: a `m:`/`c:`/`k:` line now answers for the header it names.

Workspace: 2122 tests passed, 0 failed. Clippy on `sip-net`: identical warnings
before and after (3 lib + 1 test, all pre-existing SDP/loop-index categories).

**Suspicions raised, not fixed:**
- **`sip_message::parser::custom::structured_headers::parse_rack` has no
  consumer left outside `sip-message`'s own optional-header extraction and its
  ABNF fuzz corpus.** It is not on the M12 deletion list; it should be, or the
  optional-header path should read `header::RAck` like everyone else now does.
- The audit still round-trips every message through a **second, lenient parse**
  (`lenient_parser`), and several rules parse the same recorded bytes two or
  three times (`build_branch_index` runs per slot inside two rules). Typed reads
  made each read cheaper but did not touch the pass count; a single lenient
  parse per wire entry, shared across rules, is the real win here and is
  independent of ADR-0025.
- `DialogModel`'s tags and dialog URIs are still `String`, and `from_tag` /
  `to_tag` / `from_uri` / `to_uri` still read the OLD parsed fields — they
  return `&str` borrowed from the message, which the transitional accessors
  (which build a value per call) cannot do. M12's stored `MessageCore` is what
  lets those become typed reads; porting them now would mean allocating a value
  per rule per message.
- `starter_peer::RecordRouteRule` still detects a B2BUA's Record-Route by
  `rr.contains("callRef=")`, a substring probe over the raw value. It is a
  deliberate *policy* probe for our own stack's cookie params rather than a
  grammar read, so it stayed raw — but `RecordRouteEntry::uri().param("callRef")`
  would say it exactly, and would stop a display name containing the text from
  firing it.

### M10 — sip-pcap + loadgen

`sip-pcap` has **zero** `message_helpers::*` / `get_header` call sites left,
`src`, `bin` and `tests` alike. **`loadgen` needed no port** — the plan's guess
was right: its mux reads unparsed datagrams by design, so every sip-message call
it makes is `sniff::*` plus `message_helpers::is_invite_request_buffer`
(`preparse`, the Tier-1 brake's classifier, not on the M12 list). Nothing in it
touches a header value, a typed field or a header name string.

Three readers were missing and landed in their own sip-message commit:
`Uri::user_identity` / `Uri::same_user_identity` (the canonical subscriber a URI
names), `Uri::text` (the wire text, borrowed while the URI still carries the
bytes it was read from) and `Params::parse_list` (a header VALUE that is itself
a parameter list). `SipMessage::raw_text` completes the enum's share of the read
surface. `message_helpers::{uri_user_identity, header_param_value}` now delegate
to them, so each rule has one implementation.

**Every deletion target the plan listed is gone**, plus what the sweep found:

| deleted | replaced by |
|---|---|
| `query/project.rs` `uri_user` (the `<`/`sip:`/`tel:`/`@`/`;` peel) | `Uri::user_identity()`, host as the userless fallback |
| `flow.rs` `same_user_identity(&str, &str)` on the identity-adjacency pass | `inv_i.from_uri.same_user_identity(&inv_j.from_uri)` — no re-parse |
| `flow.rs` `header_param_value` on the `HeaderParam` correlation strategy | `Params::parse_list(&msg.raw_text(name))` → `value(param)` |
| `flow.rs` / `query/eval.rs` / `bin/sipflow.rs` `get_header(name)` ×4 | `msg.raw(HeaderName::from(name))` |
| `query/eval.rs` `get_header("Reason") + header_param_value(v, "cause")` | `msg.list::<Reason>()` → `param("cause")` |
| `query/eval.rs` `r.from` / `r.to` field reads (the request/response match) | `msg.from()` / `msg.to()` on the enum — the direction match disappears |
| `txn.rs` `branch_of(&sip_message::Via)` over `r.via.first().branch`, `r.to.tag` ×2 | `msg.top_via().branch()`, `r.to().tag()` |
| `flow.rs` retransmission key `a.via.first().branch` ×2 | `a.top_via().branch()` |
| `emit.rs` `r.from.uri` / `r.from.tag` / `r.to.*` / `r.uri` summary fields | `msg.from()`/`to()`/`cseq()`, `r.request_uri()` |

**`InviteSummary` carries parsed `Uri` values**, not URI text: the model is what
a consumer asks for a user identity, and the re-parse the old shape forced on
every projection and every adjacency comparison is gone. `Uri::text()` is what
the JSON and the ladder print, and it returns the source bytes verbatim, so the
emitted document is byte-identical and `EMIT_SCHEMA_VERSION` stays at 4.

**Behaviour deltas, all deliberate:**
- **`ruri_user` / `from_user` / `to_user` are the URI's user IDENTITY.** The
  userinfo `;`-params (`verstat`, `phone-context`) drop out, `tel:` and `sip:`
  forms of one subscriber agree, and RFC 3966 visual separators normalize — so
  `tel:+1-408-555-1212` and `sip:+14085551212@gw` now share a neighbour key,
  which is what the field exists to do. A userless URI yields its host WITHOUT
  the port, where the old peel returned `host:port`.
- **A comma-folded `Reason` line yields every cause, not just the first.** RFC
  3326 makes Reason foldable; the old param scan read the whole line and
  stopped at its first `cause=`. A Reason no reader accepts is now silent
  rather than scanned as a flat param list.
- **Header predicates are `HeaderName`-keyed and therefore compact-form aware**
  (`{"header": {"name": "Contact"}}` and `sipflow --header Contact` now answer
  for an `m:` line), where `get_header` matched the long spelling only.
- A `HeaderParam` correlation value that is a bare FLAG no longer yields a
  token. It never did in effect — the old `Some("")` was filtered by the
  emptiness check on the next line.

Workspace: 2127 tests passed, 0 failed. Clippy on `sip-pcap`: clean before and
after (the lane's only warnings are the pre-existing sip-message parser ones).

**Suspicions raised, not fixed:**
- **`flow.rs::looks_like_sip` is a raw SIP datagram classifier living in a
  consumer crate**, and `sniff` is the sanctioned home for those. It reads no
  header, so it is not an ADR-0025 item and porting it would touch nobody else
  — but it is the last raw SIP scan outside sip-message on this path, and
  `sniff` has no "is this datagram SIP at all" entry point for it to use.
- **`parse_param_list` (`parser::custom::structured_headers`) now has zero
  consumers**, and `message_helpers::{header_param_value, uri_user_identity,
  same_user_identity}` have none outside `tests/message_helpers.rs`. Same shape
  as the `parse_rack` note M9 left: they belong on the M12 deletion list.
- **`InviteSummary` holds three `Uri` values (272 B each on this branch) where
  it held three `String`s** — the `size_of`-of-the-value-model debt M5 and M8
  logged, now paid three times per leg of a capture. A capture with tens of
  thousands of legs is the case to watch; M12's value-storage pass owns it.
- `Node::FromUri` / `Node::ToUri` at a message binding clone the URI out of the
  transitional `from()`/`to()` accessor once per evaluated message, because the
  accessor builds a value rather than borrowing one. M12's stored `MessageCore`
  turns that back into a borrow.
- `Node::Ruri` at a message binding still text-matches `r.uri` (the raw
  Request-URI field) rather than `r.request_uri().text()`. Identical bytes and
  one less value built, but it is a public field M12 privatizes.

### M11 — b2bua-harness + failover-harness

Both crates have **zero** `message_helpers::*` / `get_header` / `get_headers`
call sites left, and zero remaining string surgery over header values
(`split(',')`/`split(';')`/`split_whitespace`/`trim_end_matches('>')` over a
header, `contains(";tag=")`). No reader was missing — the port needed nothing
added to `sip-message`. Workspace: 2127 tests passed, 0 failed.

**Every deletion target the plan listed is gone**, plus what the sweep found:

| deleted | replaced by |
|---|---|
| `failover.rs` `pri_bak_from_cookie` + `cookie_param` (the `;`-split over a Record-Route value), `runner.rs` / `limiter_ha.rs` / `call_terminate_on_backup.rs` `parse_uri_params` cookie reads ×4 | `failover_harness::cookie` — one `record_route_set()` → `uri().param(..)` reader the runner and all three test binaries share |
| `runner.rs` `req_tags` (`get_header` + `extract_tag` ×2) | `r.from().tag()` / `r.to().tag()` |
| `dual_face.rs` `rr_values` / `top_via_branch` (header-vec `eq_ignore_ascii_case` scans + a `branch=` prefix walk) and the twelve `rr.contains("host:port" / "outbound" / "w_pri=b1")` probes | `record_route_set()` + one `assert_rr_entry(entry, face, outbound, what)` reading `uri().host_port()` / `uri().param(..)`; `req.top_via().branch()` |
| `suppress_18x.rs` / `promote_pem.rs` / `fake_prack.rs` `has_token(Option<&str>, &str)` — three copies of the option-tag splitter | `header::<Require/Supported>()` → `contains(token)`, behind a typed `has_token<K: TokenKind>` that states the absent/unreadable policy once |
| `fake_prack.rs` `rack_matches` + `prack_update_forking.rs` `rack_of` (`split_whitespace` over the RAck row), `suppress_18x.rs`'s inline twin | `header::<RAck>()` compared to `RAck::new(rseq, seq, Method::Invite)` |
| eight copies of `assert_notify`'s `get_header("event"/"subscription-state")` + `starts_with(prefix)` | `header::<Event>()?.is("refer")` + `header::<SubscriptionState>()?.is(state)` |
| `numbering_plan.rs` From/To `contains("+1555…@trunk.example")`, PAI equality, the two `headers.iter().any(name == "to" && value.contains(..))` scans, `get_headers("contact")` + `contains("q=1")` | `from()/to().uri().user()/host()`, `header::<PAssertedIdentity>()?.uri().text()`, `list::<Contact>()` → `uri().text()` + `param("q")` |
| `proxy_b2bua.rs` `rr.contains("127.0.0.1:5080") && rr.contains(";lr")` ×2 | `record_route_set()` → `is_lb_proxy_route` (`host_port()` + `is_loose_route()`) |
| `tier3_admission_gate.rs` / `promote_pem.rs` Reason `contains("text=\"overload\"" / "cause=NNN")` ×3 | `header::<Reason>()` → `param("text"/"cause")` |
| `fake_prack.rs` / `update_matrix.rs` `content-type.to_ascii_lowercase().contains("application/sdp")` ×3, `announcement.rs` / `refer_gating.rs` Content-Type equality | `header::<MediaType>()?.is(..)` |
| `keepalive_via_proxy.rs` `get_headers(.., "via").len()` ×2 | `req.via().len()` |
| `reinvite_cancel.rs` `via.first().branch` / `cseq.seq` field reads, `refer_*` `req.cseq.seq`, 56 `to.tag` field reads | `top_via().branch()`, `cseq().seq()`, `to().tag()` |

**`failover_harness::cookie` is a new module, not a fifth copy.** Five call
sites read the proxy's stickiness cookie and each had rolled its own splitter
(two spellings of the same bug: `parse_uri_params` on a *header* value, and a
`;`-split that trims a trailing `>`). The reader is one function on the harness
lib — `record_route_set()` → first entry → `uri().param(name)` — so the tests
ask the value model instead of the bytes, and a comma-folded Record-Route now
yields its first ENTRY rather than the whole line.

**Behaviour deltas, all deliberate:**
- **`Event: refer` is asserted as the event package, not as the whole header
  value.** `Event: refer;id=42` is legal RFC 3515 and used to fail the
  equality; the token is what the assertion means. Same shape for
  `Subscription-State`, where `starts_with(prefix)` became `is(state)` — a
  strengthening, since every call site passed a bare state token and a prefix
  match would also have accepted `terminated-ish`.
- **A Contact `leg=` probe reads the URI parameter, not the header text.**
  `contact.contains("leg=b")` matched a display name or a header parameter
  spelling it; `uri().param("leg")` names the place the b2bua writes it.
- **`numbering_plan`'s From/To assertions read user and host separately.** The
  old `contains("+15551000@trunk.example")` would also pass on a display name
  carrying the text.
- **The dual-face Record-Route assertions read the entry, not the line.** A
  fold whose second entry named the other face used to satisfy the first
  entry's probe; the `w_pri` cookie is now asserted on each entry by name.
- **A Record-Route / Route / Via no reader accepts fails the test loudly**
  (`expect("readable …")`) where the old text probes silently read past it.
  The one place the raw values are still scanned is `dual_face.rs`'s
  `assert_no_proxy_route`, deliberately: it asserts the ABSENCE of a leak, so
  an unreadable entry must not be able to hide one.

**Suspicions raised, not fixed:**
- **`b2bua/tests/rules.rs` still does header-vec string surgery** — the top-Via
  and Route reads at `:734`, `:783`, `:795`, the Content-Type census at
  `:1041`, and the §7.3.1 duplicate counts at `:1365`/`:1407` all walk
  `.headers` with `eq_ignore_ascii_case`, and `:864` pushes a `SipHeader`
  literal. They are M6 residue (that phase's sweep counted
  `message_helpers`/`get_header` call sites, which these are not), and
  `raw(HeaderName::X)` says every one of them. M12 privatizes `.headers`, so
  they must move before the teardown compiles.
- `scenario-harness/tests/template_emission.rs` reads `.headers` the same way,
  but there it is the *subject*: the lane exists to pin captured header-name
  spelling, which only the raw entry carries. It needs the spelling-preserving
  `thaw` M7 logged, not a typed read.
- **Eight copies of `assert_notify` and three of `has_token` survive as
  copies**, one per integration-test binary. Typing them made each correct but
  did not remove the duplication; a shared REFER-assertion module on the
  `b2bua-harness` lib is the fix, and it is large-file-cleanup work rather than
  header-model work.
- `failover.rs` binds the primary worker by comparing the cookie ordinal to the
  string `"b1"`. The cookie now comes back through a typed reader, but the
  ordinal itself is still a `String` on both sides; a `WorkerOrdinal` newtype
  would make the mis-bind unrepresentable.

### M12 — teardown

The deletion landed. `SipRequest` / `SipResponse` are `{ start, inner }` over a
private [`MessageCore`], every public field is gone, and the typed core is
populated ONCE at parse instead of twice.

**Acceptance:** `grep -rE 'get_header\(|message_helpers::' crates
--include='*.rs'` returns NOTHING — not "only sip-message internals": the
`message_helpers` namespace no longer exists, and the two test files that kept a
local first-value/all-values helper name it for what it does. Workspace: 2105
tests passed, 0 failed. Clippy: the workspace's pre-existing categories only
(doc indentation, `too_many_lines`, variant size); the needless borrows this
port introduced are fixed.

**Measurements** (`cargo test -p sip-message --test alloc_budget --release`,
1000 ops/case, same box, M11 → now):

| case | allocs/msg | bytes/msg |
|---|---|---|
| decode/invite | 14 → **12** | 3145 → 3409 |
| decode/invite_sdp | 15 → **13** | 3481 → 3745 |
| decode/200_ok | 15 → **12** | 3612 → 3242 |
| proxy_hop/invite | 27 → **22** | 5899 → 6080 |
| proxy_hop/invite_sdp | 27 → **23** | 6539 → 7047 |
| build/invite_sdp | 48 → **45** | 6576 → 6848 |
| build/bye | 54 → **50** | 5966 → 4188 |
| build/response_200 | 37 → **32** | 7316 → 6268 |

Allocations are down in every case: the parse no longer stages a second copy of
the mandatory fields, and `freeze` no longer re-derives a legacy field set the
message does not have. Bytes move both ways — a parsed message now holds the
WIDE typed values (`Uri` 272 B, `Via` 232 B) where it used to hold narrow spans,
which costs on the decode cases and pays back on the build ones, where nothing
is converted any more. The `proxy_hop` case is also a different hop: it is
`thaw → with_uri → push_front(Record-Route) → freeze_bytes`, the ADR-0025 shape,
not the old clone-and-string-surgery one.

**What was deleted**

| deleted | replaced by |
|---|---|
| the whole `message_helpers` namespace: `headers` (`get_header`/`get_headers`/`set_header`/`remove_header`/`name_matches`), `header_params`, `name_addr` (`extract_tag`/`strip_tag`/`extract_contact_uri`/`extract_name_addr_uri`), `route`, `uri` (`parse_sip_uri`/`parse_uri_params`/`extract_host_port`/`ParsedSipUri`/`uri_user_identity`/`same_user_identity`), `via` (`parse_via_params`/`ViaParams`/`via_sent_by`/`stamp_received_rport_on_via`) | the typed read surface + `HeaderName` |
| `serialize_request_parts` / `serialize_response_parts` | `Draft::freeze_bytes` on the relay paths; `template::emitted_wire` for the one header block a caller still states |
| `types::{NameAddr, Via, Contact, CSeq, RequestUri, Uri, Rack, Replaces, ReferTo, Params, ParamValue}` | `header::*` — one shape per value, no parallel parsed model |
| `TypedHeader` + `SipMessage::typed` + `SipMessage::get_header`/`has_header` | `msg.header::<H>()` / `msg.raw(HeaderName)` (already open to extension headers) |
| `structured_headers::{parse_rack, parse_replaces, parse_refer_to, parse_param_list}` + `ParsedRack`/`ParsedReplaces`/`ParsedReferTo` | `RAck::parse`, `ReferTo::parse`, `Uri::escaped_header`, `Params::parse_list` |
| `template::HeaderClass` (Regenerated/Frozen) | `header::HeaderClass` (Structural/EndToEnd) — the two names were the same axis (M1 note), so the template now queries the one table |

The surviving members of the old namespace were NOT deleted, they were promoted
out of it: `sip_message::{emergency, param_codec, preparse, reject_503}` are
top-level modules now, because each is its own concern (emergency
classification, the B2BUA correlation-param codec, the strict pre-parse
classifiers, the Tier-1 503 template) and none of them is header access.

**The message shape**

`MessageCore { headers, core: CoreHeaders, optional, body, image }`, shared by
both directions; `SipRequest { start: RequestLine, inner }` and
`SipResponse { start: StatusLine, inner }`. The read surface is written once
over the core and attached to both, so neither direction carries a copy. The
start-line types are the draft's own `RequestLine`/`StatusLine` — thaw and
freeze move a start line across, they do not translate one.

Naming: the datagram is `msg.image()`, because `msg.raw(HeaderName)` is the
header escape hatch. That resolves the readability trap M2 logged.

**Behaviour deltas, all deliberate:**
- **Parameter lists keep their wire order and spelling everywhere.** The
  scan-side `Params` is now the ordered small-vec, so the old lowercase-copy of
  a mixed-case parameter name is gone and a value read off a message renders
  back as it arrived. This is what M2 said "nothing should round-trip before
  M12 stores the real core" was waiting for.
- **A URI the strict reader rejects is kept whole and says so.** The parser
  stores `Uri::parse_or_opaque`, and `Uri::is_opaque()` is the guard a router
  asks — the proxy's worker-outbound hop answers 400 on one instead of
  resolving its text as a host. `Uri::opaque` also records its source, so
  `text()` still borrows and an unreadable value is never reformatted.
- **`Refer-To`, `RAck` and the P-header family are read by the value model.** A
  Refer-To whose URI has no scheme is now an `Err` on `optional().refer_to`
  where the old scanner accepted it and left the strict pass to complain; the
  embedded `Replaces` is `uri().escaped_header("Replaces")` (decoded text), not
  a parsed struct — nothing consumed the struct.
- **`Method` and `CallId` compare across a borrow.** `PartialEq<Method> for
  &Method` and `CallId`'s `str`/`String` comparisons exist because the read
  surface hands out references and a comparison against a literal must not have
  to clone.
- `sip_message::Bytes` is re-exported: the public surface takes and hands back
  `Bytes`, so a consumer must be able to name it.

**Two tests changed shape because their subject became unrepresentable:**
- `rfc3261_peer::cancel_with_invite_cseq_is_flagged` built its subject by
  mutating a parsed CSeq. A frozen message cannot be edited into a
  CANCEL-with-INVITE-CSeq and neither parser accepts one on the wire, so the
  rule's decision is factored out (`cancel_cseq_mismatch`) and the test drives
  it directly, plus asserts the wire form is rejected. Coverage is unchanged.
- `tests/serializer.rs` (the Content-Length safety net) can no longer build a
  message that disagrees with its own body. It exercises the net at the one
  place a wrong length can still arrive — `emitted_wire`, where the caller
  states the header block.

`tests/message_helpers.rs` and `tests/header_registry_extension.rs` are deleted
with the APIs they pinned (the extension-header contract is
`msg.header::<H>()`, covered by `header_round_trip.rs`); the ABNF `replaces`
target goes with `parse_replaces`.

**Deferred, deliberately, as M12b: the stringly `Generate*Opts` fields.**
Everything else on the M12 deletion list landed. Flattening `values` into the
opts and typing `from`/`to`/`vias`/`cseq`/`request_uri`/`content_type` touches
27 consumer files and — for `GenerateRelayedResponseOpts` — changes the wire
bytes of every relayed response, because the echoed Via/From/To/CSeq stop being
memcpy'd and start being re-rendered. It also forces the `call`-crate
string-versus-value decision that ADR-0025 explicitly puts out of scope
(`PendingRequest` and `StackDialog` are the source of those strings). That is a
port with its own risk surface and its own test pass; bundling it into the
teardown would have put the b2bua relay path and the teardown in one
unreviewable commit.

**Suspicions raised, not fixed:**
- **A parsed message is wider than it was.** `CoreHeaders` holds `From`, `To`,
  `CallId`, `CSeq`, `NonEmpty<Via>` and the Contact set as full values, so the
  decode cases gained ~250 bytes each. That is the `size_of` debt M5, M8 and
  M10 all logged, now paid once per message instead of once per read — a better
  trade, but the value widths (`Uri` 272 B) are still the thing to shrink.
- **`optional` is still eagerly extracted on every parse**, and nothing outside
  `sip-message` reads it any more except one RAck site in the audit. ADR-0025
  keeps the eager + non-fatal semantics deliberately, so it stays — but it is
  now a pass whose only consumer is `validate_strict` and a handful of tests.
  Worth its own decision before M13 measures it.
- **`freeze` still re-runs the eager field extraction over the block it just
  rendered.** M2 logged this as M12 work; it is not fixed here. Building
  `CoreHeaders` straight from the typed entries in hand needs the draft to know
  which entry is the From (it holds `dyn HeaderValue`, so it cannot downcast
  cheaply), and the extraction is also where the mandatory-header gate lives.
  This is the remaining allocation on the build path.
- `apply_name_forms` / `apply_remote_target_emits` still rewrite a header
  vector, and `emitted_wire` renders it. That is the template lane's
  spelling-preservation seam and the only remaining "caller states the header
  block" path; M7's spelling-preserving `thaw` would retire all three.
- `Draft::with_uri` on a thawed request replaces the Request-URI wholesale, and
  the proxy hop in `alloc_budget.rs` / the bench now uses it. Nothing checks the
  new URI is not opaque at that seam.

### M13 — perf re-baseline

The budgets gate again. Before this phase `decode/*` and `proxy_hop/*` carried
the pre-zero-copy numbers (87/93/82/176/189 allocs), 4–7× the measured cost, so
they passed whatever the code did; every entry is now the measured cost + 20 %.
The cases and their fixtures moved into `tests/perf/mod.rs`, shared by
`tests/alloc_budget.rs` and `benches/sip_parser.rs`, so the two views measure
the same operations by construction rather than by two copies of the fixture.

**Measurements** (`cargo test -p sip-message --test alloc_budget --release`,
1000 ops/case, same box; "M12" = the numbers that phase logged).

| case | allocs/msg (M12 → M13) | bytes/msg | budget allocs / bytes |
|---|---|---|---|
| decode/invite | 12 → **12** | 3409 | 14 / 4090 |
| decode/invite_sdp | 13 → **13** | 3745 | 15 / 4490 |
| decode/200_ok | 12 → **12** | 3242 | 14 / 3890 |
| hop/rewrite_set | new → **29** | 8395 | 34 / 10070 |
| hop/stamp_rport | new → **8** | 2343 | 9 / 2810 |
| proxy_hop/invite | 22 → **21** | 6080 → 5729 | 25 / 6870 |
| proxy_hop/invite_sdp | 23 → **22** | 7047 → 6464 | 26 / 7750 |
| build/blank_draft | new → **27** | 7053 | 32 / 8460 |
| build/invite_sdp | 45 → **45** | 6848 | 54 / 8210 |
| build/bye | 50 → **50** | 4188 | 60 / 5020 |
| build/response_200 | 32 → **32** | 6268 | 38 / 7520 |

The `proxy_hop/*` drop is not an optimization: the case used to end in
`.to_vec()`, so it charged one copy of the rendered datagram that no forwarding
path performs — a hop sends the `Bytes` `freeze_bytes` hands it.

**Three new cases, one per ADR-0025 guardrail that had none:**

- `hop/rewrite_set` — the full RFC 3261 §16.4/§16.6 rewrite on a message parsed
  OUTSIDE the measured region: pop the two Route entries this proxy recorded,
  stamp received/rport on the top Via, state the decremented Max-Forwards, push
  the direction-carrying Record-Route pair, push this hop's own Via. Guardrail 2
  budgets the *thawed-draft hop*, so the parse must not be inside it.
- `hop/stamp_rport` — the received/rport stamp alone, the smallest edit a hop
  can make and therefore the floor a thaw→freeze costs.
- `build/blank_draft` — origination straight onto `RequestDraft::new`, no
  options struct and no string assembly, for the same INVITE the decode cases
  parse.

**Against the ADR targets: the hop target is met, the parse target is missed by
two, and the full rewrite set misses it by a lot.** No budget was loosened to
hide either; both gaps are below.

- *Parse ≤ ~10 allocs/msg* — **measured 12–13.** The whole gap is the eager
  Contact set: the same INVITE with its Contact line removed parses in **9**
  allocs / 1458 bytes, one extra Via costs +1 alloc / +441 bytes, and
  `parse_shared` (caller owns the datagram) saves the image copy at **11**. The
  three Contact allocations are the index's `Vec<&SipStr>`, the set's
  `Vec<header::Contact>`, and that vector's growth to the four-element minimum
  capacity `Vec::new()` + `push` gives a 432-byte element — 1728 bytes reserved
  for the one Contact almost every message carries.
- *Thawed-draft hop ≤ ~12 allocs/hop* — **met by the hop itself**:
  `hop/stamp_rport` is 8, and the minimal hop is `proxy_hop/invite` (21) minus
  its parse (12) ≈ 9. **`hop/rewrite_set` is 29** for six edits, and the
  dominant term is that the route set is read twice — once as
  `req.list::<RouteEntry>()` to classify, then again inside each `pop_top`,
  which re-parses the line it pops. That is exactly the `try_list` gap M5, M6
  and M8 each logged from their own side, now with a number on it.

**Criterion** (`cargo bench -p sip-message --bench sip_parser`, capped scope,
`--measurement-time 2`, WSL2 — comparable to each other, not to another box):

| case | time | throughput |
|---|---|---|
| decode/invite | 3.48 µs | 287 Kmsg/s |
| decode/invite_sdp | 3.68 µs | 272 Kmsg/s |
| decode/200_ok | 3.05 µs | 328 Kmsg/s |
| decode_shared/invite | 3.46 µs | 289 Kmsg/s |
| decode_shared/invite_sdp | 3.57 µs | 280 Kmsg/s |
| hop/rewrite_set | 3.92 µs | 255 Kmsg/s |
| hop/stamp_rport | 1.35 µs | 742 Kmsg/s |
| proxy_hop/invite | 4.17 µs | 240 Kmsg/s |
| proxy_hop/invite_sdp | 4.36 µs | 229 Kmsg/s |
| build/blank_draft | 3.89 µs | 257 Kmsg/s |
| build/invite_sdp | 4.11 µs | 243 Kmsg/s |
| build/bye | 4.24 µs | 236 Kmsg/s |
| build/response_200 | 5.78 µs | 173 Kmsg/s |

**The draft engine is 40 % cheaper than the recipe over stringly options** —
`build/blank_draft` 27 allocs against `build/invite_sdp` 45 for the same INVITE,
with the caller's values built outside the measured region in both cases. That
is the measurement M12b was deferred without.

Workspace: 2105 tests passed, 0 failed. `loadgen --test smoke`: 27 passed, 0
failed, 5 ignored (slow lane). Clippy on `sip-message --all-targets`: the
workspace's pre-existing categories only, none from the new files.

**Suspicions raised, not fixed:**
- **The Contact set is the parse target's whole gap** (above). The vector
  growth is a one-line reservation; the two vectors are the eager model. A
  parse that filed contacts straight from the dispatch walk into one
  right-sized vector would land the ADR's ≤ 10 without changing any semantics.
- **A `SipRequest`/`SipMessage` is 2320 bytes by value** (`Uri` 272, `Via` 232,
  `Contact`/`From` 432 each), so every move of a message memcpies 2.3 KB and
  every `Option<SipMessage>` or channel of them carries it. This is the
  `size_of` debt M5, M8, M10 and M12 each logged, now measured at the message
  rather than at the value: shrinking `Uri` is the lever.
- **`hop/rewrite_set` pays for reading the route set twice.** A `try_list` that
  hands the draft back alongside the parse error — the primitive M5 asked for —
  would collapse the classify pass and the pop pass into one and is the single
  biggest win available on the forwarding path.
- The benches have no committed baseline, so a regression is visible only by
  comparing against these numbers by hand. `criterion --save-baseline` writes
  under `target/`, which does not survive a clean; a numbers-in-the-log
  convention is what this table is.
- `hop/*` parses its fixture once and thaws it 1000 times, which is the honest
  shape for the guardrail but means the case never sees a cold image. A hop on
  a message whose image is not already in cache costs more than this says.

### Engine hardening — post-migration cleanup

Findings from the final review of the sip-message engine, one group per commit.
Everything here is inside `sip-message`; no consumer semantics change except
where stated.

**1. `retarget` no longer carries parameters onto a kind that has none.**
`NameAddrHeader::retarget::<J>()` accepted any `NameAddrKind`, so
`msg.to().retarget::<kind::PAssertedIdentity>()` produced a PAI still carrying
`;tag=…` — a value RFC 3325 §9.1's bare `name-addr / addr-spec` has nowhere to
render and no reader accepts. The kind axis now states both sides: `NoParams`
joins `RichParams` as a marker trait (P-Asserted-Identity and
P-Preferred-Identity implement it; together the two cover every
`NameAddrKind`), `retarget` is bounded on `RichParams`, and `retarget_bare` is
the conversion towards a `NoParams` kind — it drops the parameters, because
carrying them would be the defect. The back door is closed at compile time:
there is no conversion that puts a tag on a P-header. Pinned by
`a_tag_cannot_ride_onto_a_header_whose_grammar_has_no_parameters`.
`NameAddr::without_params` is the address-level primitive it uses.

**2. `NumericHeader` is bounded by its kind's registry entry.**
`MaxForwards::new(1000)` used to freeze into a request our own header-block
parser (`numeric_header_rule`, gate 255) rejects — the stack could build a
message it would not accept. `NumericKind` now carries `MIN`/`MAX`, stated once
per header with the RFC that fixes it: Max-Forwards `0..=255` (§20.22, the same
number the parser gates), Content-Length `0..=2^31-1` (§20.14, CSeq's ceiling),
Expires / Min-Expires / Min-SE `0..=2^32-1` (`delta-seconds`), RSeq
`1..=2^31-1` (RFC 3262 §7.1 — zero is not a sequence number). Enforcement is on
every path into the type: `parse` refuses out of range, `new` clamps to the
nearest legal value (a count computed from configuration or a body length
cannot promise the range, and the ceiling is the honest answer for one that
overshoots), `checked` refuses for a peer-supplied number, `decremented` stops
at `MIN` rather than at zero. `MaxForwards::DEFAULT` names RFC 3261 §8.1.1.6's
70 once, and `emit::DEFAULT_MAX_FORWARDS` derives from it.

Two deltas worth naming: RFC 4028's "Min-SE MUST NOT be less than 90" is a
session policy rather than a grammar bound, so it is NOT gated (gating it would
make a peer's `Min-SE: 60` unreadable instead of refusable); and
`rfc3262_peer::Reliable1xxHeadersRule` keeps flagging `RSeq: 0` — it reads
through `value_of::<RSeq>`, which now yields `None` on the range error and
lands on the same finding.

**Suspicions raised, not fixed:**
- **`MinSe` parse and the parser's numeric gate disagree about a param tail.**
  `numeric_header_rule` allows `Min-SE: 90;refresher=uac` (RFC 4028) through the
  header-block gate, but `NumericHeader::parse` reads the whole value as digits
  and rejects it. Nothing reads `header::<MinSe>()` today, so it is latent; the
  fix is either a `TokenParamsHeader` shape for Min-SE (as Session-Expires
  already has) or a digits-prefix read.
- The audit's `RSEQ_MAX` (`rfc3262_peer.rs`) and the parser's `INT_32_MAX` /
  `255` registry are now a third and fourth statement of bounds the kind axis
  states. `numeric_header_rule` could be a query against `NumericKind::MAX`;
  it is not, because the gate is keyed on `HeaderName` at scan time and the
  kinds are types.

**3. `push_raw` is documented as what it is: the verbatim seam.** Its doc called
itself a test-lane escape hatch, while every generator recipe lowers its
stringly options onto it and every verbatim echo (the CANCEL's Via, the non-2xx
ACK, the relayed response) rides it in production. It now states the contract —
a value carried verbatim, not parsed, not validated, memcpy'd at freeze — and
names what still gates it (`freeze` reads the mandatory headers and the ones the
typed core comes from, so a raw line in one of those fails). The test-lane claim
moved to `render_unchecked` alone, which is the reviewable grep target it always
was.

**4. `Folding::LinePerValue` split into `Opaque` and `SetPerLine`.** One variant
carried two different facts: the credentials family (RFC 3261 §20.7) keeps
commas *inside* one value, while Require/Supported/Allow are comma-separated
token sets that `TokenListHeader::parse` reads into one set. The doc stated only
the first, so it was false for six of the ten kinds that used it. `Opaque` is
"a comma on this line is data"; `SetPerLine` is "the commas are this value
grammar's, and several lines union". `parse_line` treats them alike — one line,
one value — which is precisely why the distinction had to be in the name rather
than in a branch.

**5. `Wire::bytes` deleted.** Zero callers, and it silently dropped a whole
non-ASCII slice — a renderer that quietly emits nothing is the worst available
failure. `Wire::byte` keeps the drop (it is what protects `as_str`'s UTF-8
invariant) but now `debug_assert`s first and documents the property that makes
the arm unreachable: every call site writes an ASCII grammar literal (`;`, `=`,
`<`, space).

**6. `header_round_trip.rs` now also gates LOSS, not just the fixpoint.**
`parse(render(v)) == v` is blind to a value both passes lose identically: drop a
parameter at parse and the render is short, the re-parse is short the same way,
and the property still holds. On the subset of each corpus a render is expected
to reproduce octet for octet, the rendered length is now asserted against the
trimmed input's — so a dropped parameter, a truncated host or a lost URI header
fails the lane. The subsets are stated as predicates rather than by outcome
(that would be circular): a name-addr already bracketed with no whitespace or
quoted string, a Via without a quoted-pair, a CSeq/RAck without a redundant
leading zero, a URI without the two known losses below. Each carries its own
floor, as the fixpoints do, so a shrinking subset cannot make it vacuous.

Coverage on the frozen corpus: From 440, Contact/Route 93 each, Refer-To 392,
P-Asserted-Identity 122, Via 518, CSeq 949, RAck 897, SIP-URI 1000 — all
byte-preserving today. Verified to have teeth by making `Uri::parse` drop the
escaped-header list: the fixpoint stays green (both passes lose it) and the new
assertion fails, which is exactly the blind spot it was added for.

**Two live parser losses, RAISED not fixed** (the step was explicitly not to
change parser behaviour; both are excluded by the URI predicate):
- **An escaped-header pair with no `=` is dropped.** `Uri::parse` skips a
  `?`-section pair that carries no `=` (`uri.rs`, the `else { continue }` in the
  header loop), so `sip:a@h?X-Trace` normalizes to `sip:a@h` and
  `sip:a@h?a=b&X-Trace&c=d` to `sip:a@h?a=b&c=d`. Unedited URIs render their
  source, so this shows only once something touches the URI — a retarget, a
  `without_escaped_headers`, a built URI. RFC 3261 §19.1.1's `header` production
  requires `hname "=" hvalue`, so the input is malformed; silently deleting part
  of a Request-URI is still the wrong answer to it.
- **An unbracketed IPv6 host is TRUNCATED, not rejected.** `sip:2001:db8::1`
  parses as host `2001` with no port, and `sip:a@2001:db8::1` likewise. RFC 3261
  §19.1.1 requires the brackets, so the value is malformed — but a router that
  resolves `2001` is worse than one that refuses the URI, and `Uri::parse`
  refuses far less malformed input elsewhere (an out-of-range port). This is the
  same class as the port guard M5 replaced with a value-type property.

A third delta is a normalization, not a loss, and is excluded on those grounds:
a redundant leading zero (`sip:h:007`, `CSeq: 007 INVITE`) renders as the number
it means.

**7. ADR-0025's "Performance invariants" section states the achieved number
beside each target.** The four guardrails read as unqualified targets, so the
two that are missed (parse at 12–13 against ≤ ~10, the full §16.4/§16.6 rewrite
set at 29 against ≤ ~12) looked met to anyone reading the ADR without the M13
log. Each item now carries its measurement, its verdict, and the one-line cause
M13 diagnosed — the eager Contact set for parse, the route set read twice for
the rewrite set — plus a pointer to this file for the tables. No number here is
new; this is the ADR catching up with the measurement.

### Consumer hardening — post-migration cleanup

Findings from the final review of the *consumer* side of the port, one group per
commit. Everything here is in `b2bua`.

**1. An unreadable Record-Route can no longer make a dialog pod-direct.**
`dialog_track.rs` read both dialog route sets as
`list::<RecordRouteEntry>().unwrap_or_default()`, so a recorded route no reader
accepts produced an EMPTY route set — silently, and indistinguishably from "the
peer recorded nothing". An empty dialog route set sends every in-dialog request
(BYE, keepalive OPTIONS, re-INVITE) straight at the peer's Contact, i.e.
pod-direct, which the deployment forbids: a pod IP is not routable
peer-to-peer, so the call is lost the moment either side moves. That is the
long-call-loss class, and M6 logged the cliff on the way past.

The route-set readers (`uac_route_set`, `uas_route_set`) are now fallible, and
one seam — `ActionExecutor::dialog_route_set` — states the policy for all three
call sites (early dialog, 2xx confirmation, a-leg UAS dialog): name the call on
stderr (`WARN: call <ref> leg <id>: a recorded route does not read …`) and fall
back to `relay::outbound_proxy_route_set(config)` — the one loose `Route` at the
configured front proxy, the same entry the b-leg bootstrap preloads. With no
outbound proxy configured (local/dev, where the transport IS peer-direct) the
set stays empty, but the read still fails loudly instead of passing for an
absent Record-Route.

Swept for the same pattern on routing-critical reads, and fixed the same way:
- `relay.rs::apply_b_leg_egress`'s freeze-error arm returned `(req, dest)` — the
  callee's own address — so a failed Route preload fell off the proxy path
  entirely. It now warns and forwards to the proxy WITHOUT the preload: the
  Request-URI already names the callee, so the proxy forwards and record-routes
  it. This also makes the arm agree with `leg_egress_dest`, whose "keep in sync"
  contract it was quietly breaking on that branch. (The arm is still unreachable
  — a thawed draft cannot be incomplete — but "unreachable" is not a routing
  policy.)
- `dialog_track.rs::contact_uri` conflated "no Contact" with "a Contact no
  reader accepts". The unreadable case now warns and keeps the dialog's current
  remote target rather than silently retargeting.
- `relay.rs::target_dest` keeps its fallback (a stored dialog target that does
  not read is resolved as a host name — the `call` crate stores text by design,
  ADR-0008) but no longer takes it silently. Empty targets stay quiet: a dialog
  with no learned target yet is normal, not a failure.

Pinned by `dialog_track`'s new unit tests: an unreadable recorded route
(`<sip:10.0.0.9:70596;lr>` — the message parses, the route does not) yields the
front-proxy route set on BOTH dialog sides and never an empty one; without a
configured proxy the fallback is empty but the read still errs; and the readable
path keeps its dialog order (UAC reversed, UAS forward, a comma-combined line
teaching both halves).

Workspace: 2111 tests passed, 0 failed. Clippy on `b2bua --all-targets`:
unchanged from baseline.

**Suspicion raised, not fixed:** the fallback route set is the *configured*
proxy, not the route the peer actually recorded, so a dialog that takes it is
routed correctly only because every worker route goes through that one proxy. In
a deployment with several front proxies the fallback would pin the dialog to the
configured one rather than to the recorder — still better than pod-direct, but
it is a deployment assumption living in a rules action.

**2. `b2bua/tests/rules.rs` reads headers through the typed surface.** The six
raw scans M11 logged (`headers().iter().find/filter(|h|
h.name.eq_ignore_ascii_case(..))` for the b-leg INVITE's Via, the CANCEL's Route
set and Via, the Content-Type dedup census, and the two §7.3.1 duplicate counts
on Allow/Supported) are `raw(HeaderName::X)` reads now. The file has zero
`headers()` / `eq_ignore_ascii_case` / `SipHeader`-literal sites left.

Two things the port buys beyond tidiness: the scans were compact-form-blind, so
a `v:`/`c:`/`k:` spelling would have made every one of them read "header absent"
and the assertions pass vacuously (`raw` resolves the name); and the duplicate
counts now ask the same question the assertion above them asks, through the same
accessor, so a count and a value read can no longer disagree about which lines
they mean.

Workspace: 2111 tests passed, 0 failed. Clippy on `b2bua --all-targets`:
identical warning set before and after.
