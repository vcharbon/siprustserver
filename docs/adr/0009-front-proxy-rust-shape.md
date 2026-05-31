# 0009 — Front proxy + load balancer Rust shape

**Status:** accepted (2026-05-31)

**Source:** sipjsserver @ `fffc4ac69c8aeef26cf48fe73469503145c9732b`,
`src/sip-front-proxy/` (`ProxyCore.ts`, `RoutingStrategy.ts`, `CancelBranchLru.ts`,
`strategies/`, `registry/`, `health/`, `observability/`).

## Context

The SIP front proxy is a **stateless** RFC 3261 §16 proxy that fans new dialogs
across a pool of B2BUA workers, pins in-dialog traffic to the chosen worker via
a signed Record-Route cookie, and tracks worker liveness with OPTIONS health
probes. It is the next migration slice (`crates/sip-proxy`) and is shared
infrastructure a future B2B2UA will sit behind. The user fixed the scope:
port the data path + load balancer + worker registry (static + simulated) +
OPTIONS health probing + the metrics layer; **exclude** the SIP registrar /
REGISTER path; **stub** the proxy self-overload gate; and port **band
classification only** of the worker-overload machinery (defer the AIMD bucket).

## Decision X1 — stateless proxy: reuse `IdGen`, not the transaction FSMs

`ProxyCore` is stateless (RFC 3261 §16): it manipulates Via / Record-Route /
Route directly and forwards, never instantiating a transaction state machine.
So `sip-proxy` does **not** depend on `sip-txn`'s FSMs; it reuses only
`sip-txn::IdGen` (the RNG seam) for Via branch generation, and `sip-clock::Clock`
for timestamps. CANCEL/ACK correlation is a proxy-local `(Call-ID|CSeq#)` LRU
(`cancel_lru`), keyed per RFC 3261 §9.1 so it works at any hop and survives the
load balancer re-sharding a fallback selection.

## Decision X2 — the scenario harness gains a System-Under-Test seam

The recording-first scenario harness (ADR-0006) had **no SUT**: its agents talk
peer-to-peer. The proxy is the first real SUT. `Harness::bind_sut(name, addr)`
binds a raw endpoint on the *same* recording-wrapped `SimulatedSignalingNetwork`
the agents use and registers a `Core` lane for it; the proxy runs its own recv
loop in a spawned task, parked on `recv().await` and woken by the sim's delivery
timer exactly like an agent. Every hop still flows through the recorder, so the
**recording remains the trace** — ADR-0006 holds unchanged, only extended.
`scenario-harness` gains **no** dependency on `sip-proxy`: the SUT glue lives in
`sip-proxy`'s test-support (`tests/common`), built on the generic `bind_sut`
seam, so the only crate edge is `sip-proxy [dev] → scenario-harness`.

## Decision X3 — cookie = HMAC-SHA256-trunc-128; HRW = SHA-1 (non-crypto)

The Record-Route stickiness cookie (`v=3|w_pri|w_bak|e|c`) is signed with
HMAC-SHA256 (`hmac` + `sha2`), truncated to the first 128 bits (RFC 4868 §2.6
short-token tradeoff), and base64url-no-pad encoded (`base64`). Verify compares
the prefix in constant time (`subtle`, the `timingSafeEqual` analogue), trying
the current then previous key (rotation overlap). Rendezvous (HRW) worker
selection uses **SHA-1** (`sha1`) purely as a fast, well-distributed hash —
top 8 bytes as a `u64`, multiplied by weight in `u128` — **not** as a crypto
primitive. These six deps (`hmac`/`sha2`/`sha1`/`base64`/`subtle`/`arc-swap`)
are justified in the root manifest.

## Decision X4 — proxy self-gate stubbed; overload = OPTIONS health + tail-drop

The source `ProxySelfGate` (ELU EWMA + CPS token bucket on the proxy's own
load) is replaced by an always-admit stub (`self_gate::AlwaysAdmitGate`). The
seam + the request path's admission branch + the `note_bypass` calls are wired
exactly as the real gate will need, so it drops in later with no surface change.
Until then, overload protection relies on (a) OPTIONS-driven worker health/band
classification (the LB filters `above_critical` workers from new-dialog
selection) and (b) `sip-net`'s receive-buffer tail-drop (`PacketQueue`).

## Decision X5 — lock-free registry snapshot; `broadcast` change stream

The routing hot path reads `snapshot`/`resolve`/`lookup_by_address`
synchronously and lock-free — the worker set lives behind an
`arc_swap::ArcSwap<Vec<WorkerEntry>>`; only background mutators (the health
probe via the `WorkerRegistryControl` seam, a future k8s watcher) write.
`changes()` is a `tokio::sync::broadcast` of deltas (no backfill). This is the
Rust expression of the source's D4 non-blocking invariant (`Ref.get` reads).

## Decision X6 — worker overload: band classification only, AIMD bucket deferred

`WorkerLoadObserver` ports the per-worker ELU-band state machine (with
hysteresis) fed by the `X-Overload` OPTIONS payload, so `above_critical`
workers are excluded from non-emergency new-dialog selection. The per-worker
**AIMD rate-cap token bucket** (`try_consume_for` / cooldown) is **deferred**
(user scope): `select_for_new_dialog` never raises `RateCapExhausted`. That
error variant and the request path's 503/`Retry-After` branch are kept (dead)
so the bucket lands later with no surface change.

## Consequences

- `sip-proxy` is a near-leaf production crate (deps: `sip-message`, `sip-net`,
  `sip-clock`, `sip-txn`-for-`IdGen`, tokio, the six hashing/concurrency crates).
- The metrics layer is atomics-backed (`ProxyMetrics`) with a hand-rolled tokio
  Prometheus `/metrics` HTTP server (no HTTP framework dep).
- The harness can now drive any in-process SUT, not just the proxy — useful for
  the B2BUA slice.
- Deferred (see MIGRATION_STATUS slice 9 "Un-ported"): the registrar/REGISTER
  path, the real self-gate, the AIMD bucket, the kubernetes registry, and the
  failover/replication tests.
