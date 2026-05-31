//! Standalone, containerizable B2BUA worker process.
//!
//! Wires the `b2bua` library over the **real, non-recording** UDP transport
//! (`sip_net::RealSignalingNetwork` — no `Recorder` decorator, no simulated
//! fabric) and a **system wall clock** (`Clock::system`, so transaction/dialog
//! timers fire). Production-shaped deps:
//!   - CDR  : `BufferedCdrWriter` (drop-on-overload) over a discarding sink, so
//!            an endurance run does not accumulate records in memory.
//!   - store: `InMemoryCallStore` (the only ported store; HA/Redis deferred).
//!   - limit: `NoopLimiter` (the real sliding-window limiter is a later slice).
//!   - route: `ScriptedDecisionEngine::route_all_to(DEST)` (the HTTP call-control
//!            adapter is a deferred slice; routing all calls to a fixed UAS
//!            mirrors the k8s `worker -> sipp-uas` topology).
//! It also serves a Prometheus `/metrics` + `/healthz` endpoint so the endurance
//! recorder can scrape worker application metrics alongside container CPU/mem.
//!
//! Config via env (all optional):
//!   B2BUA_LISTEN    SIP/signaling listen addr        (default 0.0.0.0:5060)
//!   B2BUA_DEST      downstream UAS host:port          (default 127.0.0.1:5070)
//!   B2BUA_METRICS   Prometheus HTTP listen addr       (default 0.0.0.0:9091)
//!   B2BUA_QUEUE     inbound UDP queue depth (packets)  (default 8192)
//!   B2BUA_ORDINAL   worker ordinal stamped in callRef  (default w0)
//!   B2BUA_CDR_QUEUE buffered-CDR submit queue depth    (default 1024)

use std::env;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::cdr::{BufferedCdrWriter, CdrRecord, CdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::limiter::NoopLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps};
use call::Call;
use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use sip_txn::IdGen;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A CDR sink that discards every record. Production B2BUA writes CDRs to an
/// external store; that adapter is a deferred slice, and for load/endurance we
/// must not accumulate records in process memory. Wrapped by `BufferedCdrWriter`
/// so the buffer/drainer machinery is still exercised.
struct NullCdrWriter;

#[async_trait]
impl CdrWriter for NullCdrWriter {
    async fn write(&self, _call: &Call, _terminated_at: i64) {}
    async fn read_all(&self) -> Vec<CdrRecord> {
        Vec::new()
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn resolve(addr: &str) -> SocketAddr {
    addr.to_socket_addrs()
        .unwrap_or_else(|e| panic!("cannot resolve {addr:?}: {e}"))
        .next()
        .unwrap_or_else(|| panic!("no address resolved from {addr:?}"))
}

/// Minimal Prometheus exposition server: GET /metrics -> render, /healthz -> ok.
/// Hand-rolled (no HTTP framework dep), mirroring `sip-proxy`'s MetricsServer.
async fn serve_metrics(addr: SocketAddr, metrics: B2buaMetrics) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    eprintln!("b2bua-runner metrics on http://{}/metrics", listener.local_addr()?);
    loop {
        let (mut stream, _) = listener.accept().await?;
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let (status, body) = if req.starts_with("GET /metrics") {
                ("200 OK", metrics.prometheus_text())
            } else if req.starts_with("GET /healthz") {
                ("200 OK", "ok\n".to_string())
            } else {
                ("404 Not Found", String::new())
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    }
}

#[tokio::main]
async fn main() {
    let listen = env_or("B2BUA_LISTEN", "0.0.0.0:5060");
    let dest = env_or("B2BUA_DEST", "127.0.0.1:5070");
    let metrics_addr = env_or("B2BUA_METRICS", "0.0.0.0:9091");
    let queue_max: usize = env_or("B2BUA_QUEUE", "8192").parse().expect("B2BUA_QUEUE");
    let cdr_queue: usize = env_or("B2BUA_CDR_QUEUE", "1024").parse().expect("B2BUA_CDR_QUEUE");
    let ordinal = env_or("B2BUA_ORDINAL", "w0");

    let listen_sa = resolve(&listen);
    let dest_sa = resolve(&dest);
    let metrics_sa = resolve(&metrics_addr);

    // Real, non-recording transport: a plain tokio UDP socket.
    let net = RealSignalingNetwork::new();
    let endpoint = net
        .bind_udp(BindUdpOpts::new(listen_sa, queue_max))
        .await
        .unwrap_or_else(|e| panic!("bind {listen_sa} failed: {e:?}"));
    let local = endpoint.local_addr();

    let config = B2buaConfig {
        self_ordinal: ordinal.clone(),
        sip_local_ip: local.ip().to_string(),
        sip_local_port: local.port(),
        cdr_buffer_queue_max: cdr_queue,
        ..Default::default()
    };

    let cdr = Arc::new(BufferedCdrWriter::spawn(Arc::new(NullCdrWriter), cdr_queue));

    let deps = B2buaDeps {
        config,
        decision: Arc::new(ScriptedDecisionEngine::route_all_to(
            dest_sa.ip().to_string(),
            dest_sa.port(),
        )),
        limiter: Arc::new(NoopLimiter),
        cdr,
        store: Arc::new(InMemoryCallStore::new()),
        clock: Clock::system(),
        id_gen: Arc::new(IdGen::from_entropy()),
    };

    let core = B2buaCore::spawn(endpoint, deps);
    let metrics = core.metrics().clone();

    eprintln!(
        "b2bua-runner pid={} listening UDP {local} -> routing all calls to {dest_sa} (ordinal={ordinal}, queue={queue_max}, cdr_queue={cdr_queue})",
        std::process::id()
    );

    tokio::spawn(async move {
        if let Err(e) = serve_metrics(metrics_sa, metrics).await {
            eprintln!("b2bua-runner metrics server error: {e}");
        }
    });

    // Run until Ctrl-C / SIGTERM.
    tokio::signal::ctrl_c()
        .await
        .expect("install ctrl-c handler");
    eprintln!("b2bua-runner shutting down");
    drop(core);
}
