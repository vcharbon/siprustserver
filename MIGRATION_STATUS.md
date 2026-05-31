# Migration status

Port of [sipjsserver](./portsource/sipjsserver) → Rust. One **layer** per row;
each maps to a workspace **crate**. Status legend:

- ✅ **ported** — code + tests ported and passing
- 🟡 **scaffolded** — directories, manifests, trait/types in place; bodies are `todo!()`
- ⬜ **pending** — not started

| Layer | Crate | Source | Status |
|---|---|---|---|
| **Clock** (test-time / timestamp seam) | `crates/sip-clock` *(slice 0)* | Effect `Clock`/`TestClock` (`tests/harness/runner.ts`) | ✅ monotonic-anchored `Clock` (`system`/`test_at`/`now_ms`) + `testkit` chunked-advance helper; 5 tests green. Behaviour stays on `tokio::time` directly — this is timestamps only ([plan §2](docs/MIGRATION_PLAN_B2B.md)) |
| **SIP message** (parse/serialize/validate) | `crates/sip-message` | `src/sip/` (message core) | ✅ parser + serializer + sdp + generators (incl. loose/strict in-dialog routing — `first_route_is_loose`/`strip_route_uri_to_request_uri`, RFC 3261 §16.12) + message-helpers/sipfrag ported; full test corpus green. rvoip oracle behind `rvoip-oracle`; ABNF corpus pending `abnfgen` |
| **Network / UDP** (transport, SignalingNetwork) | `crates/sip-net` *(slice 2)* | `src/sip/{UdpTransport,SignalingNetwork,BufferedUdpEndpoint}.ts` | ✅ `SignalingNetwork` ported — trait (DI seam) + real (tokio `UdpSocket`) + simulated (in-memory fabric) + recording/`scopedAudit` & `paranoidInputs` contract decorators; 22 tests green. `UdpTransport` facade + `BufferedUdpEndpoint` + `ConnectivityGate` **deferred** (Slice 2 §un-ported) |
| **Test/contract foundation** (Recorder, RunContext, 4 wrappers) | `crates/layer-harness` | `src/test-harness/framework/*` | ✅ Recorder + typed channels + projectors + RunContext/severity + recording helpers + 4-wrapper vocabulary ported (test-only, SIP-agnostic); 5 tests green. Recorder now stamps `at_ms` via an injected `sip-clock` `Clock` (deterministic report timestamps). [ADR-0004](docs/adr/0004-layer-harness-test-foundation.md) |
| **Scenario harness + reports** (DSL, driver, SVG/txt/HTML) | `crates/scenario-harness` | `src/test-harness/framework/{dsl,interpreter,recorder,message-builder,*-report,svg-sequence-diagram}.ts` | ✅ **fluent dialog-aware DSL** (`Harness`/`Agent`/`invite`/`receive`/`respond`/`ack`/`bye`) auto-generating correct-by-default B2B messages via `sip-message::generators` + tracked `StackDialog` state, **plus** the thin scenarios-as-data DSL as a raw escape hatch; recording-first driver; SVG (clickable in HTML)/global.txt/per-endpoint/HTML renderers; trace **projected from the recording** (`sip-net::to_sip_entries`); virtual-time `advance` + 100 ms fake-net transit delay; UAC/UAS route-set construction from Record-Route + a loose-routing `Proxy` test agent (for the LB/front-proxy slice); 4 e2e tests green (incl. `proxy_record_route`). `or`/`parallel`/media/chaos **deferred**. [ADR-0006](docs/adr/0006-scenario-harness-recording-first.md) |
| **Transaction** (RFC 3261 §17 FSMs + retransmit timers) | `crates/sip-txn` *(slice 4)* | `src/sip/TransactionLayer.ts` | ✅ client/server INVITE+non-INVITE FSMs, A/B/E/F/H/J timers, dedup, CANCEL→200+487, ACK absorption, cached-response retransmit, bounded drop-newest event queue; actor owns the map + a single `DelayQueue` ([ADR-0007](docs/adr/0007-transaction-layer-rust-shape.md)); 14 tests green. RNG seam (`IdGen`) ported here |
| **Dispatch / per-call FIFO** | `sip-router` / `call` slice | `src/sip/{SipRouter,PerCallDispatcher}.ts` | ⬜ pending — B2BUA-only (the txn layer is proxy+B2BUA shared); needs the call layer + rule engine ([ADR-0007 §X3](docs/adr/0007-transaction-layer-rust-shape.md)) |
| **CallContext data model** | `crates/call` | `src/call/` (`CallModel`/`timer-helpers`/`codec`) | ✅ data model + codec ported — Call→Leg→Dialog structs + lens/index helpers + `callRef`/index-keys + positional-msgpack `CallBodyCodec`; 24 tests green. Stateful `CallState` + `TimerService` + protobuf codec **deferred** ([slice below](#slice--callcontext-data-model-cratescall), [ADR-0008](docs/adr/0008-call-context-data-model.md)) |
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
| `html-report.ts` | `report/html.rs` (static `<details>` + click-to-reveal JS; arrows carry `data-trace-index`) | ✅ |
| `recorder.ts` `AgentProxy`/`DialogRef` + `message-builder.ts` dialog state | `agent.rs` — fluent `Harness`/`Agent`/`ClientInvite`/`Dialog`/`ServerTxn` over `sip-message::generators` + `StackDialog` | ✅ |
| `tests/harness/runner.ts` 100 ms-chunk advance | `Advance` step → `sip-clock::testkit::advance_in_100ms_chunks` | ✅ |

### Tests
| Area | Rust home | Status |
|---|---|---|
| fluent full dialog (INVITE/180/200/ACK/BYE/200) auto-generated; recording projects 6 entries; CSeq + To-tag + Call-ID asserted from the record; reports rendered | `tests/fluent_dialog.rs` | ✅ 1 |
| low-level hand-authored full dialog (raw-bytes escape hatch) + report assertions | `tests/alice_calls_bob.rs` | ✅ 1 |

### Un-ported with justification
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

---

## Slice 4 — Transaction layer (`crates/sip-txn`)

**Source release:** sipjsserver @ submodule commit
`fffc4ac69c8aeef26cf48fe73469503145c9732b`. Source: `src/sip/TransactionLayer.ts`.
**Scope (confirmed with user):** the RFC 3261 §17 transaction state machines +
retransmission timers only. `SipRouter` / `PerCallDispatcher` (the per-call FIFO
dispatch) are **deferred** — they are B2BUA-only and depend on the unported call
layer + rule engine, whereas the transaction layer is shared by the proxy and
the B2BUA ([ADR-0007 §X3](docs/adr/0007-transaction-layer-rust-shape.md)).

### Decisions (see [ADR-0007](docs/adr/0007-transaction-layer-rust-shape.md))
- **Timers (X1):** one `tokio_util::time::DelayQueue` driver holds every pending
  SIP timer keyed by branch — flat memory at 50K calls vs. ~100–150K timer
  tasks. It rides `tokio::time`, so `pause`/`advance` drives it in tests.
- **Txn map (X2):** a single **owner task** ("the actor") owns the map + the
  DelayQueue and is the only writer; the send API funnels commands over an mpsc
  (oneshot replies). No lock, no `DashMap` — the Rust expression of the source's
  single fiber, and the structural single-writer seam at this layer.
- **Scope (X3):** `TransactionLayer` only, own crate `sip-txn`.
- **RNG seam:** `newTag`/`newBranch` (deferred from slice 1) land here as
  `IdGen` — an injectable value, `seeded`/`from_entropy`, mirroring the clock seam.

### Source modules → Rust (port checklist)
| Source (`TransactionLayer.ts`) | Rust module | Status |
|---|---|---|
| `TransactionEvent` / `ClientTransactionHandle` / drop-reason | `event.rs` | ✅ |
| `TransactionLayerMetrics` (atomics-backed) | `metrics.rs` | ✅ (breakdown gauge deferred) |
| SIP timer constants (T1/T2/B/F/H/J, sweep) | `timers.rs` | ✅ |
| `newTag`/`newBranch` RNG seam | `rng.rs` (`IdGen`) | ✅ |
| ingest loop + handlers + outbound API + timers + sweep | `layer.rs` (the actor) | ✅ |

### Tests ported
| Source test | Rust home | Status |
|---|---|---|
| `transaction-layer-handles.test.ts` | `tests/handles.rs` | ✅ 2 |
| `transaction-layer-100-absorb.test.ts` (INVITE/BYE/OPTIONS CSeq) | `tests/absorb_100.rs` | ✅ 3 |
| `transaction-layer-bounded-queue.test.ts` | `tests/bounded_queue.rs` | ✅ 2 |
| `transaction-layer-cancel-on-evict.test.ts` (T1/T2/T3/T5) | `tests/cancel_on_evict.rs` | ✅ 4 |
| (RNG seam unit tests) | `src/rng.rs` | ✅ 3 |

### Tests authored (native — the source covers these only via full B2BUA
scenarios, which depend on the unported call + rules layers; far easier to pin
at the transaction-layer seam)
| Behaviour (RFC 3261 §17) | Rust home | Status |
|---|---|---|
| Timer A retransmit cadence (initial + @500 + @1500) | `tests/fsm.rs` | ✅ |
| Provisional 1xx stops retransmit | `tests/fsm.rs` | ✅ |
| Timer B emits a `Timeout` event + removes the txn | `tests/fsm.rs` | ✅ |
| CANCEL → 200 OK (CANCEL) + 487 (INVITE) + `Cancelled` event | `tests/fsm.rs` | ✅ |
| ACK for non-2xx absorbed (not surfaced); ACK for 2xx passes through | `tests/fsm.rs` | ✅ 2 |
| Client auto-ACK for a non-2xx final | `tests/fsm.rs` | ✅ |
| Duplicate request → cached-response retransmit (no re-surface) | `tests/fsm.rs` | ✅ |

### Un-ported with justification
- **`transaction-layer-cancel-on-evict` T4** (`CallState.remove` drives the
  cancel) — exercises the call layer's eviction path (`CallState`), a later
  slice. The transaction-layer behaviour it relies on (`cancel_txns_for_call`
  tears down a call's timers) is covered directly by T1/T2/T3/T5.
- **`transaction-layer-bounded-queue` "Queue.bounded contract" block** — an
  Effect-`Queue`-specific unit test; the Rust primitive is `tokio::mpsc` whose
  `try_send`-drop-on-full is std behaviour, exercised end-to-end by the overflow
  test. Not re-asserted in isolation.
- **Tier-3 overload admission gate** + `buildStatelessReject503Buffer` /
  `isEmergencyRequest` — depend on `OverloadController`/`AppConfig` (b2bua slice);
  this layer admits unconditionally for now ([ADR-0007 "Deferred"](docs/adr/0007-transaction-layer-rust-shape.md)).
- **`transactionBreakdown` gauge**, **OTel span re-parenting / `ForkSiteTracker`**,
  **legacy `send` wrapper** — see ADR-0007 "Deferred".
- **No `propertyTest` / `parity`** — the source `TransactionLayer` has neither
  (those wrap `SignalingNetwork`); the ritual's property/comparison step is N/A
  here.

---

## Slice — CallContext data model (`crates/call`)

**Source release:** sipjsserver @ `fffc4ac69c8aeef26cf48fe73469503145c9732b`.
Source: `src/call/{CallModel.ts, timer-helpers.ts, codec/}` (the **pure** half of
`src/call/`; `CallLimiter*.ts` is a separate layer, `CallState`/`TimerService`
are deferred — see below).
**Scope (confirmed with user):** the data model + body codec only (round-trip,
**no parity**). The stateful `CallState` + `TimerService` are a later slice
(their Redis/cache/`AppConfig`/`CdrWriter`/metrics/limiter/dispatcher deps are
unported).

### Decisions (see [ADR-0008](docs/adr/0008-call-context-data-model.md))
- Pure synchronous **leaf** crate (serde stack only — no `sip-message`/`sip-net`/
  `tokio`). Data model + lens helpers + `callRef`/index-keys + codec trait/impl.
- Single **positional msgpack** `CallBodyCodec` (`rmp-serde` default array
  encoding): ~42 % smaller + ~1.75× faster to encode than a named/self-describing
  layout (measured 2445 B vs 4241 B on the representative fixture), at the cost of
  strict field-order stability — fine under redeploy-from-scratch. Captures the
  source's msgpackr "records" win without a shared-structure registry.
- `Schema.optional`/`NullOr` → `Option<T>`; the one load-bearing three-way
  absent/null/value field (`policyUpdateBody`) is preserved as
  `Option<PolicyUpdateBody>`. Opaque `ext` → `BTreeMap<String, serde_json::Value>`
  (BTree for deterministic encode). Byte fields via `serde_bytes`; the INVITE
  handle stores raw bytes (no `sip-message` dep). RNG CSeq seam deferred (dialog
  constructors take the initial CSeq as a parameter).

### Source → Rust (port checklist)
| Source | Rust module | Status |
|---|---|---|
| `CallModel.ts` (Call→Leg→Dialog + all schemas/enums) | `model.rs` | ✅ |
| `decision/schemas/features.ts` (`FeatureActivations`) | `features.rs` | ✅ embedded in `call` for now (ADR-0002) |
| `CallModel.ts` lens/accessor helpers + `timer-helpers.ts` | `helpers.rs` | ✅ all helpers + `replaceTimerById`/`TERMINATING_TIMEOUT_MS` |
| `CallModel.ts` `deriveCallRef`/`parseCallRef`/`callIndexKeys`/`callIndexKeysFromUnknown` | `callref.rs` | ✅ (`fromUnknown` walks the serde-JSON projection — see ADR-0008) |
| `codec/CallBodyCodec.ts` (tag/interface) + `codec/msgpack.ts` + `CallCodec.ts` | `codec.rs` (`CallBodyCodec` trait + `MsgpackCodec` + `CallDecodeError`) | ✅ positional msgpack |

### Tests ported
| Area | Rust home | Status |
|---|---|---|
| `round-trip-property.test.ts` codec properties (P1/P2/P3/P5/P6/P7/P8/P14) | `tests/codec_roundtrip.rs` | ✅ 4 proptest + 4 unit (representative round-trip, empty-decode error, P7 size sweep, `policyUpdateBody` 3-state) |
| (pure helper smoke — no direct TS counterpart) | `tests/model_helpers.rs` | ✅ 11 (lens/CSeq/pending/tag/peering/resolution/ext/dialog-ctor/timer) |
| `callRef` derive/parse + index-key parity | `tests/callref.rs` | ✅ 5 |
| representative `Call` fixture + proptest `Strategy` (port of `tests/bench/call-codec/fixture*.ts`) | `tests/common/mod.rs` | ✅ |

### Un-ported with justification
- **`CallState.ts`** — stateful (per-call semaphores, in-memory call/sip indexes,
  Redis persistence via `PartitionedRelayStorage`/`BufferedTerminateWriter`,
  orphan sweep, HA topology, flush dedup). Depends on cache, `AppConfig`,
  `CdrWriter`, `MetricsRegistry`, `CallLimiter`, `PerCallDispatcher` — all
  unported. Later slice.
- **`TimerService.ts`** — live fiber scheduling + per-handler safety timeout +
  metrics. This slice carries only the *serializable* `TimerEntry`; when ported,
  firing rides `sip-txn`'s single `DelayQueue` ([ADR-0007](docs/adr/0007-transaction-layer-rust-shape.md)),
  not a new wheel. Later slice.
- **Protobuf codec + `call.proto`** — needs a prost/`build.rs` toolchain + field-
  mapping shims (`_topology`/`featuresJson`/`extJson`/`*IsNull` flags). The
  `CallBodyCodec` trait keeps the slot; deferred.
- **Codec contract decorator wrappers** (`paranoidInputs`/`parity`/`scopedAudit`
  + Recorder typed-channel recording, `codec/contracts.ts`) — **parity** dropped
  by the user for this slice; **paranoidInputs** collapses into the type system +
  `decode`'s `Result` (PA1/PA5 compile-time/range-trivial; PA2/PA4 = the decode
  error path); **scopedAudit** size-budget/alias-check deferred. Property checks
  ported as a plain proptest suite instead — same call the `sip-net` slice made
  for its `propertyTest`/`parity` decorators ([ADR-0005](docs/adr/0005-network-layer-rust-shape.md)).
- **P10/P11/P13** codec properties — hold by construction in Rust (`encode(&Call)`
  cannot mutate its input; `decode` returns the typed `Call`, so "schema
  conformance" is the type). Noted, not separately tested.
- **`CallState`/`TimerService`/limiter test files** (`callstate-arms-safety…`,
  `TimerService-*`, `limiter-*`, `forcepurge-*`, `codec/typed-channel.test.ts`)
  — defer with their subjects.
- **`randomInitialCSeq` RNG seam** — kept out of the pure model; the dialog
  constructors take the initial CSeq as a parameter. Plumbed from `sip-txn`'s
  `IdGen` when `CallState` lands (mirrors the message slice's deferred RNG
  identifier generators).
