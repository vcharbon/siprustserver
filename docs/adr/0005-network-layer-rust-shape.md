# Network layer Rust shape (`sip-net`)

**Status:** accepted (2026-05-31)

The source's `SignalingNetwork` is an Effect service: `bindUdp` returns an
endpoint scoped to an Effect `Scope`, `send` returns an `Effect`, and inbound
packets arrive on a `Stream`. Per the migration strategy's "adapt the shape to
the available methods on Rust" directive, slice 2 reshapes this to tokio idioms.

## Decisions

- **`SignalingNetwork` + `UdpEndpoint` are `#[async_trait]` traits** — the DI
  seam (per the Effect→Rust construct map). `bind_udp` returns
  `Box<dyn UdpEndpoint>` so decorators can wrap the endpoint, mirroring the
  source's `UdpEndpoint` interface being wrappable.
- **Receiver-style endpoint, not a `Stream`.** `recv().await` / `try_recv()`
  replace the source's `Stream` + `take`/`poll`. The recording decorator
  records each `recv` inline, so the `Stream` combinator surface
  (`Stream.tap`/`ensuring`, used only by `recordStreamLifecycle` in the source)
  buys nothing here, and dropping it avoids a `futures`/`tokio-stream`
  dependency. A `messages()` stream adapter can be added if a later consumer
  (the dispatcher) wants combinators.
- **Method names track `tokio::net::UdpSocket`:** `send_to(&[u8], SocketAddr)`,
  `local_addr()`. `RemoteInfo { address, port }` → `std::net::SocketAddr`
  throughout; the ip/port pair on `bindUdp` → a single `addr: SocketAddr`.
- **Hand-rolled `PacketQueue`** (bounded `VecDeque` + `Notify`) backs both
  impls' inbound side, because the audit needs `depth()` (which `tokio::mpsc`
  doesn't expose) and the fabric needs an `offer` that reports "full" to drive
  the tail-drop counter — i.e. the source's `Queue.bounded` semantics exactly.
- **`scopedAudit` ships as a recording decorator; `paranoidInputs` as a second
  decorator.** Canonical order `paranoidInputs(scopedAudit(impl))`. Paranoid
  violations are programmer errors → `panic!` (the Rust analogue of
  `Effect.die`), surfaced via `layer_harness::ParanoidViolation`.
- **`propertyTest` and `parity` are skipped, by the same reasoning as the
  source.** `propertyTest`: `bind_udp` opens a socket and `send_to` is
  fire-and-forget UDP — no natural per-call input/output domain to assert over.
  `parity`: the real and simulated impls are not output-equivalent (one is
  wall-clock dgram, the other an in-memory fabric with virtual transit), so a
  deep-equal comparator would be meaningless.

## Deferred (tracked in MIGRATION_STATUS)

- `ConnectivityGate` (per-fiber partition gating) — belongs with the k8s
  cluster harness, not the base fabric.
- `reuse_port` (SO_REUSEPORT) — accepted on `BindUdpOpts` but not yet wired;
  tokio has no direct knob, so honoring it needs a `socket2` detour. Loopback
  tests don't need it.
- The `UdpTransport` facade (Tier-1 overload brake, Prometheus metrics shape,
  `BufferedUdpEndpoint` per-peer drainer) — depends on `AppConfig` /
  `MetricsRegistry`, which are later slices. The `PreIngressHook` primitive the
  brake is built on **is** ported.
- The legacy `NetworkTraceEntry` / `drainTrace` path — superseded by the typed
  `Recorder` channel (the single recording path in this port). The `realTracing`
  on/off boolean split therefore disappears: recording is a decorator, not a
  base-impl variant.
- The second-fabric `SignalingNetworkCore` Tag — needed only by the proxy's
  dual-bind; revisit at the proxy slice.
- The per-test RFC `exceptions` ledger on `ScopedAuditOptions` — the rule packs
  it gates land in the rules slice.
