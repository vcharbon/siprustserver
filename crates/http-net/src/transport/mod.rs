//! The unary HTTP transport seam: traits, DTOs, and error types.

use std::net::SocketAddr;

use async_trait::async_trait;

mod recording;
#[cfg(feature = "real")]
mod real;
mod simulated;

#[cfg(feature = "real")]
pub use real::RealHttpNetwork;
pub use recording::{CapturedExchange, Direction, ExchangeOutcome, RecordingHttpNetwork};
pub use simulated::{Fault, SimulatedHttpNetwork};

/// A one-shot HTTP request. Headers are intentionally omitted — the
/// content-type is fixed `application/json` and framing is Content-Length, set
/// by the real transport; the simulated fabric moves the [`body`](Self::body)
/// bytes verbatim. Keep this minimal: it is the whole wire contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// e.g. `"POST"` / `"GET"`.
    pub method: String,
    /// Request target, e.g. `"/v1/admit"`.
    pub path: String,
    /// Opaque body bytes (JSON, set by the consumer).
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// A `POST <path>` carrying `body`.
    pub fn post(path: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: "POST".into(),
            path: path.into(),
            body,
        }
    }

    /// A `GET <path>` with an empty body.
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            path: path.into(),
            body: Vec::new(),
        }
    }
}

/// A one-shot HTTP response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code (e.g. `200`, `404`).
    pub status: u16,
    /// Opaque body bytes.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A `200 OK` carrying `body`.
    pub fn ok(body: Vec<u8>) -> Self {
        Self { status: 200, body }
    }

    /// A status-only response with an empty body.
    pub fn status(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
        }
    }
}

/// The server side: an application that answers requests. The simulated fabric
/// invokes this **in-process** (the real handler runs, after the transit
/// delay), so a test drives client → fabric → real-server → fabric → client
/// deterministically under a paused clock.
#[async_trait]
pub trait HttpService: Send + Sync {
    /// Handle one request and produce a response. Infallible at this layer —
    /// application errors travel as non-2xx [`HttpResponse`]s; only the
    /// *transport* fails with [`HttpError`].
    async fn handle(&self, req: HttpRequest) -> HttpResponse;
}

/// A bound server. Dropping it deregisters the service (simulated) / shuts the
/// listener down (real), so further requests to its address fail with
/// [`HttpError::Connect`].
pub trait HttpServerHandle: Send + Sync {
    /// The bound local address (single source of truth for the URL the client
    /// dials).
    fn local_addr(&self) -> SocketAddr;
}

/// The transport seam: bind a handler on one side, fire requests at it from the
/// other. Real, simulated, and recording impls all wear this trait so the
/// consumer is wired identically in production and in tests.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Bind `service` at `addr`. The returned handle owns the binding.
    async fn serve(
        &self,
        addr: SocketAddr,
        service: std::sync::Arc<dyn HttpService>,
    ) -> Result<Box<dyn HttpServerHandle>, BindError>;

    /// Fire a one-shot request at `dst` and await the response. The caller owns
    /// the timeout (wrap this in `tokio::time::timeout`); the transport only
    /// reports transport-level failures.
    async fn request(&self, dst: SocketAddr, req: HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// Failure binding a server.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BindError {
    /// Another server already owns this local address.
    #[error("address already in use: {0}")]
    AlreadyInUse(SocketAddr),
    /// Underlying I/O failure (real transport only).
    #[error("bind io error: {0}")]
    Io(String),
}

/// A transport-level request failure. Distinct from a non-2xx response: a
/// [`HttpResponse`] with `status >= 400` is still `Ok` here. The caller maps
/// **both** any `HttpError` and a timeout to "backend unavailable" (fail-open).
#[derive(Debug, Clone, thiserror::Error)]
pub enum HttpError {
    /// Could not reach a server at `dst` — nothing bound there, or the
    /// destination was cut ([`Fault::Cut`]) / refused.
    #[error("connection refused: no server at {0}")]
    Connect(SocketAddr),
    /// The connection was established but then failed mid-flight
    /// ([`Fault::ErrorAfter`]: ECONNRESET-style), or a real I/O error.
    #[error("io error to {addr}: {reason}")]
    Io {
        /// The destination that failed.
        addr: SocketAddr,
        /// Human-readable cause (for logs / recording).
        reason: String,
    },
}
