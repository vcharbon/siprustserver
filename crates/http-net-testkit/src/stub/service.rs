//! The stub: its mappings, the sets registered beside them, its default
//! answer, its record and its shutdown, shared by every listener it serves
//! and its control service.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::Instant;

use super::answer::StubAnswer;
use super::control::Control;
use super::mapping::StubMapping;
use super::query;
use super::record::StubRecord;
use crate::{HttpAnswer, HttpRequest, HttpResponse, HttpService};

/// A stub as a document states it: where each listener binds, where its
/// control service binds (none where absent), the default answer, the
/// mappings in the order they are tried.
#[derive(Clone, Debug, Deserialize)]
pub struct StubSpec {
    /// Listener name → the address its caller binds it at.
    pub listeners: BTreeMap<String, SocketAddr>,
    /// Where the control service ([`Stub::control`]) binds.
    #[serde(default)]
    pub control: Option<SocketAddr>,
    /// What a request no mapping matches gets.
    pub default: StubAnswer,
    /// The mappings, tried in order, before every registered set.
    #[serde(default)]
    pub mappings: Vec<StubMapping>,
}

/// How many requests the record keeps by default: the latest ones.
pub const RECORD_CAP: usize = 100_000;

/// The stub. Clones share its mappings, record and shutdown.
#[derive(Clone)]
pub struct Stub {
    shared: Arc<Shared>,
}

struct Shared {
    /// The mappings the stub was made with, tried first.
    mappings: Vec<StubMapping>,
    /// Each registered set, by tag, in registration order, tried after them.
    sets: Mutex<Vec<(String, Vec<StubMapping>)>>,
    default: StubAnswer,
    records: Mutex<VecDeque<StubRecord>>,
    /// How many requests the record keeps.
    cap: usize,
    /// When the stub was made: what a record's `at` counts from.
    born: Instant,
    /// `true` once shut down; every answer waits on it beside its delay.
    stopped: watch::Sender<bool>,
}

impl Stub {
    /// A stub answering `mappings`, first match first, then the sets
    /// registered later, and `default` otherwise.
    pub fn new(mappings: Vec<StubMapping>, default: StubAnswer) -> Self {
        Self {
            shared: Arc::new(Shared {
                mappings,
                sets: Mutex::new(Vec::new()),
                default,
                records: Mutex::new(VecDeque::new()),
                cap: RECORD_CAP,
                born: Instant::now(),
                stopped: watch::Sender::new(false),
            }),
        }
    }

    /// The stub `spec` states; its listeners' addresses are the caller's to bind.
    pub fn from_spec(spec: &StubSpec) -> Self {
        Self::new(spec.mappings.clone(), spec.default.clone())
    }

    /// The same stub keeping the latest `cap` requests in its record. Called
    /// before the stub is cloned or a listener taken; a shared stub keeps its cap.
    pub fn with_record_cap(mut self, cap: usize) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.cap = cap;
        }
        self
    }

    /// The service of the listener `name`, for a transport to bind.
    pub fn listener(&self, name: impl Into<String>) -> Arc<dyn HttpService> {
        Arc::new(Listener { stub: self.clone(), name: name.into() })
    }

    /// The control service (`control.rs`), for a transport to bind.
    pub fn control(&self) -> Arc<dyn HttpService> {
        Arc::new(Control::new(self.clone()))
    }

    /// The set `tag` registered with `mappings`, replacing the set of that tag.
    pub fn register(&self, tag: &str, mappings: Vec<StubMapping>) {
        let mut sets = self.shared.sets.lock().unwrap_or_else(PoisonError::into_inner);
        match sets.iter_mut().find(|(own, _)| own == tag) {
            Some((_, held)) => *held = mappings,
            None => sets.push((tag.to_string(), mappings)),
        }
    }

    /// The set `tag` gone, and every request it answered gone from the record.
    pub fn clear(&self, tag: &str) {
        self.shared
            .sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(own, _)| own != tag);
        self.shared
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|record| record.set.as_deref() != Some(tag));
    }

    /// Every set gone, and every request a set answered gone from the record.
    pub fn clear_all(&self) {
        self.shared.sets.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.shared
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|record| record.set.is_none());
    }

    /// How many sets are registered.
    pub fn sets(&self) -> usize {
        self.shared.sets.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Every request the record keeps, in arrival order.
    pub fn records(&self) -> Vec<StubRecord> {
        self.shared.records.lock().unwrap_or_else(PoisonError::into_inner).iter().cloned().collect()
    }

    /// End every pending answer, and every later one, with a close at once.
    pub fn shutdown(&self) {
        self.shared.stopped.send_replace(true);
    }

    /// Resolves once the stub is shut down.
    pub async fn stopped(&self) {
        let mut stopped = self.shared.stopped.subscribe();
        let _ = stopped.wait_for(|stopped| *stopped).await;
    }

    /// The mapping that answers `req` on `listener`, with the tag of its set
    /// (none for one the stub was made with).
    fn found(&self, listener: &str, req: &HttpRequest) -> Option<(Option<String>, StubMapping)> {
        let matches = |m: &StubMapping| m.matches(listener, &req.method, &req.path);
        if let Some(own) = self.shared.mappings.iter().find(|m| matches(m)) {
            return Some((None, own.clone()));
        }
        let sets = self.shared.sets.lock().unwrap_or_else(PoisonError::into_inner);
        sets.iter().find_map(|(tag, held)| {
            held.iter().find(|m| matches(m)).map(|m| (Some(tag.clone()), m.clone()))
        })
    }

    /// The answer `req` gets on `listener`, recorded on arrival, sent once
    /// its delay ran or as a close at the shutdown, whichever comes first.
    async fn answer(&self, listener: &str, req: HttpRequest) -> HttpAnswer {
        let found = self.found(listener, &req);
        let answer = found.as_ref().map_or(&self.shared.default, |(_, m)| &m.answer).clone();
        self.record(listener, req, found.as_ref());
        let mut stopped = self.shared.stopped.subscribe();
        tokio::select! {
            biased;
            _ = stopped.wait_for(|stopped| *stopped) => HttpAnswer::Abort,
            () = tokio::time::sleep(answer.delay) => answer.to_http(),
        }
    }

    fn record(
        &self,
        listener: &str,
        req: HttpRequest,
        found: Option<&(Option<String>, StubMapping)>,
    ) {
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
            set: found.and_then(|(set, _)| set.clone()),
            tag: found.and_then(|(_, m)| m.tag.clone()),
        };
        let mut records = self.shared.records.lock().unwrap_or_else(PoisonError::into_inner);
        records.push_back(record);
        while records.len() > self.shared.cap {
            records.pop_front();
        }
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
