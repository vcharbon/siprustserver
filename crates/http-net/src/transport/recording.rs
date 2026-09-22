//! `RecordingHttpNetwork` — a decorator that records every exchange onto the
//! `layer-harness` [`Recorder`] under the typed channel [`HTTP_TAG`], so HTTP
//! shares the run's one sequence and clock with every other recorded layer.
//!
//! Two sides, each recorded at its own instant:
//! - the CLIENT side: [`request`](HttpTransport::request) records
//!   [`HttpNetworkEvent::Sent`] before the inner request is awaited and
//!   [`HttpNetworkEvent::Received`] when it ends;
//! - the SERVED side: [`serve`](HttpTransport::serve) wraps the service, which
//!   records [`HttpNetworkEvent::Served`] at handler entry and
//!   [`HttpNetworkEvent::Answered`] at completion, and registers the bound
//!   address as a [`NetworkTag::Service`] lane.
//!
//! Each side ends [`HttpOutcome::Abandoned`] when its future is dropped first
//! (the caller's timeout firing on a withheld answer), so an exchange never
//! leaves a gap the record cannot explain. On the simulated fabric the served
//! handler runs inside the client's request future, so the served record names
//! the client exchange it answers; on a real socket it names the peer address.
//!
//! Test-only: the decorator needs a `Recorder`, which production never builds.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use layer_harness::{lane_key, Channel, EventSequencer, LaneKey, NetworkTag, Recorder};

use super::{
    peer, BindError, HttpAnswer, HttpError, HttpRequest, HttpResponse, HttpServerHandle,
    HttpService, HttpTransport,
};

/// The `layer-harness` channel key HTTP exchanges are recorded under.
pub const HTTP_TAG: &str = "http-net/HttpNetwork";

/// The lane name a served address gets when none was given.
const DEFAULT_SERVICE_NAME: &str = "http-service";

/// How one side of an exchange ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpOutcome {
    /// A response.
    Response(HttpResponse),
    /// The transport failed, as the client saw it.
    Error(String),
    /// The service closed the connection without a response.
    Abort,
    /// The side's future was dropped before the exchange ended.
    Abandoned,
}

/// One observation on the [`HTTP_TAG`] channel. An ending event names the
/// `seq` of the event that opened its side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpNetworkEvent {
    /// A client on lane `client` sent `request` to `dst`.
    Sent { client: LaneKey, dst: SocketAddr, request: HttpRequest },
    /// The client exchange opened at seq `exchange` ended.
    Received { exchange: u64, outcome: HttpOutcome },
    /// The service on lane `service` took `request`. `exchange` is the client
    /// exchange it answers when a recording client on the same recorder sent
    /// it; `peer` the connection's remote address when the transport has one.
    Served {
        service: LaneKey,
        peer: Option<SocketAddr>,
        exchange: Option<u64>,
        request: HttpRequest,
    },
    /// The service's answer to the request served at seq `served`.
    Answered { served: u64, outcome: HttpOutcome },
}

/// The client exchange whose future is running: the recorder it was recorded
/// on (by its sequencer) and the `seq` of its [`HttpNetworkEvent::Sent`].
#[derive(Clone)]
struct ClientExchange {
    sequencer: Arc<EventSequencer>,
    seq: u64,
}

tokio::task_local! {
    static EXCHANGE: ClientExchange;
}

/// One side's pending end: recorded by [`settle`](Self::settle), or as
/// [`HttpOutcome::Abandoned`] on drop.
struct Pending {
    channel: Channel<HttpNetworkEvent>,
    opened: u64,
    served: bool,
    settled: bool,
}

impl Pending {
    fn end(&self, outcome: HttpOutcome) {
        let opened = self.opened;
        self.channel.record(if self.served {
            HttpNetworkEvent::Answered { served: opened, outcome }
        } else {
            HttpNetworkEvent::Received { exchange: opened, outcome }
        });
    }

    fn settle(mut self, outcome: HttpOutcome) {
        self.settled = true;
        self.end(outcome);
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.settled {
            self.end(HttpOutcome::Abandoned);
        }
    }
}

/// Records every exchange that flows through the wrapped transport. Clones
/// share the channel and the service names.
#[derive(Clone)]
pub struct RecordingHttpNetwork {
    inner: Arc<dyn HttpTransport>,
    recorder: Recorder,
    channel: Channel<HttpNetworkEvent>,
    client: LaneKey,
    names: Arc<Mutex<HashMap<SocketAddr, String>>>,
}

impl RecordingHttpNetwork {
    /// Wrap `inner`, recording onto `recorder`. Requests are recorded as sent
    /// from the lane `client` (the requester's lane key on the ladder).
    pub fn new(
        inner: Arc<dyn HttpTransport>,
        recorder: &Recorder,
        client: impl Into<LaneKey>,
    ) -> Self {
        Self {
            inner,
            recorder: recorder.clone(),
            channel: recorder.for_tag(HTTP_TAG),
            client: client.into(),
            names: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Name the service lane [`serve`](HttpTransport::serve) registers for
    /// `addr` (the requested address).
    pub fn with_service_name(self, addr: SocketAddr, name: impl Into<String>) -> Self {
        self.names.lock().unwrap().insert(addr, name.into());
        self
    }

    /// The channel this network records on.
    pub fn channel(&self) -> Channel<HttpNetworkEvent> {
        self.channel.clone()
    }

    /// The exchanges this network's client lane sent, in request order, read
    /// from the channel.
    pub fn exchanges(&self) -> Vec<super::CapturedExchange> {
        super::entries::client_exchanges(&self.channel.snapshot(), &self.client)
    }
}

#[async_trait]
impl HttpTransport for RecordingHttpNetwork {
    async fn serve(
        &self,
        addr: SocketAddr,
        service: Arc<dyn HttpService>,
    ) -> Result<Box<dyn HttpServerHandle>, BindError> {
        let bound = Arc::new(OnceLock::new());
        let recorded = Arc::new(RecordedService {
            inner: service,
            channel: self.channel.clone(),
            sequencer: self.recorder.sequencer(),
            requested: addr,
            bound: bound.clone(),
        });
        let handle = self.inner.serve(addr, recorded).await?;
        let local = handle.local_addr();
        let _ = bound.set(lane_key(local));
        let name = {
            let names = self.names.lock().unwrap();
            names.get(&addr).or_else(|| names.get(&local)).cloned()
        };
        self.recorder.register_lane(
            local,
            name.unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string()),
            NetworkTag::Service,
        );
        Ok(handle)
    }

    async fn request(&self, dst: SocketAddr, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let mut seq = 0;
        self.channel.record_with(|s| {
            seq = s;
            HttpNetworkEvent::Sent { client: self.client.clone(), dst, request: req.clone() }
        });
        let pending =
            Pending { channel: self.channel.clone(), opened: seq, served: false, settled: false };
        let exchange = ClientExchange { sequencer: self.recorder.sequencer(), seq };
        let result = EXCHANGE.scope(exchange, self.inner.request(dst, req)).await;
        pending.settle(match &result {
            Ok(resp) => HttpOutcome::Response(resp.clone()),
            Err(e) => HttpOutcome::Error(e.to_string()),
        });
        result
    }
}

/// A served service, recording the served side of every request it answers.
struct RecordedService {
    inner: Arc<dyn HttpService>,
    channel: Channel<HttpNetworkEvent>,
    sequencer: Arc<EventSequencer>,
    requested: SocketAddr,
    bound: Arc<OnceLock<LaneKey>>,
}

impl RecordedService {
    fn open(&self, req: &HttpRequest) -> Pending {
        // Only a client exchange recorded on this recorder is ours to name.
        let exchange = EXCHANGE
            .try_with(ClientExchange::clone)
            .ok()
            .filter(|x| Arc::ptr_eq(&x.sequencer, &self.sequencer))
            .map(|x| x.seq);
        let service = self.bound.get().cloned().unwrap_or_else(|| lane_key(self.requested));
        let mut seq = 0;
        self.channel.record_with(|s| {
            seq = s;
            HttpNetworkEvent::Served {
                service,
                peer: peer::current(),
                exchange,
                request: req.clone(),
            }
        });
        Pending { channel: self.channel.clone(), opened: seq, served: true, settled: false }
    }
}

#[async_trait]
impl HttpService for RecordedService {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        let pending = self.open(&req);
        let resp = self.inner.handle(req).await;
        pending.settle(HttpOutcome::Response(resp.clone()));
        resp
    }

    async fn answer(&self, req: HttpRequest) -> HttpAnswer {
        let pending = self.open(&req);
        let answer = self.inner.answer(req).await;
        pending.settle(match &answer {
            HttpAnswer::Response(resp) => HttpOutcome::Response(resp.clone()),
            HttpAnswer::Abort => HttpOutcome::Abort,
        });
        answer
    }
}
