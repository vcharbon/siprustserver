//! Test-only harness: binds a real [`b2bua::B2buaCore`] as a System-Under-Test
//! on the `scenario-harness` simulated network, so deterministic
//! alice ↔ b2bua ↔ bob flows run end-to-end through the recording. Extends the
//! `bind_sut` seam (ADR-0006/0009) to the B2BUA (ADR-0010).

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::cdr::{CdrRecord, InMemoryCdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::{CallDecisionEngine, ScriptedDecisionEngine};
use b2bua::limiter::{CallLimiter, NoopLimiter};
use b2bua::metrics::B2buaMetrics;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps, ReplicationSetup};
use scenario_harness::{Agent, Dialog, Harness};
use sip_clock::Clock;
use sip_net::UdpEndpoint;
use sip_txn::IdGen;

// ===========================================================================
// Shared b2bua spawn primitive
// ===========================================================================
//
// The bind→config→deps→spawn wiring for a `b2bua::B2buaCore` is identical
// between the single-SUT harness here (`B2buaSut::start_inner`) and the
// multi-node `failover-harness` (`ReplicatedB2buaSut::spawn_core`): the only
// things that vary are the `replication` value, the `clock`, the `id_gen`
// seed, the config tuning, and the `services` vec. Both bind their own
// endpoint (this crate uses `bind_sut_with_roles`, failover binds via its
// harness handle); the SHARED part is everything from "build the config" to
// "spawn the core", captured by [`B2buaSpawnParams`] + [`spawn_b2bua_core`].
// `failover-harness` reuses these (it depends on this crate).

/// Everything that varies between a single-SUT and a replicated b2bua spawn.
/// The caller binds its own endpoint and passes it (plus these params) to
/// [`spawn_b2bua_core`].
pub struct B2buaSpawnParams {
    /// `self_ordinal` (e.g. "w0" single-SUT, the worker ordinal in a cluster).
    pub ordinal: String,
    /// This worker's SIP wire address (drives `sip_local_ip`/`sip_local_port`).
    pub sip_addr: SocketAddr,
    pub decision: Arc<dyn CallDecisionEngine>,
    pub limiter: Arc<dyn CallLimiter>,
    /// Callflow services to register (ADR-0016). Empty for the plain path.
    pub services: Vec<b2bua::rules::ServiceDef>,
    pub outbound_proxy: Option<(String, u16)>,
    /// Already-built replication setup (`None` → non-replicating). The caller
    /// constructs this — keeping repl-net/topology deps out of this crate.
    pub replication: Option<ReplicationSetup>,
    pub clock: Clock,
    pub id_gen: Arc<IdGen>,
    /// CDR writer; cloned into the deps (the caller keeps its own handle to
    /// snapshot records).
    pub cdr: InMemoryCdrWriter,
}

/// Builds the base [`B2buaConfig`] (ip/port/ordinal/outbound_proxy wired),
/// applies `tune`, assembles [`B2buaDeps`], and spawns the core on the
/// already-bound `endpoint`. The single home for the spawn wiring that
/// `B2buaSut` and `failover-harness`'s `ReplicatedB2buaSut` share.
///
/// The legacy `store` slot is a throwaway [`InMemoryCallStore`]: on the
/// replicating path the repl store is the drain target (the legacy slot is
/// unused), and on the non-replicating path each call site already passed a
/// fresh in-memory store. `services` is honoured via `spawn_with_services`
/// (`spawn` is just `spawn_with_services(.., vec![])`, so an empty vec is
/// behaviour-identical to the old `spawn` path).
pub fn spawn_b2bua_core(
    endpoint: Box<dyn UdpEndpoint>,
    params: B2buaSpawnParams,
    tune: impl FnOnce(&mut B2buaConfig),
) -> B2buaCore {
    let B2buaSpawnParams {
        ordinal,
        sip_addr,
        decision,
        limiter,
        services,
        outbound_proxy,
        replication,
        clock,
        id_gen,
        cdr,
    } = params;
    let mut config = B2buaConfig {
        self_ordinal: ordinal,
        sip_local_ip: sip_addr.ip().to_string(),
        sip_local_port: sip_addr.port(),
        b2b_outbound_proxy: outbound_proxy,
        ..Default::default()
    };
    tune(&mut config);
    let deps = B2buaDeps {
        config,
        decision,
        limiter,
        cdr: Arc::new(cdr),
        store: Arc::new(InMemoryCallStore::new()),
        clock,
        id_gen,
        replication,
        metrics: B2buaMetrics::new(),
    };
    B2buaCore::spawn_with_services(endpoint, deps, services)
}

// The multi-node HA failover harness (FailoverHarness / ReplicatedB2buaSut /
// ProxySut) moved to the dedicated `failover-harness` crate (ADR-0013 §0). This
// crate is the single-SUT b2bua harness.

/// Poll `cond` until it holds, yielding so spawned teardown / CDR-writer tasks
/// drain. The single home for the "wait for the async teardown to land" loop
/// every e2e test used to hand-roll.
///
/// Works under both wall-clock (`#[tokio::test]`) and `start_paused` tests:
/// under a paused runtime the `sleep` auto-advances virtual time, so the
/// awaited spawned tasks still get to run. Bounded (200 × 5 ms) so a real
/// regression — `cond` never holds — falls through to the test's assertions
/// instead of hanging.
///
/// For tests that must *advance the clock to trip a timer* (not merely drain
/// already-due tasks), advance with `Harness::advance` first, then settle here.
pub async fn settle_until(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Canonical minimal SDP offer used by [`establish_call`]. A single audio
/// `m=` line — enough for the B2BUA to relay; tests that don't probe media
/// don't care about its contents.
pub const OFFER_SDP: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
/// Canonical minimal SDP answer used by [`establish_call`]. See [`OFFER_SDP`].
pub const ANSWER_SDP: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Establish a confirmed dialog through the B2BUA and return alice's `Dialog`
/// so the caller can drive the in-dialog phase (BYE, re-INVITE, keepalive, …).
///
/// Runs the canonical happy-path handshake — alice INVITEs (with [`OFFER_SDP`])
/// through `b2bua_addr`; bob rings (180) then answers (200 with [`ANSWER_SDP`]);
/// the ACK is relayed end-to-end — the ~8 lines every "set the call up, then
/// test the interesting part" test used to re-type.
///
/// Opt-in: a test that asserts on the intermediate 18x, on the relayed SDP
/// bodies, or that injects a non-2xx final response should keep driving the
/// handshake by hand — this fixture is for tests whose subject is what happens
/// *after* the call is up.
pub async fn establish_call(alice: &Agent, bob: &Agent, b2bua_addr: SocketAddr) -> Dialog {
    let mut call = alice.invite(bob).with_sdp(OFFER_SDP).through(b2bua_addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    dialog
}

/// A running B2BUA bound on the harness fabric. Keep it alive for the duration
/// of the scenario (drop tears the worker tasks down with the endpoint).
pub struct B2buaSut {
    pub addr: SocketAddr,
    cdr: InMemoryCdrWriter,
    metrics: B2buaMetrics,
    _core: B2buaCore,
}

impl B2buaSut {
    /// Bind a B2BUA at `addr` driven by `decision`.
    pub async fn start(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
    ) -> Self {
        Self::start_with_outbound_proxy(h, name, addr, decision, None).await
    }

    /// Bind a B2BUA at `addr` driven by `decision`, optionally deployed behind
    /// the front proxy: when `outbound_proxy` is `Some((host, port))`, every
    /// b-leg outbound request traverses that proxy (see
    /// [`B2buaConfig::b2b_outbound_proxy`]).
    pub async fn start_with_outbound_proxy(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        outbound_proxy: Option<(String, u16)>,
    ) -> Self {
        Self::start_with_config(h, name, addr, decision, outbound_proxy, |_| {}).await
    }

    /// Bind a B2BUA with a config mutator hook. The base config is the default
    /// (with `self_ordinal`/local IP+port/outbound-proxy wired); `tune` may
    /// override any other field (the faithful equivalent of a per-scenario
    /// `configOverrides` — the source stack applies them worker-wide too).
    pub async fn start_with_config(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        outbound_proxy: Option<(String, u16)>,
        tune: impl FnOnce(&mut B2buaConfig),
    ) -> Self {
        Self::start_inner(h, name, addr, decision, outbound_proxy, Arc::new(NoopLimiter), Vec::new(), tune).await
    }

    /// Bind a B2BUA with a set of registered callflow services (ADR-0016): each
    /// service's `init` runs at setup and its state-gated rules compose above the
    /// core defaults. Used by the out-of-tree `announcement` capstone (slice 8) —
    /// the service is injected here, so `b2bua` never depends on it.
    pub async fn start_with_services(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        services: Vec<b2bua::rules::ServiceDef>,
    ) -> Self {
        Self::start_inner(h, name, addr, decision, None, Arc::new(NoopLimiter), services, |_| {}).await
    }

    /// [`start_with_services`](Self::start_with_services) plus the config
    /// mutator hook (the reaper scenarios register a panicking probe rule AND
    /// tune the reaper cadence).
    pub async fn start_with_services_and_config(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        services: Vec<b2bua::rules::ServiceDef>,
        tune: impl FnOnce(&mut B2buaConfig),
    ) -> Self {
        Self::start_inner(h, name, addr, decision, None, Arc::new(NoopLimiter), services, tune).await
    }

    /// Bind a B2BUA with a custom [`CallLimiter`] (e.g. an `HttpCallLimiter` over
    /// the simulated HTTP fabric serving a real `LimiterServer`). No outbound
    /// proxy; `tune` may override config (e.g. `limiter_refresh_sec`,
    /// `self_ordinal` for the cross-worker scenario).
    pub async fn start_with_limiter(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        limiter: Arc<dyn CallLimiter>,
        tune: impl FnOnce(&mut B2buaConfig),
    ) -> Self {
        Self::start_inner(h, name, addr, decision, None, limiter, Vec::new(), tune).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_inner(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        outbound_proxy: Option<(String, u16)>,
        limiter: Arc<dyn CallLimiter>,
        services: Vec<b2bua::rules::ServiceDef>,
        tune: impl FnOnce(&mut B2buaConfig),
    ) -> Self {
        // The B2BUA terminates each leg as a UA (UAS on the a-leg, UAC on the
        // b-leg) — it is NOT an RFC 3261 §16 proxy, so its bind declares
        // `{Uac, Uas}` and the proxy-subject audit rules (no-target-404,
        // 100-within-200ms, unmatched-PRACK forwarding, strict-route rewrite)
        // do not judge this lane.
        let (endpoint, sa) = h
            .bind_sut_with_roles(
                name,
                addr,
                std::collections::HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]),
            )
            .await;
        let cdr = InMemoryCdrWriter::new();
        let params = B2buaSpawnParams {
            ordinal: "w0".into(),
            sip_addr: sa,
            decision,
            limiter,
            services,
            outbound_proxy,
            replication: None,
            clock: Clock::test_at(0),
            id_gen: Arc::new(IdGen::seeded(0xB2B0)),
            cdr: cdr.clone(),
        };
        let core = spawn_b2bua_core(endpoint, params, |config| {
            // Production default is 300 s (5 min); the paused-clock keepalive
            // tests advance in 30 s steps, so the harness baseline stays at 30 s.
            // A scenario can still override via `tune`.
            config.keepalive_interval_sec = 30;
            // Production default is now 32 s; the paused-clock keepalive-timeout
            // tests advance a fixed 5 s after the probe, so the harness pins the
            // old 5 s grace to keep those steps valid (a scenario can `tune` it).
            config.keepalive_timeout_sec = 5;
            // The caller's tune runs LAST so it can still override the keepalive
            // defaults above (preserving the prior ordering).
            tune(config);
        });
        let metrics = core.metrics().clone();
        Self {
            addr: sa,
            cdr,
            metrics,
            _core: core,
        }
    }

    /// Bind a B2BUA that routes every call to `dest` (the common case).
    pub async fn route_all_to(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
    ) -> Self {
        let decision = Arc::new(ScriptedDecisionEngine::route_all_to(dest_host, dest_port));
        Self::start(h, name, addr, decision).await
    }

    /// Bind a B2BUA that routes every call to `dest` and authorizes REFER
    /// transfers via the default `X-Api-Call`-keyed `/call/refer` behavior (the
    /// REFER-scenario constructor).
    pub async fn route_all_with_refer(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
    ) -> Self {
        let decision = Arc::new(ScriptedDecisionEngine::route_all_with_refer(dest_host, dest_port));
        Self::start(h, name, addr, decision).await
    }

    /// Like [`route_all_with_refer`](Self::route_all_with_refer) but with the
    /// REFER realignment / overall-safety timers overridden (the per-scenario
    /// `configOverrides` the `refer-timers` corpus uses: push
    /// `refer_reinvite_answer` out past `refer_overall_safety` so the overall
    /// watchdog trips first while a realign re-INVITE is stuck unanswered).
    pub async fn route_all_with_refer_timers(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
        refer_reinvite_answer_sec: i64,
        refer_overall_safety_sec: i64,
    ) -> Self {
        let decision = Arc::new(ScriptedDecisionEngine::route_all_with_refer(dest_host, dest_port));
        Self::start_with_config(h, name, addr, decision, None, move |c| {
            c.refer_reinvite_answer_sec = refer_reinvite_answer_sec;
            c.refer_overall_safety_sec = refer_overall_safety_sec;
        })
        .await
    }

    /// Bind a B2BUA that routes every call to `dest` with the
    /// `relayFirst18xTo180` feature active under `strategy` (suppress / fake-prack).
    pub async fn route_all_to_with_18x(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
        strategy: call::features::RelayFirst18xStrategy,
    ) -> Self {
        let dest = (dest_host.to_string(), dest_port);
        let decision = Arc::new(
            b2bua::decision::ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    b2bua::decision::NewCallResponse::Route(
                        b2bua::decision::test_adapter::route_to_with_18x(
                            &dest.0, dest.1, strategy,
                        ),
                    )
                })
                .build(),
        );
        Self::start(h, name, addr, decision).await
    }

    /// Bind a B2BUA that routes the call to `dest_port` (bob1) with the
    /// `relayFirst18xTo180` feature active under `strategy` and a `callback_context`
    /// set (the failover-capable marker), and fails over via `/call/failure` to
    /// `failover_port` (bob2) with `failover_ruri` as the new Request-URI. Mirrors
    /// the TS `on_failure: { action: "failover", destination, new_ruri }` instruction.
    #[allow(clippy::too_many_arguments)]
    pub async fn route_all_to_with_18x_failover(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
        failover_port: u16,
        failover_ruri: &str,
        strategy: call::features::RelayFirst18xStrategy,
    ) -> Self {
        use b2bua::decision::{CallFailureResponse, NewCallResponse};
        use b2bua::decision::test_adapter::route_to_with_18x;
        let primary = (dest_host.to_string(), dest_port);
        let failover = (dest_host.to_string(), failover_port);
        let failover_ruri = failover_ruri.to_string();
        let decision = Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    let mut r = route_to_with_18x(&primary.0, primary.1, strategy);
                    r.callback_context = Some("failover-test".into());
                    // The TS relies on the 30 s platform no-answer default the Rust
                    // default route omits — set it so the no-answer failover fires.
                    r.no_answer_timeout_sec = Some(30);
                    NewCallResponse::Route(r)
                })
                .on_failure(move |_req| {
                    let mut r = route_to_with_18x(&failover.0, failover.1, strategy);
                    r.new_ruri = Some(failover_ruri.clone());
                    CallFailureResponse::Route(r)
                })
                .build(),
        );
        Self::start(h, name, addr, decision).await
    }

    /// Bind a B2BUA that routes every call to `dest` but sends its b-leg
    /// (worker→callee) traffic through the front proxy at `proxy` — the
    /// `alice → proxy → b2bua → proxy → bob` topology.
    pub async fn route_all_to_via_proxy(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
        proxy_host: &str,
        proxy_port: u16,
    ) -> Self {
        let decision = Arc::new(ScriptedDecisionEngine::route_all_to(dest_host, dest_port));
        Self::start_with_outbound_proxy(
            h,
            name,
            addr,
            decision,
            Some((proxy_host.to_string(), proxy_port)),
        )
        .await
    }

    pub fn cdr_records(&self) -> Vec<CdrRecord> {
        self.cdr.snapshot()
    }

    pub fn metrics(&self) -> &B2buaMetrics {
        &self.metrics
    }

    /// Ground-truth live call-map size (`inner.calls.len()`). An orphan-reject
    /// (481) does NOT inflate this — it never inserts a call — so it is the wrong
    /// lens for the orphan leak; use [`lock_count`](Self::lock_count) +
    /// `metrics().creations_total()/removals_total()` for that.
    pub fn active_calls(&self) -> usize {
        self._core.active_calls()
    }

    /// Live per-call serialization-lock count. Should return to 0 once traffic
    /// drains; a residue is the orphan-reject lock leak (one stranded lock per
    /// in-dialog request that 481'd without tearing its per-call state down).
    pub fn lock_count(&self) -> usize {
        self._core.lock_count()
    }

    /// The named "no leak" oracle: every call created has been reaped and no
    /// per-call state survives. Asserts the three invariants the reap tests
    /// used to spell out by hand:
    ///   1. `creations_total() == removals_total()` — every call's lifecycle
    ///      closed (the `active_calls` lens misses orphan-reject leaks, which
    ///      never insert a call; the paired counters + `lock_count` catch them).
    ///   2. `active_calls() == 0` — no live call left in the map.
    ///   3. `lock_count() == 0` — no stranded per-call serialization lock.
    ///
    /// A new leak dimension (e.g. the `b2bua_timer_queue_len − b2bua_timer_live`
    /// tombstone gap from the CLAUDE.md timer hazard) is added here once and is
    /// then checked by every reap test — the locality this oracle exists for.
    ///
    /// Call it *after* the teardown has drained (see [`settle_until`]).
    #[track_caller]
    pub fn assert_fully_reaped(&self) {
        let (creations, removals) = (
            self.metrics.creations_total(),
            self.metrics.removals_total(),
        );
        assert_eq!(
            creations, removals,
            "call leak: creations ({creations}) != removals ({removals}) — a call \
             was created but never reaped"
        );
        assert_eq!(
            self.active_calls(),
            0,
            "call leak: {} live call(s) left in the map",
            self.active_calls()
        );
        assert_eq!(
            self.lock_count(),
            0,
            "lock leak: {} stranded per-call serialization lock(s)",
            self.lock_count()
        );
        // 4. (ADR-0020) the reaper's last-touched ledger mirrors the call map;
        //    a residue is a stamp leak.
        assert_eq!(
            self._core.touched_count(),
            0,
            "stamp leak: {} stranded last-touched ledger entr(ies)",
            self._core.touched_count()
        );
    }
}
