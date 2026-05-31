# Slice 9 — Front proxy + load balancer (`crates/sip-proxy`)

## Context

Next area in the port: the **SIP front proxy + its load-balancing logic**
(`portsource/sipjsserver/src/sip-front-proxy/`). The proxy is a **stateless**
RFC 3261 §16 proxy that fans new dialogs across a pool of B2BUA workers,
pins in-dialog traffic to the chosen worker via a signed Record-Route cookie,
and tracks worker liveness with OPTIONS health probes. It is shared
infrastructure that a future B2B2UA will also sit behind.

**Source release:** sipjsserver @ submodule `fffc4ac69c8aeef26cf48fe73469503145c9732b`.

**Confirmed scope (user):**
- Port: proxy data path + LB + worker registry (static + simulated) + OPTIONS
  health probing toward the B2BUA + Record-Route/Route cookie + the metrics
  layer (counters **and** the Prometheus HTTP server).
- **Out of scope — the SIP registrar / REGISTER path** (`RegisterStrategy`,
  `Registrar`, `CoreToExtRoutingStrategy`, `RegistrarProxyConfig`, dual-endpoint
  registrar mode `handleRequestRegistrarMode`).
- **`ProxySelfGate` → empty always-admit stub** (TODO). Overload protection for
  now relies on (a) OPTIONS-driven worker health/band from the B2BUA and (b) the
  `sip-net` receive-buffer tail-drop (`PacketQueue`).
- **Worker overload = readiness + CRITICAL-band filter only.** Port band
  classification so `above_critical` workers are excluded from new-dialog
  selection; **defer the per-worker AIMD rate-cap token bucket**. The
  `RateCapExhausted` error variant + the core's 503/Retry-After branch are kept
  (dead) so the bucket drops in later with no surface change.
- Reuse `sip-txn::IdGen` for Via branch generation; reuse `sip-clock::Clock` for
  timestamps; the probe loop's scheduling rides `tokio::time` directly.

The proxy is **stateless** — it does *not* use `sip-txn`'s transaction FSMs.
CANCEL/ACK correlation is a proxy-local `(Call-ID|CSeq#)` LRU.

## Approach

New pure-ish leaf crate **`crates/sip-proxy`** (production code). The one large
piece of *new* test infrastructure is a **System-Under-Test (SUT) seam** in
`scenario-harness`: today its agents talk peer-to-peer with no in-process
component on the fabric. The proxy is the first SUT — it binds on the *same*
recording-wrapped `SimulatedSignalingNetwork` the agents use and runs a recv
loop, so alice→proxy→bob "just works" by `SocketAddr` routing and the recording
stays the trace (extends [ADR-0006](../adr/0006-scenario-harness-recording-first.md)).

### Crate module layout (`crates/sip-proxy/src/`)

```
lib.rs                 re-exports, crate docs, dependency-policy note
addr.rs                ProxyAddr {host:String, port:u16} + parse/display (source SocketAddr shape)
strategy.rs            RoutingStrategy trait + DecodeResult / SelectError / RouteParams / SelectOpts
cancel_lru.rs          CancelBranchLru: (Call-ID|CSeq#) -> {target,branch} + TTL sweep
headers.rs             proxy header surgery: prepend/upsert/remove_first(_entry), received/rport
                       on top Via, split_top_level_commas, build_record_route_value
core/mod.rs            ProxyCore: owns endpoint, recv loop, dispatch req/resp
core/request.rs        handleRequestImpl port (ProxyCore.ts L623-1133)
core/response.rs       handleResponseImpl port (ProxyCore.ts L1246-1449)
strategies/rendezvous.rs   HRW: u64(SHA-1("key:id")[..8]) * weight, pick max
strategies/forward_all.rs  dev strategy: static target, stickiness target=host:port (no HMAC)
strategies/load_balancer.rs HRW select + band filter + HMAC cookie v=3 encode/decode + routing matrix
registry/mod.rs        WorkerRegistry trait + WorkerEntry / WorkerHealth / RegistryEvent
registry/static_reg.rs parse "id@host:port,..." -> all alive, empty changes
registry/simulated.rs  control API add/remove/set_health/set_address (tests)
registry/control.rs    WorkerRegistryControl write seam (simulated adapter + noop)
health/probe.rs        OPTIONS keepalive loop (fan-out / sleep / reap / classify), tokio::time
load_observer.rs       WorkerLoadObserver — BAND CLASSIFICATION ONLY (AIMD bucket deferred)
self_gate.rs           ProxySelfGate STUB — always-admit (TODO)
security/hmac.rs       HmacKeyProvider trait + static impl (current/previous keys), constant-time verify
observability/metrics.rs        ProxyMetrics atomics counters/gauges
observability/metrics_server.rs Prometheus /metrics HTTP endpoint (tokio)
observability/logger.rs         structured routing-decision log
```

### Trait seams (mirror the `sip-net` `SignalingNetwork` async-trait pattern)

- **`RoutingStrategy`** (`strategy.rs`): `name`, `select_for_new_dialog(msg, opts)
  -> Result<ProxyAddr, SelectError>`, `decode_stickiness(params, msg) ->
  DecodeResult`, `encode_stickiness(target, msg) -> Option<RouteParams>`.
  `DecodeResult = {Forward, ForwardBackup, Reject{status,reason}, Unknown}`;
  `SelectError = {NoTarget, RateCapExhausted{retry_after_sec, worker_id}}` (the
  latter dead this slice). `RouteParams = BTreeMap<String,String>` (deterministic).
- **`WorkerRegistry`** (`registry/mod.rs`): `snapshot()`, `resolve(id)`,
  `lookup_by_address(addr)` — all **sync, lock-free** (registry behind
  `arc_swap::ArcSwap<Vec<WorkerEntry>>`); `changes() -> broadcast::Receiver`.
  `WorkerEntry{id,address,health,draining_since,first_seen_at_ms}`;
  `WorkerHealth = Unknown|Alive|NotReady|Draining|Dead`.
- **`WorkerRegistryControl`** (`registry/control.rs`): `set_health`, plus the
  simulated adapter's `add/remove/set_address`; noop impl for static mode.
- **`HmacKeyProvider`** (`security/hmac.rs`): `sign(input) -> {kid, mac:[u8;32]}`,
  `verify_truncated(input, kid, mac16) -> bool` (`subtle` constant-time; tries
  current then previous key). Static impl only.

### ProxyCore data path (the two big ports)

`core/request.rs` (each step a private fn for unit-testability):
Max-Forwards decrement → 483; ACK hop-by-hop absorption via LRU; top-Route strip
+ cookie param capture; source-based worker-outbound classification; **self-gate
(stub admit)**; target selection (CANCEL reuse → loose-route next hop →
worker-outbound R-URI → `decode_stickiness` forward/backup/reject/unknown →
`select_for_new_dialog`); received/rport stamping on top Via; Record-Route
insertion w/ cookie + `;lr` for INVITE/SUBSCRIBE; push our Via (branch from
`IdGen`, reuse for CANCEL); remember `(Call-ID|CSeq#)→{target,branch}`; serialize
+ send.

`core/response.rs`: ≥2 Via or drop; top Via must be us; next-Via received/rport;
reverse-path failover (dest worker not Alive → decode cookie `w_bak` → reroute,
else drop); pop top Via entry (`remove_first_header_entry`, comma-aware);
serialize + send; non-2xx INVITE final → synthesize hop-by-hop ACK
(`generators::generate_proxy_ack_for_non_2xx`). Drop the `;net=`/dual-fabric
registrar branching entirely (out of scope).

### Reuse (do not reimplement)

- `sip-message::generators` — `generate_response`, `generate_proxy_ack_for_non_2xx`
  (L693), `stamp_received_rport_on_via` (L235, promote `pub(crate)`→`pub` or
  re-expose), out-of-dialog OPTIONS builder (for the probe).
- `sip-message::message_helpers` — `parse_sip_uri`, `parse_via_params`,
  `get_header(s)`, `is_emergency_request`, `set_header`, `remove_header`.
- `sip-net` — `SignalingNetwork`/`UdpEndpoint`, `SimulatedSignalingNetwork`,
  `with_all_contracts`, `to_sip_entries`, `BindUdpOpts`, `PacketQueue` tail-drop.
- `sip-txn::IdGen`, `sip-clock::Clock`.

### Harness SUT seam (`scenario-harness`, test-only)

New `src/proxy_sut.rs`: `Harness::with_proxy(name, addr, ProxyConfig) -> ProxySut`
binds the proxy on `self.network` (the shared recording sim), registers a
`NetworkTag::Core` lane, and `tokio::spawn`s `ProxyCore::run()` (a task parked on
`recv().await`, woken by the sim delivery timer like an agent — the faithful SUT
model under the paused clock). `ProxySut` owns the `JoinHandle` (aborted on drop)
and exposes `addr` + `metrics`. Also `options_responder(agent, mode)` — a
simulated B2BUA that answers OPTIONS 200 / 503+Reason / silence (stands in for the
unported real B2BUA in `options-e2e`). `src/agent.rs`: add `Invite::through(&ProxySut)`
(send to proxy addr, keep R-URI = peer; learn `remote_target` from relayed
Contact) for the transparency dual-mode (direct vs withProxy).

### Cargo

New `[workspace.dependencies]`: `hmac = "0.12"`, `sha2 = "0.10"`, `sha1 = "0.10"`,
`base64 = "0.22"`, `subtle = "2"`, `arc-swap = "1"` (each with a one-line rationale
comment, matching the existing root-manifest style: SHA-1 = non-crypto HRW hash;
HMAC-SHA256-trunc-128 = cookie sig; `subtle` = `timingSafeEqual` analogue).
`sip-proxy` deps: `sip-message`, `sip-net`, `sip-clock`, `sip-txn` (IdGen only),
`tokio`, `tokio-util`, `async-trait`, `thiserror` + the six above; dev-deps
`scenario-harness`, `layer-harness`. The Prometheus HTTP server uses a tiny hand
hand-rolled tokio TCP handler (no new HTTP framework dep).

## Porting order (de-risk first)

1. Pure units, no async: `addr`, `rendezvous`, `security/hmac`, `headers` (+ unit tests).
2. `registry` (mod/static/simulated/control) + `cancel_lru` (+ tests).
3. `observability` (metrics/logger/metrics_server) (+ tests).
4. `core` transit with `ForwardAll` + `self_gate` stub — **gates the SUT seam**;
   validate `transit-only/*`.
5. `load_balancer` + `load_observer` (band-only); validate `load-balancer/*`.
6. `health/probe` + `options_responder`; validate `transparency/*` + `options-e2e`.

## Test-port mapping

Legend: **PN** unit/pure · **SUT** needs harness SUT seam · **DEFER**.

| Source test | Rust home | Class |
|---|---|---|
| RendezvousHash (in distribution) | `strategies/rendezvous.rs` tests | PN |
| `security/hmac` + `load-balancer/hmac-tampering-rejected` | `security/hmac.rs`, `load_balancer.rs` tests | PN |
| `load-balancer/distribution`, `add-remove-resharding`, `initial-health`, `unresolvable-id-falls-back`, `decode-forward-not-ready`, `decode-forward-respawn-window`, `cookie-route-fallback` | `strategies/load_balancer.rs` tests | PN |
| `registry/static`, `registry/simulated` | `registry/*.rs` tests | PN |
| `observability/metrics`, `logger`, `metrics-server` | `observability/*.rs` tests | PN |
| `load-balancer/cancel-keyed-by-callid-cseq` | `cancel_lru.rs` unit + SUT cross-check | PN+SUT |
| `transit-only/*` (bind-echo, invite-200-ack-bye, response-routing-by-via, cancel-during-ringing, reinvite-in-dialog, malformed) | `tests/transit_only.rs` (raw DSL) | SUT |
| `load-balancer/callid-routing-guard` | `tests/load_balancer_routing.rs` | SUT |
| `transparency/*` (happy-call, reinvite, cancel-during-ringing, draining) | `tests/transparency.rs` (direct vs `.through(proxy)`) | SUT |
| `transparency/health-probe`, `health-probe-late-reply` | `tests/health_probe.rs` + `options_responder` | SUT |
| `integration/options-end-to-end` | `tests/options_e2e.rs` vs simulated B2BUA responder | SUT (retargeted) |
| `load-balancer/selectForNewDialog-overload` + AIMD-bucket cases of `WorkerLoadObserver` | — | DEFER (AIMD bucket deferred) |
| `b2bua/options-readiness-distinct`, `draining-options` | — | DEFER (real B2BUA OPTIONS handler) |
| `failover/**`, `registry/kubernetes`, `registrar/**`, `registrar-503-on-drop`, `ingress-concurrency`, `lint-negative/forbidden-import` | — | DEFER / N-A |

## Un-ported, with justification (carried into MIGRATION_STATUS)

- **Registrar path** — REGISTER handling + dual-endpoint mode; user scope. Removes
  `handleRequestRegistrarMode` + `;net=` egress tag + `coreEndpoint`.
- **`ProxySelfGate` real impl** (ELU EWMA + CPS bucket) — stubbed always-admit;
  overload now = OPTIONS health/band + `sip-net` tail-drop.
- **AIMD per-worker rate-cap token bucket** — deferred (user scope B); band
  classification ported, bucket + `selectForNewDialog-overload` deferred.
- **Kubernetes registry** + k8s HMAC fs-watch — production-only; static+simulated
  cover the slice.
- **Failover / replication / call-limiter** — depend on unported B2BUA call cache
  + replication; only the proxy *request-path* `forwardBackup`/reverse-path
  mechanics are ported + tested.
- **Real B2BUA OPTIONS handler** — `options-e2e` retargeted to a simulated
  responder; `b2bua/{options-readiness-distinct,draining-options}` deferred.
- **`forbidden-import` lint** — superseded by Cargo crate-dependency boundaries.

## Docs

- **ADR-0009** `docs/adr/0009-front-proxy-rust-shape.md` (0007 format): X1 stateless
  proxy reuses `IdGen`, not the txn FSMs, LRU for CANCEL/ACK; X2 harness SUT seam
  (extends ADR-0006), new `Core` lane; X3 cookie = HMAC-SHA256-trunc-128 base64url,
  HRW = SHA-1 (deps justified); X4 self-gate stubbed; X5 lock-free registry
  (`ArcSwap`) + `broadcast` changes; X6 AIMD bucket deferred.
- **MIGRATION_STATUS.md** — new "Front proxy + LB" row + a "Slice 9" section
  (source→Rust table, ported-tests table, un-ported list). Record the SHA above.

## Verification

- `cargo test -p sip-proxy` green (unit + integration via the harness SUT).
- `cargo test -p scenario-harness` green (SUT seam additive, existing 2 e2e tests
  still pass).
- `cargo build` whole workspace + `cargo clippy -p sip-proxy` clean.
- Spot-check a withProxy happy-call report: two Via deep + Record-Route present;
  in-dialog BYE routes back through the proxy; metrics counters move.
