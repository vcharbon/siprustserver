//! The metrics socket: a minimal HTTP/1.1 server for `/metrics`, the live
//! rate, the drain and the on-demand flamegraph.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::rate::RateHandle;

/// A minimal HTTP/1.1 server (hand-rolled over `TcpListener` — no hyper
/// dependency). Routes:
///
/// - **`GET /metrics`** (and any other GET) → `render()`, the Prometheus surface.
/// - **`POST /rate?cps=<float>`** (when a [`RateHandle`] is attached) → re-targets
///   the offered call rate live (clamped to `>= 0`; `0` pauses new-call admission).
///   The governor re-anchors its grid on the next slot. Responds with the applied
///   value.
/// - **`GET /rate`** → the current target cps.
/// - **`POST /drain`** (when a [`RateHandle`] is attached) → the run places no
///   new call, waits for its calls in flight to end and exits.
/// - **`GET /debug/flamegraph[?seconds=N]`** → on-demand pprof CPU flamegraph
///   SVG of this process (default 20 s, max 120). The load driver runs the same
///   shared `sip-message` parse path the SUT tiers do, at a saturated core, so
///   it is the high-signal vantage for parse/allocation cost. Sampled on a
///   blocking thread; zero overhead when no capture is in flight.
///
/// Runs until the task is cancelled.
pub async fn serve_metrics(
    addr: SocketAddr,
    render: Arc<dyn Fn() -> String + Send + Sync>,
    rate: Option<RateHandle>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_metrics_on(listener, render, rate).await
}

/// [`serve_metrics`] over an already-bound listener. Callers that need the
/// actual bound address — port-0 binds in tests, where a fixed port can
/// collide with an unrelated listener — bind first and hand the listener over.
pub async fn serve_metrics_on(
    listener: tokio::net::TcpListener,
    render: Arc<dyn Fn() -> String + Send + Sync>,
    rate: Option<RateHandle>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut sock, _) = listener.accept().await?;
        let render = render.clone();
        let rate = rate.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let (method, path, query) = parse_request_line(&buf[..n]);

            // CPU flamegraph: binary SVG body, so it answers ahead of the
            // text/plain routes below. Sampled on a blocking thread, leaving the
            // call-driving async workers to keep offering load.
            if path == "/debug/flamegraph" {
                let secs = flamegraph_util::parse_seconds(&format!("?{query}"), 20, 120);
                let svg =
                    tokio::task::spawn_blocking(move || flamegraph_util::capture_svg(secs, 99))
                        .await
                        .unwrap_or_else(|e| Err(format!("join error: {e}")));
                let (status, ctype, body) = match svg {
                    Ok(svg) => ("200 OK", "image/svg+xml", svg),
                    Err(e) => (
                        "500 Internal Server Error",
                        "text/plain",
                        format!("flamegraph failed: {e}\n").into_bytes(),
                    ),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                return;
            }

            let (status, body) = if method == "POST" && path == "/drain" {
                match &rate {
                    None => ("404 Not Found", "no rate handle attached\n".to_string()),
                    Some(h) => {
                        h.drain();
                        ("200 OK", "ok: draining\n".to_string())
                    }
                }
            } else if path == "/rate" {
                match &rate {
                    None => ("404 Not Found", "no rate handle attached\n".to_string()),
                    Some(h) if method == "POST" => match query_get(&query, "cps")
                        .map(|s| s.parse::<f64>())
                    {
                        Some(Ok(cps)) => {
                            let applied = h.set(cps);
                            ("200 OK", format!("ok: target cps={applied}\n"))
                        }
                        // A missing or malformed `cps` is a client error (never a
                        // silent no-op that leaves the rate wherever it was).
                        _ => ("400 Bad Request", "expected POST /rate?cps=<float>\n".to_string()),
                    },
                    // GET /rate → the current target.
                    Some(h) => ("200 OK", format!("{}\n", h.cps())),
                }
            } else {
                ("200 OK", render())
            };

            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

/// Parse the first request line into `(METHOD, path, query)`. Best-effort: a
/// malformed/empty request yields `("", "", "")` and falls through to the render
/// path. The query is the raw `a=b&c=d` after `?` (empty if none).
fn parse_request_line(buf: &[u8]) -> (String, String, String) {
    let text = String::from_utf8_lossy(buf);
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    (method, path, query)
}

/// Pull `key`'s value out of a raw `a=b&c=d` query string (no percent-decoding:
/// the values are numbers).
fn query_get(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key && !v.is_empty()).then(|| v.to_string())
    })
}
