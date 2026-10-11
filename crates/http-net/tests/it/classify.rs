//! The real client's failures, classified on loopback (real clock, well
//! under 1 s): each `reqwest` error reaches the caller as the
//! [`FailureCause`] the simulated fabric reports for the same fault. A
//! backend client's URL names its target, so `Host` is the name, and its
//! name lookups go through the target.

#![cfg(feature = "real")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http_net::failures::FailureCause;
use http_net::{
    ClientSettings, HttpRequest, HttpTransport, MemoryResolver, RealHttpNetwork, ResolvedTarget,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A loopback listener that runs `serve` on each connection it accepts.
async fn listener<F, Fut>(serve: F) -> SocketAddr
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve(stream));
        }
    });
    addr
}

/// Read one request's head off `stream`: what it said.
async fn read_head(stream: &mut TcpStream) -> String {
    let mut buf = vec![0u8; 4096];
    let mut got = Vec::new();
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&got).into_owned()
}

/// A client whose every request is bounded at 200 ms.
fn net() -> RealHttpNetwork {
    let settings =
        ClientSettings { ceiling: Duration::from_millis(200), ..ClientSettings::default() };
    RealHttpNetwork::with_settings(&settings)
}

async fn cause_of(dst: SocketAddr) -> FailureCause {
    let err = net().request(dst, HttpRequest::get("/")).await.expect_err("no response");
    err.cause
}

#[tokio::test]
async fn a_closed_port_is_refused() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dst = closed.local_addr().unwrap();
    drop(closed);
    assert_eq!(cause_of(dst).await, FailureCause::Refused);
}

#[tokio::test]
async fn a_server_closing_after_the_request_is_eof() {
    let dst = listener(|mut s| async move {
        read_head(&mut s).await;
    })
    .await;
    assert_eq!(cause_of(dst).await, FailureCause::Eof);
}

#[tokio::test]
async fn a_server_closing_mid_body_is_eof() {
    let dst = listener(|mut s| async move {
        read_head(&mut s).await;
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc").await;
    })
    .await;
    assert_eq!(cause_of(dst).await, FailureCause::Eof);
}

#[tokio::test]
async fn a_server_resetting_the_connection_is_a_reset() {
    let dst = listener(|mut s| async move {
        read_head(&mut s).await;
        // SO_LINGER 0: the close sends a RST.
        let _ = socket2::SockRef::from(&s).set_linger(Some(Duration::ZERO));
    })
    .await;
    assert_eq!(cause_of(dst).await, FailureCause::ConnReset);
}

#[tokio::test]
async fn a_silent_server_is_a_request_timeout() {
    let dst = listener(|mut s| async move {
        read_head(&mut s).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    })
    .await;
    assert_eq!(cause_of(dst).await, FailureCause::RequestTimeout);
}

#[tokio::test]
async fn a_body_stalled_after_the_headers_is_a_request_timeout() {
    let dst = listener(|mut s| async move {
        read_head(&mut s).await;
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc").await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    })
    .await;
    assert_eq!(cause_of(dst).await, FailureCause::RequestTimeout);
}

/// A backend client's URL carries the target's name: `Host` is the name,
/// and the name resolves through the target.
#[tokio::test]
async fn a_backend_client_sends_the_name_and_resolves_it_through_its_target() {
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
    let dst = listener(move |mut s| {
        let tx = tx.clone();
        async move {
            let head = read_head(&mut s).await;
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(head);
            }
            let _ = s.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").await;
        }
    })
    .await;
    let resolver = Arc::new(MemoryResolver::new());
    let name = format!("backend.test:{}", dst.port());
    let target = Arc::new(ResolvedTarget::name_with(name.clone(), resolver.clone()));
    let client = Arc::new(net()).client(target, &ClientSettings::default());

    let err = client.request(HttpRequest::get("/")).await.expect_err("not resolvable yet");
    assert_eq!(err.cause, FailureCause::Dns, "{err:?}");

    resolver.point(&name, [dst]);
    let resp = client.request(HttpRequest::get("/health")).await.expect("resolves now");
    assert_eq!(resp.status, 204);
    let head = rx.await.unwrap().to_ascii_lowercase();
    assert!(head.contains(&format!("host: {}", name.to_ascii_lowercase())), "{head}");
}

/// A resolver that never answers.
struct Hanging;

#[async_trait::async_trait]
impl http_net::NameResolver for Hanging {
    async fn resolve(&self, _: &str) -> Vec<SocketAddr> {
        std::future::pending().await
    }
}

/// Hanging DNS on the real client, a lookup bound (300 ms) above the connect
/// timeout (100 ms): the request fails at the lookup bound as a name that
/// does not resolve, never as a connect timeout, and is not retried.
#[tokio::test]
async fn a_hanging_lookup_is_a_name_that_does_not_resolve() {
    let name = "hang.test:8080";
    let lookup = http_net::LookupSettings { bound: Duration::from_millis(300), max_stuck: 4 };
    let target = Arc::new(ResolvedTarget::name_with_lookup(name, Arc::new(Hanging), lookup));
    let settings =
        ClientSettings { connect_timeout: Duration::from_millis(100), ..ClientSettings::default() };
    let breaker = http_net::BreakerConfig::probing("/");
    let client = http_net::BackendClient::new(Arc::new(net()), target, settings, breaker);
    let budgets = http_net::Budgets {
        attempt: Duration::from_millis(400),
        total: Duration::from_millis(800),
    };
    let start = std::time::Instant::now();
    let outcome = client.send_with_retry(HttpRequest::get("/"), budgets).await;
    let cause = match &outcome {
        Err(http_net::AttemptError::Transport(e)) => Some(e.cause),
        _ => None,
    };
    assert_eq!(cause, Some(FailureCause::Dns), "{outcome:?}");
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(300) && took < Duration::from_millis(400), "{took:?}");
    assert_eq!(http_net::backend::metrics::retries(name), 0);
}
