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
//!
//! ## HA replication (S11) — opt-in via `B2BUA_REPL=1` (default off = legacy)
//!   B2BUA_REPL          "1"/"true" enables peer-to-peer call replication
//!   B2BUA_REPL_LISTEN   replication TCP listen addr     (default 0.0.0.0:9092)
//!   B2BUA_REPL_PORT     port peers are reached on       (default = REPL_LISTEN port)
//!   B2BUA_PEERS         static membership `ord@host,..`  (dev/local; takes precedence)
//!   B2BUA_REPL_SERVICE  headless Service to discover     (default b2bua-worker)
//!   B2BUA_NAMESPACE     namespace for k8s discovery      (default $POD_NAMESPACE / sip-test)
//!
//! Two deferred S11 decisions are resolved here:
//!   - **Incarnation gen** = boot wall-clock seconds (monotonic across pod
//!     restarts → `(new_gen,0) > (old_gen,*)` holds; see [`boot_incarnation`]).
//!   - **Replication addressing** = port-agnostic `Peer.host` + a cluster-wide
//!     `B2BUA_REPL_PORT` (see [`make_addr_resolver`]) — no per-peer port grammar.
//! And SIGTERM latches the worker into `Draining` (OPTIONS 503 + readiness
//! probe fails) so k8s steers new calls away while in-flight calls finish.

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use b2bua::cdr::{BufferedCdrWriter, CdrRecord, CdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::limiter::NoopLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::repl::ReplicatingCallStore;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps, ReplicationSetup};
use call::Call;
use repl_net::RealReplicationNetwork;
use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork};
use sip_txn::IdGen;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use topology::{Membership, Peer, StaticMembership};

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

/// Truthy env flag: `1`/`true`/`yes`/`on` (case-insensitive) → true.
fn env_flag(key: &str) -> bool {
    matches!(
        env_or(key, "0").trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn resolve(addr: &str) -> SocketAddr {
    addr.to_socket_addrs()
        .unwrap_or_else(|e| panic!("cannot resolve {addr:?}: {e}"))
        .next()
        .unwrap_or_else(|| panic!("no address resolved from {addr:?}"))
}

/// **Incarnation gen** (deferred S11 decision: boot wall-clock vs pod epoch).
/// We pick **boot wall-clock seconds**: it is monotonic across pod restarts (the
/// clock only moves forward), so a rebooted worker always serves under a higher
/// `gen` than its previous life — `(new_gen, 0) > (old_gen, *)` — and pullers
/// apply its frames without a manual reset (ADR-0011 X9). Falls back to 0 only
/// if the wall clock is before the epoch (never, in practice).
fn boot_incarnation() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// **Replication addressing** (deferred S11 decision: port offset vs peer env).
/// We keep membership port-agnostic (a Pod has one IP, many ports) and reach
/// every peer's replication server at `peer.host:repl_port`, where `repl_port`
/// is one cluster-wide config value (`B2BUA_REPL_PORT`). The resolver parses a
/// bare IP fast (the k8s case — EndpointSlice addresses are Pod IPs) and falls
/// back to DNS for a named static host. An unresolvable host yields an
/// unspecified addr the puller simply fails to connect to and retries.
fn make_addr_resolver(repl_port: u16) -> Arc<dyn Fn(&Peer) -> SocketAddr + Send + Sync> {
    Arc::new(move |peer: &Peer| {
        let addr = if let Ok(ip) = peer.host.parse::<IpAddr>() {
            SocketAddr::new(ip, repl_port)
        } else {
            (peer.host.as_str(), repl_port)
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.next())
                .unwrap_or_else(|| SocketAddr::from((Ipv4Addr::UNSPECIFIED, repl_port)))
        };
        // Fires once per puller (re)spawn — its presence proves a peer was
        // discovered and a puller is being started toward it.
        eprintln!("b2bua-runner repl: spawning puller -> peer={} addr={addr}", peer.ordinal);
        addr
    })
}

/// Resolve cluster membership for replication. `B2BUA_PEERS` (a static
/// `ord@host,..` list, used for dev/local) takes precedence; otherwise the k8s
/// EndpointSlice informer watches the headless `B2BUA_REPL_SERVICE`. Returns
/// `None` (→ replication stays off) if neither a static list nor an in-cluster
/// kube client is available — liveness over completeness, the worker still
/// serves SIP.
async fn build_membership() -> Option<Arc<dyn Membership>> {
    let peers = env_or("B2BUA_PEERS", "");
    if !peers.trim().is_empty() {
        match StaticMembership::from_string(&peers, "B2BUA_PEERS") {
            Ok(m) => {
                eprintln!("b2bua-runner replication membership: static B2BUA_PEERS={peers}");
                return Some(Arc::new(m));
            }
            Err(e) => {
                eprintln!("b2bua-runner B2BUA_PEERS parse error: {e} — replication disabled");
                return None;
            }
        }
    }
    let service = env_or("B2BUA_REPL_SERVICE", "b2bua-worker");
    let namespace =
        env::var("B2BUA_NAMESPACE").or_else(|_| env::var("POD_NAMESPACE")).unwrap_or_else(|_| "sip-test".to_string());
    // rustls 0.23 has no default CryptoProvider compiled in; install ring once
    // before the kube client opens its first TLS connection (idempotent — a
    // second call returns Err, which we ignore).
    let _ = rustls::crypto::ring::default_provider().install_default();
    match kube::Client::try_default().await {
        Ok(client) => {
            eprintln!(
                "b2bua-runner replication membership: k8s EndpointSlice informer (svc={service}, ns={namespace})"
            );
            Some(Arc::new(topology::K8sMembership::spawn(client, namespace, service)))
        }
        Err(e) => {
            eprintln!("b2bua-runner no kube client ({e}) and no B2BUA_PEERS — replication disabled");
            None
        }
    }
}

/// Minimal Prometheus exposition + probe server. Hand-rolled (no HTTP framework
/// dep), mirroring `sip-proxy`'s MetricsServer.
///   - `GET /metrics`  → Prometheus text.
///   - `GET /healthz`  → liveness: `200 ok` while the process runs.
///   - `GET /ready`    → readiness: `200 ready` iff the worker is replication-
///                       ready (re-hydrated + backup-current) **and** not
///                       draining; `503` otherwise. The k8s readinessProbe hits
///                       this so the proxy's EndpointSlice view respects
///                       `NotReady`/`Draining` (ADR-0011 X6), matching the
///                       worker's own OPTIONS self-report.
async fn serve_metrics(
    addr: SocketAddr,
    metrics: B2buaMetrics,
    ready: Arc<dyn Fn() -> bool + Send + Sync>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    eprintln!("b2bua-runner metrics on http://{}/metrics", listener.local_addr()?);
    loop {
        let (mut stream, _) = listener.accept().await?;
        let metrics = metrics.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let (status, body) = if req.starts_with("GET /metrics") {
                ("200 OK", metrics.prometheus_text())
            } else if req.starts_with("GET /healthz") {
                ("200 OK", "ok\n".to_string())
            } else if req.starts_with("GET /ready") {
                if ready() {
                    ("200 OK", "ready\n".to_string())
                } else {
                    ("503 Service Unavailable", "not-ready\n".to_string())
                }
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

    let clock = Clock::system();

    // --- Replication wiring (opt-in, S11). `None` keeps the legacy path. ---
    let replication = if env_flag("B2BUA_REPL") {
        match build_membership().await {
            Some(membership) => {
                let repl_listen = resolve(&env_or("B2BUA_REPL_LISTEN", "0.0.0.0:9092"));
                // Cluster-wide repl port peers are reached on; defaults to our
                // own listen port (homogeneous pool).
                let repl_port: u16 =
                    env_or("B2BUA_REPL_PORT", &repl_listen.port().to_string()).parse().expect("B2BUA_REPL_PORT");
                let incarnation_gen = boot_incarnation();
                let store = Arc::new(ReplicatingCallStore::new(incarnation_gen, clock.clone()));
                eprintln!(
                    "b2bua-runner replication ENABLED: listen={repl_listen} peer_port={repl_port} incarnation_gen={incarnation_gen}"
                );
                // Diagnostic: log the discovered peer set a few times so we can
                // see whether the K8sMembership informer actually populates peers
                // (it starts empty and fills async). Empty after several seconds
                // ⇒ informer/watch problem; populated ⇒ the issue is downstream.
                {
                    let m = membership.clone();
                    tokio::spawn(async move {
                        for _ in 0..6 {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            let peers: Vec<String> = m
                                .snapshot()
                                .into_iter()
                                .map(|p| format!("{}@{}", p.ordinal, p.host))
                                .collect();
                            eprintln!("b2bua-runner repl membership snapshot: [{}]", peers.join(", "));
                        }
                    });
                }
                Some(ReplicationSetup {
                    network: Arc::new(RealReplicationNetwork::new()),
                    membership,
                    store,
                    listen_addr: repl_listen,
                    addr_resolver: make_addr_resolver(repl_port),
                    incarnation_gen,
                })
            }
            None => None,
        }
    } else {
        None
    };

    let store: Arc<dyn b2bua::store::CallStore> = Arc::new(InMemoryCallStore::new());
    let deps = B2buaDeps {
        config,
        decision: Arc::new(ScriptedDecisionEngine::route_all_to(
            dest_sa.ip().to_string(),
            dest_sa.port(),
        )),
        limiter: Arc::new(NoopLimiter),
        cdr,
        store,
        clock,
        id_gen: Arc::new(IdGen::from_entropy()),
        replication,
    };

    let core = Arc::new(B2buaCore::spawn(endpoint, deps));
    let metrics = core.metrics().clone();

    eprintln!(
        "b2bua-runner pid={} listening UDP {local} -> routing all calls to {dest_sa} (ordinal={ordinal}, queue={queue_max}, cdr_queue={cdr_queue})",
        std::process::id()
    );

    // Readiness probe state: a node reports NotReady while re-hydrating and once
    // SIGTERM latches `draining`. The metrics server reads both.
    let draining = Arc::new(AtomicBool::new(false));
    {
        let core = core.clone();
        let draining = draining.clone();
        let ready: Arc<dyn Fn() -> bool + Send + Sync> =
            Arc::new(move || core.is_ready() && !draining.load(Ordering::Relaxed));
        tokio::spawn(async move {
            if let Err(e) = serve_metrics(metrics_sa, metrics, ready).await {
                eprintln!("b2bua-runner metrics server error: {e}");
            }
        });
    }

    // Graceful shutdown: SIGTERM (k8s pod termination) latches Draining — OPTIONS
    // self-reports 503 and the readiness probe flips NotReady so the proxy steers
    // new calls away — then we wait the drain grace before exiting so in-flight
    // calls finish. Ctrl-C (interactive) exits immediately.
    let drain_grace_ms: u64 = env_or("B2BUA_DRAIN_GRACE_MS", "5000").parse().unwrap_or(5000);
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("b2bua-runner SIGINT — shutting down");
        }
        _ = wait_sigterm() => {
            eprintln!("b2bua-runner SIGTERM — begin draining ({drain_grace_ms}ms grace)");
            core.begin_draining();
            draining.store(true, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(drain_grace_ms)).await;
            eprintln!("b2bua-runner drain grace elapsed — exiting");
        }
    }
    drop(core);
}

/// Await a SIGTERM (k8s sends this on pod termination). On non-unix this future
/// never resolves (only Ctrl-C drives shutdown there).
#[cfg(unix)]
async fn wait_sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(e) => {
            eprintln!("b2bua-runner cannot install SIGTERM handler: {e}");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_sigterm() {
    std::future::pending::<()>().await;
}
