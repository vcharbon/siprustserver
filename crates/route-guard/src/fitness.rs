//! One reading of the proxy's `GET /fitz` (probe-http): `200 fit` is fit,
//! `503 unfit <reason>` unfit for that reason, anything else within the
//! bound, or nothing, no answer.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::switch::Reading;

/// The most of an answer read: a fitness answer is a few dozen bytes.
const MAX_ANSWER: usize = 512;

/// The longest reason kept from an unfit answer.
const MAX_REASON: usize = 32;

/// Where the guard reads the proxy's fitness.
pub trait Source {
    fn read(&self) -> impl std::future::Future<Output = Reading> + Send;
}

/// The proxy's probe server at `addr`, each reading bounded by `bound`.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    pub addr: SocketAddr,
    pub bound: Duration,
}

impl Source for Probe {
    async fn read(&self) -> Reading {
        read(self.addr, self.bound).await
    }
}

/// The proxy's fitness at `addr`, read within `bound`.
pub async fn read(addr: SocketAddr, bound: Duration) -> Reading {
    match tokio::time::timeout(bound, fetch(addr)).await {
        Ok(Ok(answer)) => parse(&answer),
        Ok(Err(e)) => Reading::NoAnswer(e.to_string()),
        Err(_) => Reading::NoAnswer(format!("no answer within {} ms", bound.as_millis())),
    }
}

async fn fetch(addr: SocketAddr) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(b"GET /fitz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await?;
    let mut answer = Vec::with_capacity(128);
    let mut chunk = [0u8; 128];
    while answer.len() < MAX_ANSWER {
        let n = stream.read(&mut chunk).await?;
        let Some(read) = chunk.get(..n).filter(|r| !r.is_empty()) else { break };
        answer.extend_from_slice(read);
    }
    Ok(answer)
}

/// The reading an HTTP answer states.
fn parse(answer: &[u8]) -> Reading {
    let text = String::from_utf8_lossy(answer);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return Reading::NoAnswer("an incomplete answer".to_string());
    };
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
    match (status, body.trim()) {
        ("200", "fit") => Reading::Fit,
        ("503", body) => match body.strip_prefix("unfit ") {
            Some(reason) => Reading::Unfit(token(reason)),
            None => Reading::NoAnswer(format!("a 503 with no fitness: {}", token(body))),
        },
        (status, body) => Reading::NoAnswer(format!("{} {}", token(status), token(body))),
    }
}

/// `raw` cut to a short run of printable, space-free characters.
fn token(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_graphic()).take(MAX_REASON).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use probe_http::{Fitness, ProbeRoutes, ProbeServer, ProbeState};

    use super::*;

    const BOUND: Duration = Duration::from_millis(500);

    #[test]
    fn answers_read_as_their_fitness() {
        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nfit\n";
        assert_eq!(parse(ok), Reading::Fit);
        let unfit = b"HTTP/1.1 503 Service Unavailable\r\n\r\nunfit isolated\n";
        assert_eq!(parse(unfit), Reading::Unfit("isolated".to_string()));
        assert!(matches!(
            parse(b"HTTP/1.1 404 Not Found\r\n\r\nnot found\n"),
            Reading::NoAnswer(_)
        ));
        assert!(matches!(parse(b"HTTP/1.1 503 x\r\n\r\ndraining\n"), Reading::NoAnswer(_)));
        assert!(matches!(parse(b"HTTP/1.1 200 OK\r\n"), Reading::NoAnswer(_)));
        assert!(matches!(parse(&[0xff; 600]), Reading::NoAnswer(_)));
    }

    #[test]
    fn a_reason_is_kept_short_and_printable() {
        let long = format!("HTTP/1.1 503 x\r\n\r\nunfit {}\u{7}\n", "r".repeat(100));
        assert_eq!(parse(long.as_bytes()), Reading::Unfit("r".repeat(MAX_REASON)));
    }

    /// Over a probe server: fit, unfit, and the port closed.
    #[tokio::test]
    async fn the_proxy_is_read_over_its_probe_server() {
        let fit = Arc::new(AtomicBool::new(true));
        let reads = fit.clone();
        let routes = ProbeRoutes {
            metrics: Arc::new(String::new),
            ready: Arc::new(|| ProbeState::Ready),
            heap: None,
            fit: Some(Arc::new(move || match reads.load(Ordering::SeqCst) {
                true => Fitness::Fit,
                false => Fitness::Unfit("no-worker-alive"),
            })),
        };
        let server = ProbeServer::start("127.0.0.1:0".parse().unwrap(), routes).await.unwrap();
        let addr = server.addr();
        assert_eq!(read(addr, BOUND).await, Reading::Fit);
        fit.store(false, Ordering::SeqCst);
        assert_eq!(read(addr, BOUND).await, Reading::Unfit("no-worker-alive".to_string()));
        drop(server);
        tokio::task::yield_now().await;
        assert!(matches!(read(addr, BOUND).await, Reading::NoAnswer(_)));
    }

    /// A listener that accepts and never answers is no answer once the bound
    /// runs out.
    #[tokio::test]
    async fn a_silent_listener_is_no_answer_at_the_bound() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _held = tokio::spawn(async move {
            let conn = listener.accept().await;
            std::future::pending::<()>().await;
            drop(conn);
        });
        let reading = read(addr, Duration::from_millis(50)).await;
        assert_eq!(reading, Reading::NoAnswer("no answer within 50 ms".to_string()));
    }
}
