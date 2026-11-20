# Network layer Rust shape (`sip-net`)

**Status:** accepted (2026-05-31)

`sip-net`'s `SignalingNetwork` binds UDP endpoints that send datagrams and take
inbound packets, in a real (tokio) and a simulated implementation behind one
seam. Its shape follows tokio idioms.

## Decisions

- **`SignalingNetwork` + `UdpEndpoint` are `#[async_trait]` traits** — the DI
  seam. `bind_udp` returns `Box<dyn UdpEndpoint>` so decorators can wrap the
  endpoint.
- **Receiver-style endpoint, not a `Stream`.** `recv().await` / `try_recv()`
  take inbound packets. The recording decorator records each `recv` inline, so
  a `Stream` combinator surface buys nothing here, and dropping it avoids a
  `futures`/`tokio-stream` dependency. A `messages()` stream adapter can be
  added if a consumer wants combinators.
- **Method names track `tokio::net::UdpSocket`:** `send_to(&[u8], SocketAddr)`,
  `local_addr()`. Addresses are `std::net::SocketAddr` throughout; a bind takes
  a single `addr: SocketAddr`.
- **Hand-rolled `PacketQueue`** (bounded `VecDeque` + `Notify`) backs both
  impls' inbound side, because the audit needs `depth()` (which `tokio::mpsc`
  doesn't expose) and the fabric needs an `offer` that reports "full" to drive
  the tail-drop counter.
- **The scoped audit ships as a recording decorator
  (`RecordingSignalingNetwork`); the paranoid-inputs check as a second
  decorator (`ParanoidSignalingNetwork`).** Canonical order
  `ParanoidSignalingNetwork(RecordingSignalingNetwork(impl))`. Paranoid
  violations are programmer errors → `panic!`, surfaced via
  `layer_harness::ParanoidViolation`.
- **No property test and no parity check on this layer.** A property test:
  `bind_udp` opens a socket and `send_to` is fire-and-forget UDP — no natural
  per-call input/output domain to assert over. A parity check: the real and
  simulated impls are not output-equivalent (one is wall-clock dgram, the other
  an in-memory fabric with virtual transit), so a deep-equal comparator would
  be meaningless.
- **Recording is a decorator, not a base-impl variant.** The typed `Recorder`
  channel is the single recording path; no base impl carries a tracing switch.

## Around the seam

- The UDP transport's ingress brake and Prometheus metrics shape live
  in `b2bua::ingress_brake` / `b2bua::UdpTransportMetrics`, on this crate's
  `PreIngressHook`. There is no per-peer send drainer: a send takes a resolved
  `SocketAddr` and never blocks (ADR-0033), so there is nothing per peer to
  isolate.
