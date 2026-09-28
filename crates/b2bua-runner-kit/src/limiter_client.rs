//! The call limiter client a runner builds from its env (ADR-0038 decision 10).
//!
//! No `LIMITER_URL`: [`NoopLimiter`], no call is counted. Otherwise the HTTP
//! client to its `host:port`, whose name is looked up once at boot, waiting
//! at most one breaker probe period. A name that has not resolved by then
//! leaves the client without an address, so the worker's breaker starts open
//! and its probe looks the name up every period; a boot lookup that lands
//! after the wait is kept, and the next probe closes the breaker.

use std::sync::Arc;
use std::time::Duration;

use b2bua::limiter::{CallLimiter, NoopLimiter};
use b2bua::limiter_http::HttpCallLimiter;
use b2bua::limiter_target::{LimiterTarget, NameResolver};
use http_net::HttpTransport;

/// What the limiter client is built from.
pub(crate) struct LimiterClientSettings {
    /// The limiter's `host:port`; `None` runs without a limiter.
    pub hostport: Option<String>,
    /// The admit and health budget.
    pub timeout: Duration,
    /// The refresh budget.
    pub refresh_timeout: Duration,
    /// The release budget.
    pub release_timeout: Duration,
    /// The longest boot waits for the name's first lookup.
    pub boot_lookup: Duration,
}

/// The limiter client `settings` state, sending over `transport` and looking
/// its name up with `resolver`. See the module doc.
pub(crate) async fn limiter_client(
    settings: &LimiterClientSettings,
    transport: Arc<dyn HttpTransport>,
    resolver: Arc<dyn NameResolver>,
) -> Arc<dyn CallLimiter> {
    let Some(hostport) = settings.hostport.as_deref() else {
        return Arc::new(NoopLimiter);
    };
    let client = HttpCallLimiter::with_target(
        transport,
        LimiterTarget::name_with(hostport, resolver),
        settings.timeout,
    )
    .with_refresh_timeout(settings.refresh_timeout)
    .with_release_timeout(settings.release_timeout);
    // A name still unresolved leaves the client without an address: the
    // breaker logs its open start.
    client.lookup(settings.boot_lookup).await;
    Arc::new(client)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use b2bua::limiter::{AdmitOutcome, LimiterEntry, ReleaseAnswer};
    use b2bua::limiter_breaker::{BreakerConfig, BreakerLimiter};
    use b2bua::limiter_refresh_batch::{RefreshBatch, RefreshBatchConfig};
    use b2bua::limiter_release::{ReleaseQueue, ReleaseQueueConfig};
    use b2bua::metrics::B2buaMetrics;
    use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
    use http_net::{
        BindError, Fault, HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpService,
        SimulatedHttpNetwork,
    };
    use sip_clock::testkit::settle;
    use sip_clock::Clock;

    use super::*;

    const PROBE: Duration = Duration::from_secs(1);
    const NAME: &str = "limiter:8080";
    /// Past the probe's tick, the time its request takes on the simulated
    /// network (1 ms per hop), in 1 ms steps.
    const EPSILON_MS: u32 = 10;

    /// Let `d` pass, then [`EPSILON_MS`] for the requests it started.
    async fn elapse(d: Duration) {
        tokio::time::advance(d).await;
        settle().await;
        for _ in 0..EPSILON_MS {
            tokio::time::advance(Duration::from_millis(1)).await;
            settle().await;
        }
    }

    /// Let `d` pass in 5 ms steps, every task settled after each.
    async fn run_for(d: Duration) {
        let step = Duration::from_millis(5);
        let mut left = d;
        while !left.is_zero() {
            let now = left.min(step);
            tokio::time::advance(now).await;
            settle().await;
            left -= now;
        }
    }

    /// The names the test has made resolvable, each lookup answering after
    /// `delay`; counts every lookup and the most in flight at once.
    #[derive(Default)]
    struct Names {
        known: Mutex<HashMap<String, SocketAddr>>,
        delay: Mutex<Duration>,
        lookups: AtomicUsize,
        in_flight: AtomicUsize,
        most_in_flight: AtomicUsize,
    }

    impl Names {
        fn point(&self, name: &str, addr: SocketAddr) {
            self.known.lock().unwrap().insert(name.into(), addr);
        }

        fn slow(&self, delay: Duration) {
            *self.delay.lock().unwrap() = delay;
        }
    }

    #[async_trait]
    impl NameResolver for Names {
        async fn resolve(&self, name: &str) -> Option<SocketAddr> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_in_flight.fetch_max(now, Ordering::SeqCst);
            let delay = *self.delay.lock().unwrap();
            tokio::time::sleep(delay).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.known.lock().unwrap().get(name).copied()
        }
    }

    /// The simulated network, counting every request sent through it.
    struct Counting {
        net: SimulatedHttpNetwork,
        sent: AtomicUsize,
    }

    #[async_trait]
    impl HttpTransport for Counting {
        async fn serve(
            &self,
            addr: SocketAddr,
            service: Arc<dyn HttpService>,
        ) -> Result<Box<dyn HttpServerHandle>, BindError> {
            self.net.serve(addr, service).await
        }
        async fn request(
            &self,
            dst: SocketAddr,
            req: HttpRequest,
        ) -> Result<HttpResponse, HttpError> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            self.net.request(dst, req).await
        }
    }

    /// Two limiters on one network, and the names that lead to them.
    struct Lab {
        net: Arc<Counting>,
        names: Arc<Names>,
        a: (SocketAddr, Arc<CallStore>),
        b: (SocketAddr, Arc<CallStore>),
        _servers: Vec<Box<dyn HttpServerHandle>>,
    }

    impl Lab {
        async fn new() -> Self {
            let net =
                Arc::new(Counting { net: SimulatedHttpNetwork::new(), sent: AtomicUsize::new(0) });
            let (a, served_a) = limiter_at(&net, "10.0.0.1:8080").await;
            let (b, served_b) = limiter_at(&net, "10.0.0.2:8080").await;
            Self { net, names: Arc::default(), a, b, _servers: vec![served_a, served_b] }
        }

        fn sent(&self) -> usize {
            self.net.sent.load(Ordering::SeqCst)
        }

        /// The runner's client for [`NAME`], behind a worker's breaker (3
        /// failures, [`PROBE`]) with its probe and release queue running.
        async fn worker(&self) -> Worker {
            let settings = LimiterClientSettings {
                hostport: Some(NAME.into()),
                timeout: Duration::from_millis(150),
                refresh_timeout: Duration::from_secs(2),
                release_timeout: Duration::from_secs(2),
                boot_lookup: PROBE,
            };
            let client = limiter_client(&settings, self.net.clone(), self.names.clone()).await;
            let metrics = B2buaMetrics::new();
            let bounds = ReleaseQueueConfig { lease: Duration::from_secs(120), cap: 10 };
            let releases = ReleaseQueue::new(client.clone(), bounds, metrics.clone());
            tokio::spawn(releases.clone().run());
            let bounds = RefreshBatchConfig {
                tick: PROBE,
                max: 100,
                lease: Duration::from_secs(120),
                cap: 10,
            };
            let refreshes = RefreshBatch::new(client.clone(), bounds, metrics.clone(), |_| {});
            tokio::spawn(refreshes.clone().run());
            let (limiter, breaker) = BreakerLimiter::guard(
                client,
                BreakerConfig { failures: 3, probe: PROBE },
                releases,
                refreshes,
                metrics.clone(),
            );
            tokio::spawn(breaker.expect("the HTTP client is guarded").run());
            settle().await;
            Worker { limiter, metrics }
        }
    }

    struct Worker {
        limiter: Arc<dyn CallLimiter>,
        metrics: B2buaMetrics,
    }

    impl Worker {
        /// Admit call `key` on two limiters.
        async fn admit(&self, key: &str) -> AdmitOutcome {
            let entries = [
                LimiterEntry { id: "x".into(), limit: 100 },
                LimiterEntry { id: "y".into(), limit: 100 },
            ];
            self.limiter.admit(key, &entries, false).await
        }

        async fn release(&self, key: &str) -> ReleaseAnswer {
            self.limiter.release(&[key.to_string()]).await
        }

        fn open(&self) -> bool {
            self.metrics.limiter_breaker_open()
        }
    }

    /// A limiter served at `addr`, its store, and its binding.
    async fn limiter_at(
        net: &Counting,
        addr: &str,
    ) -> ((SocketAddr, Arc<CallStore>), Box<dyn HttpServerHandle>) {
        let addr: SocketAddr = addr.parse().unwrap();
        let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
        let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        ((addr, store), net.serve(addr, server).await.unwrap())
    }

    fn held(store: &CallStore) -> (i64, i64) {
        (store.held("x"), store.held("y"))
    }

    #[tokio::test(start_paused = true)]
    async fn an_unresolvable_name_boots_open_and_counts_calls_within_one_probe_period_of_resolving()
    {
        let lab = Lab::new().await;
        let w = lab.worker().await;
        assert!(w.open(), "boots open");
        assert_eq!(w.admit("c1#k").await, AdmitOutcome::NotSent, "fails open at once");
        assert_eq!(lab.sent(), 0, "no request");
        assert_eq!(w.metrics.limiter_breaker_admits_not_sent_total(), 1, "counted");

        elapse(PROBE + PROBE / 2).await;
        assert!(w.open(), "the probe at one period found no address");
        assert_eq!(w.admit("c2#k").await, AdmitOutcome::NotSent);

        lab.names.point(NAME, lab.a.0);
        elapse(PROBE).await;
        assert!(!w.open(), "closed within one probe period of the name resolving");
        assert_eq!(w.admit("c3#k").await, AdmitOutcome::Admitted);
        assert_eq!(held(&lab.a.1), (1, 1), "the call is counted");
        assert_eq!(w.release("c3#k").await, ReleaseAnswer::Released);
        assert_eq!(held(&lab.a.1), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_that_resolves_at_boot_starts_closed() {
        let lab = Lab::new().await;
        lab.names.point(NAME, lab.a.0);
        let w = lab.worker().await;
        assert!(!w.open());
        assert_eq!(w.metrics.limiter_breaker_opened_total(), 0);
        assert_eq!(w.admit("c1#k").await, AdmitOutcome::Admitted);
        assert_eq!(held(&lab.a.1), (1, 1));
        assert_eq!(lab.names.lookups.load(Ordering::SeqCst), 1, "looked up once, at boot");
        assert_eq!(w.release("c1#k").await, ReleaseAnswer::Released);
        assert_eq!(held(&lab.a.1), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_that_moves_while_the_breaker_is_open_closes_it_on_the_new_address() {
        let lab = Lab::new().await;
        lab.names.point(NAME, lab.a.0);
        let w = lab.worker().await;
        assert_eq!(w.admit("c1#k").await, AdmitOutcome::Admitted);
        assert_eq!(w.release("c1#k").await, ReleaseAnswer::Released);
        assert_eq!(held(&lab.a.1), (0, 0));

        lab.net.net.apply_fault(Fault::Cut { dst: lab.a.0 });
        for n in 2..5 {
            assert_eq!(w.admit(&format!("c{n}#k")).await, AdmitOutcome::Unavailable);
        }
        assert!(w.open());
        settle().await;
        lab.names.point(NAME, lab.b.0);
        elapse(PROBE).await;
        assert!(!w.open(), "the probe looks the name up again and reaches its new address");
        assert_eq!(w.admit("c5#k").await, AdmitOutcome::Admitted);
        assert_eq!(held(&lab.b.1), (1, 1), "counted on the new address");
        assert_eq!(w.release("c5#k").await, ReleaseAnswer::Released);
        assert_eq!(held(&lab.b.1), (0, 0));
        assert_eq!(held(&lab.a.1), (0, 0), "the old address is drained");
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_that_moves_after_a_failed_probe_is_looked_up_again() {
        let lab = Lab::new().await;
        lab.names.point(NAME, lab.a.0);
        let w = lab.worker().await;
        lab.net.net.apply_fault(Fault::Cut { dst: lab.a.0 });
        for n in 1..4 {
            assert_eq!(w.admit(&format!("c{n}#k")).await, AdmitOutcome::Unavailable);
        }
        assert!(w.open());
        settle().await;
        elapse(PROBE).await;
        assert!(w.open(), "the name still leads to the cut limiter");
        assert_eq!(w.metrics.limiter_breaker_probe_failures_total(), 1);
        lab.names.point(NAME, lab.b.0);
        elapse(PROBE).await;
        assert!(!w.open(), "the failed probe forgot the address; the next one finds the new one");
        assert_eq!(w.admit("c4#k").await, AdmitOutcome::Admitted);
        assert_eq!(held(&lab.b.1), (1, 1));
        assert_eq!(w.release("c4#k").await, ReleaseAnswer::Released);
        assert_eq!(held(&lab.b.1), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn a_lookup_slower_than_the_admit_budget_still_closes_the_breaker() {
        let lab = Lab::new().await;
        lab.names.slow(Duration::from_millis(300));
        let w = lab.worker().await;
        assert!(w.open(), "unresolvable at boot");
        lab.names.point(NAME, lab.a.0);
        run_for(2 * PROBE + Duration::from_millis(20)).await;
        assert!(!w.open(), "the lookup the first probe started lands, the next probe closes");
        assert_eq!(lab.names.most_in_flight.load(Ordering::SeqCst), 1, "one lookup at a time");
        assert_eq!(w.admit("c1#k").await, AdmitOutcome::Admitted);
        assert_eq!(held(&lab.a.1), (1, 1));
        assert_eq!(w.release("c1#k").await, ReleaseAnswer::Released);
        assert_eq!(held(&lab.a.1), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn a_boot_lookup_landing_after_the_boot_wait_is_kept_and_the_first_probe_closes() {
        let lab = Lab::new().await;
        lab.names.point(NAME, lab.a.0);
        lab.names.slow(PROBE + PROBE / 2);
        let w = lab.worker().await;
        assert!(w.open(), "the boot wait ended before the lookup");
        run_for(PROBE + Duration::from_millis(20)).await;
        assert!(!w.open(), "the boot lookup landed and was kept; the first probe closes");
        assert_eq!(lab.names.lookups.load(Ordering::SeqCst), 1, "the boot lookup only");
        assert_eq!(w.admit("c1#k").await, AdmitOutcome::Admitted);
        assert_eq!(held(&lab.a.1), (1, 1));
        assert_eq!(w.release("c1#k").await, ReleaseAnswer::Released);
        assert_eq!(held(&lab.a.1), (0, 0));
    }

    #[tokio::test]
    async fn no_url_runs_without_a_limiter() {
        let settings = LimiterClientSettings {
            hostport: None,
            timeout: Duration::from_millis(150),
            refresh_timeout: Duration::from_secs(2),
            release_timeout: Duration::from_secs(2),
            boot_lookup: PROBE,
        };
        let names = Arc::new(Names::default());
        let client =
            limiter_client(&settings, Arc::new(SimulatedHttpNetwork::new()), names.clone()).await;
        assert!(client.health().is_none(), "no breaker");
        let entries = [LimiterEntry { id: "x".into(), limit: 1 }];
        assert_eq!(client.admit("c#k", &entries, false).await, AdmitOutcome::NotSent);
        assert_eq!(names.lookups.load(Ordering::SeqCst), 0);
    }
}
