//! `http-net` — a reusable, fake-able **unary HTTP transport seam**.
//!
//! This is the HTTP analogue of `sip-net`'s `SignalingNetwork` and `repl-net`'s
//! `ReplicationNetwork`, deliberately the *simplest* of the three: a request
//! goes out, a response (or an error) comes back. No streaming, no
//! connection-oriented frame exchange — our payloads are tiny
//! Content-Length-framed JSON POSTs (the call-limiter RPCs).
//!
//! ## The seam
//! [`HttpTransport`] has two halves:
//! - a server *binds* a handler ([`HttpService`]) at an address via
//!   [`serve`](HttpTransport::serve), and
//! - a client fires a one-shot [`request`](HttpTransport::request) at that
//!   address and awaits the [`HttpResponse`].
//!
//! ## Three impls (mirror of repl-net)
//! - [`SimulatedHttpNetwork`] — an in-memory routing table keyed by
//!   `SocketAddr`. A `request` looks up the bound service, applies the per-`dst`
//!   [`Fault`] (pause / cutoff / error), waits the transit delay, and invokes
//!   the **real** handler in-process. Mandatory for the paused-clock tests:
//!   real sockets do not obey `tokio::time::pause`, so deterministic scenarios
//!   cannot use real HTTP. This is the workhorse.
//! - `RealHttpNetwork` — hyper server + a pooled `reqwest` client; **feature
//!   `real`**, filled in by the runner slice. Its tests run on a real
//!   (non-paused) runtime.
//! - [`RecordingHttpNetwork`] — a decorator that tees every client exchange
//!   into a capture sink (stamped with the injected [`sip_clock::Clock`]) for
//!   test assertions.
//!
//! ## Fail-open lives in the client, not here
//! The transport reports the honest outcome (`Ok(response)` or
//! [`HttpError`]). The *timeout budget* and the fail-open policy are the
//! caller's job (it wraps `request` in `tokio::time::timeout`): under a paused
//! clock a [`Fault::Stall`]ed request simply never completes until the caller's
//! timeout fires when the harness advances. See `b2bua::limiter_http`.

mod transport;

pub use transport::{
    BindError, CapturedExchange, Direction, ExchangeOutcome, Fault, HttpError, HttpRequest,
    HttpResponse, HttpService, HttpServerHandle, HttpTransport, RecordingHttpNetwork,
    SimulatedHttpNetwork,
};
