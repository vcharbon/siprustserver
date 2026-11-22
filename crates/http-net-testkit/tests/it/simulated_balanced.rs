//! `SimulatedHttpNetwork` connections: a backend client's pool (most recently
//! returned first, idle timeout, a closed connection never handed out, the
//! ejection rules closing a broken or shedding backend's connection, the
//! body cap) and a
//! balanced address choosing a backend per new connection, on the paused
//! clock.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use http_net::backend::metrics;
use http_net::ejection::CloseCause;
use http_net::failures::FailureCause;
use http_net::{
    AttemptError, BackendClient, BreakerConfig, ClientSettings, HttpClient, HttpRequest,
    HttpResponse, HttpService, HttpTransport, ResolvedTarget,
};
use http_net_testkit::{Behaviour, Fault, Pick, SimulatedHttpNetwork};

const FRONT: &str = "10.0.0.9:80";
const A: &str = "10.0.1.1:80";
const B: &str = "10.0.1.2:80";

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// Answers `status` and `body` after `delay`, counting the connections it
/// hears of.
struct Backend {
    status: Mutex<u16>,
    body: Mutex<Vec<u8>>,
    delay: Mutex<Duration>,
    served: AtomicUsize,
    opened: AtomicUsize,
    closed: AtomicUsize,
}

impl Backend {
    fn new(status: u16) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(status),
            body: Mutex::new(Vec::new()),
            delay: Mutex::new(Duration::ZERO),
            served: AtomicUsize::new(0),
            opened: AtomicUsize::new(0),
            closed: AtomicUsize::new(0),
        })
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl HttpService for Backend {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        self.served.fetch_add(1, Ordering::SeqCst);
        let delay = *self.delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let body = self.body.lock().unwrap().clone();
        HttpResponse::status(*self.status.lock().unwrap()).with_body(body)
    }

    fn connection_opened(&self) {
        self.opened.fetch_add(1, Ordering::SeqCst);
    }

    fn connection_closed(&self) {
        self.closed.fetch_add(1, Ordering::SeqCst);
    }
}

struct Pair {
    net: SimulatedHttpNetwork,
    a: Arc<Backend>,
    b: Arc<Backend>,
    _servers: Vec<Box<dyn http_net::HttpServerHandle>>,
}

async fn pair(a_status: u16) -> Pair {
    let net = SimulatedHttpNetwork::new();
    let (a, b) = (Backend::new(a_status), Backend::new(200));
    let servers = vec![
        net.serve(addr(A), a.clone()).await.unwrap(),
        net.serve(addr(B), b.clone()).await.unwrap(),
    ];
    net.balance(addr(FRONT), &[addr(A), addr(B)], Pick::RoundRobin);
    Pair { net, a, b, _servers: servers }
}

fn client(net: &SimulatedHttpNetwork) -> Arc<dyn HttpClient> {
    Arc::new(net.clone())
        .client(Arc::new(ResolvedTarget::addr(addr(FRONT))), &ClientSettings::with_max_in_flight(8))
}

async fn get(c: &Arc<dyn HttpClient>) -> Result<HttpResponse, http_net::HttpError> {
    c.request(HttpRequest::get("/")).await
}

#[tokio::test(start_paused = true)]
async fn a_request_without_a_client_opens_a_connection_each_spread_over_the_members() {
    let p = pair(200).await;
    for _ in 0..4 {
        p.net.request(addr(FRONT), HttpRequest::get("/")).await.unwrap();
    }
    assert_eq!((p.a.served(), p.b.served()), (2, 2));
    assert_eq!((p.net.connections_opened(addr(A)), p.net.connections_opened(addr(B))), (2, 2));
    assert_eq!(p.net.connections_open(addr(A)), 0, "each closed after its answer");
}

#[tokio::test(start_paused = true)]
async fn a_pooled_client_keeps_its_backend_for_every_sequential_request() {
    let p = pair(200).await;
    let c = client(&p.net);
    for _ in 0..10 {
        get(&c).await.unwrap();
    }
    assert_eq!((p.a.served(), p.b.served()), (10, 0), "one connection, on the first member");
    assert_eq!(p.net.connections_open(addr(A)), 1, "kept idle in the pool");
}

#[tokio::test(start_paused = true)]
async fn a_broken_backends_first_answer_closes_its_connection_and_the_next_one_spreads_again() {
    let p = pair(500).await;
    let c = client(&p.net);
    for _ in 0..4 {
        get(&c).await.unwrap();
    }
    assert_eq!(
        (p.a.served(), p.b.served()),
        (1, 3),
        "A's 500 closed it; the new connection, B's, stays"
    );
    assert_eq!(p.net.connections_open(addr(A)), 0);
    assert_eq!(p.net.connections_open(addr(B)), 1);
}

/// Each close the client makes outside an answer is counted by its cause,
/// under the target's own name.
#[tokio::test(start_paused = true)]
async fn an_expired_an_abandoned_and_a_retired_connection_are_counted_by_cause() {
    const COUNTED: &str = "10.0.0.8:80";
    let p = pair(200).await;
    p.net.balance(addr(COUNTED), &[addr(A)], Pick::RoundRobin);
    let target = Arc::new(ResolvedTarget::addr(addr(COUNTED)));
    let c = Arc::new(p.net.clone()).client(target, &ClientSettings::with_max_in_flight(8));
    get(&c).await.unwrap();
    tokio::time::sleep(ClientSettings::default().pool_idle_timeout).await;
    get(&c).await.unwrap();
    *p.a.delay.lock().unwrap() = Duration::from_millis(100);
    assert!(tokio::time::timeout(Duration::from_millis(50), get(&c)).await.is_err());
    *p.a.delay.lock().unwrap() = Duration::ZERO;
    get(&c).await.unwrap();
    drop(c);
    let closes = |cause| metrics::connection_closes(COUNTED, cause);
    assert_eq!(
        (closes(CloseCause::Idle), closes(CloseCause::Timeout), closes(CloseCause::Rotation)),
        (1, 1, 1)
    );
    assert_eq!(p.net.connections_open(addr(A)), 0);
}

#[tokio::test(start_paused = true)]
async fn a_shedding_backends_third_answer_in_a_row_closes_its_connection() {
    let p = pair(503).await;
    let c = client(&p.net);
    for _ in 0..5 {
        get(&c).await.unwrap();
    }
    assert_eq!((p.a.served(), p.b.served()), (3, 2), "three 503s on A's connection, then B's");
    assert_eq!(p.net.connections_open(addr(A)), 0);
}

#[tokio::test(start_paused = true)]
async fn a_shed_answer_between_two_successes_closes_nothing() {
    let p = pair(200).await;
    let c = client(&p.net);
    for status in [200, 503, 200, 503, 503, 200, 429, 200] {
        *p.a.status.lock().unwrap() = status;
        assert_eq!(get(&c).await.unwrap().status, status);
    }
    assert_eq!((p.a.served(), p.b.served()), (8, 0), "one connection throughout");
    assert_eq!(p.net.connections_opened(addr(A)), 1);
}

#[tokio::test(start_paused = true)]
async fn past_the_close_budget_an_error_answer_keeps_the_connection() {
    let p = pair(500).await;
    let mut settings = ClientSettings::with_max_in_flight(8);
    settings.ejection.closes_per_s = 0;
    let c = Arc::new(p.net.clone()).client(Arc::new(ResolvedTarget::addr(addr(FRONT))), &settings);
    for _ in 0..5 {
        assert_eq!(get(&c).await.unwrap().status, 500);
    }
    assert_eq!(p.a.served(), 5, "every request back on the backend that answered 500");
    assert_eq!(p.net.connections_opened(addr(A)), 1);
}

/// Every answer passes the body cap: each fails `BodyTooLarge` on a
/// connection of its own, spread again, and the backend answered, so its
/// breaker stays closed.
#[tokio::test(start_paused = true)]
async fn answers_past_the_body_cap_close_their_connections_and_keep_the_breaker_closed() {
    let p = pair(200).await;
    *p.a.body.lock().unwrap() = b"answer".to_vec();
    *p.b.body.lock().unwrap() = b"answer".to_vec();
    let settings = ClientSettings { max_body_bytes: 4, ..ClientSettings::with_max_in_flight(8) };
    let target = Arc::new(ResolvedTarget::addr(addr(FRONT)));
    let backend =
        BackendClient::new(Arc::new(p.net.clone()), target, settings, BreakerConfig::probing("/"));

    for _ in 0..4 {
        let outcome = backend.attempt(HttpRequest::get("/"), Duration::from_millis(500)).await;
        let cause = match outcome {
            Err(AttemptError::Transport(e)) => Some(e.cause),
            _ => None,
        };
        assert_eq!(cause, Some(FailureCause::BodyTooLarge));
    }

    assert!(!backend.is_open(), "four answers inside the window: the backend is alive");
    assert_eq!((p.a.served(), p.b.served()), (2, 2), "a new connection each, spread again");
    assert_eq!(p.net.connections_open(addr(A)) + p.net.connections_open(addr(B)), 0);
    assert_eq!(metrics::bodies_refused(FRONT, metrics::BodyRefusal::Declared), 4);
}

#[tokio::test(start_paused = true)]
async fn the_most_recently_returned_connection_is_taken_first() {
    let p = pair(200).await;
    *p.a.delay.lock().unwrap() = Duration::from_millis(100);
    let c = client(&p.net);
    // Two at once: the first on A (slow), the second on a new connection to B.
    let (r1, r2) = tokio::join!(get(&c), get(&c));
    assert!(r1.is_ok() && r2.is_ok());
    // B came back first, A last: the next request takes A's connection.
    get(&c).await.unwrap();
    assert_eq!((p.a.served(), p.b.served()), (2, 1));
    assert_eq!(p.net.connections_opened(addr(A)) + p.net.connections_opened(addr(B)), 2);
}

#[tokio::test(start_paused = true)]
async fn an_idle_connection_past_the_idle_timeout_is_closed_and_a_new_one_opened() {
    let p = pair(200).await;
    let c = client(&p.net);
    get(&c).await.unwrap();
    tokio::time::sleep(ClientSettings::default().pool_idle_timeout).await;
    get(&c).await.unwrap();
    assert_eq!((p.a.served(), p.b.served()), (1, 1), "the new connection went to the next member");
    assert_eq!(p.a.closed.load(Ordering::SeqCst), 1, "the expired connection closed");
}

#[tokio::test(start_paused = true)]
async fn a_backend_closing_its_connections_is_never_handed_out_again() {
    let p = pair(200).await;
    let c = client(&p.net);
    get(&c).await.unwrap();
    p.net.apply_fault(Fault::Close { dst: addr(A) });
    get(&c).await.unwrap();
    assert_eq!((p.a.served(), p.b.served()), (1, 1), "a new connection, to B");
    assert_eq!(p.net.connections_open(addr(A)), 0);
}

#[tokio::test(start_paused = true)]
async fn a_refusing_member_fails_its_new_connections_and_closes_the_open_ones() {
    let p = pair(200).await;
    let c = client(&p.net);
    get(&c).await.unwrap();
    p.net.apply_fault(Fault::Behave { dst: addr(A), behaviour: Behaviour::Eof });
    // The idle connection to A is closed: B, then A (refused at once), then B again.
    get(&c).await.unwrap();
    assert_eq!(p.b.served(), 1);
    let c2 = client(&p.net);
    assert_eq!(
        get(&c2).await.unwrap_err().cause,
        FailureCause::Eof,
        "the next new connection is A's"
    );
}

#[tokio::test(start_paused = true)]
async fn a_request_in_flight_when_its_backend_closes_the_connection_fails_eof() {
    let p = pair(200).await;
    *p.a.delay.lock().unwrap() = Duration::from_millis(100);
    let c = client(&p.net);
    let pending = tokio::spawn({
        let c = c.clone();
        async move { get(&c).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    p.net.apply_fault(Fault::Close { dst: addr(A) });
    assert_eq!(pending.await.unwrap().unwrap_err().cause, FailureCause::Eof);
}

#[tokio::test(start_paused = true)]
async fn a_request_its_caller_drops_closes_its_connection() {
    let p = pair(200).await;
    p.net.apply_fault(Fault::Stall { dst: addr(A) });
    let c = client(&p.net);
    assert!(tokio::time::timeout(Duration::from_millis(100), get(&c)).await.is_err());
    p.net.apply_fault(Fault::Resume { dst: addr(A) });
    get(&c).await.unwrap();
    assert_eq!((p.a.served(), p.b.served()), (0, 1), "no connection to A left in the pool");
}

#[tokio::test(start_paused = true)]
async fn a_member_removed_takes_no_new_connection_and_its_dropped_server_closes_the_rest() {
    let net = SimulatedHttpNetwork::new();
    let (a, b) = (Backend::new(200), Backend::new(200));
    let server_a = net.serve(addr(A), a.clone()).await.unwrap();
    let _server_b = net.serve(addr(B), b.clone()).await.unwrap();
    net.balance(addr(FRONT), &[addr(A), addr(B)], Pick::RoundRobin);
    let c = client(&net);
    get(&c).await.unwrap();
    net.set_members(addr(FRONT), &[addr(B)]);
    get(&c).await.unwrap();
    assert_eq!(a.served(), 2, "an open connection keeps its backend");
    drop(server_a);
    assert_eq!(a.closed.load(Ordering::SeqCst), 0, "closed lazily, when next looked at");
    get(&c).await.unwrap();
    assert_eq!((a.served(), b.served()), (2, 1));
    assert_eq!(a.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_client_closes_its_idle_connections() {
    let p = pair(200).await;
    let c = client(&p.net);
    get(&c).await.unwrap();
    assert_eq!(p.a.opened.load(Ordering::SeqCst), 1);
    drop(c);
    assert_eq!(p.a.closed.load(Ordering::SeqCst), 1);
    assert_eq!(p.net.connections_open(addr(A)), 0);
}

#[tokio::test(start_paused = true)]
async fn a_seeded_front_spreads_new_connections_the_same_way_every_run() {
    let spread = || async {
        let p = pair(200).await;
        p.net.balance(addr(FRONT), &[addr(A), addr(B)], Pick::Seeded(42));
        for _ in 0..200 {
            p.net.request(addr(FRONT), HttpRequest::get("/")).await.unwrap();
        }
        (p.a.served(), p.b.served())
    };
    let first = spread().await;
    assert_eq!(first, spread().await);
    assert!((70..130).contains(&first.0), "{first:?}");
}
