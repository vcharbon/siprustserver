//! A two-worker replicating cluster behind the load-balancing proxy, with the
//! genuine limiter on a simulated HTTP fabric that outlives a worker's crash,
//! built from a [`ClusterRecipe`]. The verbs a test steers one call by live in
//! [`crate::roles`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::http::HttpCallLimiter;
use b2bua::limiter::CallLimiter;
use b2bua::rules::{ComposeOptions, ServiceDef};
use call::LimiterEntry;
use call_limiter::{CallStore, LimiterMetrics, LimiterServer};
use http_net::{HttpServerHandle, HttpService, HttpTransport};
use http_net_testkit::SimulatedHttpNetwork;
use sip_clock::Clock;

use crate::{FailoverHarness, ProxySut, ReplicatedB2buaSut};

/// The proxy every party dials and every worker's legs egress through.
pub const PROXY: &str = "127.0.0.1:5080";
/// The two workers, by ordinal and SIP address.
pub const WORKERS: [(&str, &str); 2] = [("b1", "127.0.0.1:5091"), ("b2", "127.0.0.1:5092")];
/// The limiter server on the HTTP fabric.
pub const LIMITER: &str = "10.0.0.1:8080";
/// The limiter set every call the default decision routes holds.
pub const TRUNK: &str = "trunk-A";

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("a socket address")
}

/// What a recipe's builders see of the worker they build for.
pub struct WorkerEnv<'a> {
    pub ordinal: &'a str,
    pub sip_addr: SocketAddr,
    /// The HTTP fabric the limiter and the recipe's servers live on.
    pub http: &'a SimulatedHttpNetwork,
}

/// Builds one worker's decision engine.
pub type DecisionFn = Arc<dyn Fn(&WorkerEnv<'_>) -> Arc<dyn CallDecisionEngine> + Send + Sync>;
/// Builds the adaptation port every worker's services send their requests through.
pub type AdaptationFn =
    Arc<dyn Fn(&SimulatedHttpNetwork) -> b2bua::AdaptationHttpPort + Send + Sync>;
/// A worker configuration mutator, applied on every incarnation.
pub type TuneFn = Arc<dyn Fn(&mut b2bua::B2buaConfig) + Send + Sync>;

/// How the cluster's workers are built: what decides their calls, the services
/// they run, their configuration, and the HTTP servers they reach.
#[derive(Clone)]
pub struct ClusterRecipe {
    pub decision: DecisionFn,
    pub services: fn() -> Vec<ServiceDef>,
    pub compose: ComposeOptions,
    pub adaptation_http: Option<AdaptationFn>,
    pub tune: TuneFn,
    /// The workers' replication tunables (a backup ceiling, the reboot budget).
    pub replica: b2bua::ReplicaConfig,
    /// HTTP servers the cluster serves on its fabric, beside the limiter.
    pub http: Vec<(SocketAddr, Arc<dyn HttpService>)>,
}

impl ClusterRecipe {
    /// Every call routed to `dest`, holding one set of [`TRUNK`] on the
    /// cluster's limiter, with the built-in rules only.
    pub fn routing_to(dest: SocketAddr) -> Self {
        Self::deciding(move |_| limited_route_to(dest))
    }

    /// Every call decided by the engine `decision` builds per worker.
    pub fn deciding(
        decision: impl Fn(&WorkerEnv<'_>) -> Arc<dyn CallDecisionEngine> + Send + Sync + 'static,
    ) -> Self {
        Self {
            decision: Arc::new(decision),
            services: Vec::new,
            compose: ComposeOptions::default(),
            adaptation_http: None,
            tune: Arc::new(|_| {}),
            replica: b2bua::ReplicaConfig::default(),
            http: Vec::new(),
        }
    }
}

/// A scripted engine routing every call to `dest` with one set of [`TRUNK`].
pub fn limited_route_to(dest: SocketAddr) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to(&dest.ip().to_string(), dest.port());
                r.call_limiter = vec![LimiterEntry { id: TRUNK.into(), limit: 8 }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// The running cluster: the harness, the proxy, both workers and the limiter.
pub struct Cluster {
    pub fh: FailoverHarness,
    pub proxy: ProxySut,
    /// The workers, in [`WORKERS`] order.
    pub(crate) workers: Vec<ReplicatedB2buaSut>,
    /// The limiter's store: its total is the sets every live call holds.
    pub limiter: Arc<CallStore>,
    pub http: SimulatedHttpNetwork,
    _servers: Vec<Box<dyn HttpServerHandle>>,
}

impl Cluster {
    /// Stand the cluster up from `recipe` and drive both workers ready. The
    /// proxy runs no health probe: its view of a worker changes only by the
    /// verbs of [`crate::roles`].
    pub async fn start(name: &str, recipe: &ClusterRecipe) -> Cluster {
        let http = SimulatedHttpNetwork::new();
        // The production lease: a set no owner refreshes any more is freed by
        // its lease (ADR-0040).
        let limiter =
            Arc::new(CallStore::new(call_limiter::LimiterConfig::default(), Clock::test_at(0)));
        let server = Arc::new(LimiterServer::new(limiter.clone(), LimiterMetrics::new()));
        let mut servers = vec![http.serve(addr(LIMITER), server).await.expect("limiter binds")];
        for (at, service) in &recipe.http {
            servers.push(http.serve(*at, service.clone()).await.expect("recipe server binds"));
        }

        let ordinals: Vec<&str> = WORKERS.iter().map(|(o, _)| *o).collect();
        let tune = recipe.tune.clone();
        let mut fh = FailoverHarness::new(name, &ordinals)
            .with_worker_services(recipe.services)
            .with_worker_compose(recipe.compose.clone())
            .with_worker_tune(move |c| tune(c))
            .with_worker_replica(recipe.replica);
        if let Some(port) = &recipe.adaptation_http {
            fh = fh.with_worker_adaptation_http(port(&http));
        }
        let pool: Vec<(&str, SocketAddr)> = WORKERS.iter().map(|(o, a)| (*o, addr(a))).collect();
        let proxy = fh.spawn_proxy(PROXY, &pool).await;
        let px = addr(PROXY);
        let mut workers = Vec::new();
        for (ordinal, sip) in WORKERS {
            let peers: Vec<&str> = ordinals.iter().copied().filter(|o| *o != ordinal).collect();
            let env = WorkerEnv { ordinal, sip_addr: addr(sip), http: &http };
            let decision = (recipe.decision)(&env);
            let client: Arc<dyn CallLimiter> = Arc::new(HttpCallLimiter::new(
                Arc::new(http.clone()),
                addr(LIMITER),
                Duration::from_millis(150),
            ));
            // The default destination only names the worker's spec: every
            // call is routed by `decision`.
            let worker = fh
                .spawn_worker_limited(
                    ordinal,
                    ordinal,
                    sip,
                    &peers,
                    ("127.0.0.1", 5070),
                    (&px.ip().to_string(), px.port()),
                    decision,
                    client,
                )
                .await;
            workers.push(worker);
        }
        fh.advance(Duration::from_millis(500)).await;
        assert!(workers.iter().all(|w| w.is_ready()), "[{name}] both workers ready");
        Cluster { fh, proxy, workers, limiter, http, _servers: servers }
    }

    /// The worker `ordinal`.
    pub fn node(&self, ordinal: &str) -> &ReplicatedB2buaSut {
        self.workers
            .iter()
            .find(|w| w.ordinal() == ordinal)
            .unwrap_or_else(|| panic!("no worker {ordinal}"))
    }

    pub(crate) fn node_mut(&mut self, ordinal: &str) -> &mut ReplicatedB2buaSut {
        self.workers
            .iter_mut()
            .find(|w| w.ordinal() == ordinal)
            .unwrap_or_else(|| panic!("no worker {ordinal}"))
    }

    /// Every worker, in [`WORKERS`] order.
    pub fn nodes(&self) -> Vec<&ReplicatedB2buaSut> {
        self.workers.iter().collect()
    }

    /// Whether no worker serves or holds any trace of `call_ref`, both hold
    /// no per-call memory, and the limiter holds no set.
    pub async fn is_clear_of(&self, call_ref: &str) -> bool {
        for w in &self.workers {
            if !w.memory_clean() || w.holds_any_trace(call_ref).await {
                return false;
            }
        }
        self.limiter.stats().current_total == 0
    }
}
