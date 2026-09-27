//! The witness limiter a scenario drives itself: a real [`CallStore`] served
//! by a [`LimiterServer`] on a private simulated HTTP fabric, reached by the
//! production client, with one witness hold per id, probed per id and
//! restartable, and the fault seams it can put in front of its server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use call_limiter::wire::AdmitEntry;
use call_limiter::{AdmitResult, CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{
    HttpRequest, HttpResponse, HttpServerHandle, HttpService, HttpTransport, SimulatedHttpNetwork,
};
use sip_clock::Clock;

use crate::settle_until;

/// A seam a [`WitnessRig`] puts in front of its limiter server.
type ServerWrap = dyn Fn(Arc<dyn HttpService>) -> Arc<dyn HttpService> + Send + Sync;

/// The ids a [`WitnessRig`] serves; each carries one witness hold.
pub const WITNESS_IDS: [&str; 3] = ["x", "y", "z"];

/// Where a [`WitnessRig`] listens on its private fabric.
pub const WITNESS_LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The limiter `inner` answering every request `after` it applied it: what a
/// client past its budget sees as a timeout landed on the server.
struct AnswersLate {
    inner: Arc<dyn HttpService>,
    after: Duration,
}

#[async_trait]
impl HttpService for AnswersLate {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        let resp = self.inner.handle(req).await;
        tokio::time::sleep(self.after).await;
        resp
    }
}

/// A real [`LimiterServer`] on a private simulated HTTP fabric, reached by the
/// production client, with one **witness** hold per id of [`WITNESS_IDS`],
/// each under a call of its own (`witness-<id>`), so a surplus release reads
/// below the witness instead of vanishing under the store's floor at 0. A
/// scenario probes the call's holds per id while it is up and drains them to
/// the witnesses after it ends.
pub struct WitnessRig {
    /// The fabric the limiter is served on (for faults).
    pub http: SimulatedHttpNetwork,
    pub store: Arc<CallStore>,
    /// The production client over the fabric, for the SUT.
    pub client: Arc<dyn CallLimiter>,
    cfg: LimiterConfig,
    wrap: Arc<ServerWrap>,
    _server: Box<dyn HttpServerHandle>,
}

impl WitnessRig {
    /// Serve a store under `cfg`, reached by a client with `budget` as its
    /// fail-open budget; `answers_after` makes the server answer every request
    /// late, after it applied it.
    pub async fn serve(
        cfg: LimiterConfig,
        budget: Duration,
        answers_after: Option<Duration>,
    ) -> Self {
        Self::serve_wrapped(cfg, budget, move |server| match answers_after {
            Some(after) => Arc::new(AnswersLate { inner: server, after }),
            None => server,
        })
        .await
    }

    /// [`serve`](Self::serve) with the limiter server wrapped by `wrap` (a
    /// fault seam in front of the store); a restart wraps the new server the
    /// same way.
    pub async fn serve_wrapped(
        cfg: LimiterConfig,
        budget: Duration,
        wrap: impl Fn(Arc<dyn HttpService>) -> Arc<dyn HttpService> + Send + Sync + 'static,
    ) -> Self {
        let laddr: SocketAddr = WITNESS_LIMITER_ADDR.parse().expect("witness limiter address");
        let http = SimulatedHttpNetwork::new();
        let wrap: Arc<ServerWrap> = Arc::new(wrap);
        let (store, handle) = Self::serve_store(&http, cfg, wrap.as_ref()).await;
        let client: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr, budget));
        Self { http, store, client, cfg, wrap, _server: handle }
    }

    /// An empty store under `cfg` with the witnesses admitted, served on
    /// `http` at the rig's address behind `wrap`.
    async fn serve_store(
        http: &SimulatedHttpNetwork,
        cfg: LimiterConfig,
        wrap: &ServerWrap,
    ) -> (Arc<CallStore>, Box<dyn HttpServerHandle>) {
        let laddr: SocketAddr = WITNESS_LIMITER_ADDR.parse().expect("witness limiter address");
        let store = Arc::new(CallStore::new(cfg, Clock::test_at(0)));
        for id in WITNESS_IDS {
            let witness = store.admit(
                &format!("witness-{id}"),
                &[AdmitEntry { id: id.into(), limit: 100 }],
                false,
            );
            assert_eq!(witness, AdmitResult::Admitted, "witness on {id}");
        }
        let server: Arc<dyn HttpService> =
            Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        let handle = http.serve(laddr, wrap(server)).await.expect("witness limiter binds");
        (store, handle)
    }

    /// Restart the limiter: the store is gone with its process, an empty one
    /// (the witnesses re-admitted) answers at the same address. The SUT's
    /// client notices nothing. Returns the dead store, which a SUT built on
    /// it still reads.
    pub async fn restart(&mut self) -> Arc<CallStore> {
        let old = std::mem::replace(&mut self._server, Box::new(NoServer));
        drop(old);
        let wrap = self.wrap.clone();
        let (store, handle) = Self::serve_store(&self.http, self.cfg, wrap.as_ref()).await;
        self._server = handle;
        std::mem::replace(&mut self.store, store)
    }

    /// The holds the call owns on `id`: the store's count less the witness.
    /// Negative = a release matched no hold of the call.
    pub fn holds(&self, id: &str) -> i64 {
        self.store.held(id) - 1
    }

    /// The call's holds on every id of [`WITNESS_IDS`], in order.
    pub fn all_holds(&self) -> [i64; 3] {
        WITNESS_IDS.map(|id| self.holds(id))
    }

    /// Settle until the call's holds read `expected`, then assert them.
    pub async fn expect_holds(&self, expected: [i64; 3], why: &str) {
        settle_until(|| self.all_holds() == expected).await;
        assert_eq!(self.all_holds(), expected, "holds on {WITNESS_IDS:?}: {why}");
    }

    /// Settle until the call holds nothing, then release the witnesses so the
    /// store reads empty for the reaped check.
    pub async fn expect_drained(&self, why: &str) {
        self.expect_holds([0, 0, 0], why).await;
        for id in WITNESS_IDS {
            self.store.release(&format!("witness-{id}"));
        }
    }

    /// Extend every witness's lease.
    pub fn refresh_witnesses(&self) {
        for id in WITNESS_IDS {
            assert_eq!(
                self.store.refresh(&format!("witness-{id}"), &[id.to_string()]),
                call_limiter::RefreshResult::Extended,
                "witness on {id} is known"
            );
        }
    }
}

/// The placeholder handle while a [`WitnessRig`] restarts its server.
struct NoServer;

impl HttpServerHandle for NoServer {
    fn local_addr(&self) -> SocketAddr {
        WITNESS_LIMITER_ADDR.parse().expect("witness limiter address")
    }
}
