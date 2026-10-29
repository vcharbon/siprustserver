//! The metrics socket: a minimal HTTP/1.1 server for `/metrics`, the live
//! report, the live rate, the drain and the on-demand flamegraph.

use std::net::SocketAddr;
use std::sync::Arc;

use e2e_model::LoadRunIndex;
use loadrun_render::{Live, RenderOpts, DEFAULT_REFRESH_SECS};
use tokio::io::AsyncWriteExt;

use crate::rate::RateHandle;

mod request;
use request::{read_request, Request};

/// A stored sample page by run-dir-relative path, if any.
pub type SamplePageFn = dyn Fn(&str) -> Option<Arc<str>> + Send + Sync;

/// The live report a running load generator serves: its index as of now and
/// its stored sample pages.
#[derive(Clone)]
pub struct LiveReport {
    /// The run's index as of the call (`finished: false`).
    pub index: Arc<dyn Fn() -> LoadRunIndex + Send + Sync>,
    /// The stored sample page at a run-dir-relative path, if any.
    pub page: Arc<SamplePageFn>,
}

/// What the metrics socket serves.
#[derive(Clone)]
pub struct MetricsSurface {
    /// The Prometheus text of `/metrics`.
    pub metrics: Arc<dyn Fn() -> String + Send + Sync>,
    /// The live rate `/rate` and `/drain` act on; `None` answers them 404.
    pub rate: Option<RateHandle>,
    /// The live report of `/`; `None` serves `/metrics` there.
    pub report: Option<LiveReport>,
}

/// A minimal HTTP/1.1 server (hand-rolled over `TcpListener`: no hyper
/// dependency). Routes:
///
/// - **`GET /`** (when a [`LiveReport`] is attached) → the report page
///   rendered from the live index, reloading every 60 s; `?refresh=N` sets the
///   period, `?refresh=0` freezes it.
/// - **`GET /load-result.json`** → the live index; **`GET /callflows/…`** → a
///   stored sample page, from memory.
/// - **`GET /metrics`** (and any other GET) → the Prometheus surface.
/// - **`POST /rate?cps=<float>`** (when a [`RateHandle`] is attached) →
///   re-targets the offered call rate live (clamped to `>= 0`; `0` pauses
///   new-call admission), answering the applied value. `cps` may come as a
///   form field instead; a form post is answered by a redirect to `/`.
/// - **`GET /rate`** → the current target cps.
/// - **`POST /drain`** → the run places no new call, waits for its calls in
///   flight to end and exits (a form post redirects to `/`).
/// - **`GET /debug/flamegraph[?seconds=N]`** → an on-demand pprof CPU
///   flamegraph SVG of this process (default 20 s, max 120), sampled on a
///   blocking thread.
///
/// Runs until the task is cancelled.
pub async fn serve_metrics(addr: SocketAddr, surface: MetricsSurface) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_metrics_on(listener, surface).await
}

/// [`serve_metrics`] over an already-bound listener. Callers that need the
/// actual bound address — port-0 binds in tests, where a fixed port can
/// collide with an unrelated listener — bind first and hand the listener over.
pub async fn serve_metrics_on(
    listener: tokio::net::TcpListener,
    surface: MetricsSurface,
) -> std::io::Result<()> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let surface = surface.clone();
        tokio::spawn(async move {
            let req = read_request(&mut sock).await;
            let reply = route(&surface, &req).await;
            let mut head = format!(
                "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                reply.status,
                reply.ctype,
                reply.body.len()
            );
            if let Some(location) = reply.location {
                head.push_str(&format!("Location: {location}\r\n"));
            }
            head.push_str("\r\n");
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&reply.body).await;
        });
    }
}

struct Reply {
    status: &'static str,
    ctype: &'static str,
    body: Vec<u8>,
    location: Option<&'static str>,
}

const TEXT: &str = "text/plain; version=0.0.4";
const HTML: &str = "text/html; charset=utf-8";

fn text(status: &'static str, body: String) -> Reply {
    Reply { status, ctype: TEXT, body: body.into_bytes(), location: None }
}

/// A rate or drain request's answer: the text for a client, a redirect back
/// to the page for a browser form.
fn acted(req: &Request, body: String) -> Reply {
    if req.is_form() {
        Reply { location: Some("/"), ..text("303 See Other", body) }
    } else {
        text("200 OK", body)
    }
}

async fn route(surface: &MetricsSurface, req: &Request) -> Reply {
    let (method, path) = (req.method.as_str(), req.path.as_str());
    if path == "/debug/flamegraph" {
        let secs = flamegraph_util::parse_seconds(&format!("?{}", req.query), 20, 120);
        let svg = tokio::task::spawn_blocking(move || flamegraph_util::capture_svg(secs, 99))
            .await
            .unwrap_or_else(|e| Err(format!("join error: {e}")));
        return match svg {
            Ok(svg) => {
                Reply { status: "200 OK", ctype: "image/svg+xml", body: svg, location: None }
            }
            Err(e) => text("500 Internal Server Error", format!("flamegraph failed: {e}\n")),
        };
    }
    let no_rate = || text("404 Not Found", "no rate handle attached\n".to_string());
    match (method, path, &surface.rate, &surface.report) {
        ("POST", "/drain", None, _) | (_, "/rate", None, _) => no_rate(),
        ("POST", "/drain", Some(rate), _) => {
            rate.drain();
            acted(req, "ok: draining\n".to_string())
        }
        ("POST", "/rate", Some(rate), _) => match req.param("cps").map(|s| s.parse::<f64>()) {
            Some(Ok(cps)) => {
                let applied = rate.set(cps);
                acted(req, format!("ok: target cps={applied}\n"))
            }
            // A missing or malformed `cps` is a client error (never a silent
            // no-op that leaves the rate wherever it was).
            _ => text("400 Bad Request", "expected POST /rate?cps=<float>\n".to_string()),
        },
        (_, "/rate", Some(rate), _) => text("200 OK", format!("{}\n", rate.cps())),
        ("GET", "/", _, Some(report)) => {
            let refresh = req
                .param("refresh")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(DEFAULT_REFRESH_SECS);
            let opts = RenderOpts {
                sample_prefix: String::new(),
                live: Some(Live { refresh_secs: refresh }),
            };
            let page = loadrun_render::render_page(&(report.index)(), &opts);
            Reply { status: "200 OK", ctype: HTML, body: page.into_bytes(), location: None }
        }
        ("GET", "/load-result.json", _, Some(report)) => {
            let json = serde_json::to_string_pretty(&(report.index)()).unwrap_or_default();
            Reply {
                status: "200 OK",
                ctype: "application/json",
                body: json.into_bytes(),
                location: None,
            }
        }
        ("GET", p, _, Some(report)) if p.starts_with("/callflows/") => {
            match (report.page)(&p[1..]) {
                Some(page) => Reply {
                    status: "200 OK",
                    ctype: HTML,
                    body: page.as_bytes().to_vec(),
                    location: None,
                },
                None => text("404 Not Found", "no such sample page\n".to_string()),
            }
        }
        _ => text("200 OK", (surface.metrics)()),
    }
}
