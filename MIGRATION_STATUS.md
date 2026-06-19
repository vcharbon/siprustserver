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
| **Network / UDP** (transport, SignalingNetwork) | `crates/sip-net` *(slice 2)* | `src/sip/{UdpTransport,SignalingNetwork,BufferedUdpEndpoint}.ts` | ✅ `SignalingNetwork` ported — trait (DI seam) + real (tokio `UdpSocket`) + simulated (in-memory fabric) + recording/`scopedAudit` & `paranoidInputs` contract decorators; 22 tests green. `UdpTransport` facade's Tier-1 brake (migration/11) + `UdpTransportMetrics` shape & registry wiring (migration/17) ported into `crates/b2bua`; `BufferedUdpEndpoint` **producer** + `ConnectivityGate` still **deferred** (Slice 2 §un-ported) |
| **Test/contract foundation** (Recorder, RunContext, 4 wrappers) | `crates/layer-harness` | `src/test-harness/framework/*` | ✅ Recorder + typed channels + projectors + RunContext/severity + recording helpers + 4-wrapper vocabulary ported (test-only, SIP-agnostic); 5 tests green. Recorder now stamps `at_ms` via an injected `sip-clock` `Clock` (deterministic report timestamps). [ADR-0004](docs/adr/0004-layer-harness-test-foundation.md) |
| **Scenario harness + reports** (DSL, driver, SVG/txt/HTML) | `crates/scenario-harness` | `src/test-harness/framework/{dsl,interpreter,recorder,message-builder,*-report,svg-sequence-diagram}.ts` | ✅ **fluent dialog-aware DSL** (`Harness`/`Agent`/`invite`/`receive`/`respond`/`ack`/`bye`) auto-generating correct-by-default B2B messages via `sip-message::generators` + tracked `StackDialog` state, **plus** the thin scenarios-as-data DSL as a raw escape hatch; recording-first driver; SVG (clickable in HTML)/global.txt/per-endpoint/HTML renderers; trace **projected from the recording** (`sip-net::to_sip_entries`); virtual-time `advance` + 100 ms fake-net transit delay; UAC/UAS route-set construction from Record-Route + a loose-routing `Proxy` test agent (for the LB/front-proxy slice); 4 e2e tests green (incl. `proxy_record_route`). `or`/`parallel`/media/chaos **deferred**. [ADR-0006](docs/adr/0006-scenario-harness-recording-first.md) |
| **Media** (RTP/RTCP framing, G.711, SDP O/A, paced transport) | `crates/media` + `crates/media-harness` *(slice S-media)* | `src/media/**`, `src/test-harness/media/audio/**` | ✅ transport-agnostic engine over `SignalingNetwork` (sim + real UDP); hand-rolled RFC 3550 framing **+ webrtc-rs `rtp` witness** cross-checked; G.711 PCMA/PCMU; RFC 3264/3262/5009 offer/answer engine (typed `SdpRule` refusals); paced sender + per-(remote,SSRC) demux + counts-only RTCP. Test-only `media-harness`: deterministic clips + MFCC (`rustfft`) classifier + `negotiate_call`. Slices 0/1/2/3a/3b green (41 tests; 3b drives real RTP through the real B2BUA via relayed SDP). RTCP report-block contents, jitter/loss stats, multiple m-lines, video **deferred** ([slice below](#slice--media-cratesmedia--cratesmedia-harness)) |
| **Transaction** (RFC 3261 §17 FSMs + retransmit timers) | `crates/sip-txn` *(slice 4)* | `src/sip/TransactionLayer.ts` | ✅ client/server INVITE+non-INVITE FSMs, A/B/E/F/H/J timers, dedup, CANCEL→200+487, ACK absorption, cached-response retransmit, bounded drop-newest event queue; actor owns the map + a single `DelayQueue` ([ADR-0007](docs/adr/0007-transaction-layer-rust-shape.md)); 14 tests green. RNG seam (`IdGen`) ported here |
| **Dispatch / per-call FIFO** | `crates/b2bua` | `src/sip/{SipRouter,PerCallDispatcher}.ts`, `src/call/CallState.ts` | ✅ per-call queue+worker+global-semaphore dispatcher + router (`routeKey`/`withCall`/`processResult` typed-effect interpreter) + in-memory `CallState` over a replication-aware `CallStore` seam (HA params no-op'd) + B2BUA-local timer `DelayQueue` + decision-engine adapter seam (scripted jssip-emulating test impl) + buffered CDR; alice↔b2bua↔bob basic/failure/reject e2e green ([ADR-0010](docs/adr/0010-b2bua-dispatch-rules-rust-shape.md), [slice below](#slice--b2bua-dispatch--rule-engine-cratesb2bua)) |
| **CallContext data model** | `crates/call` | `src/call/` (`CallModel`/`timer-helpers`/`codec`) | ✅ data model + codec ported — Call→Leg→Dialog structs + lens/index helpers + `callRef`/index-keys + positional-msgpack `CallBodyCodec`; 24 tests green. Stateful `CallState` now ported in `crates/b2bua` (in-memory; Redis/HA-replication transport still deferred). `TimerService` ported as the B2BUA-local `DelayQueue`. protobuf codec **deferred** ([slice below](#slice--callcontext-data-model-cratescall), [ADR-0008](docs/adr/0008-call-context-data-model.md)) |
| **Rule engine** | `crates/b2bua` (`rules/`) | `src/b2bua/rules/` | ✅ first-match/layer-ranked engine (declarative `Match` + `ActionExecutor` + `InvariantEnforcer` + bye-disposition net) + the basic-B2BUA default rule set (relay/dialog/absorb/lifecycle/terminating/corner-case/failure/timer) + the first **SERVICE_LAYER** rule `relayFirst18xTo180` (`drop-sdp`/suppress + `fake-prack`: bare-180 downgrade, 18x suppression, B2BUA-originated PRACK, per-dialog SDP cache + cached-SDP-at-200 injection, To-tag continuity, UPDATE skeleton-fit answer/488, delayed-offer self-disable) + the **SERVICE_LAYER** `promote18xPemTo200` early-media service (`promote-pem-to-200`: 183+SDP+PEM → synthetic 200, promotion-window gating, SDP-diff resync re-INVITE toward A, upstream-fork re-seed, diagnostic-Reason teardown) + the **SERVICE_LAYER** `referTransfer` blind-transfer service (Slice 5a, source SHA `fffc4ac6`): REFER seed rules (`transfer-intercept-refer` 202 + seed + sub-expiry/overall timers + NOTIFY 100 + async `/call/refer`; `transfer-reject-replaces` → 501 with `overrides`; `transfer-reject-a-leg-refer` → 501), the `refer-authorizing → c-ringing → c-realigning` portion (`transfer-http-allow` held-SDP `create-leg`, `transfer-http-reject`/`-timeout`, `transfer-reject-second-refer` → 491, `transfer-c-1xx-to-notify` + dedup, `transfer-c-200-initial`, `transfer-c-fail-initial`, `transfer-c-no-answer`), the typed `Call.transfer` slice + `call::helpers`, `SendNotify`/`ReferAsyncHttp`/`SetTransfer` actions + `CreateLeg` body/header overrides, the `fire_and_forget` interpreter (`ReferAsyncHttp` → `call_refer` → re-enter via a re-entry channel), and the scripted `call_refer` (`X-Api-Call`). **Slice 5b (source SHA `fffc4ac6`):** the c-realign phase — `transfer-c-realign-200` (ACK C, cancel C's `refer_reinvite_answer`, arm A's, re-INVITE A with **C's active c-realign answer** — the one-way-audio guard — → `a-realigning`), `transfer-c-realign-fail` + `transfer-c-realign-timeout` (rollback via `begin-termination`, BYE all three legs), the `refer_reinvite_answer` (32s) timer arm/cancel, and the realigning-phase gating rules `transfer-c-glare-reinvite` (C re-INVITE → 491) + `transfer-b-in-cre-are-reject` (B non-BYE → 481). Scenarios `refer-allow.ts` (5/5) + `refer-c-realign.ts` (5/5) ported. **Slice 5c (source SHA `fffc4ac6`):** the a-realign + merge + post-merge teardown — `transfer-a-realign-200` (ACK A, cancel A's `refer_reinvite_answer` + overall-safety, `merge(a, cLegId)` → A↔C bridged, CDR `transfer-completed`, clear slice), `transfer-a-realign-fail` + `transfer-a-realign-timeout` (rollback via `begin-termination`, BYE all three legs), `transfer-a-glare-reinvite` (A re-INVITE during realigning → 491), and the cross-phase `transfer-overall-timeout` (the 120s safety watchdog → rollback). A BYE from A mid-a-realigning rides the CORE `relay-bye` path (begin-termination BYEs the orphaned B + C — no dedicated rule). Scenario `refer-full-transfer.ts` (5/5) ported. **Slice 5d (source SHA `fffc4ac6`):** the phase-gating matrix verified end-to-end — Regime 1 transparency (`refer-authorizing`, `c-ringing`: A re-INVITE / A INFO / B INFO relay through CORE `relay-reinvite`/`relay-info`; no transfer rule over-matches the earlier phases) and Regime 2 rejection (`c-realigning`: A re-INVITE → 491 via `transfer-a-glare-reinvite`; second REFER in `c-ringing`/`c-realigning` → 491 via `transfer-reject-second-refer`). No new rules required — the 5a–5c phase filters already gate correctly. Scenario `refer-gating.ts` (8/8) ported (harness gained `InDialogTxn::expect_tolerating` to absorb keepalive-OPTIONS retransmits racing a response under the paused clock). **Slice 5e (source SHA `fffc4ac6`):** the reject-path corpus — `refer-reject.ts` (5/5): HTTP reject 403 (NOTIFY 100 active + NOTIFY 403 terminated;noresource), HTTP-timeout (60s subscription-expiry → NOTIFY 500 terminated;timeout via `transfer-http-timeout`), Replaces= REFER → 501 (seed `transfer-reject-replaces`), out-of-dialog REFER → 481 (router `maybe_reject_orphan` pre-rule path; harness gained `Agent::send_out_of_dialog_refer` minting a REFER with a bogus stamped `callRef`), second REFER while `refer-authorizing` → 491 (`transfer-reject-second-refer`). No new rules required. **Slice 5f (source SHA `fffc4ac6`):** the safety-timer corpus — `refer-timers.ts` (1/1): `referOverallSafetyFires` — C answers its initial INVITE, the c-realign re-INVITE toward C fires but is never answered; the per-scenario config pushes `refer_reinvite_answer` to 600s and pulls `refer_overall_safety` to 10s so the cross-phase overall watchdog (`transfer-overall-timeout`) trips first → begin-termination BYEs all three legs. Harness gained `B2buaSut::start_with_config` + `route_all_with_refer_timers` (per-scenario REFER-timer overrides, the faithful equivalent of the source `configOverrides`). No new rules required. **The REFER blind-transfer corpus (refer-allow / -c-realign / -full-transfer / -gating / -reject / -timers) is now complete and green.** **ADR-0016 slice 5 (media/INFO primitives, source SHA `fffc4ac6`):** `SendRequestToLeg` gains an opaque `body`+`content_type` (MSCML INFO rides `application/mediaservercontrol+xml`, defaulting to `application/sdp` when a body is present and no type given); new `SendProvisionalToLeg` brokers an unadopted leg's SDP onto the a-leg as an unreliable 183 (RFC 3262 §3 / RFC 5009 P-Early-Media, minting+persisting the B2BUA early to-tag); `CreateLeg` gains `kind: Option<LegKind>` (a service parks an unadopted `media` leg); and the generic relay-to-peer implicit `→ a` fallback is **gated on `is_adopted`** (`resolve_peer`) so a parked media/`transfer-target` leg is never mis-routed to A (the latent `adopted: Some(false)` pins on the a-/destination legs were corrected to derive from kind). Ports `tests/b2bua/leg-kind-gate.test.ts` (5 unit tests in `b2bua/tests/rules.rs`). **ADR-0016 slice 7 (transfer retrofit, pure refactor of the already-ported corpus, source SHA `fffc4ac6`):** `referTransfer` re-expressed via `define_service!` — `TransferPhase` is the declared `transfer` machine; the 18 phase-gated rules are `sm_rule!`s gated by `active_states` (the `phase(ctx)` match-column removed) declaring their forward `transitions` for the diagram; handlers unchanged. The cursor is a read-only **projection** of the authoritative `Call.transfer.phase` in `invariants::finalize` (mirroring `global-call`, slice 2; clearing the slice deactivates the machine), so transitions are documentation, never enforced. The 3 machine-less seed rules stay core. Registered in `b2bua-runner::compose_services()`; committed `docs/sm/transfer.md`. All 29 transfer e2e tests + freshness test pass unchanged. **ADR-0016 slice 8 (announcement crate — out-of-crate capstone):** new `crates/announcement` depending on `b2bua-sdk` **only** (+ call/sip-message) — no `b2bua` dependency, the boundary proof. A 3-state early-media machine (`OfferingMrf → Announcing → Bridging`) backed by `call.ext["announcement"]`: `init` (active iff the decision's `service_ext` requests it, with `defer_routing`) parks an unadopted `media` leg toward the MRF; on its 200 → 183 early media to A + MSCML `<play>` INFO; on the MRF's MSCML `<response>` success → BYE media + dial the real destination (the framework's core `confirm-dialog` then bridges); media failure → `BeginTermination`. `apply_route` gained a generic `defer_routing` branch (skips normal dest-leg creation); `B2buaCore::spawn_with_services` + `B2buaSut::start_with_services` inject out-of-tree services; registered in `b2bua-runner::compose_services()`; committed `docs/sm/announcement.md`. e2e (`b2bua-harness/tests/announcement.rs`): happy path (alice ↔ b2bua ↔ {MRF, dest}), MRF-rejects, caller-cancels-mid-clip — all green (733 total). ([ADR-0010](docs/adr/0010-b2bua-dispatch-rules-rust-shape.md), [ADR-0016](docs/adr/0016-callflow-service-state-machines.md)) |
| **Draining / readiness / overload** (health-check) | `crates/b2bua` | `src/b2bua/{DrainingState,OverloadController}.ts`, `WorkerReadiness` | 🟡 partial — **migration/08: the `X-Overload` worker→proxy load-signal surface is ported** (`overload.rs`: `LoadSampler` seam + `LiveLoadSampler`/`simulated`, `Ewma` α=0.2, `OverloadSignal` with `x_overload_header_value()` `v=1; elu=…; gc=…; adm=…` + `increment_non_emergency_admitted()` + `metrics()`; a 100 ms `tokio::time::interval` sampler task in `b2bua_core` feeds the EWMAs; stamped on the OPTIONS **200** reply in `router::build_options_health_response`; consumed by the already-ported `sip_proxy::load_observer::parse_x_overload_header`). All 4 named TS publish-surface tests ported (`overload::tests`, 7/7) + responder wiring (`repl::s7_tests::options_200_stamps_x_overload_503_does_not`) + end-to-end through the real proxy parser (`b2bua-harness/tests/x_overload_signal.rs`, 2/2). **migration/09: the Tier-3 admission gate is now ported** (`overload.rs`: `TokenBucket` lazy-refill hard-CPS gate on `tokio::time::Instant`, `AdmitDecision`/`AdmitReason`, `OverloadSignal::should_admit(is_emergency)` = emergency-bypass-but-`consume_forced` → hard CPS `try_consume` → panic-ELU backstop, `configure_admission(&B2buaConfig)` seeding it from `cps_bucket_{size,rate}`/`overload_panic_elu_threshold`/`retry_after_base_sec`; wired into `router::process`'s initial-INVITE branch BEFORE any call/dialog state — a reject sends a stateless 503 `Reason: …text="overload"` + `Retry-After` via `build_stateless_overload_503` *through the INVITE server txn* (`send_response`), an admit advances `adm` via `increment_non_emergency_admitted`; `b2bua_overload_rejected_total` metric + the `reject_{bucket_empty,panic_elu}_total` / `token_bucket_level` on `OverloadMetrics`). Ported tests: 8 unit (`overload::tests::{admits_until_…drained,…refills_over_time,emergency_…overdraft,panic_elu_…,configure_admission_…,token_bucket_level_…,admit_reason_tags_…}`) + 3 end-to-end (`b2bua-harness/tests/tier3_admission_gate.rs`). The TS gate has NO dedicated source unit test (the `shouldAdmit`/`TokenBucket` behaviour was untested upstream), so these pin the ported behaviour directly. **The rest of the row stays deferred** — the serving/draining/ready matrix ([ADR-0010 §X8](docs/adr/0010-b2bua-dispatch-rules-rust-shape.md)). **Tracked fidelity debts / deliberate divergences (carry-forward, not omissions):** (1) **ELU is a coarse starvation proxy, not a true ELU** — `LiveLoadSampler::elu()` returns `clamp01((elapsed − window)/window)`, so a healthy loop reads ~0 (the 100 ms task lands on time) and it only rises once the runtime starves the *sampler task itself*; laggier/coarser than TS `performance.eventLoopUtilization()`, and under a paused clock `advance` always fires the interval on schedule so produced ELU is ~0 in every test (TODO(migration/08) in `overload.rs`: switch to `tokio::runtime::Handle::current().metrics()` busy-accounting for a non-zero moderate-load reading). Currently nobody mis-acts on it: the proxy AIMD rate-cap that would consume the band is itself deferred (ADR-0009 scope note in `load_observer.rs`). (2) ✅ **RESOLVED (migration/09)** — `increment_non_emergency_admitted()` is now wired into the non-emergency new-dialog INVITE admit path in `router::process` (mirroring `TransactionLayer.ts:709-711`), so the LB's per-worker treated-rate `adm` diff input is live (pinned end-to-end by `tier3_admission_gate::admitted_non_emergency_invite_advances_the_adm_counter`). (3) **503 self-reports carry NO `X-Overload`** (deliberate divergence) — TS stamps it on the boot-drain 503 too (`SipRouter.ts:789`); Rust omits it because a 503 already excludes the node from new-dialog selection and the consumer has no `noteRejectionPayload` fast-path (rate-cap deferred). `options_200_stamps_x_overload_503_does_not` pins this divergent behaviour, so revisit it when the `noteRejectionPayload`/AIMD-cap consumer item is ported. (4) **draining-new (200 + `elu=1.000; reason=draining`)** has no analogue — the Rust readiness model collapses draining → 503 (`repl/readiness.rs`); the TS draining-new emission belongs to the **draining-state** migration item and should reuse `x_overload_header_value()` as its base (as TS reuses `xOverloadHeaderValue()`). (5) **`OverloadSignal::metrics()` (elu_ewma / gc_fraction_ewma / non_emergency_admitted_total / reject_{bucket_empty,panic_elu}_total / token_bucket_level) is not surfaced** on `/status`/Prometheus — wire it into the b2bua `/status`/exporter item to match `registry.overload` in TS (`OverloadController.ts:294`). The Tier-3 reject *count* IS exported as `b2bua_overload_rejected_total` (migration/09), but the per-reason split + bucket level on `OverloadMetrics` are not yet on the wire. (6) **(migration/09) the panic-ELU backstop is DISABLED in the test harness baseline** (`b2bua-harness` `start`: `overload_panic_elu_threshold = 1.1`) — a deliberate divergence, not a debt in the gate: the backstop reads the `LiveLoadSampler` busy-proxy ELU, and a paused-clock `Harness::advance` of N seconds makes the next 100 ms sampler tick land N s "late" → ELU ≈ 1.0, which would spuriously panic-503 the next new INVITE in any scenario that advances time between calls. The backstop is a production safety net for a *real* starved loop; its behaviour is pinned by the simulated-sampler unit tests, so the harness disable is semantics-preserving. This is the same root cause as debt (1) (the coarse advance-saturated ELU proxy); a true `RuntimeMetrics` busy-ratio ELU would let the harness keep the backstop armed. |
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
| `SipFragUtils.ts`, `MessageHelpers.ts` | `sipfrag.rs`, `message_helpers.rs` | ✅ ported — sipfrag whole; MessageHelpers **pure half** (header accessors + structured readers) **plus all four Tier-1 overload-brake byte helpers** — `bufferHasEmergencyMarker` (migration/03), then **migration/10** added the other three: `isInviteRequestBuffer` (`is_invite_request_buffer`), `buildStatelessReject503Buffer` (`build_stateless_reject_503_buffer` — distinct from the parsed Tier-3 `b2bua::router::build_stateless_overload_503`: byte-slices verbatim, adds no To-tag), and `jitteredRetryAfter` (`jittered_retry_after`, with the `Math.random()` source recast as an **injected `roll` closure** so the pure crate stays RNG-free). The Tier-1 `preIngress` *consumer* (the `UdpTransport.layer` glue) is still deferred to slice 2 with `AppConfig`/`MetricsRegistry`; the helper-level tests pin the three named `UdpTransport-brake.test.ts` cases. RNG identifier generators + the dispatcher-only `bufferHasToTag` remain deferred to slice 2 (see un-ported list). Source: sipjsserver @ `fffc4ac6`. |

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
- The four **Tier-1 overload-brake byte helpers** in `MessageHelpers.ts`
  (`bufferHasEmergencyMarker`, `isInviteRequestBuffer`,
  `buildStatelessReject503Buffer`, `jitteredRetryAfter`) are now **all ported**
  (migration/03 + migration/10) as pure `message_helpers.rs` functions — they
  are the allocation-free, pre-parse leaves the Tier-1 UDP `preIngress` hook
  composes. Their *consumer* — the brake closure (`build_tier1_brake_hook`) — is
  now **ported and wired into production** (migration/11; `b2bua::tier1_brake` +
  `b2bua-runner` `.with_pre_ingress`), so the end-to-end `UdpTransport-brake.test.ts`
  cases are ported too (`b2bua/tests/tier1_brake.rs`, through the real
  `PreIngressHook` seam + simulated fabric). The Rust port also pins each helper's
  byte-level contract directly (`message_helpers::{buffer_emergency_tests,
  is_invite_request_buffer_tests, jittered_retry_after_tests,
  build_stateless_reject_503_tests}`) **and** replayed the three named brake cases
  at the helper-composition layer (`message_helpers::tier1_brake_helper_composition_tests`,
  migration/10) — the composition tests now have a wired-hook counterpart.
- The dispatcher-only byte helper `bufferHasToTag` (initial-INVITE vs in-dialog
  discriminator for the dispatcher fast-path — NOT a Tier-1 brake input) remains
  deferred to **slice 2** with its dispatcher consumer.
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
- **`UdpTransport.ts`** — the **Tier-1 overload brake `preIngress` policy is now
  ported** (migration/11) as `b2bua::tier1_brake` (`build_tier1_brake_hook` +
  `Tier1BrakeConfig` + `Tier1BrakeCounters`, the `dropsTier1Brake`/`tier1RejectSent`
  surface) and **wired into the production caller** (`b2bua-runner` binds the
  worker socket `.with_pre_ingress(hook)` and exports `b2bua_udp_tier1_*_total` on
  `/metrics`) — closing the gap where the brake was absent in production (the
  runner bound bare `BindUdpOpts::new`). The three `UdpTransport-brake.test.ts`
  cases are ported end-to-end through the real `PreIngressHook` seam + simulated
  fabric (`b2bua/tests/tier1_brake.rs`). The brake is factored into the `b2bua`
  crate (not a standalone `UdpTransport` facade) because the Rust runner binds
  `bindUdp` directly — there is no Effect `Layer` facade to reassemble.
  **migration/17: the `UdpTransportMetrics` *shape* + registry wiring is now
  ported** (`b2bua::metrics::UdpTransportMetrics` + `BufferedSendCounters` +
  `LiveGauge`): the unified Prometheus surface the TS `registry.udp = metrics`
  assignment exposes — `dropsTier1Brake`/`tier1RejectSent` (the `Tier1BrakeCounters`
  it folds in), the **live** `queueDepth`/`queueMax`/`dropsTailDrop` getters proxied
  off the bound `UdpEndpoint` (`queue_depth()` / `queue_max()` /
  `counters().tail_dropped`, each an injected `LiveGauge = Arc<dyn Fn()->u64>` so the
  shape is decoupled from the concrete endpoint type and reads the *instantaneous*
  value like the TS `get queueDepth()`), plus the `bufferedSend` six-counter shape +
  `bufferedSendPeerCount`. Rendered as `b2bua_udp_*` (`prometheus_text`) and **wired
  into the production runner** (`b2bua-runner` binds into a shared
  `Arc<dyn UdpEndpoint>` — new forwarding `impl UdpEndpoint for Arc<dyn UdpEndpoint>`
  in `sip-net` lets the core own one boxed clone while the metrics getters read
  another — and `/metrics` now renders the full shape, **superseding** the
  brake-only `tier1_brake_metrics_text`). `localAddress` stays superseded by
  `UdpEndpoint::local_addr()` (single source of truth, already used for
  Via/Contact stamping). The metrics-*read* half of the three
  `UdpTransport-brake.test.ts` cases (`udp.metrics.{dropsTier1Brake,tier1RejectSent,
  queueDepth,dropsTailDrop}`) is ported end-to-end through the simulated fabric in
  `b2bua/tests/udp_transport_metrics.rs` (the decision half is in
  `tests/tier1_brake.rs`), plus 4 shape unit tests in `metrics::tests`. Source:
  sipjsserver @ `fffc4ac6`.
- **`BufferedUdpEndpoint.ts`** (non-blocking per-peer outbound drainer) — the
  **producer** (the `wrapEndpoint` per-peer queue + drainer fiber + idle-LRU/cap
  eviction) remains deferred; an outbound-path optimization layered on the
  endpoint. migration/17 ports its **counter value-shape** only
  (`b2bua::metrics::BufferedSendCounters`, the six `BufferedSendCounters` fields as
  shareable atomics) so the `UdpTransportMetrics` surface is complete/stable — these
  read zero (a flat declared series) until the drainer lands and writes through them
  (`TODO(migration/BufferedUdpEndpoint)` in `metrics.rs` marks the wiring seam:
  hand the drainer the same `BufferedSendCounters` handle + feed
  `bufferedSendPeerCount` from its `peerCount()`).
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
- ~~**`reuse_port`** wiring~~ — DONE (sip-proxy Pass 9): wired via `socket2`
  in `real.rs`; backs the proxy's `PROXY_RECV_SHARDS` recv-loop sharding.
  Still ignored by the simulated fabric (one endpoint per addr).

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
- **Tier-3 overload admission gate** — ✅ **ported in migration/09, but in the
  b2bua layer, NOT here** (`b2bua::overload::OverloadSignal::should_admit` +
  `b2bua::router` stateless-503). The TS gate lives in `TransactionLayer.ts`, but
  the Rust split keeps sip-txn free of any dependency on the b2bua's
  `OverloadController`/`AppConfig` ([ADR-0007 "Deferred"](docs/adr/0007-transaction-layer-rust-shape.md)),
  so the gate moved up one crate to where that state lives — see the **overload
  row** for the full description. This layer still admits unconditionally (it
  creates the INVITE server txn + auto-100 before emitting to the b2bua, which
  then runs the gate and rejects *through* that server txn). `isEmergencyRequest` /
  `buildStatelessReject503Buffer` were ported alongside the gate
  (`sip_message::is_emergency_request`, `b2bua::router::build_stateless_overload_503`).
- **`transactionBreakdown` gauge**, **OTel span re-parenting / `ForkSiteTracker`**,
  **legacy `send` wrapper** — see ADR-0007 "Deferred".
- **No `propertyTest` / `parity`** — the source `TransactionLayer` has neither
  (those wrap `SignalingNetwork`); the ritual's property/comparison step is N/A
  here.

---

## Slice — Media (`crates/media` + `crates/media-harness`)

**Source release:** sipjsserver @ `fffc4ac69c8aeef26cf48fe73469503145c9732b`.
Source: `src/media/**` (engine, framing, codec, sdp, rtcp), the two framing
impls (`ts/` + `native/`), and the test support
`src/test-harness/media/audio/**` + `tests/media/**`.
**Scope (confirmed with user):** the full media layer whose primary purpose is
**verifying RTP/SDP negotiation** — slices 0–3b, including the real-B2BUA
integration. Plan: [docs/plan/lexical-doodling-boot.md](docs/plan/lexical-doodling-boot.md).

### Decisions
- **Test-vs-real is the transport seam, not the framing.** The engine is written
  once over `Arc<dyn SignalingNetwork>` and runs unchanged on the simulated
  fabric (deterministic, paused clock) and real UDP — exactly like `sip-txn`. The
  TS project's *two implementations* (`MediaEndpointTs`/`MediaEndpointRtpJs`) are
  not test/real; they are two **framing** impls that cross-check each other. Rust
  mirrors this with the `RtpFraming` trait + `HandRolled` (RFC 3550 by hand, the
  `tsFraming` analog) and `WebRtcRs` (wraps the webrtc-rs `rtp` crate, the
  `rtp.js` witness).
- **Behavioural timing on `tokio::time` directly; `Clock` is timestamps only**
  (per CLAUDE.md). The paced sender / RTCP reporter sleep on `tokio::time`;
  `sip-clock::Clock` is consulted only for RTCP NTP stamps. The shared transport
  lock is never held across an `.await`.
- **Media-timescale advance.** A 20 ms paced loop is finer than the harness's
  100 ms advance chunks; advancing past many ptime deadlines in one step starves
  the loop. `media-harness::testkit::advance_media` steps ≤ ptime with a
  `yield_now` between — the media-timescale version of CLAUDE.md's "drive the
  protocol between advances."
- **Structured SDP lives in `media`**, separate from the b2bua-scoped string SDP
  in `sip-message` (left untouched). The offer/answer engine is hand-rolled (the
  negotiation policy is the thing under test) with typed per-RFC-MUST refusals.

### Source → Rust (port checklist)
| Source | Rust module | Status |
|---|---|---|
| `media/codec/g711.ts` | `media/codec/g711.rs` | ✅ canonical ITU PCMA/PCMU |
| `media/rtp/packet.ts` (`RtpFraming` seam + `tsFraming`) | `media/rtp/packet.rs` (`RtpFraming` + `HandRolled`) | ✅ |
| `media/native/MediaEndpointRtpJs.ts` (`rtpJsFraming`) | `media/rtp/webrtc_framing.rs` (`WebRtcRs`, wraps `rtp` crate) | ✅ witness |
| `media/rtp/rtcp.ts` | `media/rtp/rtcp.rs` | ✅ SR/RR counts-only + RFC 5761 demux |
| `media/sdp/{types,parse}.ts` | `media/sdp/mod.rs` | ✅ structured SDP + parse/build |
| `media/sdp/negotiator.ts` | `media/sdp/negotiator.rs` | ✅ O/A engine + `SdpRule` + RFC 5009 gate |
| `media/transport.ts` (`mediaEndpointLayer`) | `media/transport.rs` (`MediaEndpoint`/`MediaTransport`/`MediaSession`) | ✅ |
| `media/ts` + `native` layer wiring | `media::{ts_endpoint, webrtc_endpoint}` | ✅ |
| `test-harness/media/audio/clips.ts` | `media-harness/audio/clips.rs` | ✅ deterministic formant/ringback synth |
| `test-harness/media/audio/spectral.ts` | `media-harness/audio/spectral.rs` | ✅ MFCC via `rustfft` |
| `test-harness/media/audio/classify.ts` | `media-harness/audio/classify.rs` | ✅ verdict + sequence |
| `tests/media/support-negotiate.ts` | `media-harness/negotiate.rs` | ✅ `negotiate_call` + rewrites |

### Tests ported
| Source test | Rust home | Status |
|---|---|---|
| `audio-comparator.test.ts` (slice 0) | `media-harness/tests/audio_comparator.rs` | ✅ 6 |
| `rtp-media.test.ts` framing cross-check (slice 1) | `media/tests/rtp_framing.rs` | ✅ 6 (2×2 witness matrix + CSRC + non-v2) |
| `rtp-media.test.ts` play→record + RTCP (slice 1) | `media/tests/rtp_media.rs` | ✅ 3 (both framing flavors; exact stats) |
| `rtp-media-live.test.ts` (slice 1 live) | `media/tests/rtp_media_live.rs` | ✅ 1 (real UDP, loss-tolerant) |
| `sdp-negotiation.test.ts` (slice 2) | `media/tests/sdp_negotiation.rs` | ✅ 13 (per-rule refusals + direction/hold + PEM gate) |
| `media-e2e.test.ts` (slice 3a) | `media-harness/tests/media_e2e.rs` | ✅ 2 (relay + corrupt-SDP misdirection) |
| `basic-call-media.test.ts` (slice 3b) | `b2bua-harness/tests/basic_call_media.rs` | ✅ 1 (real RTP through the real B2BUA) |
| (codec/rtcp unit) | `media/src/**` unit tests | ✅ 10 |

### Un-ported with justification
- **HTML media-panel reporting** (the slice-3b `Media (RTP)` report table) — the
  Rust b2bua-harness asserts the `MediaVerdict` directly; the HTML report
  renderer's media panel is a reporting nicety, deferred with the broader report
  work.
- **The declarative scenario-DSL media steps** (`dialog.media.plays`/`hears`) —
  the Rust harness is imperative (no scenarios-as-data interpreter driving
  media), so 3b uses the agent API directly. The DSL `plays`/`hears` steps stay
  on the deferred list noted in the Slice 3 section.
- **RTCP Sender/Receiver Report *contents*** (report blocks, jitter, cumulative
  loss, LSR/DLSR) — first-cut scope is counts-only (matches the TS), so SR/RR are
  well-formed but carry no report blocks; the field-content asserts are deferred.
- **rtp.js-specific behaviours / multiple codecs beyond G.711, multiple m-lines,
  video** — the negotiation engine and SDP model are audio/G.711-scoped exactly
  as the source; extending is a later slice.
- **`scripts/fetch-media-clips.ts`** (render clips to `.wav` + the CC0-speech
  upgrade path) — a tooling convenience, not test surface. The deterministic
  synth is ported; rendering to wav is not needed for the hermetic tests.

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
| `ProxySelfGate.ts` + `observability/LoadSampler.ts` (sipjsserver @ `ea64aede`) | `self_gate.rs` | ✅ **migration/14: real ELU/CPS gate ported** — `EluCpsGate` (port of TS `ProxySelfGate`): an `Ewma` (α=0.2) over a `LoadSampler` ELU read seam + a per-class `TokenBucket` (inlined, as in the TS source, since `sip-proxy` has no `b2bua` dep) shed external new-dialog non-emergency INVITEs. `try_admit_external` order is faithful: `elu_ewma > elu_critical` → reject `proxy_overload_elu` (Retry-After 1, **no token spent**) → `bucket.try_consume()` empty → reject `proxy_overload_cps` (Retry-After = `bucket.retry_after_sec()`, 60 s fallback at rate 0) → else admit. Defaults copied verbatim from `defaultProxySelfGateConfig` (`elu_critical 0.80`, `cps_bucket_size 50`, `cps_bucket_rate 100`, `elu_smoothing_alpha 0.2`, `sampler_interval 100 ms`). `LoadSampler` ported with `LiveLoadSampler` (same coarse busy-proxy ELU as the b2bua side — debt (1)) + `simulated()`/`SimulatedLoadControl` single-cell test double; `note_bypass(Emergency\|Internal)` + a `metrics()`/`ProxySelfGateMetrics` snapshot (elu_ewma/gc_fraction/cps_bucket_{level,max}/admitted/rejected_{elu,cps}/{internal,emergency}_bypassed). **Rides `tokio::time`, NOT the TS raw `setInterval`** (CLAUDE.md): the bucket refills on `tokio::time::Instant`, the EWMA is fed by an explicit `sample()` tick from a 100 ms `tokio::time::interval` task in `sip-proxy-runner` — so paused-clock tests drive both with `tokio::time::advance`. Wired into the runner: `PROXY_SELF_GATE` (default on) + `PROXY_SELF_GATE_{ELU_CRITICAL,CPS_SIZE,CPS_RATE}`, gate gauges/counters appended to `/metrics` (`sip_proxy_self_*`); the request path's branch (`core/request.rs`) + `note_bypass` were already wired against the prior stub. `AlwaysAdmitGate` retained as the no-protection default in `ProxyCoreBuilder`. 13 unit tests in `self_gate::tests` (the TS source has **no** `ProxySelfGate.test.ts`, so these pin the ported behaviour, including a `start_paused` running-sampler-task → shed loop). **Deliberate divergences:** (a) the EWMA-feed `sample()` is an explicit tick (not a self-arming `setInterval`), so the test fixture needs no real clock; (b) `LiveLoadSampler::elu()` is the coarse advance-saturated busy proxy shared with migration/08 (TODO: `RuntimeMetrics` busy-ratio); (c) the TS Effect `Metric` push-on-each-decision is replaced by a sampled `/metrics` render of the snapshot (matches the `ProxyMetrics` idiom). |
| `observability/Metrics.ts` | `observability/metrics.rs` | ✅ atomics counters/gauges + Prometheus text |
| `observability/MetricsServer.ts` | `observability/metrics_server.rs` | ✅ hand-rolled tokio `/metrics` HTTP |
| `observability/Logger.ts` | `observability/logger.rs` | ✅ structured routing-decision log (trait + capturing/noop) |
| `RoutingStrategy.ts` `SocketAddr` | `addr.rs` | ✅ `ProxyAddr` policy type |
| (new) harness SUT seam | `scenario-harness::Harness::bind_sut` | ✅ binds a SUT endpoint on the shared recording fabric |

### Tests ported
| Source test | Rust home | Status |
|---|---|---|
| `load-balancer/{hmac-tampering,cookie-route-fallback,decode-forward-not-ready,decode-forward-respawn-window,unresolvable-id-falls-back,add-remove-resharding,initial-health,distribution,selectForNewDialog-overload(band+emergency-bypass parts)}` | `tests/load_balancer.rs` | ✅ 12 |
| RendezvousHash distribution + HMAC sign/verify + header surgery + registry parse + cancel-LRU + observer bands + metrics/logger/metrics-server | `src/**` `#[cfg(test)]` | ✅ 46 |
| `transit-only/{invite-200-ack-bye, malformed-message-rejected, max-forwards(483)}` | `tests/transit_only.rs` (real `ProxyCore` SUT) | ✅ 3 |
| `load-balancer/callid-routing-guard` + `distribution` (wire) | `tests/load_balancer_routing.rs` (real SUT) | ✅ 1 |
| `integration/options-end-to-end` | `tests/options_e2e.rs` (probe ↔ simulated B2BUA responder) | ✅ 1 |

### Un-ported with justification
- **Registrar / REGISTER path** (`RegisterStrategy`, `Registrar`,
  `CoreToExtRoutingStrategy`, `RegistrarProxyConfig`, `handleRequestRegistrarMode`,
  dual-endpoint `;net=` egress) — user scope. The single-endpoint path drops the
  dual-fabric branching entirely. Tests: all `registrar/**`, `registrar-503-on-drop`.
- ✅ **RESOLVED (migration/14)** — **`ProxySelfGate` real impl** (ELU EWMA + CPS
  bucket) is now `self_gate.rs::EluCpsGate` (see the ported-modules row above). It
  layers on the OPTIONS-driven worker health/band + `sip-net` receive-buffer
  tail-drop. `ingress-concurrency` (the TS concurrency soak) is **not** ported —
  it is a load/throughput soak, not a behaviour test, and the gate's behaviour is
  pinned by the 13 `self_gate::tests` unit cases instead (the TS source has no
  `ProxySelfGate.test.ts`).
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

### Item 07 — `LoadBalancer.selectForNewDialog` v3-cookie / CRITICAL-band / emergency

The LoadBalancer v3 surface (`COOKIE_VERSION='3'`, the `e=<0|1>` emergency flag in
`build_stickiness_input`, the `select_for_new_dialog` CRITICAL-band filter with its
emergency/in-dialog bypass, and the `SelectOpts::emergency_override` shortcut) was
**already in-tree** — introduced wholesale in the pre-migration commit `959f588`
("sip proxy with LB") and cleaned up in `8a03a97`, not re-ported on a per-item
branch. The item-07 deliverable on `migration/07-…` is therefore this attribution
plus the previously-missing emergency-bypass round-trip coverage:
- `emergency_cookie_round_trips_e1_and_is_emergency` — pins the `e=1` ENCODE
  (RPH INVITE ⇒ `params['e']=="1"`) → DECODE (`is_emergency: true` on the
  DecodeResult) contract that the in-tree code plumbs but nothing exercised.
- `above_critical_band_filtered_for_non_emergency_only` now drives the bypass via
  an on-wire `Resource-Priority: esnet.0` INVITE (`is_emergency_invite` on the
  select path), in addition to the `emergency_override` opts path.
- `hmac_tampering_rejected` restored the v3 wire-format assertions (`v==3`,
  `e==0`, `w_pri`/`w_bak`/`kid`/`sig` present) and flips a single base64url char
  (length-preserving) to hit the MAC verify-mismatch reject branch specifically.

The two AIMD `selectForNewDialog-overload` cases (`RateCapExhausted` on empty
bucket; emergency bypasses the AIMD bucket) remain un-ported with the rest of the
AIMD token bucket (X6) — the Rust LB is band-only per ADR-0009 and never raises
`RateCapExhausted`. They are picked up with the AIMD-bucket item, not item 07.

---

### Item 16 — b2bua-side `LoadSampler` / ELU / event-loop sampler (NO standalone delta)

The migration-16 item (`b2bua-side-loadsampler-elu-gc-event-loop`) names the
worker-side load-sampling surface in `crates/b2bua/src/overload.rs` — the
`LoadSampler` read seam, `LiveLoadSampler`, `SimulatedLoadSampler` + `simulated()`,
`Ewma` (α=0.2), `OverloadSignal`, `x_overload_header_value()`,
`increment_non_emergency_admitted()`, the 100 ms `tokio::time::interval` sampler
task in `b2bua_core`, and the `b2bua-harness/tests/x_overload_signal.rs` end-to-end
test. **That work was already ported under `migration/08`** (commits `9a02edb`
"emit X-Overload worker load signal on the OPTIONS-200 self-report" and `c181f5f`
"add the OverloadSignal sampler-injection seam + paused-clock it.live port"), and
is fully described in the **Draining / readiness / overload** row above. Both
commits are ancestors of every later branch tip, so
`git diff migration/15-… migration/16-…` is **empty** and the `migration/16-…`
label was created from HEAD with no commit of its own.

Like `migration/05` (emergency markers, collapsed into `72b864c`) and item 07
above, **branch `migration/16-…` therefore carries no standalone code delta** — it
must NOT be merged as a fresh "LoadSampler port" (doing so would misattribute the
migration/08 work). The item-16 deliverable is this attribution note plus the
fidelity/coverage confirmation: the four named TS publish-surface cases are ported
and green (`overload::tests::{header_value_follows_v1_schema,
increment_non_emergency_admitted_advances_adm_in_header,
ewmas_start_at_zero_before_the_sampler_fires,
load_sampler_injection_drives_elu_ewma_once_sampled}`) with the end-to-end
injected-value path in `x_overload_signal.rs::injected_sampler_drives_published_elu_through_the_running_task`
— re-verified `cargo test -p b2bua --lib overload::` = 15/15 and
`cargo test -p b2bua-harness --test x_overload_signal` = 3/3.

**Tracked follow-up (carry-forward, was debt (1) on the overload row):** the real
**tokio-`RuntimeMetrics` busy-ratio ELU** is its own future item.
`LiveLoadSampler::elu()` is still the coarse "lateness of the sampler task"
busy proxy (`clamp01((elapsed − window)/window)`), which reads ~0 on a healthy or
paused-clock runtime; the proxy band classifier keys on `elu` only, so the
`AboveCritical` exclusion is **effectively inert in production until a true ELU
lands** (TODO(migration/08) in `overload.rs`). This is not yet load-bearing — the
proxy-side ELU-band AIMD rate-cap that would consume it is itself deferred (X6) —
but it must be tracked so that AIMD item is not silently driven by a ~constant
placeholder. Swap to `tokio::runtime::Handle::current().metrics()` busy-duration
accounting (shared with the `sip-proxy` `self_gate.rs` `LiveLoadSampler`, same
debt) when an ELU definition matching the proxy band thresholds is settled.

---

### Item 18 — non-emergency admitted counter + `adm` publish (NO standalone delta)

The migration-18 item (`non-emergency-admitted-counter-adm-publi`) names the
worker's per-worker treated-rate signal: the `adm` admitted-call counter, its
publish on the `X-Overload` header, and the increment on the admit path. **That
work was already ported under earlier migration items** — there is nothing left
to commit here:

- **The publish surface** (commit `9a02edb`, item 08): `OverloadSignal`'s
  `non_emergency_admitted: Arc<AtomicU64>` counter, `increment_non_emergency_admitted()`,
  the `adm=` field emitted by `x_overload_header_value()` (`v=1; elu=…; gc=…; adm=…`),
  and `non_emergency_admitted_total` on the `metrics()` snapshot — all in
  `crates/b2bua/src/overload.rs`.
- **The admit-path wiring** (commit `f02331c`, item 09): the increment is called
  on the non-emergency new-dialog INVITE admit branch in `router::process`
  (`router.rs:1123`, mirroring `TransactionLayer.ts:709-711`), so the LB's
  per-worker treated-rate `adm` diff input is live. This is debt (2) on the
  **Draining / readiness / overload** row, already marked ✅ RESOLVED there.

Both commits are ancestors of every later branch tip, so
`git diff migration/17-… migration/18-…` is **empty** (both tips are `bac571a`)
and the `migration/18-…` label was created from HEAD with no commit of its own.

Like `migration/05` (emergency markers, collapsed into `72b864c`), item 07, and
item 16 above, **branch `migration/18-…` therefore carries no standalone code
delta** — it must NOT be merged/PR'd as a fresh "adm counter port" (doing so
would misattribute the migration/08 + migration/09 work). The item-18 deliverable
is this attribution note plus the fidelity/coverage confirmation:

- **Coverage** — the `adm` publish-and-advance path is pinned end-to-end:
  `overload::tests::increment_non_emergency_admitted_advances_adm_in_header` (unit:
  three increments advance `adm` in the header and `metrics()`);
  `repl::s7_tests::options_200_stamps_x_overload_503_does_not` (the responder
  *publishes* adm only on the 200 — it never increments — and a 503 carries no
  header); the increment-vs-not semantic is pinned separately by the tier3
  admit/emergency tests
  `b2bua-harness/tests/tier3_admission_gate.rs::admitted_non_emergency_invite_advances_the_adm_counter`
  (non-emergency admit through `router::process` advances `adm` 0→1, sheds nothing)
  and `…::emergency_invite_bypasses_the_empty_bucket_and_establishes` (emergency
  admits but is not counted); and `b2bua-harness/tests/x_overload_signal.rs`
  (the counter advance flows out the running worker's published header into the
  real proxy parser). Re-verified `cargo test -p b2bua --lib overload::` = 15/15.
- **Fidelity (`adm` width, informational)** — the producer emits `adm` as a `u64`
  (`overload.rs`: `format!("… adm={adm}")` over `AtomicU64::load`) and the consumer
  parses it as `f64` (`sip-proxy/src/load_observer.rs:181`,
  `let adm: f64 = params.get("adm")?.parse()`). This is **identical to the TS
  source** — `OverloadController.ts` uses a JS `number` (f64) on both ends, "uint53-safe";
  above 2^53 admits precision would degrade, but no worker reaches that in its
  lifetime and the consumer only diffs successive samples for a rate. Faithful
  port, no action — noted for completeness so a later reviewer does not "fix" the
  type and diverge from the contract.

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
| `b2bua/stack-identity.ts` | `stack_identity.rs` (+ `rules/relay.rs` leg builders) | ✅ Via/Contact `cr`/`lg` stamping + param codec; **emergency markers wired end-to-end** (slices 05+06, collapsed into one commit `72b864c`: the `;em=1`/`;emerg=1` marker primitives and their `legStackIdentity` wiring shipped together — branch `migration/05-…` carries **no standalone delta** and points at the slice-04 boundary `1b184b5`): the `;em=1` (Via) / `;emerg=1` (Contact) markers are now stamped on every real outbound hop by threading `is_emergency = call.emergency == Some(true)` (port of `legStackIdentity`, stack-identity.ts L137) through `relay::{leg_via,leg_contact,build_b_leg,ack_b_leg}` + `bye_on_dialog` to all ~20 `actions.rs`/`apply_route.rs` call sites — so an admitted emergency call's in-dialog traffic carries the signal `buffer_has_emergency_marker` scans (the Tier-1 overload brake never 503s it). The `StackIdentity` public read-API (`from_config`/`advertised_{host,port}`) is a forward-looking consumer seam (no in-tree caller yet), mirroring the TS `StackIdentity` DI service |
| `b2bua/helpers.ts` b2bOutboundProxy + `ActionExecutor` `applyEgressRouting` | `rules/relay.rs` (`apply_b_leg_egress`) + `config.b2b_outbound_proxy` | ✅ b-leg INVITE/ACK/BYE preload `;outbound` Route at the proxy; wire dest = proxy, R-URI = callee (RFC 3261 §16.12). Empty-routeSet error-fallback + the `;outbound` source-IP hardening folded into the one helper |
| `B2buaCore.ts` (layer composition) | `b2bua_core.rs` | ✅ |

### Tests
| Area | Rust home | Status |
|---|---|---|
| per-call FIFO order + cap/queue drops | `dispatch.rs` | ✅ 2 |
| timer fire/cancel under paused clock | `timers.rs` | ✅ 2 |
| scripted decision route/reject | `decision/test_adapter.rs` | ✅ 2 |
| stack-identity param round-trip + emergency markers (`em`/`emerg` branch) + `StackIdentity` public read-API | `stack_identity.rs` | ✅ 9 |
| emergency markers ON THE WIRE: an emergency call's relayed b-leg INVITE Via carries `;em=1` + Contact carries `;emerg=1` (non-emergency carries neither) | `rules/relay.rs` (`identity_tests`) | ✅ 2 |
| matcher ranking + invariant enforcement | `tests/rules.rs` | ✅ 3 |
| alice↔b2bua↔bob basic call (INVITE/180/200/ACK/BYE) + one CDR | `b2bua-harness/tests/basic_call.rs` | ✅ 1 |
| b-leg 486 relayed + terminate; decision reject 403 | `b2bua-harness/tests/failure.rs` | ✅ 2 |
| alice→proxy→b2bua→proxy→bob basic call (port of `basicCall` on the `proxy+b2b` SUT): real LB `ProxyCore` fronting one real `B2buaCore` worker behind `b2bOutboundProxy`; asserts INVITE **and** BYE make all four hops + one CDR | `b2bua-harness/tests/proxy_b2bua.rs` (+ `tests/common`) | ✅ 1 |
| **goal-2 simulated failover** (slice S10b): the `FailoverHarness` ties the scenario-harness SIP plane + a recording-wrapped SIM repl fabric + a real LB `ProxyCore` SUT + 2 replicating `B2buaCore` workers (`ReplicatedB2buaSut`: `crash`/`reboot`/`is_ready`/repl-store introspection) + alice/bob under ONE fake clock. Canonical 5-step failover (establish→replicate→crash B1→in-dialog fails over to acting-backup B2→reboot B1 EMPTY higher-gen→re-hydrate→next msg back on B1) + 4 matrix faults (crash mid-INVITE, crash during re-hydration, partition during failover, double-fault) + a combined SIP+replication recording report (reuses the S9 `ha-harness` renderer). Wired the live flush-on-mutation + acting-backup hydrate-from-replica + the in-dialog `callRef` URI-param case fix in `b2bua` (router/store). 6 tests, stable 6×. ([ADR-0011 X10](docs/adr/0011-ha-replication-peer-to-peer.md)) | `b2bua-harness/tests/failover.rs` (+ `src/failover.rs`) | ✅ 6 |

#### SIP-behaviour scenarios (ports of `tests/scenarios/*`)
| Source scenario | Behaviour exercised | Rust home | Status |
|---|---|---|---|
| `prack.ts` | **transparent / default reliable-provisional path** — active when NO non-transparent 18x service (`relayFirst18xTo180` `drop-sdp`/`fake-prack`, rows below) is configured. end-to-end reliable provisional: 183(100rel,RSeq) → PRACK(RAck) → 200(PRACK) → 200(INVITE). The B2BUA relays PRACK/100rel **transparently** as a back-to-back UA (`relay-prack` + `relay-non-invite-200`; it does NOT run the RFC 3262 state machine — RSeq/RAck bookkeeping stays end-to-end between the UAs) with **per-dialog CSeq** (`relayCSeqDelta`), **RAck CSeq rewrite** (RFC 3262 §7.2), b-leg early-dialog capture, and pending-request response correlation (`generate_relayed_response`). Verified green 2026-06-14 | `b2bua-harness/tests/prack.rs` | ✅ 1 |
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
| `suppress-18x.ts` (`failoverNoAnswer`, `failoverReject`) | `/call/failure` b-leg failover with tag-continuity across the leg swap: no-answer/503 on bob1 → CANCEL/terminate + failover to bob2 (new R-URI); the `relay_first_18x` slice survives so bob2's 200 reuses bob1's first-180 To-tag | `b2bua-harness/tests/suppress_18x.rs` | ✅ 2 |
| `fake-prack.ts` (`basic`) | strategy `fake-prack`: bob reliable 183(100rel,SDP) → bare 180 to alice, B2BUA PRACKs bob + **caches** his SDP, bob's bodyless 200 → alice's 200 carries the cached SDP; `Supported:100rel` **kept** to bob | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`multiple-18x`) | two reliable 18x, one PRACK each, latest cached SDP wins on the 200 | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`update-happy`) | bob UPDATE(offer) → B2BUA local 200 with skeleton-fit answer (codec ∩ alice's INVITE) + cache advances | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`update-codec-mismatch`) | bob UPDATE(opus-only) → no codec overlap → B2BUA local 488; call continues on the original cached SDP | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`delayed-offer-fallback`) | alice INVITE has no SDP → outbound INVITE strips `Supported:100rel` and the policy self-disables (plain relay) | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`no-policy-control`) | no policy → full end-to-end PRACK (183/Require:100rel relayed verbatim, alice PRACKs end-to-end) | `b2bua-harness/tests/fake_prack.rs` | ✅ 1 |
| `fake-prack.ts` (`forking`, `failover`) | `/call/failure` failover-on-503: bob1 reliable (183/100rel → bare 180 + PRACK + cached SDP) then 503 → failover to bob2 (unreliable); bob1's per-leg cache discarded with its leg, so alice's 200 carries bob2's own SDP | `b2bua-harness/tests/fake_prack.rs` | ✅ 2 |

#### Limit-enforcement & message-crossing edge cases (Rust-native, 2026-06-14)

Beyond the TS scenario ports, a suite of **limit-enforcement and
message-crossing** edge cases is pinned end-to-end through the single-SUT
harness. Each wires a **real `LimiterServer`** and asserts the limiter hold
drains back to 0 (no leaked slot, no double-decrement) **plus** full reap
(`assert_fully_reaped`) **plus** the `h.finish()` RFC compliance gate. These have
no 1:1 TS scenario (the TS enforces the same limits in `SipRouter.ts` /
`rules/defaults` but ships no dedicated scenario tests for them).

| Limit / race | Behaviour exercised | Rust home | Status |
|---|---|---|---|
| No-answer / ring forever | a-leg `SetupTimeout` (150 s; >180 s unreachable — sip-txn `INVITE_INITIAL_TIMEOUT` backstop is 158 s) → 408 + CANCEL b-leg + limiter release | `b2bua-harness/tests/setup_timeout.rs` | ✅ 2 |
| Max call duration (never hung up) | established call → `GlobalDuration` (`features.platform.max_duration_sec`) → BYE both legs + `max_duration` CDR + limiter release | `limit_cases.rs::max_duration_byes_both_legs_*` | ✅ 1 |
| Max-duration mid-re-INVITE | cap fires while a re-INVITE is pending → teardown abandons the re-INVITE + limiter release | `limit_cases.rs::max_duration_fires_mid_reinvite_*` | ✅ 1 |
| Provisional storm before connect | >200 `18x` to the initial INVITE → `MAX_MESSAGES_PER_CALL` cap → 503 to caller + CANCEL b-leg + limiter release | `limit_cases.rs::provisional_storm_*` | ✅ 1 |
| Reliable-provisional / PRACK-loop storm | >200 events from a `183(100rel)`/PRACK/`200(PRACK)` loop → cap trips with an in-flight PRACK txn open → 503 to a-leg + CANCEL b-leg, PRACK abandoned with no leak, RFC gate clean + limiter release | `limit_cases.rs::prack_loop_storm_*` | ✅ 1 |
| In-dialog message storm | >200 in-dialog OPTIONS round-trips on an up call → cap → BYE both legs + limiter release | `limit_cases.rs::in_dialog_message_storm_*` | ✅ 1 |
| 200/CANCEL crossing | CANCEL races the callee's 200 OK → `cancel-200-crossing` confirms + ACKs + BYEs the b-leg + limiter release (sibling of `crossingReInvite`) | `b2bua-harness/tests/cancel_200_crossing.rs` | ✅ 1 |
| BYE/BYE glare | both parties hang up at once → call reaps once + limiter released exactly once | `teardown_races.rs::bye_bye_glare_*` | ✅ 1 |
| Re-INVITE crossing a BYE | re-INVITE crosses the peer's BYE → BYE wins, re-INVITE abandoned + limiter release | `teardown_races.rs::reinvite_crossing_bye_*` | ✅ 1 |
| Limiter at capacity | Nth call over cap → 486 (no over-increment), release-on-BYE frees the slot, shared cross-worker count, failover-on-reject | `b2bua-harness/tests/limiter.rs` | ✅ 5 |

**Compliance fix found while writing these:** the `MAX_MESSAGES_PER_CALL` cap path
ran `begin-termination` **alone**. `begin_termination` assumes an unanswered a-leg
was already replied to by the firing rule (true for `setup-timeout`), but the cap
fires from the **router, not a rule** — so a cap trip **before connect** left the
caller's INVITE with no final response (caller hangs to its own Timer B; limiter
slot held until the ~32 s `TerminatingTimeout`). Fixed in `b2bua/src/router.rs`:
reply **503** to a `Trying/Early` a-leg before `begin-termination` (answered legs
still take the BYE path). **The TS source (`SipRouter.ts`) carries the identical
latent gap — port the fix back.**

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
  `failoverReject`; `fake-prack` `forking` / `failover`) — **now ported** (the
  `/call/failure` b-leg failover service): `route-failure` / `no-answer` invoke
  `CallDecisionEngine::call_failure` via the same fire-and-forget + re-entry
  channel REFER uses (`FailureAsyncHttp` effect → `call-failure-result` internal
  event → `failover-create-leg` / `failover-terminate` resolution rules). On
  `Failover` a fresh b-leg is created toward the new destination (A's INVITE
  snapshot + new R-URI / header / no-answer overrides) with the failed leg's
  no-answer timer cancelled; the `relay_first_18x` slice is intentionally NOT
  cleared so the first-180 To-tag survives the leg swap (the property these cases
  assert). The pre-failover behaviour (relay + tear down) is preserved when no
  `callback_context` is set. Fix: `resolve_peer` now prefers the merge's
  `active_peer` over the tag map (matching TS `getPeer`-first ordering) so an
  a-leg in-dialog request after failover routes onto the live leg, not the stale
  same-a-tag mapping of the terminated one. `crates/b2bua/src/rules/defaults.rs`
  (`route-failure`/`no-answer`/`failover-create-leg`/`failover-terminate`) +
  router `FailureAsyncHttp` wiring. `crates/b2bua-harness/tests/failover.rs` is HA
  worker-crash failover (a different mechanism), not this.
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
- **Failover via `call_failure`** — **now ported**: `route-failure` / `no-answer`
  call `/call/failure` through the fire-and-forget + re-entry channel and create a
  failover b-leg on `Failover` (else relay + tear down). See the 4 failover-shaped
  18x scenarios above.
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
