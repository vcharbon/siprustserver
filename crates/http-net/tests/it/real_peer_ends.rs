//! The real client's connections whose end is no close of the client's, on
//! loopback and the real clock (each run well under 1 s): one its peer ends
//! by an answer that disables keep-alive, and one a client that keeps no
//! pool (the breaker's probe) closes.

#![cfg(feature = "real")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http_net::backend::metrics;
use http_net::ejection::CloseCause;
use http_net::{
    BackendClient, BreakerConfig, ClientSettings, HttpRequest, HttpResponse, HttpService,
    HttpTransport, RealHttpNetwork, ResolvedTarget,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// How a connection ended, as its peer read it.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    /// An orderly close (FIN).
    Closed,
    /// A reset.
    Reset,
    /// Bytes: the client sent another request on it.
    Reused,
}

async fn read_head(stream: &mut TcpStream) -> bool {
    let mut buf = [0u8; 4096];
    let mut seen = Vec::new();
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
    }
    true
}

async fn ending(stream: &mut TcpStream) -> Ending {
    let mut buf = [0u8; 64];
    match stream.read(&mut buf).await {
        Ok(0) => Ending::Closed,
        Ok(_) => Ending::Reused,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => Ending::Reset,
        Err(e) => panic!("the connection failed otherwise: {e}"),
    }
}

/// A peer answering each connection's first request with `answer`, closing
/// its own side after it when `fin`, and reporting how each ended.
async fn peer(answer: &'static [u8], fin: bool) -> (SocketAddr, mpsc::UnboundedReceiver<Ending>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                if !read_head(&mut stream).await {
                    return;
                }
                let _ = stream.write_all(answer).await;
                if fin {
                    let _ = stream.shutdown().await;
                }
                let _ = tx.send(ending(&mut stream).await);
            });
        }
    });
    (addr, rx)
}

const CLOSE: &[u8] = b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok";
const HTTP_10: &[u8] = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok";

#[tokio::test]
async fn a_connection_its_peer_ends_by_an_answer_without_keep_alive_is_no_close_of_the_client() {
    for (n, (answer, fin)) in
        [(CLOSE, false), (CLOSE, true), (HTTP_10, false)].into_iter().enumerate()
    {
        let (addr, mut ended) = peer(answer, fin).await;
        let client = Arc::new(RealHttpNetwork::new())
            .client(Arc::new(ResolvedTarget::addr(addr)), &ClientSettings::default());

        for _ in 0..2 {
            let resp = client.request(HttpRequest::get("/")).await.expect("an answer");
            assert_eq!(resp.body, b"ok", "case {n}");
            assert_eq!(ended.recv().await, Some(Ending::Closed), "case {n}: never reused");
        }
        drop(client);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let peer = addr.to_string();
        let counted: Vec<u64> =
            CloseCause::ALL.iter().map(|c| metrics::connection_closes(&peer, *c)).collect();
        assert_eq!(counted, [0; 5], "case {n}: status, shed, timeout, rotation, idle");
    }
}

/// Answers every request 200, counting them.
#[derive(Default)]
struct Health {
    served: AtomicUsize,
}

#[async_trait]
impl HttpService for Health {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        self.served.fetch_add(1, Ordering::SeqCst);
        HttpResponse::ok(b"up".to_vec())
    }
}

#[tokio::test]
async fn an_answered_probe_of_an_open_breaker_counts_no_connection_close() {
    let net = RealHttpNetwork::new();
    let health = Arc::new(Health::default());
    let server = net.serve("127.0.0.1:0".parse().unwrap(), health.clone()).await.unwrap();
    let addr = server.local_addr();
    let breaker =
        BreakerConfig { probe_period: Duration::from_millis(50), ..BreakerConfig::probing("/") };
    let client = BackendClient::new(
        Arc::new(RealHttpNetwork::new()),
        Arc::new(ResolvedTarget::addr(addr)),
        ClientSettings::default(),
        breaker,
    );
    let mut open = client.breaker_state();

    client.trip();
    while *open.borrow_and_update() {
        open.changed().await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(health.served.load(Ordering::SeqCst), 1, "one probe, answered");
    let peer = addr.to_string();
    let counted: Vec<u64> =
        CloseCause::ALL.iter().map(|c| metrics::connection_closes(&peer, *c)).collect();
    assert_eq!(counted, [0; 5], "status, shed, timeout, rotation, idle");
}
