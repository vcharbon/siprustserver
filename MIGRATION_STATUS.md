# Migration status

Port of [sipjsserver](./portsource/sipjsserver) → Rust. One **layer** per row;
each maps to a workspace **crate**. Status legend:

- ✅ **ported** — code + tests ported and passing
- 🟡 **scaffolded** — directories, manifests, trait/types in place; bodies are `todo!()`
- ⬜ **pending** — not started

| Layer | Crate | Source | Status |
|---|---|---|---|
| **Clock** (test-time / timestamp seam) | `crates/sip-clock` *(slice 0)* | Effect `Clock`/`TestClock` (`tests/harness/runner.ts`) | ✅ monotonic-anchored `Clock` (`system`/`test_at`/`now_ms`) + `testkit` chunked-advance helper; 5 tests green. Behaviour stays on `tokio::time` directly — this is timestamps only ([plan §2](docs/MIGRATION_PLAN_B2B.md)) |
| **SIP message** (parse/serialize/validate) | `crates/sip-message` | `src/sip/` (message core) | ✅ parser + serializer + sdp + generators + message-helpers/sipfrag ported; full test corpus green (159 tests). rvoip oracle behind `rvoip-oracle`; ABNF corpus pending `abnfgen` |
| **Network / UDP** (transport, SignalingNetwork) | `crates/sip-net` *(slice 2)* | `src/sip/{UdpTransport,SignalingNetwork,BufferedUdpEndpoint}.ts` | ✅ `SignalingNetwork` ported — trait (DI seam) + real (tokio `UdpSocket`) + simulated (in-memory fabric) + recording/`scopedAudit` & `paranoidInputs` contract decorators; 22 tests green. `UdpTransport` facade + `BufferedUdpEndpoint` + `ConnectivityGate` **deferred** (Slice 2 §un-ported) |
| **Test/contract foundation** (Recorder, RunContext, 4 wrappers) | `crates/layer-harness` | `src/test-harness/framework/*` | ✅ Recorder + typed channels + projectors + RunContext/severity + recording helpers + 4-wrapper vocabulary ported (test-only, SIP-agnostic); 5 tests green. Recorder now stamps `at_ms` via an injected `sip-clock` `Clock` (deterministic report timestamps). [ADR-0004](docs/adr/0004-layer-harness-test-foundation.md) |
| **Scenario harness + reports** (DSL, driver, SVG/txt/HTML) | `crates/scenario-harness` | `src/test-harness/framework/{dsl,interpreter,*-report,svg-sequence-diagram}.ts` | ✅ thin scenarios-as-data DSL (named agents + `Send`/`Expect`/`Advance`) + recording-first driver + SVG/global.txt/per-endpoint/HTML renderers; the trace is **projected from the recording** (`sip-net::to_sip_entries`), not interpreter state; 1 e2e test green. Fluent dialog builder, `or`/`parallel`/media/chaos **deferred**. [ADR-0006](docs/adr/0006-scenario-harness-recording-first.md) |
| **Transaction / dispatch** | `sip-net` or own crate | `src/sip/{TransactionLayer,SipRouter,PerCallDispatcher}.ts` | ⬜ pending |
| **CallContext data model** | `call` | `src/call/` | ⬜ pending |
| **Rule engine** | `rules` | `src/b2bua/rules/` | ⬜ pending |
| **Call limiter** | `limiter` | `src/call/CallLimiter*.ts` | ⬜ pending |

---

## Slice 0 — Clock layer (`crates/sip-clock`)

**Source release:** sipjsserver @ submodule commit pinned in `.gitmodules`.
Source: Effect `Clock`/`TestClock`; virtual-time advance loop in
`tests/harness/runner.ts` (100 ms chunks).
**Scope (confirmed with user):** the injectable wall-clock `now_ms()` timestamp
seam **only**. All load-bearing behaviour (timers, deadlines, windows,
idle-sweeps) runs on monotonic `tokio::time` directly and is tested with
`tokio::time::pause`/`advance` — *not* wrapped in a trait.

### Decisions (see [plan §2](docs/MIGRATION_PLAN_B2B.md))
- Effect fused "what time is it?" + "wake me later" in one runtime-injected
  seam. Rust splits them: **scheduling → `tokio::time` (no seam)**, **timestamps
  → a tiny injectable `Clock` value (not a trait)**.
- `now_ms` is **monotonic-anchored** (`anchor_wall_ms + tokio Instant elapsed`):
  never decreasing in prod, and in tests one `tokio::time::advance` moves both
  behaviour timers and `now_ms` together (no separate `TestClock` counter).
- Rejected the draft `Clock` trait with `sleep_until`/`timer` — duplicates tokio
  and tempts a worse timer-wheel re-implementation; only `now()` is load-bearing.

### Source → Rust
| Source | Rust | Status |
|---|---|---|
| Effect `Clock` (prod `now`) | `Clock::system()` + `now_ms()` | ✅ |
| Effect `TestClock` (settable now) | `Clock::test_at(anchor_ms)` (rides `tokio::time::pause`) | ✅ |
| `TestClock.adjust` 100 ms-chunk loop | `testkit::advance_in_chunks` / `advance_in_100ms_chunks` (`testkit` feature) | ✅ |

### Tests
| Area | Rust home | Status |
|---|---|---|
| now_ms lockstep with `advance`, monotonic, clones share timeline | `src/lib.rs` unit tests | ✅ 4 |
| property: `now_ms == anchor + advanced` (replaces "deadline = f(now,timeout)") | `src/lib.rs` proptest | ✅ |
| chunked-advance helper steps + lands on total | `src/lib.rs` (`--features testkit`) | ✅ 1 |

### Un-ported with justification
- **`Clock.sleep`/`sleep_until`/timer scheduling** — *not ported as a seam by
  design.* Behaviour calls `tokio::time` directly (monotonic), tested with
  `pause`/`advance`. Discipline: behavioural code uses `tokio::time::Instant`,
  never `std::Instant`; pure logic takes `now`/durations as data.
- **Failover deadline reconstruction** — deferred to the HA/failover slice.
  `Instant`s are not portable across replicas, so replicated events will carry a
  remaining duration or absolute wall deadline; not needed until then.

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

---

## Slice 2 — Network layer (`crates/sip-net`) + test foundation (`crates/layer-harness`)

**Source release:** sipjsserver @ submodule commit pinned in `.gitmodules`.
**Scope (confirmed):** the `SignalingNetwork` service — its trait (DI seam),
the two production-shape implementations (real dgram-backed + simulated
in-memory fabric), and the contract decorators (recording/`scopedAudit` +
`paranoidInputs`). Plus the **shared test foundation** the wrappers stand on,
extracted into `layer-harness` (Recorder, RunContext, severity, recording
helpers). The `UdpTransport` policy facade and transaction/dispatch are out of
scope (later slices).

### Decisions (see ADRs)
- Network shape adapted to tokio: `#[async_trait]` traits, receiver-style
  endpoint (`recv`/`try_recv`, no `Stream`), `SocketAddr` everywhere,
  hand-rolled bounded `PacketQueue` ([ADR-0005](docs/adr/0005-network-layer-rust-shape.md)).
- The effect-layer-test machinery is a **test-only** `layer-harness` crate; the
  4 wrappers become **decorator structs** implementing the same trait
  ([ADR-0004](docs/adr/0004-layer-harness-test-foundation.md)).
- `propertyTest` and `parity` are **skipped with justification** (no input
  domain; impls not output-equivalent) — see ADR-0005.

### Source modules → Rust (port checklist)
| Source | Rust module | Status |
|---|---|---|
| `SignalingNetwork.ts` (Tag + public types + errors) | `sip-net/net.rs` + `sip-net/types.rs` | ✅ trait pair, `BindUdpOpts`, `UdpPacket`, `UaRole`, `PreIngressAction/Hook`, `BindError`/`SendError`, counters |
| `SignalingNetwork.real.ts` (+ `realTracing`) | `sip-net/real.rs` | ✅ tokio `UdpSocket`, recv-pump task, pre-ingress dispatch, tail-drop. `realTracing` boolean **folded away** — recording is now a decorator, not a base variant |
| `SignalingNetwork.simulated.ts` | `sip-net/simulated.rs` | ✅ in-memory routing by `SocketAddr`, forked transit delay, `send_fault`, `undeliverable`, `in_flight`/`await_in_flight`, `queue_depths` |
| (TS `Queue.bounded` usage) | `sip-net/queue.rs` | ✅ `PacketQueue` (bounded `VecDeque` + `Notify`) |
| `SignalingNetwork.contracts.ts` (`scopedAudit`) | `sip-net/contracts.rs` (`RecordingSignalingNetwork`) | ✅ typed event recording + per-bind & cross-message rules + layer-close structural checks (`inFlightImbalance`/`undeliverable`/`queueLeak`) |
| `SignalingNetwork.contracts.ts` (`paranoidInputs`) | `sip-net/contracts.rs` (`ParanoidSignalingNetwork`) | ✅ PA2/PA3/PA4/PA5 (PA1 enforced by `SocketAddr`); violations `panic!` (defect) |
| `effectLayerTest.ts` (`withCanonicalContracts`) | `sip-net/contracts.rs` (`with_all_contracts`) | ✅ canonical order `paranoidInputs(scopedAudit(impl))` |
| `report-recorder/Recorder.ts` + `types.ts` | `layer-harness/recorder.rs` + `scenario.rs` + `anomaly.rs` | ✅ typed channels (`for_tag`), projectors, lane registry, anomaly ledger, snapshot |
| `RunContext.ts` | `layer-harness/run_context.rs` | ✅ 3-tier severity + `severity_for` |
| `EventSequencer.ts` | `layer-harness/event_sequencer.rs` | ✅ atomic monotonic counter |
| `recordingHelpers.ts` | `layer-harness/recording.rs` | ✅ `record_call` + `ReleaseGuard` (two of four — see un-ported) |

### Tests ported
| Area | Rust home | Status |
|---|---|---|
| Recorder / channels / severity / lane conflict | `layer-harness/tests/recorder.rs` | ✅ 5 |
| Simulated routing, already-bound, undeliverable, send-fault, tail-drop, pre-ingress, unbind-on-drop | `sip-net/tests/simulated.rs` | ✅ 7 |
| Real loopback send/recv, pre-ingress reply round-trip, no-transit invariants | `sip-net/tests/real_loopback.rs` | ✅ 3 |
| Recording capture, paranoid defects, queue-leak (advisory), undeliverable (deferred-fail), peer-rule fail, real-run silencing | `sip-net/tests/contracts.rs` | ✅ 7 |

### Un-ported with justification
- **`UdpTransport.ts`** (Tier-1 overload brake, `UdpTransportMetrics`,
  `localAddress` source-of-truth) — a policy facade over `bindUdp` that depends
  on `AppConfig` + `MetricsRegistry` (unported slices). The `PreIngressHook`
  primitive the brake is built on **is** ported, so the facade is a thin
  reassembly when those deps land.
- **`BufferedUdpEndpoint.ts`** (non-blocking per-peer outbound drainer) — same
  reason; an outbound-path optimization layered on the endpoint, deferred with
  `UdpTransport`.
- **`ConnectivityGate.ts`** (per-fiber partition gating) — belongs with the k8s
  cluster reliability harness, not the base fabric.
- **`SignalingNetwork.realTracing.ts`** — folded away: in this port recording is
  a decorator (`RecordingSignalingNetwork`), not an on/off boolean on the real
  impl. The `NetworkTraceEntry` / `drainTrace` legacy buffer is superseded by
  the typed `Recorder` channel; the `toSipWire`/`toNetworkTrace` projectors will
  be ported when the report-renderer slice needs them.
- **`SignalingNetworkCore`** (second-fabric Tag) — only the proxy's dual-bind
  needs it; revisit at the proxy slice.
- **`recordSync` + `recordStreamLifecycle`** helpers — no sync-pure method and
  no `Stream` on the network surface (ADR-0005), so only `record_call` +
  `ReleaseGuard` were needed. Port the other two when a layer with those shapes
  arrives.
- **`propertyTest` / `parity` wrappers for `SignalingNetwork`** — skipped, same
  justification as the source (no per-call input domain; real vs simulated are
  not output-equivalent). See ADR-0005.
- **`ScopedAuditOptions.exceptions`** (per-test RFC-rule downgrade ledger) — the
  RFC rule packs it gates are a rules-slice concern; the `should_audit_bind`
  escape valve **is** ported.
- **`reuse_port`** wiring — accepted on `BindUdpOpts` but a no-op pending a
  `socket2` detour (tokio exposes no direct knob). Loopback tests don't need it.

---

## Slice 3 — Scenario harness + report renderers (`crates/scenario-harness`)

**Source release:** sipjsserver @ submodule
`c74e62da7c9bd8e04e183a8b074cad8029daa946`.
Source: `src/test-harness/framework/{dsl,interpreter,recorder,message-builder,
svg-sequence-diagram,text-report,html-report}.ts`, `tests/harness/runner.ts`.
**Scope (confirmed with user):** migrate the harness + **one** basic e2e test
that just exercises the harness; lean on the recording layer so pseudo-agents
record what they send/receive and the reports are generated from the record.

### Decisions (see [ADR-0006](docs/adr/0006-scenario-harness-recording-first.md) + [plan §4](docs/MIGRATION_PLAN_B2B.md))
- **Verified: do *not* replicate the fluent DSL + interpreter closely.** Its
  dialog-state engine is the unported transaction/call layers and there is no
  SUT to drive against. Confirms plan §4(ii) **B** (slim, scenarios-as-data).
- **Recording-first — the record is the trace.** Agents send/recv over the
  recording-wrapped simulated `SignalingNetwork`; reports are projected from the
  recorded `SignalingNetworkEvent`s + the lane registry, not from interpreter
  state. The driver keeps no trace and no dialog state.
- **Wire projection lives in `sip-net`** (`report::to_sip_entries`,
  `RecordedSipEntry`) — its documented home; byte-level, no SIP parser.
- **Deterministic report timestamps** via the clock seam: `layer-harness`
  `Recorder` stamps `at_ms` through an injected `sip-clock` `Clock`
  (`Recorder::with_clock`); harness anchors `Clock::test_at(0)`. Under a paused
  runtime the relative-time labels advance with `tokio::time::advance`.

### Source → Rust
| Source | Rust | Status |
|---|---|---|
| `dsl.ts` fluent builder + `recorder.ts` `Step[]` | `dsl.rs` — `Scenario`/`agent`/`Send`/`Expect`/`Advance` (scenarios-as-data) | ✅ thin |
| `interpreter.ts` two-phase engine + `trace` | `run.rs` — bind on recording sim net, replay steps, `RunReport` | ✅ recording-first |
| `report-recorder` → `RecordedSipEntry`/`toSipWire` | `sip-net::report::to_sip_entries` | ✅ |
| `svg-sequence-diagram.ts` (`wireText`, `renderSequenceDiagram`) | `report/{wire,svg}.rs` | ✅ trimmed |
| `text-report.ts` (global + per-endpoint) | `report/text.rs` | ✅ |
| `html-report.ts` | `report/html.rs` (static `<details>`, no JS) | ✅ minimal |
| `tests/harness/runner.ts` 100 ms-chunk advance | `Advance` step → `sip-clock::testkit::advance_in_100ms_chunks` | ✅ |

### Tests
| Area | Rust home | Status |
|---|---|---|
| alice INVITE → bob 200 OK e2e: expects pass, recording projects 2 paired entries, SVG/global.txt/per-endpoint/HTML rendered | `tests/alice_calls_bob.rs` | ✅ 1 |

### Un-ported with justification
- **Fluent dialog builder** (`invite`/`ack`/`bye`, CSeq/route-set/tag/offer-
  answer tracking in `message-builder.ts`) — *this is the transaction +
  call-context layers* (slices 4–6), not ported yet, and there is no SUT to
  drive against. When those land the DSL grows step-emitting constructors over
  the same recording-first driver.
- **`or`-branching / `parallel`** — racing-response and concurrent-fiber
  composition; no consumer yet (single linear flows only). Plan §4(ii) keeps
  them on the "useful ideas" list for later.
- **Media (RTP) steps** (`plays`/`hears`, ADR-0017) — media layer not ported.
- **Infra / k8s chaos steps** (`kill`/`partition`/`expectReplicatedTo`) — HA /
  failover slice; the replication fabric is not ported.
- **SUT / tier machinery** (`runOn`, `tier`, `withSut`, final 24h sweep +
  `verifyCleanState`) — **dropped permanently** for the agent-to-agent harness:
  there is no SUT topology to select, and cleanup assertions move to call-shape
  rules (mirrors the source's own drive-only runner, which sets
  `skipFinalSweep`).
- **RFC validation checks** (`validation.ts`) — the drive-only runner disables
  inline validation and defers to post-hoc rules; the rule engine is a later
  slice, so no validation is wired into the harness yet.
