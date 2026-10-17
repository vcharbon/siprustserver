//! The receipt stamp a served request carries: the real server stamps it when
//! the request head arrives, before the body is read, and the simulated fabric
//! states none.

#![cfg(feature = "real")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use http_net::{
    HttpRequest, HttpResponse, HttpService, HttpTransport, RealHttpNetwork, Receipt,
    SimulatedHttpNetwork,
};
use sip_clock::testkit::advance_settled;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Keeps the receipt each answered request saw.
#[derive(Default)]
struct Stamps(Mutex<Vec<Option<Receipt>>>);

#[async_trait]
impl HttpService for Stamps {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        self.0.lock().unwrap().push(Receipt::current());
        HttpResponse::ok(req.body)
    }
}

/// A client sends the head, waits, then the body: the receipt the service
/// reads is the head's arrival, earlier than the body's, and its process time
/// counts the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_receipt_is_stamped_before_the_body_arrives() {
    let stamps = Arc::new(Stamps::default());
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), stamps.clone()).await.unwrap();
    let wall_before = SystemTime::now();

    let mut stream = tokio::net::TcpStream::connect(handle.local_addr()).await.unwrap();
    stream
        .write_all(
            b"POST /slow HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let body_sent = Instant::now();
    stream.write_all(b"ping").await.unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.unwrap();
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("ping"), "{answer}");

    let seen = stamps.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "one request answered: {seen:?}");
    let receipt = seen[0].expect("the real server states a receipt");
    assert!(
        receipt.instant < body_sent,
        "the receipt is the head's arrival, {:?} before the body was sent",
        body_sent.saturating_duration_since(receipt.instant)
    );
    assert!(receipt.at >= wall_before, "the wall stamp is the receipt's own");
    assert!(receipt.unix_us() > 0);
}

/// The simulated fabric moves no bytes, so it states no receipt.
#[tokio::test(start_paused = true)]
async fn the_simulated_fabric_states_no_receipt() {
    let stamps = Arc::new(Stamps::default());
    let net = SimulatedHttpNetwork::new();
    let addr = "10.0.0.1:8080".parse().unwrap();
    let _server = net.serve(addr, stamps.clone()).await.unwrap();
    let sent = tokio::spawn({
        let net = net.clone();
        async move { net.request(addr, HttpRequest::post("/x", Vec::new())).await }
    });
    advance_settled(Duration::from_millis(10)).await;
    assert_eq!(sent.await.unwrap().unwrap().status, 200);
    assert_eq!(*stamps.0.lock().unwrap(), vec![None]);
}
