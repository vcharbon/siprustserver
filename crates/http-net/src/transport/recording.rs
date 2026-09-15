//! `RecordingHttpNetwork` — a decorator that tees every client exchange into a
//! shared capture sink, stamped with the injected [`Clock`]'s timestamp.
//!
//! Wraps any [`HttpTransport`]; [`serve`](HttpTransport::serve) passes straight
//! through, while each [`request`](HttpTransport::request) records what was sent
//! and what came back (response status/body or transport error). This is the
//! raw feed test assertions read via [`captured`](RecordingHttpNetwork::captured).
//!
//! The row is opened BEFORE the inner request is awaited and settled when it
//! answers, so a caller that stops waiting — a `timeout` around the call drops
//! the future at that await — leaves the exchange it made rather than a gap the
//! record cannot explain. The stamp is therefore the instant the request went
//! out, which is where a ladder puts it.
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

/// What an exchange says of itself when the caller stopped waiting for it.
const ABANDONED: &str = "timed out: the caller dropped the request before an answer";

/// One captured request/response exchange with endpoint + timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedExchange {
    /// When the request WENT OUT (ms), from the injected `Clock`.
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

/// A row already in the sink, waiting for its outcome.
///
/// Settled by [`settle`](OpenRow::settle) when the inner request answers, and
/// by its own `Drop` when the caller gave up first: either way the row states
/// how the exchange ended.
struct OpenRow {
    sink: Sink,
    index: usize,
    settled: bool,
}

impl OpenRow {
    /// Open a row for `exchange`, whose outcome is not known yet.
    fn open(sink: &Sink, exchange: CapturedExchange) -> Self {
        let mut rows = sink.lock().unwrap();
        rows.push(exchange);
        OpenRow { sink: Arc::clone(sink), index: rows.len() - 1, settled: false }
    }

    fn write(&self, outcome: ExchangeOutcome) {
        if let Some(row) = self.sink.lock().unwrap().get_mut(self.index) {
            row.outcome = outcome;
        }
    }

    /// State how the exchange ended. The row is then done with.
    fn settle(mut self, outcome: ExchangeOutcome) {
        self.settled = true;
        self.write(outcome);
    }
}

impl Drop for OpenRow {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.write(ExchangeOutcome::Error(ABANDONED.to_string()));
    }
}

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
        Self { inner, sink: Arc::new(Mutex::new(Vec::new())), clock }
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
        let row = OpenRow::open(
            &self.sink,
            CapturedExchange {
                at_ms: self.clock.now_ms(),
                dst,
                method: req.method.clone(),
                path: req.path.clone(),
                req_headers: req.headers.clone(),
                req_body: req.body.clone(),
                outcome: ExchangeOutcome::Error(ABANDONED.to_string()),
            },
        );
        let result = self.inner.request(dst, req).await;
        row.settle(match &result {
            Ok(resp) => ExchangeOutcome::Response {
                status: resp.status,
                headers: resp.headers.clone(),
                body: resp.body.clone(),
            },
            Err(e) => ExchangeOutcome::Error(e.to_string()),
        });
        result
    }
}
