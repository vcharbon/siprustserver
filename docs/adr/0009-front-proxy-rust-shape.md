# Front proxy + load balancer Rust shape

**Status:** accepted

The [appendix](appendix/0009-front-proxy-rust-shape.md) holds the full routing rules of X1 and
the memo, the paths without it and the accepted residuals of X7.

## Context

The SIP front proxy (`crates/sip-proxy`) is a stateless RFC 3261 §16 proxy. It fans new dialogs
across a pool of B2BUA workers, pins in-dialog traffic to the chosen worker by a signed
Record-Route cookie, and tracks worker liveness with OPTIONS health probes. It carries the data
path, the load balancer, the worker registry (static and simulated), the health probing and the
metrics. It has no registrar or REGISTER path.

## Decision X1. Stateless proxy: reuse `IdGen`, not the transaction FSMs

`ProxyCore` manipulates Via, Record-Route and Route directly and forwards, never instantiating a
transaction state machine. It reuses only `sip-txn::IdGen` for Via branches and
`sip-clock::Clock` for timestamps. CANCEL and ACK correlation is a proxy-local LRU
(`cancel_lru`) keyed on the INVITE transaction as received (top-Via branch and sent-by). A CANCEL
matching no remembered INVITE is forwarded statelessly (§16.10) as X7 places it. Reverse-path
failover reads the backup from this proxy's own Record-Route cookie, or from the cookie on the
Via it pushed. These direction rules hold on the internal face only (ADR-0048).
Detail: [appendix X1](appendix/0009-front-proxy-rust-shape.md#decision-x1-stateless-proxy-reuse-idgen-not-the-transaction-fsms).

## Decision X2. The scenario harness gains a System-Under-Test seam

The proxy is a real SUT of the recording-first scenario harness (ADR-0006).
`Harness::bind_sut(name, addr)` binds a raw endpoint on the same recording-wrapped
`SimulatedSignalingNetwork` the agents use and registers a `Core` lane for it. The proxy runs its
own recv loop in a spawned task, woken by the sim's delivery timer like an agent. Every hop flows
through the recorder, so the recording stays the trace. `scenario-harness` has no dependency on
`sip-proxy`: the SUT glue lives in `sip-proxy`'s `tests/common`, so the only crate edge is
`sip-proxy [dev] → scenario-harness`.

## Decision X3. Cookie = HMAC-SHA256-trunc-128; HRW = SHA-1 (non-crypto)

The stickiness cookie (`v=3|w_pri|w_bak|e|c`), on a Record-Route entry or on the proxy's own Via
(X1), is signed with HMAC-SHA256 (`hmac` + `sha2`), truncated to 128 bits (RFC 4868 §2.6), and
base64url-no-pad encoded (`base64`). Verify compares the prefix in constant time (`subtle`),
trying the current then the previous key. Rendezvous (HRW) worker selection uses SHA-1 (`sha1`)
as a fast, well-distributed hash only: the top 8 bytes as a `u64`, multiplied by weight in
`u128`. The root manifest justifies the six deps (`hmac`/`sha2`/`sha1`/`base64`/`subtle`/
`arc-swap`).

## Decision X4. Proxy self-gate; overload = self-gate + OPTIONS health + tail-drop

The proxy's own load gate is `self_gate::EluCpsGate`, on by default (`PROXY_SELF_GATE`, ADR-0046 §5).
`PROXY_SELF_GATE=0` selects the always-admit `self_gate::AlwaysAdmitGate`.
Overload protection rests on OPTIONS-driven worker health and band classification (the LB filters
`above_critical` workers from new-dialog selection) and on `sip-net`'s receive-buffer tail-drop
(`PacketQueue`).

## Decision X5. Lock-free registry snapshot; `broadcast` change stream

The routing hot path reads `snapshot`, `resolve` and `lookup_by_address` synchronously and
lock-free: the worker set lives behind an `arc_swap::ArcSwap<Vec<WorkerEntry>>`. Only background
mutators (the health probe through the `WorkerRegistryControl` seam, the k8s membership watcher, ADR-0012 D4) write.
`changes()` is a `tokio::sync::broadcast` of deltas, with no backfill.

## Decision X6. Worker overload: band classification and the per-worker AIMD bucket

`WorkerLoadObserver` runs the per-worker ELU-band state machine (with hysteresis) fed by the
`X-Overload` OPTIONS payload, so `above_critical` workers are excluded from non-emergency
new-dialog selection. Each observed worker has an AIMD rate-cap token bucket
(`try_consume_for` / cooldown), and a non-emergency new dialog spends one token.

The spill rule. The new dialog goes to the first candidate, in rendezvous order for its Call-ID,
whose bucket has a token (`sip_proxy_lb_rate_cap_spilled_total`). The proxy answers `503` with
`Retry-After` (`RateCapExhausted`, the earliest refill among the candidates) only when no
candidate has a token. Emergency and in-dialog requests take the rendezvous winner and spend no
token. A retransmission repeats its first selection from the memo, and a copy of a refused INVITE
draws the same refusal (RFC 3261 §8.2.7). A CANCEL or ACK no memo places spends no token (X7).

## Decision X7. Two active instances: the memo, and every path without it

Two proxy instances run active behind one advertised address, and an ECMP hop hashes each flow on
its own. A call placed on its rendezvous winner routes from either instance with no memo. Each
instance keeps a branch memo (`cancel_lru`) for the exceptions, sized so it evicts only under
abnormal load. Without the memo every request of an admitted transaction still routes and meets
no gate: by its Route and Request-URI, by its cookie, or to the rendezvous winner. Every crossing
has a cell in `sip-proxy` `tests/it/active_active`.
Detail: [appendix X7](appendix/0009-front-proxy-rust-shape.md#decision-x7-two-active-instances-the-memo-and-every-path-without-it).

## Consequences

- `sip-proxy` is a near-leaf production crate (deps: `sip-message`, `sip-net`, `sip-clock`,
  `sip-txn`-for-`IdGen`, tokio, the six hashing and concurrency crates).
- The metrics layer is atomics-backed (`ProxyMetrics`) with a hand-rolled tokio Prometheus
  `/metrics` HTTP server (no HTTP framework dep).
- The harness can drive any in-process SUT.
- Not ported (MIGRATION_STATUS slice 9): the registrar/REGISTER path, and the failover and
  replication tests.
- HA is ADR-0056. Two anti-affined replicas run active-active, each node announcing the VIPs over
  BGP while its proxy is Ready, and X7 states what two active instances need. Each proxy's rate
  budgets stay its own. The stateless §16 design and the HMAC cookie (X3) let either replica serve
  in-dialog traffic, and the stable VIP keeps Record-Route valid across a flow move or a restart.
