//! The stub: its mappings, its default answer, its record and its shutdown,
//! shared by every listener it serves.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::Instant;

use super::answer::StubAnswer;
use super::mapping::StubMapping;
use super::query;
use super::record::StubRecord;
use crate::{HttpAnswer, HttpRequest, HttpResponse, HttpService};

/// A stub as a document states it: where each listener binds, the default
/// answer, the mappings in the order they are tried.
#[derive(Clone, Debug, Deserialize)]
pub struct StubSpec {
    /// Listener name → the address its caller binds it at.
    pub listeners: BTreeMap<String, SocketAddr>,
    /// What a request no mapping matches gets.
    pub default: StubAnswer,
    /// The mappings, tried in order.
    #[serde(default)]
    pub mappings: Vec<StubMapping>,
}

/// The stub. Clones share its mappings, record and shutdown.
#[derive(Clone)]
pub struct Stub {
    shared: Arc<Shared>,
}

struct Shared {
    mappings: Vec<StubMapping>,
    default: StubAnswer,
    records: Mutex<Vec<StubRecord>>,
    /// When the stub was made: what a record's `at` counts from.
    born: Instant,
    /// `true` once shut down; every answer waits on it beside its delay.
    stopped: watch::Sender<bool>,
}

impl Stub {
    /// A stub answering `mappings`, first match first, and `default` otherwise.
    pub fn new(mappings: Vec<StubMapping>, default: StubAnswer) -> Self {
        Self {
            shared: Arc::new(Shared {
                mappings,
                default,
                records: Mutex::new(Vec::new()),
                born: Instant::now(),
                stopped: watch::Sender::new(false),
            }),
        }
    }

    /// The stub `spec` states; its listeners' addresses are the caller's to bind.
    pub fn from_spec(spec: &StubSpec) -> Self {
        Self::new(spec.mappings.clone(), spec.default.clone())
    }

    /// The service of the listener `name`, for a transport to bind.
    pub fn listener(&self, name: impl Into<String>) -> Arc<dyn HttpService> {
        Arc::new(Listener { stub: self.clone(), name: name.into() })
    }

    /// Every request received so far, in arrival order.
    pub fn records(&self) -> Vec<StubRecord> {
        self.shared.records.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// End every pending answer, and every later one, with a close at once.
    pub fn shutdown(&self) {
        self.shared.stopped.send_replace(true);
    }

    /// The answer `req` gets on `listener`, recorded on arrival, sent once
    /// its delay ran or as a close at the shutdown, whichever comes first.
    async fn answer(&self, listener: &str, req: HttpRequest) -> HttpAnswer {
        let found =
            self.shared.mappings.iter().find(|m| m.matches(listener, &req.method, &req.path));
        self.record(listener, req, found);
        let answer = found.map_or(&self.shared.default, |m| &m.answer);
        let mut stopped = self.shared.stopped.subscribe();
        tokio::select! {
            biased;
            _ = stopped.wait_for(|stopped| *stopped) => HttpAnswer::Abort,
            () = tokio::time::sleep(answer.delay) => answer.to_http(),
        }
    }

    fn record(&self, listener: &str, req: HttpRequest, found: Option<&StubMapping>) {
        let (path, query) = query::split(&req.path);
        let unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let record = StubRecord {
            at: self.shared.born.elapsed(),
            at_unix_ms: u64::try_from(unix.as_millis()).unwrap_or(u64::MAX),
            listener: listener.to_string(),
            method: req.method.clone(),
            path: path.to_string(),
            query: query.map(str::to_string),
            headers: req.headers,
            body: req.body,
            matched: found.is_some(),
            tag: found.and_then(|m| m.tag.clone()),
        };
        self.shared.records.lock().unwrap_or_else(PoisonError::into_inner).push(record);
    }
}

/// One listener of a [`Stub`]: what it answers is the stub's.
struct Listener {
    stub: Stub,
    name: String,
}

#[async_trait]
impl HttpService for Listener {
    /// A close reads as `502 Bad Gateway` here; transports call [`answer`](Self::answer).
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        match self.answer(req).await {
            HttpAnswer::Response(resp) => resp,
            HttpAnswer::Abort => HttpResponse::status(502),
        }
    }

    async fn answer(&self, req: HttpRequest) -> HttpAnswer {
        self.stub.answer(&self.name, req).await
    }
}
