//! Standalone, containerizable B2BUA worker process.
//!
//! Wires the `b2bua` library over the **real, non-recording** UDP transport
//! (`sip_net::RealSignalingNetwork` — no `Recorder` decorator, no simulated
//! fabric) and a **system wall clock** (`Clock::system`, so transaction/dialog
//! timers fire). The generic runner plumbing — env grammar, bind + Tier-1
//! brake, advertise coercion, deps defaults, probe server, gauge sampler,
//! SIGTERM/drain — lives in `b2bua-runner-kit` (shared with downstream runner
//! binaries per ADR-0016); this binary keeps only its OWN composition choices:
//!   - route: `ScriptedDecisionEngine::route_all_to_with_limiter(DEST, stress)`
//!            (the HTTP call-control adapter is a deferred slice; routing all
//!            calls to a fixed UAS mirrors the k8s `worker -> sipp-uas` topology).
//!            It attaches an always-on `B2BUA_STRESS_LIMITER` entry to every call
//!            (full-chain stress) and honors an inbound `X-Api-Call` `call_limiter`
//!            array so a dedicated stream can enforce a real cap.
//!   - CDR  : RabbitMQ sink when `B2BUA_CDR_RABBITMQ_URL` is set, else the
//!            kit's discarding `NullCdrWriter` — either way behind the bounded
//!            `BufferedCdrWriter` (drop-on-overload).
//!   - HA   : opt-in peer-to-peer replication (S11) with static or kube
//!            EndpointSlice membership.
//!   - alloc: jemalloc (+ heap-profiling `/debug/heap` route, jemalloc stats
//!            appended to `/metrics`).
//!
//! Config via env (all optional; the generic `B2BUA_*`/`LIMITER_*`/`WORKER_*`
//! knobs are parsed by `b2bua_runner_kit::RunnerEnv` — see its field docs):
//!   B2BUA_LISTEN    SIP/signaling listen addr        (default 0.0.0.0:5060)
//!   B2BUA_ADVERTISE SIP host[:port] stamped on Via/Contact/b-leg Call-ID
//!                   (default: bound IP, or loopback if bind is 0.0.0.0).
//!                   In k8s inject the pod IP via downward API `status.podIP`,
//!                   else peers route responses to 0.0.0.0 (a storm).
//!   B2BUA_DEST      downstream UAS host:port          (default 127.0.0.1:5070)
//!   B2BUA_OUTBOUND_PROXY  front-proxy host:port every b-leg (worker→callee)
//!                   request is forced through (preloaded `Route ;lr;outbound`).
//!                   REQUIRED in the k8s cluster: a peer's internal pod IP is not
//!                   routable peer-to-peer in a real deployment, so ALL outbound
//!                   SIP must traverse the LB proxy — never go pod-direct. Unset →
//!                   b-leg goes straight to the callee (local/dev only). (unset)
//!   B2BUA_METRICS   Prometheus HTTP listen addr       (default 0.0.0.0:9091)
//!   B2BUA_QUEUE     inbound UDP queue depth (packets)  (default 8192)
//!   B2BUA_UDP_SNDBUF SO_SNDBUF on the signalling socket, bytes (default 4 MiB;
//!                   empty = kernel wmem_default; clamped at wmem_max) — ADR-0033
//!   B2BUA_UDP_RCVBUF SO_RCVBUF on the signalling socket, bytes (default 4 MiB;
//!                   empty = kernel rmem_default; clamped at rmem_max)
//!   B2BUA_ORDINAL   worker ordinal stamped in callRef  (default w0)
//!   B2BUA_CDR_QUEUE buffered-CDR submit queue depth    (default 1024; 0 = unbuffered, refused beside a RabbitMQ URL)
//!   B2BUA_CDR_RABBITMQ_URL / _QUEUE / _DECLARE / _MAX_LEN / _WINDOW / _*_TIMEOUT_MS / _BACKOFF*_MS
//!                   the RabbitMQ CDR sink (see `b2bua_runner_kit::RabbitMqCdrSettings`)
//!   B2BUA_CONCURRENCY handler concurrency ceiling       (default 8192; safety, not a rate cap)
//!   B2BUA_CALL_CAP  max concurrent calls before drop    (default 1_000_000)
//!   B2BUA_KEEPALIVE_SEC in-dialog OPTIONS keepalive interval (default 300 = 5 min, min 120)
//!   B2BUA_REBOOT_BUDGET_SEC replicated-backup TTL / reboot budget (default 600; min 60 and >= keepalive)
//!   B2BUA_SETUP_TIMEOUT_SEC a-leg total setup deadline, reroutes included (default 150, strictly below B2BUA_INVITE_TXN_TIMEOUT_SEC; <= 0 disables)
//!   B2BUA_INVITE_TXN_TIMEOUT_SEC out-of-dialog INVITE txn bound, both call halves (default 158; range 33..=600)
//!   B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC b-leg initial INVITE give-up when NOTHING answers, not even a 100 (default 32 = RFC 3261 Timer B; range 2..=32 — tightening is a deliberate §17.1.1.2 deviation, telephony policy: 2 s buys 2 re-sends, 5 s 3, 10 s 4, 32 s 6; a provisional swaps in B2BUA_INVITE_TXN_TIMEOUT_SEC; in-dialog INVITE and non-INVITE keep 64·T1)
//!   B2BUA_CANCEL_STRICT_RFC_WAIT truthy (1/true/yes/on) = literal RFC 3261 §9.1 CANCEL wait; default = ADR-0028 bounded hold (CANCEL always sent at grace expiry)
//!   B2BUA_CALL_CONTROL_TIMEOUT_MS decision-backend deadline per round-trip (default 5000; <= 0 disables — ADR-0022)
//!   WORKER_ALLOWED_TARGET_SUFFIXES b-leg target-admission allow-list, comma-separated (default .svc.cluster.local; `*` = allow all, rollback sentinel; non-IP non-matching hosts are 503'd pre-leg)
//!   B2BUA_RELAY_HEADERS opt-in transparent header relay, comma-separated names copied from the a-leg INVITE onto every originated b-leg INVITE (default empty = no relay; structural headers never relayable)
//!   B2BUA_CDR_MESSAGE_RING per-leg message-ring cap on the call record: the last N distinct SIP messages a leg received or sent (default 0 = off)
//!   B2BUA_CDR_CAPTURED_HEADERS header names whose values every ring entry captures, comma-separated (default empty)
//!
//! ## Call limiter
//!   LIMITER_URL             the shared limiter, [http://]host:port; unset → NoopLimiter (fail-open)
//!   LIMITER_REFRESH_SECONDS lease refresh cadence, below the service lease    (default 40)
//!   LIMITER_TIMEOUT_MS      per-request fail-open budget                     (default 150)
//!   B2BUA_STRESS_LIMITER_ID always-on limiter id on every call; "" disables  (default global-stress)
//!   B2BUA_STRESS_LIMITER_LIMIT cap for that entry (never rejects in practice) (default 999999)
//!
//! ## HA replication (S11) — opt-in via `B2BUA_REPL=1` (default off: unwired node)
//!   B2BUA_REPL / _REPL_LISTEN / _REPL_PORT / B2BUA_PEERS / B2BUA_REPL_SERVICE /
//!   B2BUA_NAMESPACE  the replication grammar (see `b2bua_runner_kit::ReplicationSettings`)
//!
//! SIGTERM latches the worker into `Draining` (OPTIONS 503 + readiness
//! probe fails) so k8s steers new calls away while in-flight calls finish.

// Use jemalloc instead of the glibc system allocator. Under the many tokio
// worker threads, glibc malloc spawns up to 8×ncpu arenas and retains freed
// chunks (it caps arena *count*, not per-arena high-water mark), so a churning
// SIP B2BUA's RSS ratchets monotonically up under sustained load and never
// returns memory to the OS — a 2026-06-13/14 no-chaos soak measured ~209 MiB/h
// growth with all logical state (active_calls/store/txn/repl) dead flat, leading
// to a node-cgroup OOM. jemalloc's decay-based purging returns dirty/muzzy pages
// to the OS (tuned aggressively via _RJEM_MALLOC_CONF on the worker container),
// bounding steady-state RSS. No logical leak exists; this is purely allocator
// retention. See deploy/k8s/manifests/20-worker.yaml.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::env;
use std::sync::Arc;

use b2bua::decision::{CallLimiterEntry, ScriptedDecisionEngine};
use b2bua_runner_kit::{env_or, split_host_port, validate_default_dest, RunnerEnv};

/// Always-on "stress" limiter entry attached to every routed call so the full
/// admit/release/refresh chain is exercised on all traffic (the endurance suite
/// drives this). `B2BUA_STRESS_LIMITER_ID` empty disables it; the default cap
/// (`B2BUA_STRESS_LIMITER_LIMIT`, default 999999) is high enough to never reject.
fn stress_limiter_from_env() -> Option<CallLimiterEntry> {
    let id = env_or("B2BUA_STRESS_LIMITER_ID", "global-stress");
    if id.trim().is_empty() {
        return None;
    }
    let limit = env::var("B2BUA_STRESS_LIMITER_LIMIT")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(999_999);
    Some(CallLimiterEntry { id, limit })
}

#[tokio::main]
async fn main() {
    // Loud confirmation the jemalloc decay config (_RJEM_MALLOC_CONF) actually
    // parsed — a typo is silently ignored. Mirrored by the jemalloc_opt_*_decay_ms
    // gauges on /metrics.
    #[cfg(not(target_env = "msvc"))]
    jemalloc_stats::log_config();

    // The b-leg callee (`B2BUA_DEST`) is passed to the decision engine as an
    // unresolved host:port: the core never resolves a destination name for
    // sending. Behind an IP-literal `B2BUA_OUTBOUND_PROXY` the proxy resolves the
    // Request-URI name; otherwise a name destination is dropped at send.
    let dest = env_or("B2BUA_DEST", "127.0.0.1:5070");
    let (dest_host, dest_port) = split_host_port(&dest);

    // Generic runner plumbing (b2bua-runner-kit): env grammar → bind (Tier-1
    // brake installed) → advertise coercion → validated config + metrics/clock.
    let base = RunnerEnv::from_env().bind("b2bua-runner").await;

    // Runner-only coherence (not visible to `B2buaConfig::validate`): the default
    // callee must be admissible under the worker's own allow-list — refuse to
    // boot with a clear message rather than silently 503 every call.
    validate_default_dest(&dest_host, &base.config.worker_allowed_target_suffixes)
        .unwrap_or_else(|e| panic!("invalid B2BUA config: {e}"));

    // CDR sink: RabbitMQ when `B2BUA_CDR_RABBITMQ_URL` is set, else the kit's
    // discarding default, either way behind the kit's bounded buffer.
    let cdr_sink = base.rabbitmq_cdr_sink_from_env();

    let mut deps = base.deps(
        Arc::new(ScriptedDecisionEngine::route_all_to_with_limiter(
            dest_host.clone(),
            dest_port,
            stress_limiter_from_env(),
        )),
        cdr_sink,
    );

    // Replication (opt-in, S11): `None` leaves the node unwired.
    deps.replication = base.replication_setup_from_env().await;

    // No extra ServiceDefs: the in-tree services (transfer, relay-first-18x)
    // ride `default_rules()` at runtime; `compose_services()` (lib.rs) is the
    // doc-generation registry.
    let core = base.spawn(deps, Vec::new());

    tracing::info!(
        pid = std::process::id(),
        listen = %base.local,
        %dest_host,
        dest_port,
        ordinal = %base.env.ordinal,
        queue = base.env.queue_max,
        cdr_queue = base.env.cdr_queue,
        "listening; all calls route to the default callee (resolved per-call)"
    );

    // jemalloc footprint/purge/decay-config counters appended to `/metrics`,
    // and the `/debug/heap` jemalloc heap profile (needs the profiling build +
    // _RJEM_MALLOC_CONF=prof:true) so an RSS leak's sources are attributed,
    // not guessed. Same cfg as the #[global_allocator].
    #[cfg(not(target_env = "msvc"))]
    let extra_metrics: Option<probe_http::MetricsFn> =
        Some(Arc::new(jemalloc_stats::prometheus_text));
    #[cfg(target_env = "msvc")]
    let extra_metrics: Option<probe_http::MetricsFn> = None;
    #[cfg(not(target_env = "msvc"))]
    let heap: Option<probe_http::HeapDumpFn> = Some(Arc::new(jemalloc_stats::dump_profile));
    #[cfg(target_env = "msvc")]
    let heap: Option<probe_http::HeapDumpFn> = None;

    // Held in a binding for the process lifetime (accept loop aborts on drop).
    let _probe = base.spawn_probe_server(&core, extra_metrics, heap).await;

    base.spawn_gauge_sampler(&core);
    base.run_until_shutdown(&core).await;
}
