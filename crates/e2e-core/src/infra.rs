//! Infra shapes (ADR-0018): the compiled-Rust topology + clock a Callflow shape
//! runs under. An Infra shape resolves the scenario's logical roles to addresses
//! and — for a **fake** shape — spawns the SUT (LSBC LB + b2bua) in-process on
//! the simulated fabric, wired so the b-leg egresses **through the LB** (never
//! pod-direct). The *same* shape body runs over any Infra shape; only transport +
//! clock + `recv_timeout` differ.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use crate::egress::{CalleeTarget, EgressPolicy};
use b2bua::decision::test_adapter::{default_call_refer, route_to};
use b2bua::decision::{CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::B2buaSut;
use scenario_harness::{Agent, Harness, RunReport};
use sip_clock::Clock;
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::{
    LoadBalancerConfig, LoadBalancerStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics,
    RoutingStrategy,
};
use sip_txn::IdGen;
use tokio::task::JoinHandle;

/// Which end of the fake/real spectrum an Infra shape sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfraKind {
    /// Simulated fabric, paused clock, SUT (LB + b2bua) spawned in-process.
    Fake,
    /// Real sockets, wall clock, SUT is an external cluster (not spawned).
    Real,
}

/// The JSON **Endpoint config** (ADR-0018): binds one Infra shape's logical
/// roles (alice, bob1, lb, b2bua) to concrete addresses, plus the agent recv
/// bound. Addresses are infra-specific, so a config names the Infra shape it
/// is for — `build` rejects a mismatch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointConfig {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// The Infra shape these addresses bind (e.g. `fake-lsbc-b2bua`).
    pub infra_shape: String,
    pub roles: BTreeMap<String, SocketAddr>,
    /// Per-`recv` wait bound handed to every agent (real clock needs a wide one).
    pub recv_timeout_ms: u64,
    /// One-hop simulated transit delay (fake only; coerced to ≥1ms).
    #[serde(default)]
    pub transit_delay_ms: u64,
}

impl EndpointConfig {
    pub fn addr(&self, role: &str) -> SocketAddr {
        *self
            .roles
            .get(role)
            .unwrap_or_else(|| panic!("endpoint config is missing role {role:?}"))
    }

    pub fn recv_timeout(&self) -> Duration {
        Duration::from_millis(self.recv_timeout_ms)
    }

    /// The RTP address for a media-exchanging agent: an explicit
    /// `"<role>.rtp"` role wins; otherwise the agent's signaling IP with
    /// `default_port` (fine on the simulated fabric; real configs should pin
    /// `<role>.rtp` to avoid port clashes).
    pub fn media_addr(&self, role: &str, default_port: u16) -> SocketAddr {
        self.roles
            .get(&format!("{role}.rtp"))
            .copied()
            .unwrap_or_else(|| SocketAddr::new(self.addr(role).ip(), default_port))
    }

    /// Fail loudly when a config authored for one infra is handed to another.
    fn assert_binds(&self, infra_id: &str) {
        assert_eq!(
            self.infra_shape, infra_id,
            "endpoint config is for infra {:?}, not {infra_id:?}",
            self.infra_shape
        );
    }
}

/// A live running Infra shape handed to a Callflow shape: the recording harness,
/// the scenario-driven Test Agents by logical name, and the SUT ingress (LB VIP)
/// — the single address agents send to. The SUT guards keep the in-process
/// LB/b2bua tasks alive for a fake shape; they are `None` for a real one.
pub struct InfraRuntime {
    harness: Harness,
    pub agents: BTreeMap<String, Agent>,
    /// The single SUT ingress agents address (the LB VIP).
    pub sut_ingress: SocketAddr,
    /// The LB VIP (== `sut_ingress`); exposed for `${infra.lbVip}` checks later.
    pub lb_vip: SocketAddr,
    /// The Endpoint config this runtime was built from (media addrs etc.).
    pub cfg: EndpointConfig,
    /// How THIS layout realizes a logical INVITE on its wire (ADR-0018, the
    /// generic egress rewrite). The real cluster pins the b-leg via the
    /// proprietary `X-Api-Call` header ([`EgressPolicy::ApiCallPin`]); the
    /// register front proxy rewrites the R-URI to the registered AOR
    /// ([`EgressPolicy::RegistrarAor`]); the fake LB+b2bua and direct-peer infras
    /// are [`EgressPolicy::Transparent`]. Shapes never branch on it — they call
    /// [`outgoing_invite`](Self::outgoing_invite), which consults this.
    egress: EgressPolicy,
    /// The RAW (un-recorded) network — media endpoints ride the same fabric as
    /// the signaling but BELOW the SIP recording/audit decorators (RTP bytes
    /// must not enter the SIP trace or the RFC rule engine).
    raw_net: Arc<dyn sip_net::SignalingNetwork>,
    /// Audio a media-exchanging shape captured, folded into the result later.
    media: std::cell::RefCell<Vec<crate::media::MediaCapture>>,
    _proxy: Option<ProxyGuard>,
    _b2bua: Option<B2buaSut>,
    /// The in-process register front proxy (the `fake-register-proxy` shape);
    /// `None` for every other infra. Aborts its recv loop on drop.
    _register_proxy: Option<crate::registrar::RegisterProxyGuard>,
}

impl InfraRuntime {
    pub fn agent(&self, role: &str) -> &Agent {
        self.agents
            .get(role)
            .unwrap_or_else(|| panic!("infra has no agent for role {role:?}"))
    }

    /// Label a message `role` just received with a canonical [`Anchor`]
    /// (ADR-0019) — e.g. `rt.anchor("bob1", Anchor::InitialInvite,
    /// uas.request())`. Surfaced on the [`RunReport`] for the check engine.
    pub fn anchor(
        &self,
        role: &str,
        anchor: crate::shape::Anchor,
        keys: impl Into<scenario_harness::AnchorKeys>,
    ) {
        self.harness.tag_anchor(self.agent(role), anchor.as_str(), keys);
    }

    /// The raw network for media endpoints (same fabric, below the SIP
    /// recording decorators).
    pub fn raw_network(&self) -> Arc<dyn sip_net::SignalingNetwork> {
        self.raw_net.clone()
    }

    /// A media-exchanging shape deposits what each agent received here; the
    /// run executor folds it into `.wav` artifacts + "hears" check verdicts.
    pub fn push_media(&self, capture: crate::media::MediaCapture) {
        self.media.borrow_mut().push(capture);
    }

    /// Drain the media captures (call before [`finish`](Self::finish)).
    pub fn take_media(&self) -> Vec<crate::media::MediaCapture> {
        std::mem::take(&mut *self.media.borrow_mut())
    }

    /// Resolve a logical callee **role** (e.g. `"bob1"`, `"bob2"`) to how THIS
    /// layout addresses it — the one generic primitive every shape uses for ANY
    /// callee: the a-leg INVITE target, a reroute candidate, a REFER transfer
    /// target. So a shape never hard-codes `cfg.addr("bob2")` or an AOR. Returns
    /// the topology-correct URI (registered AOR / `sip:<role>@<addr>`) and the
    /// callee's wire address (for an `X-Api-Call` destination pin).
    pub fn callee(&self, role: &str) -> CalleeTarget {
        let addr = self.cfg.addr(role);
        CalleeTarget { role: role.to_string(), uri: self.egress.callee_uri(role, addr), addr }
    }

    /// Realize a Callflow shape's logical INVITE on THIS layout's wire — the
    /// single seam that replaces every shape's hand-coded `api_call_destination`
    /// + From/To/R-URI block. `callees` is the ordered candidate list: the
    /// **primary** first, then any **failover** targets (the rerouting shape
    /// passes `["bob1", "bob2"]`; a plain call passes `["bob1"]`). It (1) routes
    /// through the SUT ingress, (2) applies the Test case's `core` From/To/R-URI
    /// (the author's logical intent), then (3) applies the layout's egress rewrite
    /// (the topology's wire reality), which has the final say — a register
    /// layout's AOR R-URI supersedes a topology-agnostic authored one, and a
    /// pinned layout turns several candidates into an `X-Api-Call` failover plan.
    pub fn outgoing_invite<'a>(
        &self,
        callees: &[&str],
        input: &crate::model::Input,
        invite: scenario_harness::Invite<'a>,
    ) -> scenario_harness::Invite<'a> {
        let mut invite = invite.through(self.sut_ingress);
        if let Some(from) = &input.core.from {
            invite = invite.from(from);
        }
        if let Some(to) = &input.core.to {
            invite = invite.to(to);
        }
        if let Some(ruri) = &input.core.ruri {
            invite = invite.ruri(ruri);
        }
        let targets: Vec<CalleeTarget> = callees.iter().map(|r| self.callee(r)).collect();
        self.egress.rewrite_for(&targets).apply(invite)
    }

    /// Close the recording and return the report plus the RFC hard-gate
    /// findings (non-advisory, subject-applicable, unwaived). The executor
    /// folds non-empty findings into a FAILED cell **with the report intact**
    /// — a gating RFC violation must fail the cell, but crashing it would
    /// throw away the diagram and findings table a human needs to see why.
    /// Keeps the SUT guards alive across `finish()` (the recording snapshot is
    /// read first), then drops them.
    pub async fn finish(self) -> (RunReport, Vec<sip_net::RfcFinding>) {
        // Drain already-due in-flight deliveries (SUT teardown, final 200s, CDR)
        // before the snapshot — the generic analogue of b2bua-harness
        // `settle_until`. Under a paused clock each yield auto-advances virtual
        // time; under a real clock it is a short real drain. Bounded well under
        // any keepalive interval so it never trips a timer.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let InfraRuntime {
            harness,
            agents,
            _proxy,
            _b2bua,
            _register_proxy,
            ..
        } = self;
        let (report, gate) = harness.finish_collecting().await;
        drop(agents);
        drop(_proxy);
        drop(_b2bua);
        drop(_register_proxy);
        (report, gate)
    }
}

/// A compiled Infra shape — builds an [`InfraRuntime`] for a given Endpoint config.
#[async_trait(?Send)]
pub trait InfraShape {
    fn id(&self) -> &str;
    fn kind(&self) -> InfraKind;
    async fn build(&self, scenario_name: &str, cfg: &EndpointConfig) -> InfraRuntime;
}

/// The compiled Infra-shape registry, as a by-id factory: `InfraShape` is
/// `!Send` (it builds a `Harness`), so the run executor resolves the id
/// *inside* each cell's thread instead of passing trait objects across.
pub fn by_id(id: &str) -> Option<Box<dyn InfraShape>> {
    match id {
        "fake-lsbc-b2bua" => Some(Box::new(FakeLsbcB2bua)),
        "fake-register-proxy" => Some(Box::new(FakeRegisterProxy)),
        "real-loopback-direct" => Some(Box::new(RealLoopbackDirect)),
        "real" => Some(Box::new(RealKindLb)),
        _ => None,
    }
}

/// Every registered Infra-shape id (for precise unknown-id errors).
pub fn known_ids() -> Vec<&'static str> {
    vec![
        "fake-lsbc-b2bua",
        "fake-register-proxy",
        "real-loopback-direct",
        "real",
    ]
}

/// The **fake** infra: alice / bob1 / bob2 on the simulated fabric under a paused
/// clock, with an in-process LSBC LB (a real `ProxyCore`) fronting one in-process
/// b2bua worker. The b2bua's b-leg egresses through the LB (`b2b_outbound_proxy`
/// = LB VIP) — topology identical to real, so a portable scenario exercises the
/// same path both ways.
pub struct FakeLsbcB2bua;

#[async_trait(?Send)]
impl InfraShape for FakeLsbcB2bua {
    fn id(&self) -> &str {
        "fake-lsbc-b2bua"
    }
    fn kind(&self) -> InfraKind {
        InfraKind::Fake
    }

    async fn build(&self, scenario_name: &str, cfg: &EndpointConfig) -> InfraRuntime {
        cfg.assert_binds(self.id());
        // Same seam as the real infra (`with_network_and_clock`), simulated
        // fabric + paused test clock — so the config's `recv_timeout_ms` is
        // honoured here too (0-transit coercion to ≥1ms per the sim hazard).
        let net: Arc<dyn sip_net::SignalingNetwork> = Arc::new(
            sip_net::SimulatedSignalingNetwork::new(cfg.transit_delay_ms.max(1)),
        );
        let h = Harness::with_network_and_clock(
            scenario_name.to_string(),
            net.clone(),
            Clock::test_at(0),
            layer_harness::TransportKind::Fake,
            cfg.recv_timeout(),
        );

        let mut agents = BTreeMap::new();
        for role in ["alice", "bob1", "bob2"] {
            if let Some(addr) = cfg.roles.get(role) {
                agents.insert(role.to_string(), h.agent(role, &addr.to_string()).await);
            }
        }

        let lb = cfg.addr("lb");
        let b2bua_addr = cfg.addr("b2bua");
        // Route every call to bob1, but send the b-leg through the LB (never
        // pod-direct to bob1) — the production invariant. When the config
        // binds a bob2, the engine is failover-capable: a bob1 rejection makes
        // the b2bua re-target bob2 (rejection-driven, ADR-0017 — no timer, so
        // rerouting shapes stay advance-free).
        let bob1 = cfg.addr("bob1");
        let bob2 = cfg.roles.get("bob2").copied();

        let proxy = spawn_lb_proxy(&h, lb, "b2bua", b2bua_addr).await;
        let decision = ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to(&bob1.ip().to_string(), bob1.port());
                if bob2.is_some() {
                    r.callback_context = Some("reroute:bob2".into());
                }
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| match (req.callback_context.as_deref(), bob2) {
                (Some("reroute:bob2"), Some(b2)) => {
                    let mut r = route_to(&b2.ip().to_string(), b2.port());
                    // The b-leg egresses through the LB, which forwards by
                    // R-URI — it MUST name the rerouted callee, not bob1.
                    r.new_ruri = Some(format!("sip:{}:{}", b2.ip(), b2.port()));
                    CallTreatment::Route(r)
                }
                _ => CallTreatment::Relay,
            })
            // REFER blind-transfer authorization (the `transfer-refer-media`
            // shape): the scripted `/call/refer` backend keyed on the REFER's
            // `X-Api-Call.refer_key` / `destination`. Inert for the other shapes
            // (they never REFER); composes with the failover wiring above.
            .on_refer(default_call_refer)
            .build();
        let b2bua = B2buaSut::builder(Arc::new(decision))
            .outbound_proxy(&lb.ip().to_string(), lb.port())
            .start(&h, "b2bua", &b2bua_addr.to_string())
            .await;

        InfraRuntime {
            harness: h,
            agents,
            sut_ingress: lb,
            lb_vip: lb,
            cfg: cfg.clone(),
            // The scripted decision engine already routes to bob1; the logical
            // INVITE is the wire INVITE.
            egress: EgressPolicy::Transparent,
            raw_net: net,
            media: Default::default(),
            _proxy: Some(proxy),
            _b2bua: Some(b2bua),
            _register_proxy: None,
        }
    }
}

/// The **fake register-proxy** infra (`fake-register-proxy`): alice / bob1 on the
/// simulated fabric under a paused clock, fronted by an in-process **register
/// front proxy** ([`crate::registrar`]) that faithfully mimics sipjs's
/// `sip-front-proxy/` registrar mode — UAs REGISTER an AOR→Contact binding and
/// the proxy routes an inbound INVITE by the registered AOR (the Request-URI
/// userpart), with **no** `X-Api-Call` pin.
///
/// Unlike `fake-lsbc-b2bua` (an LB + b2bua bridge whose routing is HRW + scripted
/// engine), this layout's routing is **purely binding-driven**: the proxy
/// record-routes the call and relays it straight to the registered Contact, so a
/// shape proves "this INVITE reached bob *because* bob registered" — the thing
/// the X-Api-Call pin convenience hides. `sut_ingress` is the proxy address.
///
/// Routing is a **layout property**, not shape logic (ADR-0018, the egress
/// rewrite): `build` pre-REGISTERs every bound callee's AOR, and
/// [`InfraRuntime::outgoing_invite`] rewrites alice's Request-URI to
/// `sip:<callee>@register.example` ([`EgressPolicy::RegistrarAor`]). So the SAME
/// topology-agnostic Callflow shapes that run over the LB+b2bua and real-cluster
/// infras (`basic-call`, `basic-call-media`) run here unchanged — no bespoke
/// register-* shape. The retired `register-call*` shapes' inline REGISTER + AOR
/// dial moved into this layout verbatim.
pub struct FakeRegisterProxy;

/// The AOR domain the register layout binds and dials. The userpart (the callee
/// role, e.g. `bob1`) is what the registrar keys on; the host is intentionally
/// ignored (sipjs v1 single-tenant, userpart-only AOR). A logical INVITE to
/// `bob1` is rewritten to `sip:bob1@register.example` so the proxy resolves the
/// binding bob1 registered.
const REGISTER_AOR_DOMAIN: &str = "register.example";

#[async_trait(?Send)]
impl InfraShape for FakeRegisterProxy {
    fn id(&self) -> &str {
        "fake-register-proxy"
    }
    fn kind(&self) -> InfraKind {
        InfraKind::Fake
    }

    async fn build(&self, scenario_name: &str, cfg: &EndpointConfig) -> InfraRuntime {
        cfg.assert_binds(self.id());
        let net: Arc<dyn sip_net::SignalingNetwork> = Arc::new(
            sip_net::SimulatedSignalingNetwork::new(cfg.transit_delay_ms.max(1)),
        );
        let clock = Clock::test_at(0);
        let h = Harness::with_network_and_clock(
            scenario_name.to_string(),
            net.clone(),
            clock.clone(),
            layer_harness::TransportKind::Fake,
            cfg.recv_timeout(),
        );

        let mut agents = BTreeMap::new();
        for role in ["alice", "bob1", "bob2"] {
            if let Some(addr) = cfg.roles.get(role) {
                agents.insert(role.to_string(), h.agent(role, &addr.to_string()).await);
            }
        }

        // The register front proxy — its own SUT bind, role-tagged `{Proxy}` so
        // the RFC suite judges this lane by the proxy rules.
        let proxy_addr = cfg.addr("proxy");
        let (ep, sock) = h
            .bind_sut_with_roles(
                "proxy",
                &proxy_addr.to_string(),
                std::collections::HashSet::from([sip_net::UaRole::Proxy]),
            )
            .await;
        let registrar = crate::registrar::Registrar::new(clock);
        let guard = crate::registrar::spawn_register_proxy(ep, sock, registrar);

        // The LAYOUT pre-REGISTERs every bound callee's AOR → its Contact — the
        // setup the retired `register-*` shapes did inline. With the binding
        // live, a topology-agnostic shape (basic-call, basic-call-media) reaches
        // bob purely because bob registered, the thing the X-Api-Call pin hides:
        // [`outgoing_invite`](InfraRuntime::outgoing_invite) rewrites alice's
        // R-URI to `sip:<callee>@register.example` and the proxy resolves it.
        for role in ["bob1", "bob2"] {
            if let Some(agent) = agents.get(role) {
                let aor = format!("sip:{role}@{REGISTER_AOR_DOMAIN}");
                let granted = agent.register(sock, &aor, 3600).await;
                assert!(
                    granted > 0,
                    "registrar must grant a positive Expires for {role}, got {granted}"
                );
            }
        }

        InfraRuntime {
            harness: h,
            agents,
            sut_ingress: sock,
            lb_vip: sock,
            cfg: cfg.clone(),
            // Pure SIP routing by the registered AOR — no proprietary header.
            egress: EgressPolicy::RegistrarAor { domain: REGISTER_AOR_DOMAIN.to_string() },
            raw_net: net,
            media: Default::default(),
            _proxy: None,
            _b2bua: None,
            _register_proxy: Some(guard),
        }
    }
}

/// A **real**-transport infra: agents on `RealSignalingNetwork` under a wall
/// clock, via the [`Harness::with_network_and_clock`] seam (ADR-0018, Phase A).
/// No SUT is spawned — `sut_ingress` points at bob1, so `basic-call` becomes a
/// direct peer call. This is the in-CI proof that the *same* shape body runs over
/// real sockets + real time; the external-kind-cluster infra is the same seam
/// with `sut_ingress` pointed at the LB VIP (env-gated, not run here).
pub struct RealLoopbackDirect;

#[async_trait(?Send)]
impl InfraShape for RealLoopbackDirect {
    fn id(&self) -> &str {
        "real-loopback-direct"
    }
    fn kind(&self) -> InfraKind {
        InfraKind::Real
    }

    async fn build(&self, scenario_name: &str, cfg: &EndpointConfig) -> InfraRuntime {
        cfg.assert_binds(self.id());
        let net: Arc<dyn sip_net::SignalingNetwork> = Arc::new(sip_net::RealSignalingNetwork::new());
        let h = Harness::with_network_and_clock(
            scenario_name.to_string(),
            net.clone(),
            Clock::system(),
            layer_harness::TransportKind::Live,
            cfg.recv_timeout(),
        );
        let mut agents = BTreeMap::new();
        for role in ["alice", "bob1", "bob2"] {
            if let Some(addr) = cfg.roles.get(role) {
                agents.insert(role.to_string(), h.agent(role, &addr.to_string()).await);
            }
        }
        // No SUT: alice talks straight to bob1 (the ingress is bob1).
        let bob1 = cfg.addr("bob1");
        InfraRuntime {
            harness: h,
            agents,
            sut_ingress: bob1,
            lb_vip: bob1,
            cfg: cfg.clone(),
            // alice talks straight to bob1; no rewrite.
            egress: EgressPolicy::Transparent,
            raw_net: net,
            media: Default::default(),
            _proxy: None,
            _b2bua: None,
            _register_proxy: None,
        }
    }
}

/// The **real cluster** infra (`real`): alice & bob1 on real sockets under a
/// wall clock, calling through the ALREADY-RUNNING kind cluster's LB VIP — the
/// `sip-test` namespace's `sip-front-proxy` pair fronting the `b2bua-worker`s.
/// Nothing is spawned in-process; the SUT is the live cluster.
///
/// Topology & addressing (everything comes from `e2e/infra/real.json`):
///   - `lb` = the VRRP VIP (e.g. `172.20.255.250:5060`) — `sut_ingress`.
///   - `alice`/`bob1` bind the HOST's address on the kind docker bridge (the
///     network gateway, e.g. `172.20.0.1`) so the cluster can route the b-leg
///     INVITE and in-dialog requests back out to them.
///   - The deployed worker's decision engine falls back to its own in-cluster
///     `B2BUA_DEST` unless the INVITE pins the callee via
///     `X-Api-Call.destination` — so this layout's egress policy is
///     [`EgressPolicy::ApiCallPin`] and [`InfraRuntime::outgoing_invite`]
///     attaches the header (pinning the callee role's address). The b-leg still
///     rides LB-first (`B2BUA_OUTBOUND_PROXY` = VIP): the production invariant holds.
///   - RTP (`<role>.rtp` config keys) flows host↔host directly — real UDP via
///     `RealSignalingNetwork`, same media seam as the fake fabric (ADR-0018).
pub struct RealKindLb;

#[async_trait(?Send)]
impl InfraShape for RealKindLb {
    fn id(&self) -> &str {
        "real"
    }
    fn kind(&self) -> InfraKind {
        InfraKind::Real
    }

    async fn build(&self, scenario_name: &str, cfg: &EndpointConfig) -> InfraRuntime {
        cfg.assert_binds(self.id());
        let net: Arc<dyn sip_net::SignalingNetwork> = Arc::new(sip_net::RealSignalingNetwork::new());
        let h = Harness::with_network_and_clock(
            scenario_name.to_string(),
            net.clone(),
            Clock::system(),
            layer_harness::TransportKind::Live,
            cfg.recv_timeout(),
        );
        // The cluster is a stateful, shared SUT: Call-IDs / branches must be
        // unique ACROSS runs or its transaction layer absorbs the next run's
        // INVITE as a retransmission of this one (see `Harness::seed_ids`).
        h.seed_ids(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(1),
        );
        let mut agents = BTreeMap::new();
        for role in ["alice", "bob1", "bob2"] {
            if let Some(addr) = cfg.roles.get(role) {
                agents.insert(role.to_string(), h.agent(role, &addr.to_string()).await);
            }
        }
        let lb = cfg.addr("lb");
        InfraRuntime {
            harness: h,
            agents,
            sut_ingress: lb,
            lb_vip: lb,
            cfg: cfg.clone(),
            // The deployed worker falls back to its in-cluster `B2BUA_DEST`
            // unless the INVITE pins the callee — pin the b-leg via `X-Api-Call`.
            egress: EgressPolicy::ApiCallPin,
            raw_net: net,
            media: Default::default(),
            _proxy: None,
            _b2bua: None,
            _register_proxy: None,
        }
    }
}

/// A running in-process LB (`ProxyCore`) bound as a SUT. Aborts its recv loop on
/// drop. Replicates `b2bua-harness`'s test-only `spawn_lb_proxy` from public
/// `sip-proxy` types so the run-core owns the wiring (no test-dep on a harness).
struct ProxyGuard {
    _addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for ProxyGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Spawn a real load-balancing `ProxyCore` on the harness fabric, fronting the
/// single registered b2bua worker (HRW always picks it).
async fn spawn_lb_proxy(
    h: &Harness,
    addr: SocketAddr,
    worker_name: &str,
    worker: SocketAddr,
) -> ProxyGuard {
    let registry: Arc<dyn WorkerRegistry> = Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry::alive(
            worker_name,
            ProxyAddr::new(worker.ip().to_string(), worker.port()),
        )],
        Clock::test_at(0),
    ));
    let hmac =
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
        registry.clone(),
        hmac,
        observer,
        Arc::new(ProxyMetrics::new()),
        Clock::test_at(0),
        LoadBalancerConfig::default(),
    ));

    // The LB is a real `ProxyCore` — role-tag its bind `{Proxy}` so the RFC
    // suite judges this lane by the proxy rules (and only those).
    let (ep, sock) = h
        .bind_sut_with_roles(
            "lb",
            &addr.to_string(),
            std::collections::HashSet::from([sip_net::UaRole::Proxy]),
        )
        .await;
    let core = ProxyCoreBuilder::new(ProxyAddr::from(sock), strategy, registry)
        .clock(Clock::test_at(0))
        .id_gen(Arc::new(IdGen::seeded(0xC0FFEE)))
        .metrics(Arc::new(ProxyMetrics::new()))
        .build(ep);
    let task = tokio::spawn(core.run());
    ProxyGuard { _addr: sock, task }
}
