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
- [ ] M2 value hierarchy + draft engine
- [ ] M3 generators → recipes
- [ ] M4 sip-txn
- [ ] M5 sip-proxy
- [ ] M6 b2bua-sdk + b2bua
- [ ] M7 scenario-harness
- [ ] M8 e2e-core + e2e-model + announcement
- [ ] M9 sip-net rfc_audit
- [ ] M10 sip-pcap + loadgen
- [ ] M11 harness/test crates
- [ ] M12 teardown (delete legacy, privatize, MessageCore)
- [ ] M13 perf re-baseline
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
