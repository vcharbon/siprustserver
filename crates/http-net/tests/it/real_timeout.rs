//! A request that times out on the real client, on loopback and the real
//! clock (each test well under 1 s): its connection never goes back to the
//! pool, it is closed by a reset, and the answer that comes late on it is
//! never read as the answer of another request. That the connection is
//! never reused is behaviour kept, the reset is new (ADR-0043 §14). Both
//! ways a request times out: the client's own ceiling, and a caller that
//! drops the request at the end of its budget (`BackendClient::attempt`),
//! each on a current-thread runtime and, run after run, on a multi-thread
//! one.

#![cfg(feature = "real")]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_net::backend::metrics;
use http_net::ejection::CloseCause;
use http_net::failures::FailureCause;
use http_net::{
    AttemptError, BackendClient, BreakerConfig, ClientSettings, HttpRequest, HttpTransport,
    RealHttpNetwork, ResolvedTarget,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// The bound a test request times out at.
const TIMEOUT: Duration = Duration::from_millis(100);
/// When the first connection's peer answers, past [`TIMEOUT`].
const LATE: Duration = Duration::from_millis(200);

/// How the first connection ended, as its peer read it.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    /// A reset: the client closed it with `SO_LINGER` 0.
    Reset,
    /// An orderly close (FIN).
    Closed,
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
        Err(e) => panic!("the first connection failed otherwise: {e}"),
    }
}

/// A peer whose first connection answers `late` after [`LATE`] and reports
/// how it ended; every later connection answers `fresh` at once. The count
/// of connections it accepted.
async fn peer() -> (SocketAddr, mpsc::UnboundedReceiver<Ending>, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let accepted = Arc::new(Mutex::new(0usize));
    let count = accepted.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let n = {
                let mut c = count.lock().unwrap();
                *c += 1;
                *c
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                if !read_head(&mut stream).await {
                    return;
                }
                if n > 1 {
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfresh")
                        .await;
                    std::future::pending::<()>().await;
                }
                let end = tokio::select! {
                    end = ending(&mut stream) => end,
                    _ = tokio::time::sleep(LATE) => {
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nlate").await;
                        ending(&mut stream).await
                    }
                };
                let _ = tx.send(end);
            });
        }
    });
    (addr, rx, accepted)
}

/// Runs of each case on a multi-thread runtime, where the connection's task
/// may run on another thread as the request is dropped.
const RUNS: usize = 5;

/// A request past the client's ceiling, then the next one on the same
/// client. The ceiling bounds the next request too: on a loaded box it may
/// be cut as well, never answered `late`.
async fn past_the_ceiling() {
    let (addr, mut ended, accepted) = peer().await;
    let settings = ClientSettings { ceiling: TIMEOUT, ..ClientSettings::default() };
    let client = Arc::new(RealHttpNetwork::with_settings(&settings))
        .client(Arc::new(ResolvedTarget::addr(addr)), &settings);

    let err = client.request(HttpRequest::get("/")).await.expect_err("past the ceiling");
    assert_eq!(err.cause, FailureCause::RequestTimeout, "{err:?}");
    let next_cut = match client.request(HttpRequest::get("/")).await {
        Ok(resp) => {
            assert_eq!(resp.body, b"fresh");
            0
        }
        Err(e) => {
            assert_eq!(e.cause, FailureCause::RequestTimeout, "{e:?}");
            1
        }
    };

    assert_eq!(ended.recv().await, Some(Ending::Reset));
    assert_eq!(*accepted.lock().unwrap(), 2, "the next request opened a connection");
    let closes = metrics::connection_closes(&addr.to_string(), CloseCause::Timeout);
    assert_eq!(closes, 1 + next_cut);
}

#[tokio::test]
async fn a_request_past_the_ceiling_resets_its_connection_never_pooled() {
    past_the_ceiling().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_past_the_ceiling_resets_its_connection_on_a_multi_thread_runtime() {
    for _ in 0..RUNS {
        past_the_ceiling().await;
    }
}

#[tokio::test]
async fn a_request_its_caller_drops_at_the_budget_resets_its_connection_never_pooled() {
    dropped_at_the_budget().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_its_caller_drops_at_the_budget_resets_its_connection_on_a_multi_thread_runtime()
{
    for _ in 0..RUNS {
        dropped_at_the_budget().await;
    }
}

/// A request its caller drops at the end of its budget, then the next one.
async fn dropped_at_the_budget() {
    let (addr, mut ended, accepted) = peer().await;
    let client = BackendClient::new(
        Arc::new(RealHttpNetwork::new()),
        Arc::new(ResolvedTarget::addr(addr)),
        ClientSettings::default(),
        BreakerConfig::probing("/"),
    );

    let outcome = client.attempt(HttpRequest::get("/"), TIMEOUT).await;
    assert_eq!(outcome.map(|r| r.status), Err(AttemptError::Timeout));
    let body = client.attempt(HttpRequest::get("/"), Duration::from_millis(500)).await;
    assert_eq!(body.map(|r| r.body), Ok(b"fresh".to_vec()));

    assert_eq!(ended.recv().await, Some(Ending::Reset));
    assert_eq!(*accepted.lock().unwrap(), 2, "the next request opened a connection");
    assert_eq!(metrics::connection_closes(&addr.to_string(), CloseCause::Timeout), 1);
}
