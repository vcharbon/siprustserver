//! A backend client on the real transport and the real clock, on loopback,
//! well under 1 s each: what the paused clock and the simulated fabric cannot
//! show — timer rounding against the retry's room, and the pool's connections
//! against a moved name.

#![cfg(feature = "real")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use http_net::{
    BackendClient, BreakerConfig, Budgets, ClientSettings, HttpRequest, MemoryResolver,
    RealHttpNetwork, ResolvedTarget,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_head(stream: &mut TcpStream) -> Option<()> {
    let mut buf = vec![0u8; 4096];
    let mut got = Vec::new();
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            return None;
        }
        got.extend_from_slice(&buf[..n]);
    }
    Some(())
}

/// A keep-alive server answering `tag` as the body of every request.
fn serve_tag(listener: TcpListener, tag: &'static [u8]) {
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                while read_head(&mut s).await.is_some() {
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", tag.len());
                    if s.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    if s.write_all(tag).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

fn real() -> Arc<RealHttpNetwork> {
    Arc::new(RealHttpNetwork::with_settings(&ClientSettings::default()))
}

/// The name moves A → B right after a first request; with a request every
/// 20 ms (the pooled connection to A stays busy), B is reached within one
/// rotation period.
#[tokio::test]
async fn a_moved_name_is_reached_within_one_rotation_period() {
    let a = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = a.local_addr().unwrap().port();
    let b = TcpListener::bind(("127.0.0.2", port)).await.unwrap();
    let a_addr: SocketAddr = a.local_addr().unwrap();
    let b_addr: SocketAddr = b.local_addr().unwrap();
    serve_tag(a, b"A");
    serve_tag(b, b"B");
    let name = format!("moved.test:{port}");
    let resolver = Arc::new(MemoryResolver::new());
    resolver.point(&name, [a_addr]);
    let target = Arc::new(ResolvedTarget::name_with(name.clone(), resolver.clone()));
    let period = Duration::from_millis(300);
    let settings = ClientSettings { rotation: period, ..ClientSettings::default() };
    let built = std::time::Instant::now();
    let client = BackendClient::new(real(), target, settings, BreakerConfig::probing("/"));
    let first = client.attempt(HttpRequest::get("/"), Duration::from_millis(500)).await.unwrap();
    assert_eq!(first.body, b"A");
    resolver.point(&name, [b_addr]);
    let mut seen = Vec::new();
    while built.elapsed() < period * 2 - Duration::from_millis(60) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp = client.attempt(HttpRequest::get("/"), Duration::from_millis(500)).await.unwrap();
        seen.push((built.elapsed(), resp.body[0]));
    }
    assert!(client.generation() >= 2, "rotated");
    // The first request sent past the period starts the fresh lookup on the
    // current client; the one after it (two, should the lookup land late)
    // rides the new client to B.
    let due = seen.iter().position(|(at, _)| *at >= period).expect("a request past the period");
    let reached = seen.iter().position(|(_, b)| *b == b'B');
    let trace: String = seen.iter().map(|(_, b)| *b as char).collect();
    assert!(
        reached.is_some_and(|n| n <= due + 2),
        "rotation due at request {due}, B first reached at request {reached:?}: {trace}"
    );
}

/// The default budget ratio (attempt = total / 2) on the real clock: a first
/// attempt that times out (a silent peer) is retried and answered.
#[tokio::test]
async fn a_timed_out_first_attempt_is_retried_at_the_default_ratio() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let counted = conns.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let n = counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if read_head(&mut s).await.is_none() {
                    return;
                }
                if n == 0 {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    return;
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
            });
        }
    });
    let target = Arc::new(ResolvedTarget::addr(addr));
    let client =
        BackendClient::new(real(), target, ClientSettings::default(), BreakerConfig::probing("/"));
    let budgets =
        Budgets { attempt: Duration::from_millis(150), total: Duration::from_millis(300) };
    let outcome = client.send_with_retry(HttpRequest::get("/"), budgets).await;
    assert_eq!(outcome.map(|r| r.status), Ok(200), "connections: {}", conns.load(Ordering::SeqCst));
}
