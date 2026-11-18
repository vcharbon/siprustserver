//! The debug routes, each costing CPU or memory for as long as it runs:
//!
//! - `GET /debug/flamegraph[?seconds=N]`: an on-demand pprof CPU flamegraph
//!   SVG, sampled at 99 Hz for `N` seconds (default 20, at most 120) on a
//!   blocking thread, so the runner's async workers keep serving;
//! - `GET /debug/heap`: the runner's heap profile ([`HeapDumpFn`]), `503`
//!   when the runner gives none.
//!
//! An [`AdminServer`](crate::AdminServer), bound to a loopback address,
//! serves them; a [`ProbeServer`](crate::ProbeServer) never does.

use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// On-demand heap-profile dump for `GET /debug/heap` (jemalloc `prof.dump` →
/// jeprof/pprof bytes). Injected by the runner so the lib stays allocator-
/// agnostic. May block on file I/O, so the server runs it on a blocking
/// thread.
pub type HeapDumpFn = Arc<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>;

/// Answer the debug route `route` (`path` is the same with its query) on
/// `stream`; `false` when `route` is no debug route, nothing written.
pub(crate) async fn answer(
    stream: &mut TcpStream,
    route: &str,
    path: &str,
    heap: Option<&HeapDumpFn>,
) -> std::io::Result<bool> {
    let reply = match route {
        "/debug/flamegraph" => {
            let secs = flamegraph_util::parse_seconds(path, 20, 120);
            let captured = blocking(move || flamegraph_util::capture_svg(secs, 99)).await;
            match captured {
                Ok(svg) => Reply::ok("image/svg+xml", svg),
                Err(e) => {
                    Reply::failed("500 Internal Server Error", format!("flamegraph failed: {e}"))
                }
            }
        }
        "/debug/heap" => {
            let dumped = match heap.cloned() {
                Some(heap) => blocking(move || heap()).await,
                None => Err("heap profiling unavailable (non-jemalloc build or prof:false)".into()),
            };
            match dumped {
                Ok(bytes) => Reply::ok("application/octet-stream", bytes),
                Err(e) => {
                    Reply::failed("503 Service Unavailable", format!("heap dump failed: {e}"))
                }
            }
        }
        _ => return Ok(false),
    };
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&reply.body).await?;
    stream.flush().await?;
    Ok(true)
}

/// One debug route's answer.
struct Reply {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Reply {
    fn ok(content_type: &'static str, body: Vec<u8>) -> Self {
        Self { status: "200 OK", content_type, body }
    }

    fn failed(status: &'static str, reason: String) -> Self {
        Self { status, content_type: "text/plain", body: format!("{reason}\n").into_bytes() }
    }
}

/// `work` on a blocking thread; a panic in it reads as its error.
async fn blocking(
    work: impl FnOnce() -> Result<Vec<u8>, String> + Send + 'static,
) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(work).await.unwrap_or_else(|e| Err(format!("join error: {e}")))
}
