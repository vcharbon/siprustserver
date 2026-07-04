//! `RecordingHttpNetwork` — a decorator that tees every client exchange into a
//! shared capture sink, stamped with the injected [`Clock`]'s timestamp.
//!
//! Wraps any [`HttpTransport`]; [`serve`](HttpTransport::serve) passes straight
//! through, while each [`request`](HttpTransport::request) records what was sent
//! and what came back (response status/body or transport error). This is the
//! raw feed test assertions read via [`captured`](RecordingHttpNetwork::captured).
//!
//! Mirrors `repl-net`'s recording decorator: minimal — capture only, no audit
//! rules / severity ledger.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sip_clock::Clock;

use super::{
    BindError, HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpService, HttpTransport,
};

/// Whether a captured datum was the request or the reply, for symmetry with the
/// other layers' recorders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The request the client sent.
    Sent,
    /// The reply (or transport error) the client received.
    Received,
}

/// The outcome of one recorded exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExchangeOutcome {
    /// A response came back.
    Response {
        /// HTTP status.
        status: u16,
        /// Response headers as `(name, value)` pairs.
        headers: Vec<(String, String)>,
        /// Response body bytes.
        body: Vec<u8>,
    },
    /// The transport failed (rendered to a string for capture).
    Error(String),
}

/// One captured request/response exchange with endpoint + timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedExchange {
    /// Recording timestamp (ms) from the injected `Clock`.
    pub at_ms: i64,
    /// The destination the request went to.
    pub dst: SocketAddr,
    /// Request method (e.g. `"POST"`).
    pub method: String,
    /// Request path-and-query (e.g. `"/v1/admit"` or `"/routes?debug=true"`).
    pub path: String,
    /// Request headers as `(name, value)` pairs.
    pub req_headers: Vec<(String, String)>,
    /// Request body bytes.
    pub req_body: Vec<u8>,
    /// What came back.
    pub outcome: ExchangeOutcome,
}

type Sink = Arc<Mutex<Vec<CapturedExchange>>>;

/// Records every client exchange that flows through the wrapped transport.
/// Clone shares the same capture sink, so a clone kept for `captured()` sees
/// all exchanges.
#[derive(Clone)]
pub struct RecordingHttpNetwork {
    inner: Arc<dyn HttpTransport>,
    sink: Sink,
    clock: Clock,
}

impl RecordingHttpNetwork {
    /// Wrap `inner`, stamping captures with `clock`.
    pub fn new(inner: Arc<dyn HttpTransport>, clock: Clock) -> Self {
        Self {
            inner,
            sink: Arc::new(Mutex::new(Vec::new())),
            clock,
        }
    }

    /// Snapshot every exchange captured so far.
    pub fn captured(&self) -> Vec<CapturedExchange> {
        self.sink.lock().unwrap().clone()
    }
}

#[async_trait]
impl HttpTransport for RecordingHttpNetwork {
    async fn serve(
        &self,
        addr: SocketAddr,
        service: Arc<dyn HttpService>,
    ) -> Result<Box<dyn HttpServerHandle>, BindError> {
        self.inner.serve(addr, service).await
    }

    async fn request(&self, dst: SocketAddr, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let method = req.method.clone();
        let path = req.path.clone();
        let req_headers = req.headers.clone();
        let req_body = req.body.clone();
        let result = self.inner.request(dst, req).await;
        let outcome = match &result {
            Ok(resp) => ExchangeOutcome::Response {
                status: resp.status,
                headers: resp.headers.clone(),
                body: resp.body.clone(),
            },
            Err(e) => ExchangeOutcome::Error(e.to_string()),
        };
        self.sink.lock().unwrap().push(CapturedExchange {
            at_ms: self.clock.now_ms(),
            dst,
            method,
            path,
            req_headers,
            req_body,
            outcome,
        });
        result
    }
}
