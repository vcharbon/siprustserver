//! Reusable B2BUA runner composition: everything between env vars and a
//! running [`B2buaCore`], shared by every runner binary.
//!
//! Per ADR-0016 a downstream integrator composes callflow services and injects
//! its own [`CallDecisionEngine`] in a runner binary it owns. The composition
//! *seam* (`B2buaCore::spawn_with_services`, [`B2buaDeps`], [`B2buaConfig`])
//! reuses cleanly, and everything between env vars and a populated
//! [`B2buaDeps`] lives here, published once, so a downstream runner neither
//! copies it nor diverges as upstream policy (env grammar, advertise rules,
//! overload knobs, drain behavior) evolves:
//!
//! - [`RunnerEnv::from_env`] — the `B2BUA_*`/`LIMITER_*`/`WORKER_*` env
//!   grammar parsed into a plain struct of pub fields (tweak any before bind);
//! - [`RunnerEnv::bind`] — UDP bind with the ingress brake installed,
//!   advertise-address coercion, [`B2buaConfig`] assembly + validation;
//! - [`RunnerBase::deps`] — production-shaped defaults for every dependency
//!   (store / buffered CDR / limiter-from-env / metrics / clock / id-gen),
//!   each overridable field-by-field on the returned [`B2buaDeps`];
//! - [`RunnerBase::rabbitmq_cdr_sink_from_env`] — the RabbitMQ CDR sink the
//!   `B2BUA_CDR_RABBITMQ_*` grammar selects, passed to [`RunnerBase::deps`]
//!   (`_with_encoder` publishes a runner-chosen [`CdrEncoder`] format);
//! - [`RunnerBase::replication_setup_from_env`] — the peer-to-peer
//!   replication the `B2BUA_REPL*` grammar selects ([`ReplicationSettings`]:
//!   static or EndpointSlice membership, per-attempt peer addressing), assigned
//!   to `deps.replication`;
//! - [`RunnerBase::spawn`] / [`RunnerBase::spawn_probe_server`] /
//!   [`RunnerBase::spawn_gauge_sampler`] / [`RunnerBase::run_until_shutdown`]
//!   — core spawn, the `/metrics`+`/ready`+`/healthz` probe, the
//!   memory-attribution sampler, and the stop handling (SIGTERM or the
//!   worker's own withdrawal drains; a loop task ending exits non-zero);
//! - the shared helpers ([`env_or`], [`env_flag`], [`resolve`],
//!   [`split_host_port`], [`NullCdrWriter`]).
//!
//! The in-tree `b2bua-runner` is the first consumer, so the surface provably
//! stays sufficient to build a full production worker. A minimal downstream
//! runner is the staged skeleton; it installs the process subscriber
//! ([`observe::init_production`]) first, so a refused boot is logged:
//!
//! ```no_run
//! # async fn run(
//! #     my_engine: std::sync::Arc<dyn b2bua::decision::CallDecisionEngine>,
//! # ) -> Result<std::process::ExitCode, String> {
//! let base = b2bua_runner_kit::RunnerEnv::from_env()?.bind("my-runner").await?;
//! let deps = base.deps(my_engine, None).await; // swap any B2buaDeps field before spawn
//! let core = base.spawn(deps, Vec::new()); // + your composed ServiceDefs
//! let _probe = base.spawn_probe_server(&core, None, None).await;
//! base.spawn_gauge_sampler(&core);
//! Ok(base.run_until_shutdown(&core).await)
//! # }
//! ```
//!
//! What deliberately does NOT live here: the decision engine (the one piece a
//! runner exists to choose), the composed service list, and allocator wiring —
//! those are the runner's own, injected through the seams above. Replication is
//! the runner's choice to take: a runner may assign any other
//! `ReplicationSetup` to `deps.replication`. The CDR sink is the runner's choice too:
//! the kit ships the RabbitMQ sink ([`RabbitMqCdrSettings`] holds its env
//! grammar) and the discarding default; a runner may give the RabbitMQ sink
//! its own [`CdrEncoder`] or pass any other [`CdrWriter`] to
//! [`RunnerBase::deps`].

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

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use b2bua::admission::Refusals;
use b2bua::cdr::{BufferedCdrWriter, CdrEncoder, CdrWriter};
use b2bua::config::{B2buaConfig, CdrConfig};
use b2bua::decision::CallDecisionEngine;
use b2bua::ingress_brake::{build_ingress_brake_hook, IngressBrakeConfig, IngressBrakeCounters};
use b2bua::limiter::CallLimiter;
use b2bua::metrics::{B2buaMetrics, LiveGauge, UdpTransportMetrics};
use b2bua::rules::ServiceDef;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps, ReplicationSetup};
use http_net::{RealHttpNetwork, SystemResolver};
use sip_clock::wall::millis_u64;
use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork, UdpEndpoint};

pub mod backend_url;
mod capacity_env;
mod cdr_rabbitmq;
mod drain_env;
mod entropy;
mod limiter_client;
mod limiter_env;
mod liveness_env;
mod null_cdr;
mod outbound_proxy_env;
pub mod poison;
mod probe;
mod replication;
mod runner_env;
pub mod runtime;
mod shutdown;
mod socket_buffers;
mod validate;
mod worker_metrics;
pub use cdr_rabbitmq::{
    rabbitmq_cdr_writer_from_lookup, rabbitmq_cdr_writer_from_lookup_with_encoder,
    CdrDeliveryBounds, CdrQueueDeclare, RabbitMqCdrSettings, RabbitMqCdrWriter,
    MAX_DRAINER_WAIT_MS, MAX_WAIT_MS, MAX_WINDOW,
};
pub use null_cdr::NullCdrWriter;
pub use outbound_proxy_env::{parse_outbound_proxy, validate_outbound_proxy_requirement};
pub use replication::{replication_setup_from_lookup, ReplicationSettings};
pub use runner_env::RunnerEnv;
use socket_buffers::log_socket_buffers;
pub use validate::{validate_default_dest, validate_ingress_brake_pct};
pub use worker_metrics::{txn_metrics_text, WorkerMetrics, CATALOGUE};

/// How often the gauge sampler publishes.
const GAUGE_PERIOD: Duration = Duration::from_secs(5);

/// Env var with a default when unset.
pub fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// `value` trimmed, `None` when it is absent or blank: a knob set to nothing
/// states nothing.
pub fn stated(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The comma-separated names `value` lists, `None` when it names none: a list
/// of blanks states nothing, as a blank value does.
pub fn stated_list(value: Option<String>) -> Option<Vec<String>> {
    stated(value).map(|v| split_csv(&v)).filter(|names| !names.is_empty())
}

/// The env var `key` as [`stated`] reads it.
pub fn env_stated(key: &str) -> Option<String> {
    stated(env::var(key).ok())
}

/// Truthy env flag: `1`/`true`/`yes`/`on` (case-insensitive) → true.
pub fn env_flag(key: &str) -> bool {
    is_truthy(&env_or(key, "0"))
}

fn is_truthy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Resolve a `host:port` string to a socket address; `Err` is the boot
/// refusal of one that does not resolve.
pub fn resolve(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("cannot resolve {addr:?}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address resolved from {addr:?}"))
}

/// Split a `host:port` into its parts WITHOUT DNS resolution (the host may be a
/// service name resolved per-call downstream). Port defaults to 5060 if absent
/// or unparseable. Used for a b-leg callee default (`B2BUA_DEST`-style knobs) —
/// resolving at boot would pin every call to one startup-resolved pod.
pub fn split_host_port(s: &str) -> (String, u16) {
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(5060)),
        None => (s.to_string(), 5060),
    }
}

impl RunnerEnv {
    /// Bind the real UDP endpoint (ingress brake installed), coerce the
    /// advertise address, assemble + validate the [`B2buaConfig`], and mint the
    /// process-wide metrics registry / system clock. `Err` is the boot refusal
    /// of a bind failure, an invalid config, a pre-epoch wall clock or an OS
    /// RNG that does not answer. `name` names the service in every log line.
    ///
    /// The runner installs the process subscriber (ADR-0026,
    /// [`observe::init_production`]) before it, so the refusal is logged.
    pub async fn bind(self, name: &str) -> Result<RunnerBase, String> {
        let invalid = |e: String| format!("invalid B2BUA config: {e}");
        // The per-call trace gate, configured once from the environment before
        // any call arrives (ADR-0026): with no OTLP endpoint it is inert and no
        // call path pays more than one boolean check.
        b2bua::trace::install_process_traces(std::sync::Arc::new(
            b2bua::trace::CallTraces::from_env(0),
        ));
        validate_ingress_brake_pct(self.udp_ingress_brake_pct).map_err(invalid)?;
        validate_outbound_proxy_requirement(
            self.require_outbound_proxy,
            self.outbound_proxy.is_some(),
        )
        .map_err(invalid)?;
        let clock = Clock::checked_system().map_err(|e| e.to_string())?;

        let listen_sa = resolve(&self.listen)?;
        let metrics_sa = resolve(&self.metrics_addr)?;

        // Ingress brake: the `preIngress` hook + its counters, installed
        // on the worker socket. Without it the brake (the cheapest stateless-503
        // shed, ahead of the router's rungs) is absent and a flooded ingress queue tail-drops
        // new INVITEs silently instead of returning a routable 503 + Retry-After.
        // The counters are retained for the `/metrics` scrape.
        let brake_counters = IngressBrakeCounters::new();
        let brake_config = IngressBrakeConfig {
            queue_max: self.queue_max,
            threshold_pct: self.udp_ingress_brake_pct,
        };
        // The worker's identifier generator, keyed from the OS RNG once: the
        // core's, and the To-tag secret of its refusals.
        let id_gen = Arc::new(entropy::keyed_id_gen()?);
        // The worker's one answer and memo for refused new INVITEs, shared by
        // the brake and, through `deps`, the core's transaction layer.
        let refusals = Refusals::new(
            self.retry_after_base_sec,
            self.retry_after_jitter_sec,
            brake_config.memo_capacity(),
            &id_gen,
        );
        let brake_hook =
            build_ingress_brake_hook(brake_config, brake_counters.clone(), refusals.clone());

        // Real, non-recording transport: a plain tokio UDP socket. Bind into an
        // `Arc` so the endpoint can be SHARED: the core takes ownership of one
        // boxed clone (it drives the recv loop), while a second clone backs the
        // `UdpTransportMetrics` live `queueDepth`/`dropsTailDrop` getters.
        let net = RealSignalingNetwork::new();
        let mut bind_opts =
            BindUdpOpts::new(listen_sa, self.queue_max).with_pre_ingress(brake_hook);
        if let Some(bytes) = self.udp_sndbuf {
            bind_opts = bind_opts.with_send_buffer(bytes);
        }
        if let Some(bytes) = self.udp_rcvbuf {
            bind_opts = bind_opts.with_recv_buffer(bytes);
        }
        let endpoint: Arc<dyn UdpEndpoint> = net
            .bind_udp(bind_opts)
            .await
            .map_err(|e| format!("bind {listen_sa} failed: {e:?}"))?
            .into();
        let local = endpoint.local_addr();
        log_socket_buffers(name, endpoint.as_ref(), self.udp_rcvbuf, self.udp_sndbuf);

        // The `UdpTransport` facade's Prometheus-visible shape: the brake
        // counters + live readings of the bound endpoint.
        let udp_metrics = {
            let live = |read: fn(&dyn UdpEndpoint) -> u64| -> LiveGauge {
                let endpoint = endpoint.clone();
                Arc::new(move || read(endpoint.as_ref()))
            };
            UdpTransportMetrics::new(
                self.queue_max,
                brake_counters.clone(),
                live(|ep| ep.queue_depth() as u64),
                live(|ep| ep.counters().tail_dropped),
                live(|ep| ep.counters().send_would_block),
                live(|ep| ep.counters().kernel_rx_dropped),
                live(|ep| ep.counters().pre_ingress_panics),
            )
        };
        tracing::info!(
            service = name,
            threshold = brake_config.threshold(),
            queue_max = self.queue_max,
            ingress_brake_pct = self.udp_ingress_brake_pct,
            "ingress brake armed: stateless-503 for new non-emergency INVITEs at ingress depth >= threshold"
        );

        // Advertised SIP host:port stamped on every outbound Via / Contact /
        // b-leg Call-ID (see `b2bua::stack_identity`). It MUST be an address the
        // callee / proxy can route a response back to — the *bind* address may
        // be `0.0.0.0`, which is NOT routable: a peer's 200 OK to a Via/Contact
        // of `0.0.0.0` goes nowhere, the B2BUA never sees the answer, and it
        // retransmits the INVITE then CANCELs → a retransmission storm. Take
        // `B2BUA_ADVERTISE` (`host[:port]`) verbatim when set (k8s injects the
        // pod IP via the downward API `status.podIP`); otherwise fall back to
        // the bound address, coercing an unspecified `0.0.0.0` to loopback so
        // the literal is at least routable.
        let (advertise_ip, advertise_port) = match &self.advertise {
            Some(s) => {
                let s = s.trim();
                match s.rsplit_once(':').and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h, p))) {
                    Some((h, p)) => (h.to_string(), p),
                    // No `:port` (or unparseable port) → host-only; listen port.
                    None => (s.to_string(), local.port()),
                }
            }
            None => {
                let ip = if local.ip().is_unspecified() {
                    IpAddr::V4(Ipv4Addr::LOCALHOST)
                } else {
                    local.ip()
                };
                (ip.to_string(), local.port())
            }
        };
        tracing::info!(
            service = name,
            %advertise_ip,
            advertise_port,
            bind = %local,
            "advertised SIP identity"
        );

        match &self.outbound_proxy {
            // Every b-leg request goes to the front proxy with a preloaded plain
            // `;lr` Route; its internal face classifies it worker-outbound
            // (ADR-0048) and it record-routes itself into the b-leg.
            Some((h, p)) => tracing::info!(
                service = name,
                proxy_host = %h,
                proxy_port = p,
                "b-leg egress forced through the front proxy (all worker->callee SIP traverses the LB)"
            ),
            None => tracing::warn!(
                service = name,
                "B2BUA_OUTBOUND_PROXY unset — b-leg goes pod-direct (local/dev only; NOT for the cluster)"
            ),
        }

        let config = B2buaConfig {
            self_ordinal: self.ordinal.clone(),
            sip_local_ip: advertise_ip,
            sip_local_port: advertise_port,
            b2b_outbound_proxy: self.outbound_proxy.clone(),
            cdr_buffer_queue_max: self.cdr_queue,
            event_dispatch_concurrency: self.concurrency,
            per_call_queue_cap: self.call_cap,
            new_call_permit_share_percent: self.new_call_permit_share_percent,
            new_call_queue_headroom_percent: self.new_call_queue_headroom_percent,
            max_messages_per_call: self.max_messages_per_call,
            max_messages_per_call_lifetime: self.max_messages_per_call_lifetime,
            keepalive_interval_sec: self.keepalive_sec,
            keepalive_timeout_sec: self.keepalive_timeout_sec,
            reboot_budget_sec: self.reboot_budget_sec,
            limiter_refresh_sec: self.limiter_refresh_sec,
            limiter_refresh_batch_ms: self.limiter_refresh_batch_ms,
            limiter_refresh_batch_max: self.limiter_refresh_batch_max,
            limiter_release_queue_cap: self.limiter_release_queue_cap,
            setup_timeout_ms: self.setup_timeout_ms,
            invite_txn_timeout_sec: self.invite_txn_timeout_sec,
            invite_first_response_timeout_sec: self.invite_first_response_timeout_sec,
            call_control_timeout_ms: self.call_control_timeout_ms,
            ack_timeout_sec: self.ack_timeout_sec,
            cps_bucket_size: self.cps_bucket_size,
            cps_bucket_rate: self.cps_bucket_rate,
            overload_panic_elu_threshold: self.overload_panic_elu_threshold,
            retry_after_base_sec: self.retry_after_base_sec,
            retry_after_jitter_sec: self.retry_after_jitter_sec,
            worker_allowed_target_suffixes: self.worker_allowed_target_suffixes.clone(),
            relay_headers: self.relay_headers.clone(),
            privacy_service: self.privacy_service,
            cdr: CdrConfig {
                message_ring: self.cdr_message_ring,
                captured_headers: self.cdr_captured_headers.clone(),
            },
            capacity: self.capacity,
            ..Default::default()
        };
        // Forbid booting with a config that would silently break HA (too-short a
        // keepalive, a reboot budget that cannot outlast a primary reboot or a
        // keepalive refresh gap) or call setup (an INVITE transaction bound
        // outside its supported range, an app setup deadline / reaper idle
        // window that does not sit strictly under that bound).
        config.validate().map_err(|e| invalid(e.to_string()))?;
        let call_control =
            Duration::from_millis(u64::try_from(config.call_control_timeout_ms).unwrap_or(0));
        liveness_env::check_loop_stall(self.loop_stall, call_control).map_err(invalid)?;
        tracing::info!(service = name, capacity = ?config.capacity, "capacity ceilings");

        Ok(RunnerBase {
            name: name.to_string(),
            endpoint,
            local,
            config,
            udp_metrics,
            id_gen,
            refusals,
            // The shared registry, built BEFORE any deps so components a runner
            // constructs pre-spawn (notably CDR writers) record into the SAME
            // registry the core exports at `/metrics`.
            metrics: B2buaMetrics::new(),
            clock,
            metrics_sa,
            env: self,
        })
    }
}

/// Comma-separated env list: trimmed, empties dropped.
pub(crate) fn split_csv(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// Everything a bound runner shares between its assembly steps: the endpoint,
/// the validated config, and the process-wide metrics/clock. All fields pub so
/// a runner can reach any piece (e.g. `base.metrics` for a custom CDR sink,
/// `base.clock` for a replication store).
pub struct RunnerBase {
    /// Log-line prefix (the binary's name).
    pub name: String,
    /// The parsed env this base was built from (post any field overrides).
    pub env: RunnerEnv,
    /// The shared bound UDP endpoint (core recv loop + live metrics getters).
    pub endpoint: Arc<dyn UdpEndpoint>,
    /// The actually-bound local address.
    pub local: SocketAddr,
    /// The validated worker config (advertise coercion applied).
    pub config: B2buaConfig,
    /// The `UdpTransport` metrics facade over [`Self::endpoint`].
    pub udp_metrics: UdpTransportMetrics,
    /// The worker's identifier generator, keyed from the OS RNG at bind;
    /// [`deps`](Self::deps) hands it to the core.
    pub id_gen: Arc<sip_txn::IdGen>,
    /// The worker's refusals of new INVITEs, held by the ingress brake on
    /// [`Self::endpoint`]; [`deps`](Self::deps) hands the core the same
    /// instance, so a runner swapping one swaps both.
    pub refusals: Refusals,
    /// The process-wide metrics registry the core exports at `/metrics`.
    pub metrics: B2buaMetrics,
    /// System wall clock (transaction/dialog timers fire for real).
    pub clock: Clock,
    /// Resolved probe/metrics HTTP listen address.
    pub metrics_sa: SocketAddr,
}

impl RunnerBase {
    /// The default call-limiter client: `HttpCallLimiter` against
    /// `LIMITER_URL`, its name looked up at boot within one breaker probe
    /// period (a name that has not resolved by then boots the breaker open),
    /// or `NoopLimiter` when unset.
    pub async fn limiter_from_env(&self) -> Arc<dyn CallLimiter> {
        let hostport = (!self.env.limiter_url.is_empty()).then(|| self.env.limiter_url.clone());
        if let Some(hostport) = &hostport {
            tracing::info!(
                service = %self.name,
                limiter = %hostport,
                timeout_ms = self.env.limiter_timeout_ms,
                refresh_timeout_ms = self.env.limiter_refresh_timeout_ms,
                release_timeout_ms = self.env.limiter_release_timeout_ms,
                refresh_sec = self.env.limiter_refresh_sec,
                breaker_requests = self.env.limiter_backend.breaker.min_sent,
                breaker_window_ms = millis_u64(self.env.limiter_backend.breaker.window),
                breaker_probe_ms = millis_u64(self.env.limiter_backend.breaker.probe_period),
                max_in_flight = self.env.limiter_backend.client.max_in_flight,
                "call-limiter client wired"
            );
        }
        let settings = limiter_client::LimiterClientSettings {
            hostport,
            timeout: std::time::Duration::from_millis(self.env.limiter_timeout_ms),
            backend: self.env.limiter_backend.clone(),
            refresh_timeout: std::time::Duration::from_millis(self.env.limiter_refresh_timeout_ms),
            release_timeout: std::time::Duration::from_millis(self.env.limiter_release_timeout_ms),
            boot_lookup: self.env.limiter_backend.breaker.probe_period,
        };
        limiter_client::limiter_client(
            &settings,
            Arc::new(RealHttpNetwork::new()),
            Arc::new(SystemResolver),
        )
        .await
    }

    /// The RabbitMQ CDR sink the env selects ([`RabbitMqCdrSettings`]),
    /// publishing the default [`JsonRecordEncoder`](b2bua::cdr::JsonRecordEncoder)
    /// record and recording into [`Self::metrics`]; `None` when the URL is
    /// unset. `Err` is the boot refusal of a malformed `B2BUA_CDR_RABBITMQ_*`
    /// value.
    pub fn rabbitmq_cdr_sink_from_env(&self) -> Result<Option<Arc<dyn CdrWriter>>, String> {
        let writer = rabbitmq_cdr_writer_from_lookup(|k| env::var(k).ok(), &self.metrics)
            .map_err(|e| format!("invalid B2BUA config: {e}"))?;
        Ok(writer.map(|w| self.wired_cdr_sink(w)))
    }

    /// [`Self::rabbitmq_cdr_sink_from_env`] publishing the bytes `encoder`
    /// produces: the runner chooses the record format, the kit owns delivery.
    pub fn rabbitmq_cdr_sink_from_env_with_encoder(
        &self,
        encoder: Arc<dyn CdrEncoder>,
    ) -> Result<Option<Arc<dyn CdrWriter>>, String> {
        let lookup = |k: &str| env::var(k).ok();
        let writer = rabbitmq_cdr_writer_from_lookup_with_encoder(lookup, encoder, &self.metrics)
            .map_err(|e| format!("invalid B2BUA config: {e}"))?;
        Ok(writer.map(|w| self.wired_cdr_sink(w)))
    }

    fn wired_cdr_sink(&self, writer: RabbitMqCdrWriter) -> Arc<dyn CdrWriter> {
        let settings = writer.settings();
        tracing::info!(
            service = %self.name,
            sink = "rabbitmq",
            queue = %settings.queue,
            declare = ?settings.declare,
            buffer = self.env.cdr_queue,
            "CDR sink wired"
        );
        Arc::new(writer)
    }

    /// The peer-to-peer replication the env selects ([`ReplicationSettings`]),
    /// its store on [`Self::clock`]; `None` when replication is off. `Err` is
    /// the boot refusal of a malformed replication variable or a replication
    /// listener that cannot be bound.
    pub async fn replication_setup_from_env(&self) -> Result<Option<ReplicationSetup>, String> {
        replication_setup_from_lookup(|k| env::var(k).ok(), self.clock.clone())
            .await
            .map_err(|e| format!("invalid B2BUA config: {e}"))
    }

    /// Production-shaped [`B2buaDeps`] defaults around the injected decision
    /// engine: in-memory store, limiter-from-env, `cdr_sink` (default
    /// [`NullCdrWriter`]) behind the bounded `BufferedCdrWriter`
    /// (drop-on-overload at `cdr_queue` depth), the shared metrics registry,
    /// entropy id-gen, no replication. Every field on the returned struct is
    /// pub — swap any single piece before [`spawn`](Self::spawn).
    pub async fn deps(
        &self,
        decision: Arc<dyn CallDecisionEngine>,
        cdr_sink: Option<Arc<dyn CdrWriter>>,
    ) -> B2buaDeps {
        let sink = cdr_sink.unwrap_or_else(|| Arc::new(NullCdrWriter));
        B2buaDeps {
            config: self.config.clone(),
            decision,
            limiter: self.limiter_from_env().await,
            cdr: Arc::new(BufferedCdrWriter::spawn(sink, self.env.cdr_queue, self.metrics.clone())),
            store: Arc::new(InMemoryCallStore::new()),
            // No injected store faults in production (ADR-0023: the live-path
            // probe is a no-op while disarmed).
            store_faults: Default::default(),
            wire_faults: Default::default(),
            clock: self.clock.clone(),
            id_gen: self.id_gen.clone(),
            // The refusals the ingress brake holds (see [`RunnerBase::refusals`]).
            refusals: Some(self.refusals.clone()),
            deferred_ceilings: None,
            replication: None,
            metrics: self.metrics.clone(),
            // The generic service-authorable async-HTTP port is opt-in; a runner
            // that wants it swaps this field before `spawn` (every field is pub).
            adaptation_http: None,
            // Every built-in CORE machine included by default; a downstream that
            // ships its own transfer machine swaps this for
            // `ComposeOptions::default().without_core_refer_transfer()` before
            // `spawn` (every field is pub) — ADR-0016 opt-out seam.
            compose: b2bua::rules::ComposeOptions::default(),
            // The core builds the memory admission gate over the real process
            // from `config.capacity` (ADR-0037).
            capacity: None,
        }
    }

    /// Spawn the core over one boxed clone of the shared endpoint, composing
    /// `services` (ADR-0016) above the defaults.
    pub fn spawn(&self, deps: B2buaDeps, services: Vec<ServiceDef>) -> Arc<B2buaCore> {
        Arc::new(B2buaCore::spawn_with_services(Box::new(self.endpoint.clone()), deps, services))
    }

    /// Memory-attribution sampler: push the store + replication map sizes into
    /// their gauges every 5 s so an RSS climb can be pinned to a specific map
    /// even when `active_calls` is flat. 5 s is well inside the scrape cadence;
    /// the sample is a couple of brief locks, off the call path. It evicts
    /// nothing: the core's paced replica reap is the one eviction site, since an
    /// expired deferred terminal owes its limiter release on the way out. Each
    /// sample also logs a worker loop stalling past `loop_stall`, and turning
    /// again, once per transition. A run that panics is restarted and counted
    /// ([`b2bua::supervise`]).
    pub fn spawn_gauge_sampler(&self, core: &Arc<B2buaCore>) {
        let core = core.clone();
        let stall = self.env.loop_stall;
        b2bua::supervise::spawn(b2bua::supervise::GAUGE_SAMPLER, move || {
            let core = core.clone();
            async move {
                #[expect(clippy::disallowed_methods, reason = "GAUGE_PERIOD is not zero")]
                let mut tick = tokio::time::interval(GAUGE_PERIOD);
                let mut stalled_before = Vec::new();
                loop {
                    tick.tick().await;
                    core.sample_gauges();
                    if let Some(repl) = core.repl_store() {
                        let (bodies, idx, _meta, tomb) = repl.map_lens();
                        core.metrics().set_store_map_sizes(bodies as u64, idx as u64, tomb as u64);
                    }
                    let stalled = core.loops().stalled(stall);
                    if stalled != stalled_before {
                        if stalled.is_empty() {
                            tracing::info!(loops = ?stalled_before, "worker loops turning again");
                        } else {
                            tracing::error!(
                                loops = ?stalled,
                                threshold_ms = millis_u64(stall),
                                "worker loop stalled on one piece of work — /ready and /healthz 503"
                            );
                        }
                        stalled_before = stalled;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests;
