//! The real client's connection ejection, on loopback and the real clock
//! (each run well under 1 s): two real servers behind a front that sends
//! each new TCP connection to one of them, as a layer-4 balancer does, and
//! the client's connections as each server hears of them.

#![cfg(feature = "real")]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use http_net::backend::metrics;
use http_net::ejection::CloseCause;
use http_net::{
    ClientSettings, HttpClient, HttpRequest, HttpResponse, HttpServerHandle, HttpService,
    HttpTransport, RealHttpNetwork, ResolvedTarget,
};
use tokio::net::{TcpListener, TcpStream};

/// Answers the statuses of its script in turn, then `then`, counting what
/// it served and the connections it heard of.
struct Backend {
    script: Mutex<VecDeque<u16>>,
    then: u16,
    served: AtomicUsize,
    opened: AtomicUsize,
    closed: AtomicUsize,
}

impl Backend {
    fn answering(script: &[u16], then: u16) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.iter().copied().collect()),
            then,
            served: AtomicUsize::new(0),
            opened: AtomicUsize::new(0),
            closed: AtomicUsize::new(0),
        })
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    fn opened(&self) -> usize {
        self.opened.load(Ordering::SeqCst)
    }

    /// Its connections closed, once `n` have or after 1 s.
    async fn closed_once(&self, n: usize) -> usize {
        for _ in 0..200 {
            if self.closed.load(Ordering::SeqCst) >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.closed.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl HttpService for Backend {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        self.served.fetch_add(1, Ordering::SeqCst);
        let status = self.script.lock().unwrap().pop_front().unwrap_or(self.then);
        HttpResponse::status(status).with_body(b"answer".to_vec())
    }

    fn connection_opened(&self) {
        self.opened.fetch_add(1, Ordering::SeqCst);
    }

    fn connection_closed(&self) {
        self.closed.fetch_add(1, Ordering::SeqCst);
    }
}

async fn serve(net: &RealHttpNetwork, backend: &Arc<Backend>) -> Box<dyn HttpServerHandle> {
    net.serve("127.0.0.1:0".parse().unwrap(), backend.clone()).await.unwrap()
}

/// A front that sends its first connection to `first` and every later one
/// to `rest`, relaying bytes both ways until either side closes.
async fn front(first: SocketAddr, rest: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut n = 0;
        while let Ok((mut client, _)) = listener.accept().await {
            let to = if n == 0 { first } else { rest };
            n += 1;
            tokio::spawn(async move {
                let Ok(mut server) = TcpStream::connect(to).await else { return };
                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
            });
        }
    });
    addr
}

/// A front that sends its new connections to `a` and `b` in turn.
async fn alternating_front(a: SocketAddr, b: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut n = 0usize;
        while let Ok((mut client, _)) = listener.accept().await {
            let to = if n.is_multiple_of(2) { a } else { b };
            n += 1;
            tokio::spawn(async move {
                let Ok(mut server) = TcpStream::connect(to).await else { return };
                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
            });
        }
    });
    addr
}

fn client_of(dst: SocketAddr, settings: &ClientSettings) -> Arc<dyn HttpClient> {
    Arc::new(RealHttpNetwork::with_settings(settings))
        .client(Arc::new(ResolvedTarget::addr(dst)), settings)
}

async fn statuses(client: &Arc<dyn HttpClient>, n: usize) -> Vec<u16> {
    let mut seen = Vec::new();
    for _ in 0..n {
        seen.push(client.request(HttpRequest::get("/")).await.expect("an answer").status);
    }
    seen
}

#[tokio::test]
async fn a_broken_backends_first_500_closes_its_connection_and_the_healthy_one_stays_pooled() {
    let net = RealHttpNetwork::new();
    let (broken, healthy) = (Backend::answering(&[], 500), Backend::answering(&[], 200));
    let (_a, _b) = (serve(&net, &broken).await, serve(&net, &healthy).await);
    let dst = front(_a.local_addr(), _b.local_addr()).await;
    let client = client_of(dst, &ClientSettings::default());

    assert_eq!(statuses(&client, 5).await, [500, 200, 200, 200, 200]);

    assert_eq!((broken.served(), healthy.served()), (1, 4), "the 500's connection never reused");
    assert_eq!((broken.opened(), healthy.opened()), (1, 1), "one healthy connection, pooled");
    assert_eq!(broken.closed_once(1).await, 1);
    let peer = dst.to_string();
    assert_eq!(metrics::connection_closes(&peer, CloseCause::Status), 1);
    assert_eq!(metrics::error_answers(&peer, true), 1);
}

/// Sixteen tasks on eight threads share one client: a request on another
/// thread may take a connection whose 500 closes it before the poison
/// (the real client's module doc), never a third.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn on_a_multi_thread_runtime_a_broken_backends_connection_carries_at_most_two_requests() {
    let net = RealHttpNetwork::new();
    let (broken, healthy) = (Backend::answering(&[], 500), Backend::answering(&[], 200));
    let (_a, _b) = (serve(&net, &broken).await, serve(&net, &healthy).await);
    let dst = alternating_front(_a.local_addr(), _b.local_addr()).await;
    let mut settings = ClientSettings::default();
    settings.ejection.closes_per_s = 1_000_000;
    let client = client_of(dst, &settings);

    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move {
                for _ in 0..200 {
                    let _ = client.request(HttpRequest::get("/")).await;
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }

    let peer = dst.to_string();
    let closed = metrics::connection_closes(&peer, CloseCause::Status) as usize;
    let served = broken.served();
    assert!((1..=broken.opened()).contains(&closed), "{closed} closes, {} opened", broken.opened());
    assert!(served >= closed && served <= 2 * closed, "{served} 500s on {closed} connections");
    assert_eq!(metrics::error_answers(&peer, true) as usize, served, "every 500 closed its own");
}

#[tokio::test]
async fn a_shedding_backend_keeps_its_connection_for_two_503s_and_loses_it_on_the_third() {
    let net = RealHttpNetwork::new();
    let (shedding, healthy) = (Backend::answering(&[], 503), Backend::answering(&[], 200));
    let (_a, _b) = (serve(&net, &shedding).await, serve(&net, &healthy).await);
    let dst = front(_a.local_addr(), _b.local_addr()).await;
    let client = client_of(dst, &ClientSettings::default());

    assert_eq!(statuses(&client, 5).await, [503, 503, 503, 200, 200]);

    assert_eq!((shedding.served(), shedding.opened()), (3, 1), "three in a row on one connection");
    assert_eq!(shedding.closed_once(1).await, 1);
    let peer = dst.to_string();
    assert_eq!(metrics::connection_closes(&peer, CloseCause::Shed), 1);
    assert_eq!((metrics::error_answers(&peer, false), metrics::error_answers(&peer, true)), (2, 1));
}

#[tokio::test]
async fn a_503_between_two_successes_closes_nothing() {
    let net = RealHttpNetwork::new();
    let backend = Backend::answering(&[200, 503, 200, 503, 503, 200, 429, 200], 200);
    let server = serve(&net, &backend).await;
    let client = client_of(server.local_addr(), &ClientSettings::default());

    assert_eq!(statuses(&client, 8).await, [200, 503, 200, 503, 503, 200, 429, 200]);

    assert_eq!(backend.opened(), 1, "one connection throughout");
    assert_eq!(metrics::connection_closes(&server.local_addr().to_string(), CloseCause::Shed), 0);
}

#[tokio::test]
async fn past_the_close_budget_a_500_keeps_its_connection_and_is_counted() {
    let net = RealHttpNetwork::new();
    let backend = Backend::answering(&[], 500);
    let server = serve(&net, &backend).await;
    let mut settings = ClientSettings::default();
    settings.ejection.closes_per_s = 2;
    let client = client_of(server.local_addr(), &settings);

    assert_eq!(statuses(&client, 5).await, [500; 5]);

    let peer = server.local_addr().to_string();
    let closed = metrics::connection_closes(&peer, CloseCause::Status);
    let exhausted = metrics::closes_budget_exhausted_count(&peer);
    assert!(exhausted >= 1, "five 500s within a second pass a budget of two");
    assert_eq!(closed + exhausted, 5, "each 500 closed its connection or found the budget spent");
    assert_eq!(backend.opened() as u64, 1 + closed, "a new connection after each close");
    assert_eq!(
        (metrics::error_answers(&peer, true), metrics::error_answers(&peer, false)),
        (closed, exhausted)
    );
}

/// An answer refused past the body cap closes its connection: the request
/// after it opens a new one.
#[tokio::test]
async fn an_answer_refused_past_the_body_cap_never_returns_its_connection_to_the_pool() {
    let net = RealHttpNetwork::new();
    let backend = Backend::answering(&[], 200);
    let server = serve(&net, &backend).await;
    let settings = ClientSettings { max_body_bytes: 4, ..ClientSettings::default() };
    let client = client_of(server.local_addr(), &settings);

    for _ in 0..2 {
        let err = client.request(HttpRequest::get("/")).await.expect_err("6 bytes past 4");
        assert_eq!(err.cause, http_net::failures::FailureCause::BodyTooLarge, "{err:?}");
    }

    assert_eq!((backend.served(), backend.opened()), (2, 2), "a connection each");
    let peer = server.local_addr().to_string();
    assert_eq!(metrics::bodies_refused(&peer, metrics::BodyRefusal::Declared), 2);
}

#[tokio::test]
async fn a_connection_idle_past_its_timeout_and_one_of_a_dropped_client_are_counted() {
    let net = RealHttpNetwork::new();
    let backend = Backend::answering(&[], 200);
    let server = serve(&net, &backend).await;
    let settings = ClientSettings {
        pool_idle_timeout: Duration::from_millis(100),
        ..ClientSettings::default()
    };
    let client = client_of(server.local_addr(), &settings);
    let peer = server.local_addr().to_string();

    assert_eq!(statuses(&client, 1).await, [200]);
    assert_eq!(backend.closed_once(1).await, 1, "the idle connection closed by the pool");
    assert_eq!(statuses(&client, 1).await, [200]);
    drop(client);
    assert_eq!(backend.closed_once(2).await, 2, "the dropped client's connection closed");

    assert_eq!(metrics::connection_closes(&peer, CloseCause::Idle), 1);
    assert_eq!(metrics::connection_closes(&peer, CloseCause::Rotation), 1);
}
