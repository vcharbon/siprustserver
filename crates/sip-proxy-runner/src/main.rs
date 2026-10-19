//! Standalone, containerizable SIP front-proxy process.
//!
//! Wires the `sip-proxy` library over the **real, non-recording** UDP transport
//! (`sip_net::RealSignalingNetwork`) into the production data path:
//!   - `LoadBalancerStrategy` — HRW worker selection + HMAC-signed Record-Route
//!     stickiness cookie (rotation-overlap aware).
//!   - The worker pool ([`sip_proxy_runner::discovery`]): the EndpointSlices of
//!     the worker Service (ADR-0012 D4), or a fixed `PROXY_WORKERS` list.
//!   - `HealthProbe` — OPTIONS to workers from the pool's first listing on,
//!     then every tick, feeding the `WorkerLoadObserver` band classifier
//!     (above-critical workers are excluded from new-dialog selection). Health
//!     writes flow through the registry's `control()` seam, so an unanswered
//!     worker is demoted `Dead` and routing reacts. Its first round latches
//!     the first worker view (ADR-0050 §11).
//!   - `EluCpsGate` self-gate: EWMA-smoothed intake pressure
//!     (packet age at dequeue, recorded by every recv shard) + a per-class CPS
//!     token bucket shed external new-dialog non-emergency INVITEs under
//!     self-overload (a stateless 503 + `Retry-After`, the same for every copy). A 100 ms
//!     `tokio::time::interval` task drives its load sampler. `PROXY_SELF_GATE=0`
//!     reverts to the always-admit gate.
//!   - Prometheus `/metrics` + `/healthz` + `/readyz` + `/vrrp` via the shared
//!     `probe-http`.
//!
//! Config via env, read once before the first bind
//! ([`sip_proxy_runner::config`]). Unset takes the default; a value set
//! empty, malformed or out of its range refuses the boot, naming every
//! problem (ADR-0046). Only the `PROXY_UDP_*BUF` pair gives empty a meaning.
//!   PROXY_LISTEN     SIP listen ip:port — the INTERNAL face, an IP literal
//!                    (default 0.0.0.0:5060; dual-face: never a wildcard)
//!   PROXY_ADVERTISE  host:port stamped on Via/Record-Route for internal-face
//!                    egress (default: the listen address; REQUIRED when the
//!                    listen IP is unspecified)
//!   PROXY_LISTEN_EXT external-face listen ip:port, an IP literal — enables
//!                    DUAL-FACE mode: a second UDP socket on the caller plane;
//!                    the egress socket and advertise follow the destination IP
//!   PROXY_ADVERTISE_EXT  external-face advertise (default: PROXY_LISTEN_EXT,
//!                    which is never a wildcard: each face binds its own IP)
//!   PROXY_FACE_INT_CIDRS comma-separated IPv4 CIDRs of the INTERNAL plane
//!                    (e.g. 10.244.0.0/16,172.20.0.0/16). REQUIRED when
//!                    PROXY_LISTEN_EXT is set; ignored with a warning otherwise.
//!                    Destination ∈ any CIDR → internal socket; else external.
//!   PROXY_WORKERS    fixed worker pool "id@ip:port,..." (default unset →
//!                    EndpointSlice discovery; ADR-0012 D4)
//!   PROXY_WORKER_SERVICE  headless worker Service to watch  (default b2bua-worker)
//!   PROXY_WORKER_PORT     SIP port of each pod IP, 1..65535 (default 5060)
//!   PROXY_NAMESPACE  namespace of the worker Service (default POD_NAMESPACE,
//!                    else the pod's own service-account namespace; required
//!                    for discovery when none is known)
//!   PROXY_METRICS    Prometheus HTTP listen ip:port, an IP literal
//!                    (default 0.0.0.0:9090)
//!   PROXY_QUEUE      inbound UDP queue depth, packets ≥ 1  (default 8192)
//!   PROXY_UDP_SNDBUF SO_SNDBUF requested on every signalling socket, bytes ≥ 1
//!                    (default 4 MiB; empty = kernel wmem_default; the kernel
//!                    clamps at wmem_max). Size it well above the neighbour
//!                    unresolved queue (unres_qlen_bytes) — ADR-0033
//!   PROXY_UDP_RCVBUF SO_RCVBUF requested on every signalling socket, bytes ≥ 1
//!                    (default 4 MiB; empty = kernel rmem_default; the kernel
//!                    clamps at rmem_max). Overflow is counted in
//!                    sip_proxy_udp_kernel_rx_dropped_total
//!   PROXY_SHARD_STALL_MS  a recv shard still on one packet past this is
//!                    stalled: /healthz, /readyz and /vrrp 503, gauge
//!                    sip_proxy_recv_shards_stalled, ms ≥ 1 (default 2000);
//!                    a shard waiting on an empty socket is never stalled
//!   PROXY_RECV_SHARDS  N reuse-port recv sockets/cores, 1..1024 (default 1;
//!                    >1 shards the serial recv loop across N sockets; kernel
//!                    flow-hashing keeps each UAC src:port on one socket so
//!                    per-flow ordering holds)
//!   PROXY_INTAKE_SHED_PCT  queue depth, percent 0..100 of PROXY_QUEUE, at
//!                    which new non-emergency INVITEs are shed at intake
//!                    (default 50; floored at one queued packet)
//!   PROXY_BIND_RETRY_INTERVAL_MS  cadence of bind retries while a port is
//!                    still held (EADDRINUSE), ms ≥ 1   (default 200)
//!   PROXY_BIND_RETRY_DEADLINE_MS  bind-retry give-up deadline, ms ≥ 1; exceeds
//!                    the SIGTERM drain grace so a recreated pod waits out its
//!                    predecessor instead of crash-looping (default 10000)
//!   PROXY_DRAIN_GRACE_MS  wait between SIGTERM (/readyz Draining) and exit,
//!                    ms ≥ 1                               (default 5000)
//!   PROXY_HMAC_KID   stickiness cookie key id              (default k0)
//!   PROXY_HMAC_KEY   stickiness cookie secret, ≥ 16 bytes  (REQUIRED, no
//!                    default; every proxy of a cluster shares it)
//!   HEALTH_INTERVAL_MS / HEALTH_TIMEOUT_MS  OPTIONS probe tick and reply
//!                    window, ms ≥ 1                  (default 1000 / 1500)
//!   HEALTH_THRESHOLD  consecutive misses that mark a worker Dead, ≥ 1
//!                    (default 2)
//!   LB_ELU_SOFT / LB_ELU_HARD / LB_ELU_CRITICAL  worker ELU bands, each in
//!                    (0, 1], soft < hard < critical (default 0.4 / 0.5 / 0.65)
//!   LB_BAND_HYSTERESIS  in [0, 1), below the narrower band gap (default 0.05)
//!   LB_AIMD_DECREASE_FACTOR  in (0, 1)                     (default 0.5)
//!   LB_AIMD_INCREASE_STEP_CPS  > 0                         (default 2)
//!   LB_CAP_CEILING_CPS / LB_CAP_FLOOR_CPS  > 0, floor ≤ 30 (the initial cap)
//!                    ≤ ceiling                       (default 200 / 1)
//!   PROXY_RESOLVER_PREWARM  comma-separated host:port DNS targets to resolve
//!                    at startup and pin permanently warm      (default none)
//!   PROXY_RESOLVER_POSITIVE_TTL_MS  resolver positive-cache TTL, ms ≥ 1
//!                    (default 60000)
//!   PROXY_RESOLVER_NEGATIVE_TTL_MS  resolver negative-cache TTL, ms ≥ 1
//!                    (default 5000)
//!   PROXY_RESOLVER_LOOKUP_TIMEOUT_MS  bound on one name lookup, ms ≥ 1
//!                    (default 2000)
//!   PROXY_SELF_GATE  1|true|yes|on → ELU/CPS self-gate, 0|false|no|off →
//!                    always-admit                          (default on)
//!   PROXY_SELF_GATE_ELU_CRITICAL  ELU shed threshold, (0, 1] (default 0.80)
//!   PROXY_SELF_GATE_CPS_SIZE      CPS bucket capacity, ≥ 1 (default 50)
//!   PROXY_SELF_GATE_CPS_RATE      CPS bucket refill /s, ≥ 1 (default 100)
//!   PROXY_RETRY_AFTER_JITTER_SEC  spread added to a refusal's Retry-After
//!                    base, seconds 0..3600; every copy of one request draws
//!                    the same value                           (default 5)

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

// Use jemalloc instead of glibc malloc (mirrors b2bua-runner). glibc retains
// freed arena chunks and ratchets RSS under sustained churn; jemalloc's decay
// purging returns pages to the OS and bounds steady-state RSS. The proxy is
// single-task and far less alloc-heavy than the b2bua, so this is mostly for a
// uniform memory story across tiers — but it also lets the same jemalloc_*
// /metrics probe rule the proxy in or out during a soak. Tuned via
// _RJEM_MALLOC_CONF on the container (see deploy/helm/siprust/values-kind.yaml).
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod entropy;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sip_clock::Clock;
use sip_proxy::health::FirstView;
use sip_proxy::load_observer::WorkerLoadObserver;
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::self_gate::{AlwaysAdmitGate, EluCpsGate, IntakeAgeRecorder, ProxySelfGate};
use sip_proxy::strategies::LoadBalancerStrategy;
use sip_proxy::{ExternalFaceParts, RoutingStrategy};
use sip_proxy_runner::binds::{bind_sockets, ShardSockets, Sockets};
use sip_proxy_runner::config::{refusal, ProxyConfig, WorkerSource};
use sip_proxy_runner::discovery::{discovered_pool, static_pool};
use sip_proxy_runner::health_sampler::HealthSampler;
use sip_proxy_runner::shards::SharedRouting;
use sip_proxy_runner::signal::sigterm;
use sip_proxy_runner::sweeps::{spawn_self_gate_sampler, spawn_stale_floor_sweep};

/// The lifecycle line of a proxy about to serve: its faces, its workers and
/// its socket settings.
fn log_listening(cfg: &ProxyConfig, registry: &dyn WorkerRegistry) {
    let face_note = match &cfg.external {
        Some(ext) => format!(
            " DUAL-FACE ext_listen={} ext_advertise={}:{} (int CIDRs from PROXY_FACE_INT_CIDRS)",
            ext.listen, ext.advertise.host, ext.advertise.port,
        ),
        None => String::new(),
    };
    tracing::info!(
        pid = std::process::id(),
        listen = %cfg.listen,
        advertise = %format_args!("{}:{}", cfg.advertise.host, cfg.advertise.port),
        faces = %face_note,
        workers = %registry
            .snapshot()
            .iter()
            .map(|w| format!("{}@{}:{}", w.id, w.address.host, w.address.port))
            .collect::<Vec<_>>()
            .join(","),
        queue = cfg.queue_max,
        recv_shards = cfg.recv_shards,
        udp_sndbuf = ?cfg.udp_sndbuf,
        udp_rcvbuf = ?cfg.udp_rcvbuf,
        shard_stall_ms = cfg.shard_stall_ms,
        "listening"
    );
}

fn main() -> std::process::ExitCode {
    sip_proxy_runner::runtime::block_on(serve())
}

/// The proxy from boot to exit. A refused boot is logged once while the log
/// writer runs; a refused boot or a data-path task that ended exits
/// non-zero, after the writer has drained.
async fn serve() {
    // Subscriber first (ADR-0026): every line below it is a lifecycle log, and
    // the guard drains the writer + flushes spans at exit.
    let observe_guard = match observe::init_production("sip-proxy-runner") {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("sip-proxy-runner FATAL: {e}");
            std::process::exit(1);
        }
    };
    let exit_code = match run().await {
        Ok(code) => code,
        Err(refusal) => {
            tracing::error!(error = %refusal, "refusing to start");
            1
        }
    };
    // `process::exit` runs no destructor: the writer drains first.
    drop(observe_guard);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

/// Boot and serve until a signal or a data-path task's end: the exit code,
/// or the refusal of a boot that cannot serve.
async fn run() -> Result<i32, String> {
    // Loud confirmation the jemalloc decay config (_RJEM_MALLOC_CONF) parsed —
    // a typo is silently ignored. Mirrored by the jemalloc_opt_*_decay_ms gauges.
    #[cfg(not(target_env = "msvc"))]
    jemalloc_stats::log_config();

    // Every variable is read and checked before the first bind: a value the
    // proxy cannot honour refuses the boot, naming every problem at once.
    let cfg = ProxyConfig::from_env()?;
    let sigterm = sigterm()?;
    let metrics_sa = cfg.metrics;
    let advertised = cfg.advertise.clone();
    let shard_stall_ms = cfg.shard_stall_ms;
    let recv_shards = cfg.recv_shards;
    let ext_face = cfg.external.clone();
    tracing::info!(observer = ?cfg.observer, "load-observer bands and AIMD ladder");

    // ONE process clock, built before the first bind: every signaling endpoint
    // stamps `UdpPacket::arrival_ms` on it and every `ProxyCore` ages dequeued
    // packets against it, so the self-gate's intake age is a difference of two
    // readings of a SINGLE monotonic-anchored timeline.
    let clock = Clock::checked_system().map_err(|e| e.to_string())?;
    let Sockets { metrics: metrics_listener, shards, probe: probe_ep } =
        bind_sockets(&cfg, &clock).await?;

    let hmac = Arc::new(
        StaticHmacKeyProvider::new(
            HmacKey::new(cfg.hmac_kid.clone(), cfg.hmac_key.clone().into_bytes()),
            None,
        )
        .map_err(|e| format!("bad PROXY_HMAC_KEY: {e}"))?,
    );
    let observer = Arc::new(WorkerLoadObserver::new(cfg.observer));
    let metrics = Arc::new(ProxyMetrics::new());
    let id_gen = Arc::new(entropy::keyed_id_gen()?);

    // Proxy-self ELU/CPS admission gate. On by default; the ELU
    // arm observes intake saturation — every recv shard records each packet's
    // age at dequeue into this ONE shared recorder, and the 100 ms sampler task
    // spawned below drains the window max into the gate's EWMA.
    // `PROXY_SELF_GATE=0` reverts to the always-admit gate (then `self_gate` is
    // `None`, no sampler task, no self-gate /metrics). Build the concrete
    // `EluCpsGate` once so the sampler task + the /metrics exposition can read
    // it; hand each core an `Arc<dyn ProxySelfGate>` view of the same gate.
    let intake_age = IntakeAgeRecorder::default();
    // ONE liveness pulse across every recv shard: the readiness gate reads it.
    let pulse = Arc::new(sip_proxy::liveness::ShardPulse::new(recv_shards));
    let self_gate: Option<EluCpsGate> =
        cfg.self_gate.map(|gate_cfg| EluCpsGate::intake(intake_age.clone(), gate_cfg));
    let gate_dyn: Arc<dyn ProxySelfGate> = match &self_gate {
        Some(g) => Arc::new(g.clone()),
        None => Arc::new(AlwaysAdmitGate),
    };

    // Worker pool + its health-write seam. Static PROXY_WORKERS (dev/local) or the
    // k8s EndpointSlice informer (ADR-0012 D4). Either way the OPTIONS probe writes
    // observed health through `health_control` so an unanswered worker is demoted
    // (Dead/NotReady/Draining) and in-dialog requests fail over to the backup.
    let pool = match &cfg.workers {
        WorkerSource::Static(list) => {
            static_pool(list, clock.clone()).map_err(|e| refusal(&[e]))?
        }
        WorkerSource::Discovered { service, namespace, port } => {
            discovered_pool(namespace.clone(), service.clone(), *port, clock.clone())
        }
    };
    let registry = pool.registry.clone();
    let health_control = pool.control.clone();

    let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
        registry.clone(),
        hmac,
        observer.clone(),
        metrics.clone(),
        clock.clone(),
    ));

    // The stale-payload sweep shares the probe's observer.
    let sweep_observer = observer.clone();

    // The CANCEL/rtx LRU and the refusal memo are ONE instance across shards
    // (`SharedRouting`).
    let cancel_lru = Arc::new(sip_proxy::cancel_lru::CancelBranchLru::with_clock(clock.clone()));
    let refusals =
        Arc::new(sip_proxy::refusals::Refusals::new(&id_gen, cfg.retry_after_jitter_sec));

    // Named-target resolver tuning + startup prewarm. A COLD
    // name (fresh deploy, CoreDNS restart, TTL expiry) would cost the first
    // b-leg forward a full in-path resolve — 3.5–7.5 s under kube ndots:5
    // search expansion, blowing the downstream 2 s connect timer. Prewarm
    // targets are resolved off the serving path right after core construction
    // (boot never blocks or fails on a slow/failed prewarm — outcomes land in
    // sip_proxy_resolver_refresh_total{outcome=prewarmed|prewarm_failed}) and
    // pinned so the proactive refresh keeps them permanently warm.
    let resolver_cfg = cfg.resolver;
    let prewarm_targets = cfg.prewarm.clone();

    // ONE trace registry across every recv shard: the active-trace cap and the
    // "anything sampled?" flag bound the process, not a socket (ADR-0026).
    let traces = Arc::new(sip_proxy::ProxyTraces::from_env(clock.now_ms()));

    let shared = SharedRouting {
        advertised: advertised.clone(),
        strategy,
        registry: registry.clone(),
        clock: clock.clone(),
        id_gen: id_gen.clone(),
        metrics: metrics.clone(),
        cancel_lru,
        refusals,
        self_gate: gate_dyn,
        intake_age,
        resolver: resolver_cfg,
        traces,
        pulse: pulse.clone(),
        first_view: FirstView::pending(),
    };
    // Health probe: OPTIONS to the pool from the first listing on, feeding the
    // band observer, and latching the first worker view the recv shards and
    // the probe routes read (ADR-0050 §5, §11).
    let probe = shared.health_probe(probe_ep, health_control, observer, cfg.probe);
    let mut cores = Vec::with_capacity(recv_shards);
    for (shard, ShardSockets { internal, external }) in shards.into_iter().enumerate() {
        let mut builder = shared.shard(shard);
        if let (Some(ext), Some(endpoint)) = (&ext_face, external) {
            builder = builder.external_face(ExternalFaceParts {
                endpoint,
                advertised: ext.advertise.clone(),
                int_cidrs: ext.int_cidrs.clone(),
            });
        }
        cores.push(builder.build(internal));
    }
    if !prewarm_targets.is_empty() {
        // Each recv shard owns its own resolver cache, so prewarm every core.
        for core in &cores {
            core.prewarm_named_targets(prewarm_targets.clone());
        }
        tracing::info!(
            targets = %prewarm_targets.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(","),
            recv_shards,
            outcomes = "sip_proxy_resolver_refresh_total{outcome=prewarmed|prewarm_failed}",
            "resolver prewarm pinned warm across every shard cache"
        );
    }

    log_listening(&cfg, registry.as_ref());

    // Readiness gate (ADR-0012 D4, ADR-0050 §11): the proxy is fit to take
    // traffic only once its first worker view is in and with ≥1 routable
    // (`Alive`) worker. With the k8s EndpointSlice informer the pool starts
    // empty and fills asynchronously (and stays empty if the watch is
    // RBAC-forbidden / the Service name is wrong), so `/readyz` turns a
    // misconfigured watch into a loud rollout timeout. SIP reaches the proxy
    // through the VRRP address, which follows `/vrrp` (`/healthz` and the
    // first view), not readiness. SIGTERM latches Draining here.
    let draining = Arc::new(AtomicBool::new(false));
    // A stalled recv shard (parked on one packet past PROXY_SHARD_STALL_MS —
    // see `sip_proxy::liveness`) is a proxy that black-holes a share of its
    // flows while every activity meter reads idle: it fails `/readyz`,
    // `/healthz` and `/vrrp` (ADR-0046 D4), and raises the gauge.
    let proxy_probes = shared.probes(draining.clone(), shard_stall_ms);

    // Health sampler: the worker-health, acceptance, shard-stall and
    // discovery gauges every 2 s.
    HealthSampler {
        registry: registry.clone(),
        discovery: pool.discovery.clone(),
        metrics: metrics.clone(),
        pulse: pulse.clone(),
        clock: clock.clone(),
        stall_ms: shard_stall_ms,
    }
    .spawn();

    // The observer's stale-payload sweep, and the self-gate sampler when the
    // real gate is on; each restarted and counted when a run panics.
    spawn_stale_floor_sweep(sweep_observer, registry.clone(), clock.clone(), metrics.clone());
    if let Some(gate) = self_gate.clone() {
        spawn_self_gate_sampler(gate);
    }

    // Prometheus + readiness endpoint (shared `probe-http`). NOTE: `ProbeServer`
    // aborts its accept loop on Drop, so the handle must stay live for the whole
    // process lifetime. The `/metrics` body appends the proxy-self gate gauges
    // + the jemalloc mallctl exposition (footprint/purge/decay config) per
    // scrape — the latter same cfg as the allocator dep.
    let metrics_gate = self_gate.clone();
    let routes = probe_http::ProbeRoutes {
        metrics: Arc::new(move || {
            let gate = metrics_gate.as_ref().map(EluCpsGate::metrics);
            sip_proxy_runner::metrics_body::body(
                &metrics,
                gate.as_ref(),
                &jemalloc_stats::prometheus_text,
            )
        }),
        ready: proxy_probes.ready,
        // No heap profiling on the proxy (not the leak target): /debug/heap → 503.
        heap: None,
        vrrp: Some(proxy_probes.vrrp),
        admin: None,
    };
    // The listener was bound FIRST (metrics-first bind order above); this only
    // starts the accept loop. A failure here is fatal — a proxy without its
    // health endpoint gets liveness-killed 30 s later with a lying /readyz.
    let _metrics_server =
        probe_http::ProbeServer::serve_on_live(metrics_listener, routes, proxy_probes.live)
            .map_err(|e| format!("metrics server on {metrics_sa} failed to start: {e}"))?;
    tracing::info!(addr = %_metrics_server.addr(), "metrics server listening (/metrics, readiness /readyz, VRRP check /vrrp)");

    let probe_task = tokio::spawn(probe.run());
    let mut core_tasks = tokio::task::JoinSet::new();
    for core in cores {
        core_tasks.spawn(core.run());
    }

    // Graceful shutdown on SIGTERM (k8s pod termination). The proxy holds no
    // per-call state to quiesce — it is a stateless forwarder — so the drain is
    // latch-then-grace: flip `/readyz` to Draining (k8s unpublishes the pod and
    // stops routing new datagrams here), then wait the grace so in-flight
    // transactions settle and the EndpointSlice withdrawal propagates before
    // exit. Ctrl-C (interactive) exits at once.
    let drain_grace = cfg.drain_grace;
    let drain_grace_ms = u64::try_from(drain_grace.as_millis()).unwrap_or(u64::MAX);

    // Supervise every data-path task: a panicked/exited recv loop must not
    // leave the process alive with /healthz green — k8s would never restart the
    // pod and every datagram would be silently black-holed. Exiting non-zero makes
    // the container restart the moment ANY recv shard or the probe dies.
    //
    // The select only DECIDES the exit code: `process::exit` runs no
    // destructor, so the fatal line would die queued on the log writer. It runs
    // in `serve`, after the observe guard has drained.
    let exit_code = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!(signal = "SIGINT", "shutting down");
            0
        }
        _ = sigterm => {
            tracing::info!(signal = "SIGTERM", drain_grace_ms, "draining");
            draining.store(true, Ordering::Relaxed);
            tokio::time::sleep(drain_grace).await;
            tracing::info!("drain grace elapsed — exiting");
            0
        }
        res = core_tasks.join_next() => {
            tracing::error!(outcome = ?res, "FATAL: a SIP recv shard exited — exiting for restart");
            1
        }
        res = probe_task => {
            tracing::error!(outcome = ?res, "FATAL: health probe exited — exiting for restart");
            1
        }
    };
    drop(_metrics_server);
    Ok(exit_code)
}
