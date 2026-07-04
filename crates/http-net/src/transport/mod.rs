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

/// A one-shot HTTP request.
///
/// [`headers`](Self::headers) and query strings (carried inline on
/// [`path`](Self::path), which is really a *path-and-query* target) are honored
/// end to end: the real transport maps them onto/off the wire, and the
/// simulated fabric moves the whole struct verbatim. Both default to empty, so
/// a header-agnostic service (e.g. the call-limiter, whose framing is a fixed
/// `application/json` Content-Length body set by the transport) is unchanged.
/// Any richer REST backend (query params, request headers) rides the same seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// e.g. `"POST"` / `"GET"`.
    pub method: String,
    /// Request target *including any query string*, e.g. `"/v1/admit"` or
    /// `"/routes?debug=true&seed=7"`. The real transport carries this as the
    /// URI's `path_and_query`.
    pub path: String,
    /// Request headers as ordered `(name, value)` pairs. Empty by default; the
    /// transport does not synthesize framing headers into this list (they are
    /// set on the wire), so a service sees only what the caller attached.
    pub headers: Vec<(String, String)>,
    /// Opaque body bytes (JSON, set by the consumer).
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// A `POST <path>` carrying `body`, no headers.
    pub fn post(path: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: "POST".into(),
            path: path.into(),
            headers: Vec::new(),
            body,
        }
    }

    /// A `GET <path>` with an empty body, no headers.
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            path: path.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Append one `(name, value)` header (builder style).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Replace the header list wholesale (builder style).
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }
}

/// A one-shot HTTP response.
///
/// [`headers`](Self::headers) let a served service emit response headers (e.g.
/// the Routing API's `X-Newkah-Trace-Id`); empty by default so a status+body
/// service is unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code (e.g. `200`, `404`).
    pub status: u16,
    /// Response headers as ordered `(name, value)` pairs. Empty by default.
    pub headers: Vec<(String, String)>,
    /// Opaque body bytes.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A `200 OK` carrying `body`, no headers.
    pub fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body,
        }
    }

    /// A status-only response with an empty body, no headers.
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Set the body (builder style).
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// Append one `(name, value)` header (builder style).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Replace the header list wholesale (builder style).
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
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
