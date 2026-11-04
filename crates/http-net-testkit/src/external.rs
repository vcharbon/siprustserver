//! A real backend served on the simulated fabric: a paused-clock test reaches
//! an external process the simulation cannot host.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clock_bridge::{EnvFault, RealSide};
use http_net::{HttpAnswer, HttpRequest, HttpResponse, HttpService, HttpTransport};
use tokio::time::Instant;

/// Forwards every request to `upstream` through a real transport on the
/// [`RealSide`], from the instant it is asked. On a paused clock the answer
/// is delivered `latency` of virtual time after the request; on the real
/// clock when it arrives. A stall, or the backend failing the exchange, closes
/// the connection without a response and is in the side's fault log.
pub struct ExternalHttp {
    side: Arc<RealSide>,
    transport: Arc<dyn HttpTransport>,
    upstream: SocketAddr,
    latency: Duration,
}

impl ExternalHttp {
    /// `transport` must reach `upstream` for real; it runs on `side`'s runtime.
    pub fn new(
        side: Arc<RealSide>,
        transport: Arc<dyn HttpTransport>,
        upstream: SocketAddr,
        latency: Duration,
    ) -> Self {
        Self { side, transport, upstream, latency }
    }
}

#[async_trait]
impl HttpService for ExternalHttp {
    /// A closed connection reads as `502 Bad Gateway` here; transports call
    /// [`answer`](Self::answer).
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        match self.answer(req).await {
            HttpAnswer::Response(resp) => resp,
            HttpAnswer::Abort => HttpResponse::status(502),
        }
    }

    async fn answer(&self, req: HttpRequest) -> HttpAnswer {
        let issued = Instant::now();
        let label = format!("{} {} to {}", req.method, req.path, self.upstream);
        let (transport, upstream) = (self.transport.clone(), self.upstream);
        let pending =
            self.side.start(label.clone(), async move { transport.request(upstream, req).await });
        match pending.deliver_at(issued + self.latency).await {
            Ok(Ok(resp)) => HttpAnswer::Response(resp),
            Ok(Err(e)) => {
                self.side.faults().record(EnvFault::Failed { label, detail: e.to_string() });
                HttpAnswer::Abort
            }
            Err(_) => HttpAnswer::Abort,
        }
    }
}
