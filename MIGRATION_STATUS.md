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
| **Dispatch / per-call FIFO** | `crates/b2bua` | `src/sip/{SipRouter,PerCallDispatcher}.ts`, `src/call/CallState.ts` | ✅ per-call queue+worker+global-semaphore dispatcher + router (`routeKey`/`withCall`/`processResult` typed-effect interpreter) + in-memory `CallState` over a replication-aware `CallStore` seam (HA params no-op'd) + B2BUA-local timer `DelayQueue` + decision-engine adapter seam (scripted jssip-emulating test impl) + buffered CDR; alice↔b2bua↔bob basic/failure/reject e2e green ([ADR-0010](docs/adr/0010-b2bua-dispatch-rules-rust-shape.md), [slice below](#slice--b2bua-dispatch--rule-engine-cratesb2bua)) |
| **CallContext data model** | `crates/call` | `src/call/` (`CallModel`/`timer-helpers`/`codec`) | ✅ data model + codec ported — Call→Leg→Dialog structs + lens/index helpers + `callRef`/index-keys + positional-msgpack `CallBodyCodec`; 24 tests green. Stateful `CallState` now ported in `crates/b2bua` (in-memory; Redis/HA-replication transport still deferred). `TimerService` ported as the B2BUA-local `DelayQueue`. protobuf codec **deferred** ([slice below](#slice--callcontext-data-model-cratescall), [ADR-0008](docs/adr/0008-call-context-data-model.md)) |
| **Rule engine** | `crates/b2bua` (`rules/`) | `src/b2bua/rules/` | ✅ first-match/layer-ranked engine (declarative `Match` + `ActionExecutor` + `InvariantEnforcer` + bye-disposition net) + the basic-B2BUA default rule set (relay/dialog/absorb/lifecycle/terminating/corner-case/failure/timer) + the first **SERVICE_LAYER** rule `relayFirst18xTo180` (`drop-sdp`/suppress + `fake-prack`: bare-180 downgrade, 18x suppression, B2BUA-originated PRACK, per-dialog SDP cache + cached-SDP-at-200 injection, To-tag continuity, UPDATE skeleton-fit answer/488, delayed-offer self-disable) + the **SERVICE_LAYER** `promote18xPemTo200` early-media service (`promote-pem-to-200`: 183+SDP+PEM → synthetic 200, promotion-window gating, SDP-diff resync re-INVITE toward A, upstream-fork re-seed, diagnostic-Reason teardown) + the **SERVICE_LAYER** `referTransfer` blind-transfer service (Slice 5a, source SHA `fffc4ac6`): REFER seed rules (`transfer-intercept-refer` 202 + seed + sub-expiry/overall timers + NOTIFY 100 + async `/call/refer`; `transfer-reject-replaces` → 501 with `overrides`; `transfer-reject-a-leg-refer` → 501), the `refer-authorizing → c-ringing → c-realigning` portion (`transfer-http-allow` held-SDP `create-leg`, `transfer-http-reject`/`-timeout`, `transfer-reject-second-refer` → 491, `transfer-c-1xx-to-notify` + dedup, `transfer-c-200-initial`, `transfer-c-fail-initial`, `transfer-c-no-answer`), the typed `Call.transfer` slice + `call::helpers`, `SendNotify`/`ReferAsyncHttp`/`SetTransfer` actions + `CreateLeg` body/header overrides, the `fire_and_forget` interpreter (`ReferAsyncHttp` → `call_refer` → re-enter via a re-entry channel), and the scripted `call_refer` (`X-Api-Call`). Scenario `refer-allow.ts` (5/5) ported. **Remaining (5b–5f, deferred):** c-realign/a-realign/merge rules, glare/gating (491/481), reject/timer corpora (`refer-c-realign`/`refer-full-transfer`/`refer-gating`/`refer-reject`/`refer-timers`) ([ADR-0010](docs/adr/0010-b2bua-dispatch-rules-rust-shape.md)) |
| **Draining / readiness / overload** (health-check) | `crates/b2bua` | `src/b2bua/{DrainingState,OverloadController}.ts`, `WorkerReadiness` | ⬜ pending — out-of-dialog OPTIONS answers a plain 200; the serving/draining/ready 200/503/silence matrix + Tier-3 admission gate are deferred ([ADR-0010 §X8](docs/adr/0010-b2bua-dispatch-rules-rust-shape.md)) |
| **Call limiter** | `limiter` | `src/call/CallLimiter*.ts` | ⬜ pending — a no-op `CallLimiter` seam ships in `crates/b2bua` (always admit); the real sliding-window limiter is its own slice |
| **Front proxy + LB** (stateless RFC 3261 §16) | `crates/sip-proxy` *(slice 9)* | `src/sip-front-proxy/{ProxyCore,RoutingStrategy,CancelBranchLru,strategies,registry,health,observability}` | ✅ stateless proxy data path + HRW load balancer (signed Record-Route cookie + routing matrix) + worker registry (static/simulated) + OPTIONS health probe + metrics (counters + Prometheus HTTP); scenario-harness gains a **SUT seam** (`bind_sut`) running a real `ProxyCore` on the recording fabric. 62 tests green. Registrar/REGISTER, real self-gate, AIMD bucket, k8s registry **deferred** ([slice below](#slice-9--front-proxy--load-balancer-cratessip-proxy), [ADR-0009](docs/adr/0009-front-proxy-rust-shape.md)) |
| **HA replication topology** (port-agnostic membership seam) | `crates/topology` *(slice S1a)* | `src/sip-front-proxy/registry` (membership subset) | ✅ `Membership` read seam (`snapshot`/`changes`) + `Peer`/`MemberDelta` + static/simulated impls; port-agnostic, no health (proxy-layer concern). Lock-free `ArcSwap` snapshot + broadcast deltas. Shared by proxy LB + b2bua repl |
| **HA replication wire + transport** (frames/codec/fabric) | `crates/repl-net` *(slices S2–S3)* | `src/sip/replication/**` | ✅ five positional-msgpack `Frame`s + codec + len-prefix framing (S2) + `ReplicationNetwork` seam: simulated (ordered, faults: cut/stall/resume/partition/heal/delay/drop-on-overflow) + real TCP + `RecordingReplicationNetwork` (`CapturedFrame`) (S3). Codec plays through every path (ADR-0011 X2/X9, [ADR-0008](docs/adr/0008-call-context-data-model.md)) |
| **HA replication engine** (store/changelog/server/puller/supervisor) | `crates/b2bua` (`repl/`) *(slices S4–S8)* | `src/call/replication/**` | ✅ `ReplicatingCallStore` + per-peer compacted `Changelog` (lazy TTL, dead-peer auto-clean) + `ReplServer` serve-loop + `Puller` FSM (Replog tail + Bootstrap rehydrate + hard timer) + `ReplicationSupervisor` (topology-driven, retained-W per ordinal, sticky current/bootstrap flags) + `Readiness` + write-side policy (`flush_replicated`/`replication_target`, forward/reverse takeover). 58 tests green ([ADR-0011](docs/adr/0011-ha-replication-peer-to-peer.md)) |
| **HA cluster harness** (goal-1 pure-framework test harness) | `crates/ha-harness` *(slice S9)* | `tests/failover/**` chaos steps (`kill`/`partition`/`expectReplicatedTo`) | ✅ N in-process repl-subsystem nodes under a paused clock (no SIP/router/rules) — `HaCluster`/`HaNode` driving put/delete/crash/reboot/partition/heal/cut/delay/stall/drop-on-overflow + convergence asserts + a **focused recording-first** replication-exchange report (text sequence diagram + mermaid; the SIP renderers are too `RecordedSipEntry`-coupled to reuse). 8 goal-1 scenarios + a fixed-seed eventual-convergence property test (5 seeds); 14 tests green. No b2bua pub-widening needed. ([ADR-0011 X10](docs/adr/0011-ha-replication-peer-to-peer.md), recording-first per [ADR-0006](docs/adr/0006-scenario-harness-recording-first.md)) |
| **HA simulated failover** (goal-2) | `crates/b2bua-harness` *(slice S10)* | `tests/failover/**` (sim) | ✅ proxy + ≥2 `B2buaCore` SUTs over the SIP sim fabric **and** the repl sim fabric, fake clock: crash→failover→reboot→reclaim canonical + 4-case matrix + combined SIP+replication report. Replication is opt-in via `B2buaDeps.replication: Option<ReplicationSetup>` (`None` = legacy). Green. ([ADR-0011 X10](docs/adr/0011-ha-replication-peer-to-peer.md)) |
| **HA real chaos** (goal-3) | `crates/topology` (`k8s`, feat `kube`) + `crates/b2bua-runner` + `deploy/k8s` *(slice S11)* | k8s EndpointSlice + real TCP + kind | ✅ `K8sMembership` EndpointSlice informer (`reconcile_to_desired` pure delta-translation, unit-tested; `peers_from_slices` ready-endpoint→`Peer`) — the k8s watcher written **once** (X7). `b2bua-runner` wires `RealReplicationNetwork` + membership (static `B2BUA_PEERS` / k8s) from env, **boot-wallclock incarnation gen**, **port-agnostic `peer.host:REPL_PORT`** addressing, SIGTERM→`begin_draining()`, `/ready` probe reflecting `is_ready()`. Repl-aware manifests (repl port, EndpointSlice RBAC, headless svc, `/ready` readinessProbe, drain grace) + vendored kind `cluster.yaml`/sipp scenarios + `deploy/k8s/chaos.sh` (build→kind→deploy→hold-failover under pod-kill→assert call survival). Chaos suite is a shell script (not a `cargo test`) so the workspace stays fast; run explicitly on kind. ([ADR-0011](docs/adr/0011-ha-replication-peer-to-peer.md)) |

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

---

## Slice 9 — Front proxy + load balancer (`crates/sip-proxy`)

**Source release:** sipjsserver @ submodule `fffc4ac69c8aeef26cf48fe73469503145c9732b`.
Source: `src/sip-front-proxy/`.
**Scope (confirmed with user):** the **stateless** RFC 3261 §16 proxy data path
+ the load balancer + the worker registry (static + simulated) + OPTIONS health
probing toward the B2BUA + the metrics layer (counters **and** the Prometheus
HTTP server). **Excludes** the SIP registrar / REGISTER path. The proxy
self-overload gate is a **stub** (always-admit); worker overload is **band
classification only** (the AIMD rate-cap bucket is deferred).

### Decisions (see [ADR-0009](docs/adr/0009-front-proxy-rust-shape.md))
- X1 stateless proxy — reuses `sip-txn::IdGen` (Via branches) + `sip-clock::Clock`,
  **not** the txn FSMs; CANCEL/ACK correlation is a proxy-local `(Call-ID|CSeq#)` LRU.
- X2 scenario-harness gains a **SUT seam** (`Harness::bind_sut`) — a real
  `ProxyCore` runs on the same recording-wrapped `SimulatedSignalingNetwork` the
  agents use; the recording stays the trace (extends [ADR-0006](docs/adr/0006-scenario-harness-recording-first.md)).
- X3 cookie = HMAC-SHA256 truncated to 128 bits, base64url; HRW = SHA-1 as a
  non-crypto hash. X4 self-gate stubbed (overload = OPTIONS health/band +
  `sip-net` tail-drop). X5 lock-free registry (`ArcSwap`) + `broadcast` changes.
  X6 AIMD bucket deferred.

### Source → Rust (port checklist)
| Source | Rust module | Status |
|---|---|---|
| `RoutingStrategy.ts` (Tag + DecodeResult/errors) | `strategy.rs` | ✅ trait + `DecodeResult`/`SelectError`/`RouteParams`/`SelectOpts` |
| `strategies/RendezvousHash.ts` | `strategies/rendezvous.rs` | ✅ HRW (SHA-1 top-8-bytes × weight) |
| `strategies/ForwardAll.ts` | `strategies/forward_all.rs` | ✅ static target + `target=host:port` cookie |
| `strategies/LoadBalancer.ts` | `strategies/load_balancer.rs` | ✅ HRW select + band filter + v=3 signed cookie + routing matrix (band-only select; AIMD bucket deferred) |
| `WorkerLoadObserver.ts` | `load_observer.rs` | ✅ ELU-band classification + hysteresis + `X-Overload` parse (AIMD token bucket **deferred**) |
| `security/HmacKeyProvider.ts` (`static`) | `security/hmac.rs` | ✅ HMAC-SHA256 sign + truncated constant-time verify (k8s-secret fs-watch deferred) |
| `registry/WorkerRegistry.ts` | `registry/mod.rs` | ✅ trait + `WorkerEntry`/`WorkerHealth`/`RegistryEvent`, lock-free `ArcSwap` state |
| `registry/static.ts` | `registry/static_reg.rs` | ✅ `id@host:port,…` parser, all-alive, empty changes |
| `registry/simulated.ts` | `registry/simulated.rs` | ✅ add/remove/set_health/set_address + events + draining-since stamping |
| `health/WorkerRegistryControl.ts` | `registry/control.rs` | ✅ simulated adapter + noop |
| `CancelBranchLru.ts` | `cancel_lru.rs` | ✅ `(Call-ID\|CSeq#)→{target,branch}` TTL cache + sweep |
| `ProxyCore.ts` (`handleRequestImpl`/`handleResponseImpl`) | `core/{mod,request,response}.rs` | ✅ single-endpoint data path (Max-Forwards, ACK absorption, Route strip, worker-outbound, cookie decode, received/rport, Record-Route, Via push/pop, reverse-path failover, non-2xx ACK synthesis) |
| `health/HealthProbe.ts` (`optionsKeepalive`) | `health/probe.rs` | ✅ fan-out/drain/reap tick loop, 200/503-reason/timeout→health, X-Overload→observer |
| `ProxySelfGate.ts` | `self_gate.rs` | ✅ **always-admit stub** (real ELU/CPS gate deferred) |
| `observability/Metrics.ts` | `observability/metrics.rs` | ✅ atomics counters/gauges + Prometheus text |
| `observability/MetricsServer.ts` | `observability/metrics_server.rs` | ✅ hand-rolled tokio `/metrics` HTTP |
| `observability/Logger.ts` | `observability/logger.rs` | ✅ structured routing-decision log (trait + capturing/noop) |
| `RoutingStrategy.ts` `SocketAddr` | `addr.rs` | ✅ `ProxyAddr` policy type |
| (new) harness SUT seam | `scenario-harness::Harness::bind_sut` | ✅ binds a SUT endpoint on the shared recording fabric |

### Tests ported
| Source test | Rust home | Status |
|---|---|---|
| `load-balancer/{hmac-tampering,cookie-route-fallback,decode-forward-not-ready,decode-forward-respawn-window,unresolvable-id-falls-back,add-remove-resharding,initial-health,distribution,selectForNewDialog-overload(band part)}` | `tests/load_balancer.rs` | ✅ 11 |
| RendezvousHash distribution + HMAC sign/verify + header surgery + registry parse + cancel-LRU + observer bands + metrics/logger/metrics-server | `src/**` `#[cfg(test)]` | ✅ 46 |
| `transit-only/{invite-200-ack-bye, malformed-message-rejected, max-forwards(483)}` | `tests/transit_only.rs` (real `ProxyCore` SUT) | ✅ 3 |
| `load-balancer/callid-routing-guard` + `distribution` (wire) | `tests/load_balancer_routing.rs` (real SUT) | ✅ 1 |
| `integration/options-end-to-end` | `tests/options_e2e.rs` (probe ↔ simulated B2BUA responder) | ✅ 1 |

### Un-ported with justification
- **Registrar / REGISTER path** (`RegisterStrategy`, `Registrar`,
  `CoreToExtRoutingStrategy`, `RegistrarProxyConfig`, `handleRequestRegistrarMode`,
  dual-endpoint `;net=` egress) — user scope. The single-endpoint path drops the
  dual-fabric branching entirely. Tests: all `registrar/**`, `registrar-503-on-drop`.
- **`ProxySelfGate` real impl** (ELU EWMA + CPS bucket) — `self_gate.rs` is an
  always-admit stub; overload protection = OPTIONS-driven worker health/band +
  `sip-net` receive-buffer tail-drop. Test: `ingress-concurrency`.
- **AIMD per-worker rate-cap token bucket** + `selectForNewDialog-overload` bucket
  cases — deferred (X6); band classification + the CRITICAL filter are ported and
  tested.
- **Kubernetes registry** (`registry/kubernetes.ts`) + **k8s HMAC fs-watch** —
  production-only; static + simulated cover the slice.
- **Failover / replication / call-limiter** (`failover/**`, k8s `proxy-*`) — depend
  on the unported B2BUA call cache + replication; only the proxy *request-path*
  `forwardBackup`/reverse-path mechanics are ported (covered by `decode-forward-*`).
- **Real B2BUA OPTIONS handler** (`b2bua/{options-readiness-distinct,draining-options}`)
  — `options-e2e` is retargeted to a simulated responder; the readiness-disambiguation
  tests are deferred with the B2BUA.
- **`transparency/*` dual-mode + `health-probe-late-reply`** — the happy-call,
  routing, and health-state transitions they assert are covered by `transit_only`,
  `load_balancer_routing`, and `options_e2e`; the direct-vs-withProxy equivalence
  harness + the late-reply endurance race are deferred (not load-bearing for the
  proxy mechanics).
- **`forbidden-import` lint** — superseded by Cargo crate-dependency boundaries
  (`sip-proxy` simply does not depend on the call/rule crates).

---

## Slice — B2BUA dispatch + rule engine (`crates/b2bua`)

**Source release:** sipjsserver @ `fffc4ac69c8aeef26cf48fe73469503145c9732b`.
Source: `src/sip/{SipRouter,PerCallDispatcher}.ts`, `src/call/CallState.ts`,
`src/b2bua/rules/`, `src/decision/`, `src/cdr/CdrWriter.ts`.
**Scope (confirmed with user):** the per-call FIFO dispatcher + router, the
in-memory `CallState` over a replication-aware `CallStore` seam, the rule engine
+ the **basic-B2BUA** default rule set (ignore the 18x-management strategies),
the decision-engine adapter seam + a deterministic test backend that emulates
the jssip reference backend (so SIPp scripts reuse), a no-op call limiter, and a
buffered CDR layer wired through the recording. Tested e2e through the
scenario-harness in a dedicated `crates/b2bua-harness` test crate.

### Decisions (see [ADR-0010](docs/adr/0010-b2bua-dispatch-rules-rust-shape.md))
- One crate (mutual deps); per-call queue+worker+global-semaphore FIFO (not the
  txn actor); replication-aware `CallStore` (in-memory no-ops the HA params);
  decision adapter seam + scripted backend; first-match/layer-ranked rules +
  invariants; B2BUA-local timer `DelayQueue`; buffered CDR; OPTIONS-200 minimal;
  `b2bua-harness` SUT crate.

### Source → Rust (port checklist)
| Source | Rust module | Status |
|---|---|---|
| `PerCallDispatcher.ts` | `dispatch.rs` | ✅ per-call queue + worker + global semaphore + cap/queue/saturation counters |
| `SipRouter.ts` (`routeKey`/`withCall`/`processResult`) | `router.rs` | ✅ sync callRef resolution, per-call dispatch, typed-effect interpreter |
| `CallState.ts` (in-memory map/index/lock/flush/remove) | `store/mod.rs` (`CallState`) | ✅ in-memory (Redis/HA transport deferred) |
| `PartitionedRelayStorage` + `BufferedTerminateWriter` | `store/{call_store,memory,terminate_writer}.rs` | ✅ trait + in-memory + buffered writer (HA params carried, no-op'd) |
| `TimerService.ts` | `timers.rs` | ✅ one `DelayQueue` driver firing `CallEvent::Timer` |
| `rules/framework/*` | `rules/{model,matcher(executor),actions,relay,invariants}.rs` | ✅ basic-B2BUA subset |
| `rules/defaults/*` | `rules/defaults.rs` | ✅ relay/dialog/absorb/lifecycle/terminating/corner-case/failure/timer; incl. `reinvite-glare` + `relay-reinvite-response` (re-INVITE, `CornerCaseRules.ts`) |
| `decision/CallDecisionEngine.ts` + `schemas/*` | `decision/{mod,schemas}.rs` | ✅ trait + request/response shapes |
| `decision/apply/applyRoute.ts` + `InitialInviteHandler.ts` | `decision/apply_route.rs` + `initial_invite.rs` | ✅ |
| (HTTP backend) | `decision/test_adapter.rs` (`ScriptedDecisionEngine`) | ✅ jssip-emulating test impl |
| `cdr/CdrWriter.ts` (+ `BufferedCdrLayer`) | `cdr/{mod,memory,buffered}.rs` | ✅ |
| `CallLimiter*.ts` | `limiter.rs` (`NoopLimiter`) | 🟡 no-op seam (real limiter = row 20) |
| `b2bua/stack-identity.ts` | `stack_identity.rs` | ✅ Via/Contact `cr`/`lg` stamping + param codec |
| `b2bua/helpers.ts` b2bOutboundProxy + `ActionExecutor` `applyEgressRouting` | `rules/relay.rs` (`apply_b_leg_egress`) + `config.b2b_outbound_proxy` | ✅ b-leg INVITE/ACK/BYE preload `;outbound` Route at the proxy; wire dest = proxy, R-URI = callee (RFC 3261 §16.12). Empty-routeSet error-fallback + the `;outbound` source-IP hardening folded into the one helper |
| `B2buaCore.ts` (layer composition) | `b2bua_core.rs` | ✅ |

### Tests
| Area | Rust home | Status |
|---|---|---|
| per-call FIFO order + cap/queue drops | `dispatch.rs` | ✅ 2 |
| timer fire/cancel under paused clock | `timers.rs` | ✅ 2 |
| scripted decision route/reject | `decision/test_adapter.rs` | ✅ 2 |
| stack-identity param round-trip | `stack_identity.rs` | ✅ 2 |
| matcher ranking + invariant enforcement | `tests/rules.rs` | ✅ 3 |
| alice↔b2bua↔bob basic call (INVITE/180/200/ACK/BYE) + one CDR | `b2bua-harness/tests/basic_call.rs` | ✅ 1 |
| b-leg 486 relayed + terminate; decision reject 403 | `b2bua-harness/tests/failure.rs` | ✅ 2 |
| alice→proxy→b2bua→proxy→bob basic call (port of `basicCall` on the `proxy+b2b` SUT): real LB `ProxyCore` fronting one real `B2buaCore` worker behind `b2bOutboundProxy`; asserts INVITE **and** BYE make all four hops + one CDR | `b2bua-harness/tests/proxy_b2bua.rs` (+ `tests/common`) | ✅ 1 |
| **goal-2 simulated failover** (slice S10b): the `FailoverHarness` ties the scenario-harness SIP plane + a recording-wrapped SIM repl fabric + a real LB `ProxyCore` SUT + 2 replicating `B2buaCore` workers (`ReplicatedB2buaSut`: `crash`/`reboot`/`is_ready`/repl-store introspection) + alice/bob under ONE fake clock. Canonical 5-step failover (establish→replicate→crash B1→in-dialog fails over to acting-backup B2→reboot B1 EMPTY higher-gen→re-hydrate→next msg back on B1) + 4 matrix faults (crash mid-INVITE, crash during re-hydration, partition during failover, double-fault) + a combined SIP+replication recording report (reuses the S9 `ha-harness` renderer). Wired the live flush-on-mutation + acting-backup hydrate-from-replica + the in-dialog `callRef` URI-param case fix in `b2bua` (router/store). 6 tests, stable 6×. ([ADR-0011 X10](docs/adr/0011-ha-replication-peer-to-peer.md)) | `b2bua-harness/tests/failover.rs` (+ `src/failover.rs`) | ✅ 6 |

#### SIP-behaviour scenarios (ports of `tests/scenarios/*`)
| Source scenario | Behaviour exercised | Rust home | Status |
|---|---|---|---|
| `prack.ts` | end-to-end reliable provisional: 183(100rel,RSeq) → PRACK(RAck) → 200(PRACK) → 200(INVITE). The B2BUA relays it as a back-to-back UA with **per-dialog CSeq** (`relayCSeqDelta`), **RAck CSeq rewrite** (RFC 3262 §7.2), b-leg early-dialog capture, and pending-request response correlation (`generate_relayed_response`) | `b2bua-harness/tests/prack.rs` | ✅ 1 |
| `prack-forking.ts` | delayed-offer forking: two reliable 183s with distinct callee fork-tags → two independent early dialogs on one b-leg, each mapped to its own a-facing tag; per-fork PRACK routed by To-tag through the tag map | `b2bua-harness/tests/prack_forking.rs` | ✅ 1 |
| `keepalive-happy.ts` | long call: keepalive timer sends in-dialog OPTIONS to both legs every interval, each 200 absorbed + timer re-armed; two cycles | `b2bua-harness/tests/keepalive.rs` | ✅ 1 |
| `options-keepalive-timeout.ts` | auto-cutoff: a leg that never answers its keepalive OPTIONS trips the per-leg `KeepaliveTimeout`, which terminates that leg and BYEs the healthy peer | `b2bua-harness/tests/keepalive_timeout.rs` | ✅ 1 |
| `keepalive-481.ts` | keepalive where bob answers the OPTIONS with `481 Call/Transaction Does Not Exist` → `handle-481` marks bob's leg dead (`ByeTimeout` disposition, BYE suppressed), records the 481 CDR, and `begin-termination` BYEs only the healthy peer (alice). Asserts no BYE reaches bob on the recording | `b2bua-harness/tests/keepalive_481.rs` | ✅ 1 |
| `keepalive-via-proxy.ts` (`keepaliveViaProxy`) | keepalive OPTIONS through the `proxy+b2b` SUT: both legs' in-dialog OPTIONS traverse the front proxy (a-leg via the inbound INVITE's Record-Route route set; b-leg via the `b2bOutboundProxy` `;outbound` preload), each arriving with ≥2 Via; two cycles confirm re-arm, then BYE via the proxy | `b2bua-harness/tests/keepalive_via_proxy.rs` | ✅ 1 |
| `reinvite.ts` (`aliceReInvite`) | in-dialog re-INVITE from the caller, delayed offer: bodyless re-INVITE relayed to bob → 200 carries bob's offer → ACK carries alice's answer (relayed end-to-end). Exercises `relay-reinvite` + `relay-reinvite-response` (pending-relay snapshot correlation, incl. re-INVITE now snapshotted) + ACK body passthrough | `b2bua-harness/tests/reinvite.rs` | ✅ 1 |
| `reinvite.ts` (`bobReInvite`) | in-dialog re-INVITE from the callee, offer in the re-INVITE: bob's re-INVITE(offer) relayed to alice → 200(answer) → ACK. Confirms a-leg-target ACK relay + response correlation in the from-a direction | `b2bua-harness/tests/reinvite.rs` | ✅ 1 |
| `reinvite.ts` (`crossingReInvite`) | crossing re-INVITEs → glare: alice's re-INVITE is relayed to bob; bob's crossing re-INVITE meets a pending inbound INVITE on his dialog → `reinvite-glare` rejects it 491 Request Pending while alice's completes (200/ACK) | `b2bua-harness/tests/reinvite.rs` | ✅ 1 |
| `suppress-18x.ts` (`basic`) | `relayFirst18xTo180` strategy `drop-sdp` (wire `true`): first reliable 183 → **bare 180** (no SDP/Require/RSeq), B2BUA-originated PRACK to bob, subsequent 18x suppressed, 200 OK To-tag == first 180's tag; `Supported:100rel` stripped from bob's INVITE | `b2bua-harness/tests/suppress_18x.rs` | ✅ 1 |
| `suppress-18x.ts` (`disabled`) | no policy → normal 180 relay (regression guard the SERVICE_LAYER path stays off the default flow) | `b2bua-harness/tests/suppress_18x.rs` | ✅ 1 |
| `suppress-18x.ts` (`failoverNoAnswer`, `failoverReject`) | failover-shaped tag-continuity across a second b-leg | — | ⛔ blocked — see "Un-ported" (no SIP b-leg failover yet) |
| `fake-prack.ts` (`basic`) | strategy `fake-prack`: bob reliable 183(100rel,SDP) → bare 180 to alice, B2BUA PRACKs bob + **caches** his SDP, bob's bodyless 200 → alice's 200 carries the cached SDP; `Supported:100rel` **kept** to bob | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`multiple-18x`) | two reliable 18x, one PRACK each, latest cached SDP wins on the 200 | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`update-happy`) | bob UPDATE(offer) → B2BUA local 200 with skeleton-fit answer (codec ∩ alice's INVITE) + cache advances | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`update-codec-mismatch`) | bob UPDATE(opus-only) → no codec overlap → B2BUA local 488; call continues on the original cached SDP | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`delayed-offer-fallback`) | alice INVITE has no SDP → outbound INVITE strips `Supported:100rel` and the policy self-disables (plain relay) | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`no-policy-control`) | no policy → full end-to-end PRACK (183/Require:100rel relayed verbatim, alice PRACKs end-to-end) | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`forking`, `failover`) | failover-on-503 to a second b-leg (loser cache discarded) | — | ⛔ blocked — see "Un-ported" (no SIP b-leg failover yet) |

#### Slice 4 — `promote18xPemTo200` (early-media 183→synthetic 200)

**Source release:** sipjsserver @ `fffc4ac69c8aeef26cf48fe73469503145c9732b`.
Source rule: `src/b2bua/rules/custom/promote18xPemTo200.ts` + shared
`_shared/sdpDiff.ts`. Rust: `crates/b2bua/src/rules/promote_pem.rs` +
`rules/sdp_diff.rs`; per-call state `Call.promote_pem` (`PromotePemState`) +
`call::helpers`; new RuleActions `SendReinvite`/`SetPromotePem`; `AckLeg`
extended to the a-leg; `BeginTermination` now emits a RFC 3326 `Reason:` header
on teardown BYEs when the firing rule supplies a `SIP;cause=…` value;
`MessageTransform.add_headers` (Allow/Supported stamping).

| TS scenario (`promote-pem-to-200.ts`) | what it asserts | Rust test | Status |
|---|---|---|---|
| `promotePemHappyNoResync` | 183+SDP+PEM → synthetic **200** to alice (SDP verbatim, PEM stripped, Allow + Supported-no-100rel); A ACK absorbed; B 200(same SDP) → silent bridge, no resync | `promote_pem.rs::promote_pem_happy_no_resync` | ✅ 1 |
| `promotePemResyncSdpChanged` | B 200(diff SDP) → B2BUA **re-INVITEs A** with the new SDP; A 200 → B2BUA ACK closes window; A's INFO then relays to B | `promote_pem.rs::resync_sdp_changed` | ✅ 1 |
| `promotePemBFailsPostPromote` | B 503 post-promote → **BYE A** with `Reason: SIP;cause=503` | `promote_pem.rs::b_fails_post_promote` | ✅ 1 |
| `promotePemResyncFailedByA` | A 488s the resync re-INVITE → **BYE both** legs with `Reason: …cause=488` | `promote_pem.rs::resync_failed_by_a` | ✅ 1 |
| `promotePemABYEDuringWindow` | A BYEs during the window → A's BYE 200 + **CANCEL B**'s open INVITE | `promote_pem.rs::a_bye_during_window` | ✅ 1 |
| `promotePemForkingResync` | upstream fork: 183(PEM,tag t1) promoted, 200(diff SDP,tag t2) wins → local ACK carries **t2**, resync re-INVITE to A, A-BYE routes via t2 | `promote_pem.rs::forking_resync` | ✅ 1 |
| `promotePemInDialogRejection` | window open → A UPDATE→**491**, INFO→**488**; then B 200(same SDP) closes window | `promote_pem.rs::in_dialog_rejection` | ✅ 1 |
| `promotePemNoPolicyControl` | policy OFF → A sees a **183** (not 200), SDP body survives (regression guard) | `promote_pem.rs::no_policy_control` | ✅ 1 |

All 7 TS cases (+ the no-policy guard) ported; **none required SIP b-leg
failover** (the forking case is upstream forking on one b-leg, not B2BUA
failover), so the Slice 3 failover blocker does not apply. One minor fidelity
note recorded in the test: the no-policy 183 loses its `P-Early-Media` header
because the Rust CORE relay passthrough set is Require/RSeq/Supported only — a
CORE relay-passthrough gap, independent of the PEM service.

**Behaviour ported alongside these tests** (was dormant before this slice): the
back-to-back-UA in-dialog relay now does faithful per-dialog CSeq bookkeeping
(`relay_cseq_delta` + `bump_local_cseq`/`update_remote_cseq`), the PRACK `RAck`
rewrite, reliable-provisional header passthrough (`Require`/`Supported`/`RSeq`),
b-leg early-dialog establishment from a 1xx, **multi-early-dialog forking** with
per-fork a-facing tag mapping + tag-map-routed in-dialog requests, and
non-INVITE response correlation via the pending-request snapshot. Harness gained
`Respond::with_header`/`with_to_tag` + `Dialog/ClientInvite::send_request` (RAck,
custom headers, per-fork To-tag).

> **Timer-driver fix (load-bearing):** the B2BUA `TimerService` left a fired
> timer's id in its `keys` map; `DelayQueue` recycles the freed slab slot, so a
> later `Cancel`/re-`Schedule` of that id aliased and evicted the *wrong* live
> timer (a keepalive-timeout cancel was killing the rescheduled keepalive,
> breaking the second keepalive cycle). Fired timers now prune their `keys`/
> `by_call` bookkeeping on expiry (`timers.rs`).

### Un-ported with justification
- **`relayFirst18xTo180` SERVICE_LAYER rule** — **now ported** (Slice 3) for the
  `drop-sdp` (suppress, wire `true`) and `fake-prack` strategies: bare-180
  downgrade, 18x suppression, B2BUA-originated PRACK, per-dialog SDP cache +
  cached-SDP-at-200 injection, To-tag continuity, UPDATE skeleton-fit answer/488,
  delayed-offer self-disable. `crates/b2bua/src/rules/relay_first_18x.rs` +
  `sdp_answer.rs`. The `keep-sdp` strategy variant is carried in the enum but has
  no dedicated scenario (none exists in the TS corpus). `promote-pem-to-200` is
  owned by the PEM service (Slice 4) — the enum variant is wired through but the
  rule is not implemented here.
- **The 4 failover-shaped 18x scenarios** (`suppress-18x` `failoverNoAnswer` /
  `failoverReject`; `fake-prack` `forking` / `failover`) — **BLOCKED on SIP
  b-leg failover**, which does not exist in the Rust port: `route-failure` /
  `no-answer` are pure CORE no-failover (relay + `TerminateCall` /
  begin-termination), and `CallDecisionEngine::call_failure`
  (`/call/failure` → `Failover(RouteDecision)`) is defined but never invoked
  (`grep '\.call_failure(' crates/` → 0). These four need a `/call/failure`
  failover service first (the same deferral Slice 0 recorded for
  `route-failure`/`no-answer`); they are not a `relayFirst18xTo180` gap. Tag
  continuity *across failover* (the property they assert) is implemented and
  unit-covered by the non-failover `basic` case's 200-To-tag==180-To-tag check.
  `crates/b2bua-harness/tests/failover.rs` is HA worker-crash failover (a
  different mechanism), not this.
- **`promote18xPemTo200` (PEM)** — **now ported** (Slice 4): synthetic-200
  promotion of the first 183+SDP+PEM, promotion-window gating (A UPDATE/INVITE→
  491, INFO→488), silent confirm on B's real 200, SDP-diff resync re-INVITE
  toward A, upstream-fork re-seed onto the winning To-tag, B-fails-post-promote
  and resync-failure teardown BYEs carrying RFC 3326 `Reason`, A-BYE-during-
  window CANCEL of B. `crates/b2bua/src/rules/promote_pem.rs` + `sdp_diff.rs`.
- **REFER transfer** (`referTransfer`, `TransferRules`, `/call/refer`) —
  SERVICE_LAYER policy module; Slice 5.
- **`prack-forking.ts` per-fork CSeq-independence assertion** — the source
  framework validates that each forked early dialog advances its own CSeq from
  the shared INVITE baseline (caller side). The slim Rust harness keeps one CSeq
  counter per `ClientInvite` and does not assert this; the B2BUA recomputes the
  *outbound* (callee-side) CSeq per dialog regardless (`relay_cseq_delta`), which
  `prack_forking.rs` does exercise. The loser-fork cancellation + winner-SDP
  caching shape is the `fake-prack` policy's, deferred above.
- **Real HTTP decision adapter** — the scripted backend stands in.
- **Failover via `call_failure`** — route-failure relays the failure + tears the
  call down; the async failover round-trip + `refer-async-http` re-entrant
  fire-and-forget land with the decision-HTTP/transfer slice.
- **Real CallLimiter** (row 20) — no-op admit/decrement.
- **HA replication transport + orphan sweep / `loadOwnedCalls` rehydrate** — the
  `CallStore` seam carries the HA params; the replicating impl is the HA slice.
- **Draining / readiness / overload** — minimal OPTIONS 200 only (own row above).
- **Tracing / OTel span machinery** — no tokio analogue this slice.
- **`proxy+b2b` variants beyond the single-worker basic call** — the `sipproxyHA`
  two-worker topology (`tests/support/proxyB2bFakeStack.ts`
  `sipproxyHAFakeStackLayer`), the bob-initiated-BYE direction (cookie-decoded
  reverse path), and `route-set-propagation` land with the HA slice. The
  `keepalive-via-proxy` happy path is **now ported** (`keepalive_via_proxy.rs`).
  The `keepaliveMissingOutboundProxyRegressionGuard` (built with
  `simulateMissingOutboundProxy`, asserts the b-leg OPTIONS goes worker-direct
  with exactly one Via) is **still deferred** — the Rust `B2buaSut` has no
  "missing outbound proxy" build variant, and the bug it guards is structurally
  prevented here because the b-leg always egresses via `apply_b_leg_egress`. The
  `b2bOutboundProxy` egress + the proxy's worker-outbound classifier are ported
  and exercised by `proxy_b2bua.rs` + `keepalive_via_proxy.rs`.

> **Egress-routing fix (load-bearing, ported alongside `keepalive-via-proxy`):**
> `apply_b_leg_egress` (port of `ActionExecutor.ts` `applyEgressRouting`/
> `applyRouteSet`) previously only handled the b-leg `b2bOutboundProxy` `;outbound`
> preload and ignored the dialog route set when computing the *wire destination*.
> A B2BUA-originated in-dialog request on a dialog with a loose (`;lr`) route set
> (e.g. the a-leg keepalive OPTIONS, whose route set comes from the inbound
> INVITE's proxy Record-Route) therefore went pod-direct instead of to the top
> Route — the exact k8s-endurance teardown shape `keepalive-via-proxy` guards.
> Now, when the first route is loose, the request is sent to that route's
> host:port (R-URI unchanged, Route headers from the generator), for *any* leg
> (`relay.rs`). All five in-dialog egress call sites pass the dialog route set.
