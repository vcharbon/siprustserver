//! The client side of the real transport: [`RealClient`], a pooled
//! hyper-util HTTP/1.1 client under [`ClientSettings`] that closes the
//! connection that misbehaves ([`crate::ejection`]), and [`NamedClient`], one
//! backend's client.
//!
//! One request on a [`RealClient`]:
//!
//! - It is bounded by the settings' ceiling, its connect included; past it it
//!   fails [`FailureCause::RequestTimeout`].
//! - Its answer's status goes to the client's [`Ejector`]: a connection the
//!   rules close is poisoned before the request yields again, so the pool
//!   never hands it out, and closes once the answer is read.
//! - Its body is read under `max_body_bytes` (`capped_body`): past it the
//!   answer fails [`FailureCause::BodyTooLarge`] and its connection closes.
//! - A request dropped, or cut by the ceiling, before its answer was read
//!   whole abandons its connection: poisoned, closed by a reset, so a late
//!   answer on it is never read as another request's.
//!
//! A [`NamedClient`]'s URL carries its target as configured, so `Host` is the
//! backend's name, and every name lookup of the client goes through the
//! target (`target_resolve`): bounded, single-flight, every address
//! returned, dropped when the target forgets it. No redirect is followed: a
//! 3xx is the endpoint's answer, returned to the caller as its status.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::Full;
use hyper::http::Extensions;
use hyper::{Method, Request};
use hyper_util::client::legacy::connect::{capture_connection, CaptureConnection};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use super::capped_body::{self, BodyError};
use super::connection::{ClientLife, Connector, Tag, Tracked};
use super::{HttpClient, HttpError, HttpRequest, HttpResponse};
use crate::backend::metrics::{self, BodyRefusal};
use crate::ejection::Ejector;
use crate::failures::FailureCause;
use crate::resolved_target::ResolvedTarget;
use crate::settings::ClientSettings;

/// A pooled `http://` client and what it keeps beside its pool. Dropping it
/// retires it: its connections close as they come back.
pub(super) struct RealClient {
    pool: Client<Connector, Full<Bytes>>,
    life: Arc<ClientLife>,
    ejector: Ejector,
    ceiling: Duration,
    max_body: usize,
}

impl Drop for RealClient {
    fn drop(&mut self) {
        self.life.retire();
    }
}

impl RealClient {
    /// A client under `settings`, resolving names through `target` when
    /// given, its connections counted under `peer` when given, else under
    /// the address each dials.
    pub(super) fn new(
        settings: &ClientSettings,
        target: Option<Arc<ResolvedTarget>>,
        peer: Option<&str>,
    ) -> Self {
        let life = Arc::new(ClientLife::new(settings.max_idle_per_host > 0));
        let connector = Connector::new(settings, target, peer, life.clone());
        let pool = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(settings.pool_idle_timeout)
            .pool_max_idle_per_host(settings.max_idle_per_host)
            .build(connector);
        Self {
            pool,
            life,
            ejector: Ejector::new(&settings.ejection),
            ceiling: settings.ceiling,
            max_body: settings.max_body_bytes,
        }
    }

    /// Send `req` to `uri`; a failure is counted under `peer` by its cause
    /// and returned classified (see the module doc).
    pub(super) async fn send(
        &self,
        peer: &str,
        uri: &str,
        req: HttpRequest,
    ) -> Result<HttpResponse, HttpError> {
        let mut request = request(peer, uri, req)?;
        let captured = capture_connection(&mut request);
        let taken = Taken(&captured);
        let exchange = self.exchange(taken, peer, request);
        tokio::pin!(exchange);
        let mut watch = Watch { taken, ejector: &self.ejector, peer, done: false };
        let outcome = tokio::select! {
            biased;
            outcome = &mut exchange => Some(outcome),
            () = tokio::time::sleep(self.ceiling) => None,
        };
        match outcome {
            Some(outcome) => {
                watch.done = true;
                outcome
            }
            None => {
                let why = "no whole answer within the ceiling";
                Err(failed(peer, FailureCause::RequestTimeout, why))
            }
        }
    }

    /// One request and its whole answer on the connection `taken` captures.
    async fn exchange(
        &self,
        taken: Taken<'_>,
        peer: &str,
        request: Request<Full<Bytes>>,
    ) -> Result<HttpResponse, HttpError> {
        let answer = self.pool.request(request).await.map_err(|e| {
            taken.failed();
            failed(peer, super::cause::classify(&e), e.to_string())
        })?;
        let (head, mut body) = answer.into_parts();
        let status = head.status.as_u16();
        if let Some(Tag(tracked)) = head.extensions.get::<Tag>() {
            if self.ejector.answered(peer, &tracked.record, status).is_some() {
                tracked.closed_by_answer();
                taken.poison();
            }
        }
        let body = match capped_body::read(&mut body, self.max_body).await {
            Ok(body) => body,
            Err(BodyError::Failed(e)) => {
                taken.failed();
                return Err(failed(peer, super::cause::classify_body(&e), e.to_string()));
            }
            Err(refused) => {
                taken.failed();
                let refusal = match refused {
                    BodyError::Declared => BodyRefusal::Declared,
                    _ => BodyRefusal::Read,
                };
                metrics::body_refused(peer, refusal);
                let why = format!("response body past {} bytes", self.max_body);
                return Err(failed(peer, FailureCause::BodyTooLarge, why));
            }
        };
        let headers = head
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        Ok(HttpResponse { status, headers, body })
    }
}

/// The hyper request `req` states, sent to `uri`; one that cannot be built
/// fails [`FailureCause::Other`].
fn request(peer: &str, uri: &str, req: HttpRequest) -> Result<Request<Full<Bytes>>, HttpError> {
    let method = Method::from_bytes(req.method.as_bytes())
        .map_err(|e| failed(peer, FailureCause::Other, format!("bad method: {e}")))?;
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(Full::new(Bytes::from(req.body)))
        .map_err(|e| failed(peer, FailureCause::Other, format!("bad request: {e}")))
}

/// A failure of `cause` to `peer`, counted. Cause-labelled counter only: the
/// caller's episode owns the log line (ADR-0026: no per-request output).
fn failed(peer: &str, cause: FailureCause, detail: impl Into<String>) -> HttpError {
    crate::failures::record(peer, cause);
    HttpError::new(peer, cause, detail)
}

/// The connection one request took, as the pool reports it once taken.
#[derive(Clone, Copy)]
struct Taken<'a>(&'a CaptureConnection);

impl Taken<'_> {
    /// Its record, once a connection is taken.
    fn tracked(self) -> Option<Arc<Tracked>> {
        let connected = self.0.connection_metadata();
        let mut extras = Extensions::new();
        connected.as_ref()?.get_extras(&mut extras);
        extras.get::<Tag>().map(|Tag(tracked)| tracked.clone())
    }

    /// The pool never hands it out again: it closes once free.
    fn poison(self) {
        if let Some(connected) = self.0.connection_metadata().as_ref() {
            connected.poison();
        }
    }

    /// The exchange on it failed: it is never handed out again, and its end
    /// is no close of the client's.
    fn failed(self) {
        if let Some(tracked) = self.tracked() {
            tracked.failed();
        }
        self.poison();
    }
}

/// One request followed until its answer is read whole. Dropped before, it
/// abandons the connection it took: poisoned, closed by a reset, counted.
/// It is declared after the request's exchange, so it drops first, before
/// the connection can close otherwise.
struct Watch<'a> {
    taken: Taken<'a>,
    ejector: &'a Ejector,
    peer: &'a str,
    /// The exchange ended, answered or failed.
    done: bool,
}

impl Drop for Watch<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Some(tracked) = self.taken.tracked() {
            if tracked.abandoned() {
                self.ejector.abandoned(self.peer);
            }
        }
        self.taken.poison();
    }
}

/// One backend's client: its own pool, its URL naming the target. A request
/// resolves the target before the client connects, so a lookup is bounded
/// by the target's lookup bound, never cut by the connect timeout, and a
/// name that does not resolve fails as one ([`FailureCause::Dns`]).
pub(super) struct NamedClient {
    client: RealClient,
    target: Arc<ResolvedTarget>,
    /// `http://<target>`, the authority as configured.
    base: String,
    /// The target as configured: the counters' `peer`.
    peer: String,
}

impl NamedClient {
    pub(super) fn new(target: Arc<ResolvedTarget>, settings: &ClientSettings) -> Self {
        let (host, port) = target.host_port();
        let peer = target.to_string();
        Self {
            client: RealClient::new(settings, Some(target.clone()), Some(&peer)),
            target,
            base: format!("http://{host}:{port}"),
            peer,
        }
    }
}

#[async_trait]
impl HttpClient for NamedClient {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        if self.target.resolve_all().await.is_none() {
            crate::failures::record(&self.peer, FailureCause::Dns);
            return Err(HttpError::dns(&self.peer));
        }
        let uri = format!("{}{}", self.base, req.path);
        self.client.send(&self.peer, &uri, req).await
    }
}
