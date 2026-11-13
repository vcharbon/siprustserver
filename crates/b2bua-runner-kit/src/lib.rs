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
//!   the worker's addresses and mode, [`B2buaConfig`] assembly;
//! - [`RunnerBase::deps`] — production-shaped defaults for every dependency
//!   (store / buffered CDR / limiter-from-env / metrics / clock / id-gen),
//!   each overridable field-by-field on the returned [`B2buaDeps`];
//! - [`RunnerBase::cdr_sink_from_env`] — the CDR sink the env selects, passed
//!   to [`RunnerBase::deps`]: the JSON-lines file `B2BUA_CDR_FILE` names
//!   ([`FileCdrWriter`]), else the RabbitMQ sink the `B2BUA_CDR_RABBITMQ_*`
//!   grammar selects ([`RunnerBase::rabbitmq_cdr_sink_from_env`];
//!   `_with_encoder` publishes a runner-chosen [`CdrEncoder`] format);
//! - [`RunnerBase::replication_setup_from_env`] — the peer-to-peer
//!   replication the `B2BUA_REPL*` grammar selects ([`ReplicationSettings`]:
//!   static or EndpointSlice membership, per-attempt peer addressing), assigned
//!   to `deps.replication`;
//! - [`ConfigCheck`] — the one consistency check of the final composed
//!   configuration: the runner records its own relations, and
//!   [`RunnerBase::spawn`] adds the generic ones and refuses the start on any
//!   violation, naming every variable involved;
//! - [`RunnerBase::spawn`] / [`RunnerBase::spawn_probe_server`] /
//!   [`RunnerBase::spawn_gauge_sampler`] / [`RunnerBase::run_until_shutdown`]
//!   — core spawn, the `/metrics`+`/ready`+`/healthz` probe, the memory
//!   sampler, and the stop handling (SIGTERM or the worker's own withdrawal
//!   drains; its supersession or a loop task ending exits non-zero);
//! - the shared helpers ([`env_or`], [`env_switch`], [`resolve`],
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
//! // + your composed ServiceDefs and your own relations
//! let core = base.spawn(deps, Vec::new(), b2bua_runner_kit::ConfigCheck::new())?;
//! let _probe = base.spawn_probe_server(&core, None, None).await;
//! base.spawn_gauge_sampler(&core);
//! Ok(base.run_until_shutdown(&core).await)
//! # }
//! ```
//!
//! What deliberately does NOT live here: the decision engine (the one piece a
//! runner exists to choose), the composed service list, and allocator wiring —
//! those are the runner's own, injected through the seams above. Replication is
//! the runner's choice to take: a runner may assign any other `ReplicationSetup`
//! to `deps.replication`. The CDR sink is the runner's choice too: the kit ships
//! the RabbitMQ sink ([`RabbitMqCdrSettings`] holds its env grammar), the
//! file sink and the discarding default; a runner may give the RabbitMQ sink its own
//! [`CdrEncoder`] or pass any other [`CdrWriter`] to [`RunnerBase::deps`].

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
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use b2bua::admission::Refusals;
use b2bua::cdr::{BufferedCdrWriter, CdrEncoder, CdrWriter};
use b2bua::config::{B2buaConfig, Front};
use b2bua::decision::CallDecisionEngine;
use b2bua::ingress_brake::{build_ingress_brake_hook, IngressBrakeConfig, IngressBrakeCounters};
use b2bua::limiter::CallLimiter;
use b2bua::metrics::{B2buaMetrics, LiveGauge, UdpTransportMetrics};
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps, ReplicationSetup};
use http_net::{RealHttpNetwork, SystemResolver};
use sip_clock::wall::millis_u64;
use sip_clock::Clock;
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork, UdpEndpoint};

pub mod backend_url;
mod base;
mod capacity_env;
mod capture_env;
mod cdr_file;
mod cdr_rabbitmq;
mod cdr_sink;
mod check;
mod drain_env;
mod entropy;
mod env_read;
mod front_env;
mod limiter_client;
mod limiter_env;
mod liveness_env;
mod null_cdr;
pub mod poison;
pub mod probe;
mod replication;
mod runner_env;
pub mod runtime;
mod shutdown;
mod socket_buffers;
mod spawn;
mod validate;
mod worker_metrics;
pub use base::RunnerBase;
pub use cdr_file::{file_cdr_writer_from_lookup, FileCdrWriter, CDR_FILE_VARIABLE};
pub use cdr_rabbitmq::{
    rabbitmq_cdr_writer_from_lookup, rabbitmq_cdr_writer_from_lookup_with_encoder,
    CdrDeliveryBounds, CdrQueueDeclare, RabbitMqCdrSettings, RabbitMqCdrWriter,
    MAX_DRAINER_WAIT_MS, MAX_WAIT_MS, MAX_WINDOW,
};
pub use check::ConfigCheck;
pub use env_read::{env_stated, split_host_port, stated, stated_list};
pub use front_env::{addresses_from_lookup, WorkerAddresses};
pub use null_cdr::NullCdrWriter;
pub use replication::{replication_setup_from_lookup, ReplicationSettings};
pub use runner_env::{switch, RunnerEnv};
use socket_buffers::log_socket_buffers;
pub use validate::{validate_default_dest, validate_ingress_brake_pct};
pub use worker_metrics::{txn_metrics_text, WorkerMetrics, CATALOGUE};

/// How often the gauge sampler publishes.
const GAUGE_PERIOD: Duration = Duration::from_secs(5);

/// Env var with a default when unset.
pub fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// The on/off env variable `key`: `1`/`true`/`yes`/`on` or
/// `0`/`false`/`no`/`off` (case-insensitive), `default` when unset or blank.
/// Any other value is the boot refusal naming `key`: a typo never reads as
/// off.
pub fn env_switch(key: &str, default: bool) -> Result<bool, String> {
    runner_env::switch(key, env::var(key).ok(), default)
}

/// Resolve a `host:port` string to a socket address; `Err` is the boot
/// refusal of one that does not resolve.
pub fn resolve(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("cannot resolve {addr:?}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address resolved from {addr:?}"))
}

impl RunnerEnv {
    /// Bind the real UDP endpoint (ingress brake installed), coerce the
    /// advertise address, assemble the [`B2buaConfig`], and mint the
    /// process-wide metrics registry / system clock. `Err` is the boot refusal
    /// of a bind failure, a pre-epoch wall clock or an OS RNG that does not
    /// answer. `name` names the service in every log line. Nothing is served
    /// yet: the configuration is checked once composed, by
    /// [`RunnerBase::spawn`].
    ///
    /// The runner installs the process subscriber (ADR-0026,
    /// [`observe::init_production`]) before it, so the refusal is logged.
    pub async fn bind(self, name: &str) -> Result<RunnerBase, String> {
        // The per-call trace gate, configured once from the environment before
        // any call arrives (ADR-0026): with no OTLP endpoint it is inert and no
        // call path pays more than one boolean check.
        b2bua::trace::install_process_traces(std::sync::Arc::new(
            b2bua::trace::CallTraces::from_env(0),
        ));
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
        let brake_hook = build_ingress_brake_hook(
            brake_config,
            brake_counters.clone(),
            refusals.clone(),
            self.addresses.front.parser(&self.parser_limits),
        );

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
        let bound = net.bind_udp(bind_opts).await;
        let bound = bound.map_err(|e| format!("bind {listen_sa} failed: {e:?}"))?;
        let endpoint = capture_env::captured(bound.into(), self.capture.as_deref(), name)?;
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
                {
                    let endpoint = endpoint.clone();
                    Arc::new(move || endpoint.counters())
                },
            )
        };
        tracing::info!(
            service = name,
            threshold = brake_config.threshold(),
            queue_max = self.queue_max,
            ingress_brake_pct = self.udp_ingress_brake_pct,
            "ingress brake armed: stateless-503 for new non-emergency INVITEs at ingress depth >= threshold"
        );

        // The worker's own address on every Via; behind the proxy, every
        // Contact names the proxy instead ([`front_env`]).
        let (advertise_ip, advertise_port) =
            (self.addresses.advertise.ip().to_string(), self.addresses.advertise.port());
        tracing::info!(
            service = name,
            %advertise_ip,
            advertise_port,
            bind = %local,
            "advertised SIP identity"
        );
        match &self.addresses.front {
            // Every b-leg request goes to the front proxy with a preloaded plain `;lr`
            // Route; it classifies it worker-outbound (ADR-0048) and record-routes.
            Front::Proxied { outbound_proxy: (h, p), contact_address: (ch, cp) } => tracing::info!(
                service = name,
                proxy_host = %h,
                proxy_port = p,
                contact_host = %ch,
                contact_port = cp,
                "behind the front proxy: b-leg egress through it, every Contact names it"
            ),
            Front::Standalone => tracing::info!(
                service = name,
                "standalone: b-leg goes straight to the callee, every Contact names the worker"
            ),
        }
        let identity = entropy::worker_identity(name, self.identity.clone(), &id_gen);
        let config = B2buaConfig {
            self_ordinal: self.ordinal.clone(),
            cluster_key: self.cluster_key.clone(),
            sip_local_ip: advertise_ip,
            sip_local_port: advertise_port,
            front: self.addresses.front.clone(),
            cdr_buffer_queue_max: self.cdr_queue,
            event_dispatch_concurrency: self.concurrency,
            per_call_queue_cap: self.call_cap,
            new_call_permit_share_percent: self.new_call_permit_share_percent,
            new_call_queue_headroom_percent: self.new_call_queue_headroom_percent,
            max_messages_per_call: self.max_messages_per_call,
            max_messages_per_call_lifetime: self.max_messages_per_call_lifetime,
            keepalive_interval_sec: self.keepalive_sec,
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
            cdr: self.cdr.clone(),
            capacity: self.capacity,
            caps: self.caps,
            event_queue_gate: self.event_queue_gate,
            parser_limits: self.parser_limits.clone(),
            ..Default::default()
        };
        tracing::info!(service = name, capacity = ?config.capacity, "capacity ceilings");

        Ok(RunnerBase {
            name: name.to_string(),
            endpoint,
            local,
            config,
            identity,
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

    /// The peer-to-peer replication the env selected ([`RunnerEnv::replication`]),
    /// its store on [`Self::clock`]; `None` when replication is off. `Err` is
    /// the boot refusal of a replication listener that cannot be bound.
    pub async fn replication_setup_from_env(&self) -> Result<Option<ReplicationSetup>, String> {
        let Some(settings) = self.env.replication.clone() else { return Ok(None) };
        let setup = settings.into_setup(self.clock.clone(), replication::boot_incarnation());
        let setup = setup.await.map_err(|e| format!("invalid B2BUA config: {e}"))?;
        Ok(Some(ReplicationSetup { replica: self.env.replica, ..setup }))
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
            identity: self.identity.clone(),
            decision,
            limiter: self.limiter_from_env().await,
            cdr: Arc::new(BufferedCdrWriter::spawn(sink, self.env.cdr_queue, self.metrics.clone())),
            store: Arc::new(InMemoryCallStore::new()),
            // No injected fault and no turn probe in production (ADR-0023).
            store_faults: Default::default(),
            wire_faults: Default::default(),
            turn_probe: Default::default(),
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
