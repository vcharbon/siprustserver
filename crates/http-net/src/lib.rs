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
//! ## The production impl
//! `RealHttpNetwork` — hyper server + a pooled `reqwest` client; **feature
//! `real`**. Its tests run on a real (non-paused) runtime. The server listens
//! with a 4096 backlog, sets `TCP_NODELAY` on every connection, and answers
//! each request with its [`Receipt`] in scope (when its head arrived, before
//! the body) and the connection's remote address in [`peer`] scope.
//!
//! The simulated fabric (in-memory, per-destination faults, for the
//! paused-clock tests), the recording decorator and the scripted test service
//! implement the same seam in `http-net-testkit`.
//!
//! ## Failure counters
//! The real transport classifies every client failure into
//! `http_request_failures_total{peer, cause}` ([`failures`]) — counters only,
//! never a per-request log line (ADR-0026). A runner appends
//! [`failures::prometheus_text`] to its `/metrics` body.
//!
//! ## Backend clients
//! A server calling an HTTP backend on its request path does it through a
//! [`BackendClient`] (ADR-0043): one per backend, its name looked up through a
//! [`ResolvedTarget`] (bounded, single-flight), its connections held as
//! [`ClientSettings`] state, a cap on requests in flight, a client rotated on a
//! fixed period, a circuit breaker, and the generic retry rule. The transport
//! reports the honest outcome (`Ok(response)` or a classified [`HttpError`]);
//! the budgets and what a failure means for the caller (fail the call, let
//! it through) are the caller's policy.

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

pub mod backend;
pub mod failures;
pub mod poison;
mod resolved_target;
mod resolver;
pub mod settings;
mod transport;

pub use backend::{AttemptError, BackendClient, BackendConfig, BreakerConfig, Budgets};
pub use resolved_target::{Addresses, LookupSettings, ResolvedTarget, LOOKUP_BOUND, MAX_STUCK};
pub use resolver::{MemoryResolver, NameResolver, SystemResolver};
pub use settings::ClientSettings;
#[cfg(feature = "real")]
pub use transport::RealHttpNetwork;
pub use transport::{
    peer, AddressedClient, BindError, HttpAnswer, HttpClient, HttpError, HttpRequest, HttpResponse,
    HttpServerHandle, HttpService, HttpTransport, Receipt, MAX_BODY_BYTES,
};
