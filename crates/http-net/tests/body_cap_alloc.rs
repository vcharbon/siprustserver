// Own binary (ADR-0030 X2): installs the global allocation counter.
//! The real client's answer body cap, measured at the allocator: an answer
//! declaring a body past the cap and one streaming past it are refused, and
//! no block the request asked the allocator for holds more than the cap.
//! Loopback and the real clock, well under 1 s.

#![cfg(feature = "real")]

use std::net::SocketAddr;
use std::sync::Arc;

use alloc_counter::{measure, CountingAlloc};
use http_net::backend::metrics::{self, BodyRefusal};
use http_net::failures::FailureCause;
use http_net::{ClientSettings, HttpRequest, HttpTransport, RealHttpNetwork};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// The client's cap in this test.
const CAP: usize = 1024 * 1024;
/// The streamed body's chunk: a buffer doubling from it passes [`CAP`]
/// (48 KiB × 2⁵ = 1.5 MiB) before the bytes read do.
const CHUNK: usize = 48 * 1024;

/// A peer answering every request `head` then `body` written `times` over,
/// from bytes made before any request, and holding the connection open.
async fn peer(head: &'static [u8], body: Arc<[u8]>, times: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut seen = Vec::new();
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                if stream.write_all(head).await.is_err() {
                    return;
                }
                for _ in 0..times {
                    if stream.write_all(&body).await.is_err() {
                        return;
                    }
                }
                std::future::pending::<()>().await;
            });
        }
    });
    addr
}

/// A chunk of `len` bytes in chunked transfer coding.
fn chunk(len: usize) -> Arc<[u8]> {
    let mut c = format!("{len:x}\r\n").into_bytes();
    c.extend(std::iter::repeat_n(b'x', len));
    c.extend_from_slice(b"\r\n");
    c.into()
}

#[test]
fn a_body_declared_or_streamed_past_the_cap_is_refused_without_a_block_past_it() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let declared = rt.block_on(peer(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1073741824\r\n\r\n",
        Arc::from(&b""[..]),
        0,
    ));
    let streamed = rt.block_on(peer(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        chunk(CHUNK),
        CAP / CHUNK * 2,
    ));
    let net = RealHttpNetwork::with_settings(&ClientSettings {
        max_body_bytes: CAP,
        ..ClientSettings::default()
    });

    for (dst, refusal) in [(declared, BodyRefusal::Declared), (streamed, BodyRefusal::Read)] {
        let (outcome, cost) = measure(|| rt.block_on(net.request(dst, HttpRequest::get("/"))));
        let err = outcome.expect_err("past the cap");
        assert_eq!(err.cause, FailureCause::BodyTooLarge, "{refusal:?}: {err:?}");
        assert_eq!(metrics::bodies_refused(&dst.to_string(), refusal), 1, "{refusal:?}");
        assert!(cost.largest <= CAP, "{refusal:?}: a block of {} bytes", cost.largest);
    }
}
