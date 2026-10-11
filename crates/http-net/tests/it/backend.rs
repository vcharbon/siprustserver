//! [`BackendClient`] at the transport seam, on the simulated fabric under the
//! paused clock: every fault is an outcome `reqwest` produces, scripted per
//! attempt. Each test names its own backend, so the process-wide counters it
//! reads are its own.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use http_net::backend::metrics;
use http_net::failures::FailureCause;
use http_net::{
    AttemptError, BackendClient, Behaviour, BindError, BreakerConfig, Budgets, ClientSettings,
    Fault, HttpClient, HttpRequest, HttpResponse, HttpServerHandle, HttpService, HttpTransport,
    MemoryResolver, ResolvedTarget, SimulatedHttpNetwork,
};
use sip_clock::testkit::{advance_settled, settle};
use tokio::time::Instant;

const ATTEMPT: Duration = Duration::from_millis(2_500);
const TOTAL: Duration = Duration::from_millis(5_000);

fn budgets() -> Budgets {
    Budgets { attempt: ATTEMPT, total: TOTAL }
}

/// Answers every request `status`, counting what it served by path.
#[derive(Default)]
struct Backend {
    status: Mutex<u16>,
    served: Mutex<Vec<String>>,
}

impl Backend {
    fn answering(status: u16) -> Arc<Self> {
        Arc::new(Self { status: Mutex::new(status), ..Default::default() })
    }

    fn served(&self, path: &str) -> usize {
        self.served.lock().unwrap().iter().filter(|p| *p == path).count()
    }
}

#[async_trait]
impl HttpService for Backend {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        self.served.lock().unwrap().push(req.path);
        HttpResponse::status(*self.status.lock().unwrap())
    }
}

/// The simulated fabric, counting every request sent and every client
/// built (with the idle pool it was asked for) and alive.
struct Counting {
    net: SimulatedHttpNetwork,
    sent: AtomicUsize,
    built: Mutex<Vec<usize>>,
    alive: Arc<AtomicUsize>,
}

impl Counting {
    fn new(net: &SimulatedHttpNetwork) -> Arc<Self> {
        Arc::new(Self {
            net: net.clone(),
            sent: AtomicUsize::new(0),
            built: Mutex::new(Vec::new()),
            alive: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }
}

/// A client that counts itself alive until dropped.
struct Tracked {
    inner: Arc<dyn HttpClient>,
    alive: Arc<AtomicUsize>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.alive.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl HttpClient for Tracked {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, http_net::HttpError> {
        self.inner.request(req).await
    }
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
    ) -> Result<HttpResponse, http_net::HttpError> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        self.net.request(dst, req).await
    }

    fn client(
        self: Arc<Self>,
        target: Arc<ResolvedTarget>,
        settings: &ClientSettings,
    ) -> Arc<dyn HttpClient> {
        self.built.lock().unwrap().push(settings.max_idle_per_host);
        self.alive.fetch_add(1, Ordering::SeqCst);
        let alive = self.alive.clone();
        Arc::new(Tracked { inner: Arc::new(http_net::AddressedClient::new(self, target)), alive })
    }
}

/// A backend at `addr` named `name`, served on its own fabric.
struct Rig {
    net: SimulatedHttpNetwork,
    transport: Arc<Counting>,
    backend: Arc<Backend>,
    resolver: Arc<MemoryResolver>,
    client: BackendClient,
    dst: SocketAddr,
    _server: Box<dyn HttpServerHandle>,
}

impl Rig {
    async fn new(name: &str, settings: ClientSettings) -> Self {
        let net = SimulatedHttpNetwork::new();
        let dst: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let backend = Backend::answering(200);
        let server = net.serve(dst, backend.clone()).await.unwrap();
        let transport = Counting::new(&net);
        let resolver = Arc::new(MemoryResolver::new());
        resolver.point(name, [dst]);
        let target = Arc::new(ResolvedTarget::name_with(name, resolver.clone()));
        let client = BackendClient::new(
            transport.clone(),
            target,
            settings,
            BreakerConfig::probing("/health"),
        );
        Self { net, transport, backend, resolver, client, dst, _server: server }
    }

    fn script(&self, steps: Vec<Behaviour>) {
        self.net.apply_fault(Fault::Script { dst: self.dst, steps });
    }

    fn behave(&self, behaviour: Behaviour) {
        self.net.apply_fault(Fault::Behave { dst: self.dst, behaviour });
    }

    async fn send(&self) -> Result<HttpResponse, AttemptError> {
        self.client.send_with_retry(HttpRequest::post("/call", Vec::new()), budgets()).await
    }
}

fn transport_cause(outcome: &Result<HttpResponse, AttemptError>) -> Option<FailureCause> {
    match outcome {
        Err(AttemptError::Transport(e)) => Some(e.cause),
        _ => None,
    }
}

// ── the retry rule ──────────────────────────────────────────────────────────

/// A first attempt failing at the connection level (reset, EOF, refused,
/// blackholed connect) is sent once more, on the same client, and answers.
#[tokio::test(start_paused = true)]
async fn a_connection_level_failure_is_retried_once_and_answers() {
    for (n, failure) in
        [Behaviour::Reset { after_ms: 1 }, Behaviour::Eof, Behaviour::Refuse, Behaviour::Blackhole]
            .into_iter()
            .enumerate()
    {
        let name = format!("retry-conn-{n}:8080");
        let rig = Rig::new(&name, ClientSettings::default()).await;
        rig.script(vec![failure.clone()]);
        let resp = rig.send().await.unwrap_or_else(|e| panic!("{failure:?}: {e:?}"));
        assert_eq!(resp.status, 200, "{failure:?}");
        assert_eq!(rig.transport.sent(), 2, "{failure:?}: the attempt and its retry");
        assert_eq!(metrics::retries(&name), 1, "{failure:?}");
        assert_eq!(rig.transport.built.lock().unwrap().len(), 1, "{failure:?}: one client");
    }
}

/// A silent first attempt (a pooled peer gone) ends at its 2.5 s budget and
/// the retry answers, inside the 5 s total.
#[tokio::test(start_paused = true)]
async fn a_silent_first_attempt_is_retried_inside_the_total() {
    let rig = Rig::new("retry-silent:8080", ClientSettings::default()).await;
    rig.script(vec![Behaviour::Silent]);
    let start = Instant::now();
    let resp = rig.send().await.expect("the retry answers");
    assert_eq!(resp.status, 200);
    let took = start.elapsed();
    assert!(took >= ATTEMPT && took < ATTEMPT + Duration::from_millis(20), "{took:?}");
}

/// A body that stalls after the headers is a request timeout, retried.
#[tokio::test(start_paused = true)]
async fn a_stalled_body_is_a_timeout_and_is_retried() {
    let rig = Rig::new("retry-stall-body:8080", ClientSettings::default()).await;
    rig.script(vec![Behaviour::StallBody]);
    assert_eq!(rig.send().await.expect("the retry answers").status, 200);
    assert_eq!(rig.backend.served("/call"), 2, "the stalled answer was served too");
}

/// A silent backend is asked twice and the request fails at the 5 s total.
#[tokio::test(start_paused = true)]
async fn a_silent_backend_fails_at_the_total() {
    let rig = Rig::new("retry-silent-both:8080", ClientSettings::default()).await;
    rig.behave(Behaviour::Silent);
    let start = Instant::now();
    assert_eq!(rig.send().await, Err(AttemptError::Timeout));
    assert_eq!(start.elapsed(), TOTAL);
    assert_eq!(rig.transport.sent(), 2);
}

/// An unbounded caller (`total_ms <= 0`) is bounded by the 5 s ceiling.
#[tokio::test(start_paused = true)]
async fn an_unbounded_caller_is_bounded_by_the_ceiling() {
    let rig = Rig::new("retry-ceiling:8080", ClientSettings::default()).await;
    rig.behave(Behaviour::Silent);
    let budgets = rig.client.budgets(Duration::from_secs(60), 0);
    assert_eq!(budgets, Budgets { attempt: Duration::from_secs(60), total: TOTAL });
    let start = Instant::now();
    let outcome = rig.client.send_with_retry(HttpRequest::get("/"), budgets).await;
    assert_eq!(outcome, Err(AttemptError::Timeout));
    assert_eq!(start.elapsed(), TOTAL, "one attempt, cut at the ceiling");
    assert_eq!(rig.transport.sent(), 1, "no time left for a retry");
}

/// Any HTTP response is final: a 500 is never retried.
#[tokio::test(start_paused = true)]
async fn an_http_response_is_never_retried() {
    let rig = Rig::new("retry-500:8080", ClientSettings::default()).await;
    *rig.backend.status.lock().unwrap() = 500;
    assert_eq!(rig.send().await.expect("a response").status, 500);
    assert_eq!(rig.transport.sent(), 1);
}

/// A name that does not resolve is not retried.
#[tokio::test(start_paused = true)]
async fn an_unresolved_name_is_not_retried() {
    let rig = Rig::new("retry-dns:8080", ClientSettings::default()).await;
    rig.resolver.unpoint("retry-dns:8080");
    let outcome = rig.send().await;
    assert_eq!(transport_cause(&outcome), Some(FailureCause::Dns), "{outcome:?}");
    assert_eq!(rig.resolver.lookups(), 1);
}

/// Open, the breaker sends nothing and nothing is retried.
#[tokio::test(start_paused = true)]
async fn nothing_is_sent_or_retried_while_the_breaker_is_open() {
    let rig = Rig::new("retry-open:8080", ClientSettings::default()).await;
    rig.client.trip();
    assert_eq!(rig.send().await, Err(AttemptError::BreakerOpen));
    assert_eq!(rig.transport.sent(), 0);
}

/// The failure that opens the breaker is not retried.
#[tokio::test(start_paused = true)]
async fn the_failure_that_opens_the_breaker_is_not_retried() {
    let rig = Rig::new("retry-opening:8080", ClientSettings::default()).await;
    rig.behave(Behaviour::Refuse);
    let first = rig.send().await;
    assert_eq!(transport_cause(&first), Some(FailureCause::Refused));
    assert_eq!(rig.transport.sent(), 2, "the first request and its retry");
    let second = rig.send().await;
    assert_eq!(transport_cause(&second), Some(FailureCause::Refused));
    assert!(rig.client.is_open(), "three refused inside 2 s open it");
    assert_eq!(rig.transport.sent(), 3, "no retry once open");
}

// ── the in-flight cap ───────────────────────────────────────────────────────

/// Cap 4, ten requests to a silent backend: at most 4 in flight at any
/// instant, the 6 others wait for a slot and fail at their budget unsent; the
/// wait never opens the breaker, it is counted, and another backend answers
/// throughout.
#[tokio::test(start_paused = true)]
async fn the_in_flight_cap_holds_under_a_flood() {
    let rig = Rig::new("cap-flood:8080", ClientSettings::with_max_in_flight(4)).await;
    let other = Rig::new("cap-other:8080", ClientSettings::default()).await;
    rig.behave(Behaviour::Silent);
    let rig = Arc::new(rig);
    let flood: Vec<_> = (0..10)
        .map(|_| {
            let rig = rig.clone();
            tokio::spawn(async move {
                rig.client.attempt(HttpRequest::post("/call", Vec::new()), ATTEMPT).await
            })
        })
        .collect();
    let mut most = 0;
    for _ in 0..26 {
        settle().await;
        most = most.max(rig.net.in_flight(rig.dst));
        let start = Instant::now();
        let answer = other.client.attempt(HttpRequest::get("/"), ATTEMPT).await;
        assert_eq!(answer.map(|r| r.status), Ok(200));
        assert!(start.elapsed() <= Duration::from_millis(2), "{:?}", start.elapsed());
        advance_settled(Duration::from_millis(100)).await;
    }
    let mut outcomes = Vec::new();
    for task in flood {
        outcomes.push(task.await.unwrap());
    }
    assert_eq!(most, 4, "never more than the cap in flight");
    let saturated = outcomes.iter().filter(|o| **o == Err(AttemptError::Saturated)).count();
    let timed_out = outcomes.iter().filter(|o| **o == Err(AttemptError::Timeout)).count();
    assert_eq!((timed_out, saturated), (4, 6), "{outcomes:?}");
    assert_eq!(rig.transport.sent(), 4, "the waiting requests were never sent");
    assert!(!rig.client.is_open(), "the slot wait never reaches the breaker");
    assert_eq!(metrics::slot_waits("cap-flood:8080"), 6, "every request past the 4th waited");
    assert_eq!(metrics::slot_expired("cap-flood:8080"), 6);
    assert_eq!(metrics::slot_wait_ms("cap-flood:8080"), 6 * 2_500);
}

// ── rotation ────────────────────────────────────────────────────────────────

/// Period 1 s: a request after the period rides a new client that looked
/// the name up afresh, so a name moved to a new address is reached; a
/// request in flight across the boundary completes on the old client, which
/// is dropped after it.
#[tokio::test(start_paused = true)]
async fn after_one_period_new_requests_ride_a_new_client() {
    let settings = ClientSettings { rotation: Duration::from_secs(1), ..ClientSettings::default() };
    let rig = Arc::new(Rig::new("rotate:8080", settings).await);
    let moved: SocketAddr = "10.0.0.2:8080".parse().unwrap();
    let mover = Backend::answering(200);
    let _moved = rig.net.serve(moved, mover.clone()).await.unwrap();

    assert_eq!(rig.client.attempt(HttpRequest::get("/a"), ATTEMPT).await.unwrap().status, 200);
    assert_eq!(rig.client.generation(), 1);
    rig.resolver.point("rotate:8080", [moved]);
    rig.net.apply_fault(Fault::Delay { dst: rig.dst, ms: 1_000 });
    let crossing = tokio::spawn({
        let rig = rig.clone();
        async move { rig.client.attempt(HttpRequest::get("/crossing"), ATTEMPT).await }
    });
    advance_settled(Duration::from_millis(900)).await;
    assert_eq!(rig.backend.served("/crossing"), 0, "still in transit");
    assert_eq!(rig.backend.served("/a"), 1, "the kept address until the period ends");

    advance_settled(Duration::from_millis(200)).await;
    assert_eq!(rig.backend.served("/crossing"), 1, "served, its answer in transit");
    assert_eq!(rig.client.attempt(HttpRequest::get("/b"), ATTEMPT).await.unwrap().status, 200);
    assert_eq!(rig.client.generation(), 2, "a new client after the period");
    assert_eq!(mover.served("/b"), 1, "the new client looked the moved name up");
    assert_eq!(rig.resolver.lookups(), 2);
    assert_eq!(rig.transport.alive.load(Ordering::SeqCst), 2, "the old one carries a request");

    assert_eq!(crossing.await.unwrap().unwrap().status, 200, "completes on the old client");
    assert_eq!(rig.backend.served("/crossing"), 1);
    assert_eq!(rig.transport.alive.load(Ordering::SeqCst), 1, "the old client is dropped");
}

// ── the breaker ─────────────────────────────────────────────────────────────

/// A cut backend: three requests inside 2 s open the breaker, which forgets
/// the address; open, a request fails at once and sends nothing; a probe
/// every second on a pool-less client, after a fresh lookup, until the
/// backend answers anything (here a 404), which closes it.
#[tokio::test(start_paused = true)]
async fn a_cut_backend_opens_the_breaker_and_a_404_probe_closes_it() {
    let rig = Rig::new("breaker-cut:8080", ClientSettings::default()).await;
    let mut state = rig.client.breaker_state();
    rig.behave(Behaviour::Refuse);
    for _ in 0..3 {
        let _ = rig.client.attempt(HttpRequest::get("/call"), ATTEMPT).await;
    }
    assert!(rig.client.is_open());
    assert!(*state.borrow_and_update());
    assert_eq!(rig.client.target().address(), None, "opening forgets the address");
    assert_eq!(metrics::breaker_transitions("breaker-cut:8080", true), 1);

    let sent = rig.transport.sent();
    let start = Instant::now();
    assert_eq!(
        rig.client.attempt(HttpRequest::get("/call"), ATTEMPT).await,
        Err(AttemptError::BreakerOpen)
    );
    assert_eq!(start.elapsed(), Duration::ZERO, "fails at once");
    assert_eq!(rig.transport.sent(), sent, "nothing sent");

    let lookups = rig.resolver.lookups();
    advance_settled(Duration::from_millis(3_050)).await;
    assert!(rig.client.is_open(), "the probes fail while cut");
    assert_eq!(rig.transport.sent(), sent + 3, "one probe a second");
    assert_eq!(rig.resolver.lookups(), lookups + 3, "each probe looks the name up afresh");
    assert_eq!(&rig.transport.built.lock().unwrap()[1..], &[0, 0, 0], "pool-less probes");

    *rig.backend.status.lock().unwrap() = 404;
    rig.net.apply_fault(Fault::Resume { dst: rig.dst });
    advance_settled(Duration::from_millis(1_000)).await;
    assert!(!rig.client.is_open(), "any HTTP response closes it");
    assert!(!*state.borrow_and_update());
    assert_eq!(rig.backend.served("/health"), 1, "the probe asked the health path");
    assert_eq!(metrics::breaker_transitions("breaker-cut:8080", false), 1);
    assert_eq!(rig.client.attempt(HttpRequest::get("/call"), ATTEMPT).await.unwrap().status, 404);
}

/// A breaker tripped at once (a name not resolved at boot) closes on the
/// first probe answered once the name resolves.
#[tokio::test(start_paused = true)]
async fn a_tripped_breaker_closes_once_the_name_resolves() {
    let rig = Rig::new("breaker-trip:8080", ClientSettings::default()).await;
    rig.resolver.unpoint("breaker-trip:8080");
    rig.client.trip();
    advance_settled(Duration::from_millis(2_050)).await;
    assert!(rig.client.is_open(), "not resolvable: the probes fail");
    rig.resolver.point("breaker-trip:8080", [rig.dst]);
    advance_settled(Duration::from_millis(1_000)).await;
    assert!(!rig.client.is_open());
}

/// One answering request inside the window keeps the breaker closed through
/// three failures.
#[tokio::test(start_paused = true)]
async fn an_answer_inside_the_window_keeps_the_breaker_closed() {
    let rig = Rig::new("breaker-answered:8080", ClientSettings::default()).await;
    rig.script(vec![Behaviour::Answer, Behaviour::Refuse, Behaviour::Refuse, Behaviour::Refuse]);
    for _ in 0..4 {
        let _ = rig.client.attempt(HttpRequest::get("/call"), ATTEMPT).await;
    }
    assert!(!rig.client.is_open());
}

// ── name lookup ─────────────────────────────────────────────────────────────

/// A resolver that never answers.
struct Hanging;

#[async_trait]
impl http_net::NameResolver for Hanging {
    async fn resolve(&self, _: &str) -> Vec<SocketAddr> {
        std::future::pending().await
    }
}

/// Hanging DNS: a request fails at the 2 s lookup bound, inside its budget,
/// and once 4 lookups are stuck the next request fails at once.
#[tokio::test(start_paused = true)]
async fn a_hanging_resolver_bounds_each_request_and_then_fails_at_once() {
    let net = SimulatedHttpNetwork::new();
    let target = Arc::new(ResolvedTarget::name_with("dns-hang:8080", Arc::new(Hanging)));
    let client = BackendClient::new(
        Arc::new(net),
        target,
        ClientSettings::default(),
        BreakerConfig::probing("/"),
    );
    for n in 0..http_net::MAX_STUCK {
        let start = Instant::now();
        let outcome = client.send_with_retry(HttpRequest::get("/"), budgets()).await;
        assert_eq!(transport_cause(&outcome), Some(FailureCause::Dns), "request {n}");
        assert_eq!(start.elapsed(), http_net::LOOKUP_BOUND, "request {n}");
        // The breaker would fail the next ones at once; this test reads
        // the lookup bound alone.
        if client.is_open() {
            break;
        }
    }
}

/// The lookup cap alone: four lookups stuck, the next request fails at once.
#[tokio::test(start_paused = true)]
async fn four_stuck_lookups_fail_the_next_request_at_once() {
    let target = ResolvedTarget::name_with("dns-cap:8080", Arc::new(Hanging));
    for _ in 0..http_net::MAX_STUCK {
        assert_eq!(target.resolve().await, None);
    }
    let start = Instant::now();
    assert_eq!(target.resolve().await, None);
    assert_eq!(start.elapsed(), Duration::ZERO);
}
