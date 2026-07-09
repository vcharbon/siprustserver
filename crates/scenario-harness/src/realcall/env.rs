//! Per-call context shared by the load driver and the in-process functional
//! leak gate: the bound agents + correlation/routing config a scenario operates
//! on ([`CallEnv`]), and the timing/checkpoint recorder ([`CallCtx`]).
//!
//! This is SUT-agnostic: a scenario built against [`CallEnv`] runs identically
//! whether the agents were bound by the load `AgentBinder` (mux network, real
//! cluster) or by a functional [`Harness`](crate::Harness) (simulated network,
//! in-process `B2buaCore`). The only coupling is the `scenario_harness` agent
//! DSL; correlation is carried as a data-only [`CorrelationStamp`] (the load
//! mux's strategy owns how the token is extracted back; a functional run simply
//! carries the stamp like any other INVITE content).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::anchors::{AnchorKeys, AnchorTag};
use crate::egress::{ApiCall, CalleeTarget, EgressPolicy};
use crate::realcall::auth::ChallengeResponder;
use crate::{Agent, Invite};

/// How the per-call correlation token is **written into the outgoing INVITE** —
/// the STAMP half of the pluggable correlation strategy (the EXTRACT half lives
/// in the load mux, which recovers the token from a received leg). Data-only, so
/// it stays SUT- and transport-agnostic: a functional run applies it exactly
/// like the load driver does.
#[derive(Debug, Clone)]
pub enum CorrelationStamp {
    /// Stamp `value` (the token already rendered through the strategy's value
    /// template) into the transparent header `name`. Requires the SUT to RELAY
    /// the header onto every originated leg (our b2bua:
    /// `B2BUA_RELAY_HEADERS=<name>`).
    Header { name: String, value: String },
    /// Embed the token as the To-header user-part (`sip:<token>@<callee>`) —
    /// a SIP-correct B2BUA copies the To URI onto its originated leg, so this
    /// survives a third-party SUT that strips unknown headers (zero SUT
    /// cooperation).
    ToUser,
}

/// The per-call **core identity** overrides — the load-side twin of the e2e
/// Test case's `core` From/To/R-URI input (`e2e_model::CoreInput`; duplicated
/// here as a data-only struct because `e2e-model` depends on this crate, not
/// the reverse). Resolved per call from a binding pool by the load driver;
/// `None` keeps the agent-derived default. Folded into the wire INVITE by
/// [`CallEnv::outgoing_invite`].
#[derive(Debug, Clone, Default)]
pub struct CoreIdentity {
    pub from: Option<String>,
    pub to: Option<String>,
    pub ruri: Option<String>,
}

impl CoreIdentity {
    /// A short human-readable `from=… to=… ruri=…` rendering of the SET fields
    /// (`None` if none is set) — the sampled-callflow banner fragment.
    pub fn summary(&self) -> Option<String> {
        let parts: Vec<String> = [("from", &self.from), ("to", &self.to), ("ruri", &self.ruri)]
            .into_iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={v}")))
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

/// Everything a [`RealCallScenario`](super::RealCallScenario) needs to drive one
/// call: the agents (bound on the mux or a functional harness) + the
/// correlation/routing knobs + the realistic-timing dwells. Built fresh per call
/// (so the per-call token is unique).
pub struct CallEnv<'a> {
    /// The UAC originating the call (routes through [`via`](Self::via)).
    pub alice: &'a Agent,
    /// The downstream UAS the SUT routes the callee leg to.
    pub bob: &'a Agent,
    /// The SECOND callee receiver (rerouting scenarios only): the failover
    /// target the SUT re-targets after `bob` rejects. On the load mux it shares
    /// bob's socket (the driver's leg picker demuxes by R-URI user); on a
    /// functional harness it is its own bound agent.
    pub bob2: Option<&'a Agent>,
    /// The transfer target leg (REFER scenario only).
    pub charlie: Option<&'a Agent>,
    /// Every bound callee leg `(role, agent)` in declaration order — the OPEN
    /// generalization of the fixed bob/bob2/charlie trio, populated by the load
    /// driver from the shape's named `LegSpec`s (an open-registry role such as
    /// `"mrf"` lives only here). [`Self::callee`] / [`Self::callee_agent`]
    /// resolve a role in this list first and fall back to the named fields, so
    /// a surface that leaves it empty (the functional gate) is unchanged.
    pub callees: Vec<(&'a str, &'a Agent)>,
    /// The address the initial INVITE routes *through* — the SUT (in-process
    /// b2bua addr, or the real front-proxy VIP).
    pub via: SocketAddr,
    /// How the per-call correlation token is written into the initial INVITE
    /// (relayed header vs To-user; see [`CorrelationStamp`]). The load mux
    /// demuxes on `(socket, token)` with the matching extract half; a functional
    /// run just carries it.
    pub stamp: CorrelationStamp,
    /// The call's correlation token — stamped on alice's INVITE (per
    /// [`Self::stamp`]) and carried by the SUT onto every downstream leg.
    pub token: String,
    /// Emergency call: stamps `Resource-Priority: esnet.0` on the INVITE so the
    /// SUT force-admits it (never shed by the Tier-3 / panic-ELU overload gate).
    pub emergency: bool,
    /// The per-call **core identity** (the resolved binding's From/To/R-URI
    /// overrides — the e2e Test case's `core` on the load surface). Defaults
    /// keep the agent-derived identities; see [`Self::outgoing_invite`] for
    /// where it folds in (and what supersedes it).
    pub core: CoreIdentity,
    /// How THIS run's layout realizes a logical INVITE on its wire — the
    /// environment-axis seam shared with the e2e framework (same
    /// [`EgressPolicy`] the Infra shapes declare). The callee resolver is
    /// [`Self::callee`] (role → the bound agent's socket, URI'd by the policy);
    /// scenarios never branch on the policy — they call
    /// [`Self::outgoing_invite`], which consults it. Replaces the hand-rolled
    /// `X-Api-Call` route/refer pins.
    pub egress: EgressPolicy,
    /// **Deferred-by-design auth adapter** (see [`crate::realcall::auth`]).
    /// `None` (the default) = the choreography never retries, so a `401`/`407`
    /// classifies as `status_401`/`status_407` exactly as today.
    /// `Some(responder)` wires the RFC 3261 §22.2 retry: on a challenge the INVITE
    /// choreography (and the authenticated out-of-dialog builder) ACK, ask the
    /// responder for a credential, and resend once. A library-consumer seam — no
    /// CLI flag mints one yet.
    pub challenge_responder: Option<Arc<dyn ChallengeResponder>>,
    /// Long-hold duration for the OPTIONS-keepalive scenario.
    pub options_hold: Duration,
    /// In-dialog OPTIONS keepalive cadence.
    pub options_cadence: Duration,
    /// Realistic ring time: the callee waits this long between `180 Ringing` and
    /// the `200 OK` (alice is not blocked on a receive during it, so it just dwells
    /// the early dialog). `0` = answer immediately.
    pub ring_delay: Duration,
    /// Post-connect talk time held before tearing the call down (basic call) — a
    /// realistic in-call dwell. `0` = hang up immediately.
    pub talk_time: Duration,
    /// Spacing held before AND after the re-INVITE renegotiation (reinvite
    /// scenario). `0` = back-to-back.
    pub reinvite_gap: Duration,
    /// Total hold of a long recorded call (the `long_call` scenario), split either
    /// side of its single in-dialog OPTIONS keepalive ping.
    pub long_hold: Duration,
}

impl<'a> CallEnv<'a> {
    /// A [`CallEnv`] for the **in-process functional leak gate** — agents bound on
    /// a [`Harness`](crate::Harness), routing through `via` (the bound SUT).
    /// [`EgressPolicy::Transparent`] (the functional B2BUA's scripted decision
    /// engine routes the callee by its own config, so the logical INVITE is the
    /// wire INVITE), and **realistic non-zero timing** by default so the dwell
    /// between 180→200, around the re-INVITE, and before the BYE is actually
    /// exercised. Under `#[tokio::test(start_paused)]` those sleeps
    /// auto-advance, so they cost ~zero wall-clock while still aging the dialog
    /// on the SUT.
    ///
    /// `token` is the per-call correlation value (mint a fresh one per call);
    /// `correlation_header` is the transparent header it rides (e.g.
    /// `"X-Loadgen-Id"`). Tune any timing knob on the returned value before use.
    pub fn for_functional(
        alice: &'a Agent,
        bob: &'a Agent,
        charlie: Option<&'a Agent>,
        via: SocketAddr,
        correlation_header: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        let token = token.into();
        Self {
            alice,
            bob,
            bob2: None,
            charlie,
            callees: Vec::new(),
            via,
            // The functional gate keeps the historic plain relayed-header stamp
            // (value = the bare token).
            stamp: CorrelationStamp::Header {
                name: correlation_header.into(),
                value: token.clone(),
            },
            token,
            emergency: false,
            core: CoreIdentity::default(),
            egress: EgressPolicy::Transparent,
            // Auth is deferred: no responder → no §22.2 retry (a 401/407 is a
            // counted deviation). A consumer wires one via `with_challenge_responder`.
            challenge_responder: None,
            // Realistic dwell defaults (free under a paused clock).
            options_hold: Duration::from_secs(2),
            options_cadence: Duration::from_secs(1),
            ring_delay: Duration::from_secs(2),
            talk_time: Duration::from_secs(3),
            reinvite_gap: Duration::from_secs(1),
            long_hold: Duration::from_secs(2),
        }
    }

    /// Resolve a logical callee **role** (`"bob"`, `"charlie"`) to how THIS
    /// run's layout addresses it — the callee half of the egress seam, mirroring
    /// e2e-core `InfraRuntime::callee`. The resolver is the bound agents
    /// themselves (the load mux's static endpoints / the functional harness
    /// agents): the role maps to that agent's socket, and the [`EgressPolicy`]
    /// decides the topology-correct URI (registered AOR / `sip:<role>@<addr>`).
    /// Panics on an unknown role or an unbound charlie (like the e2e resolver
    /// panics on a role missing from the Endpoint config).
    pub fn callee(&self, role: &str) -> CalleeTarget {
        let addr = self.callee_agent(role).addr();
        CalleeTarget { role: role.to_string(), uri: self.egress.callee_uri(role, addr), addr }
    }

    /// The bound agent behind a logical callee **role** — the receiver a load
    /// body drives for that leg. Resolves the open [`Self::callees`] list
    /// first (a named-`LegSpec` role such as `"mrf"`), then the historic
    /// bob/bob2/charlie fields. Panics on an unbound role — a scenario wiring
    /// bug, caught loudly.
    pub fn callee_agent(&self, role: &str) -> &'a Agent {
        if let Some((_, agent)) = self.callees.iter().find(|(r, _)| *r == role) {
            return agent;
        }
        match role {
            "bob" => self.bob,
            "bob2" => self.bob2.expect("CallEnv: callee role \"bob2\" is not bound"),
            "charlie" => self.charlie.expect("CallEnv: callee role \"charlie\" is not bound"),
            other => panic!("CallEnv has no agent for callee role {other:?}"),
        }
    }

    /// Bind the second callee receiver (`bob2`) — the rerouting scenarios'
    /// failover target. Builder-style so `for_functional` call sites stay put.
    pub fn with_bob2(mut self, bob2: &'a Agent) -> Self {
        self.bob2 = Some(bob2);
        self
    }

    /// Bind an additional NAMED callee leg (an open-registry role beyond the
    /// bob/bob2/charlie trio, e.g. `"mrf"`) — resolved by [`Self::callee`] /
    /// [`Self::callee_agent`]. Builder-style, for functional surfaces; the load
    /// driver populates [`Self::callees`] wholesale from the shape's `LegSpec`s.
    pub fn with_callee(mut self, role: &'a str, agent: &'a Agent) -> Self {
        self.callees.push((role, agent));
        self
    }

    /// Attach a [`ChallengeResponder`] — the deferred-by-design auth adapter (see
    /// [`crate::realcall::auth`]). Builder-style; the default is `None` (no
    /// retry). The load driver plugs one via its own `MixEntry`/`CallEnv` wiring.
    pub fn with_challenge_responder(
        mut self,
        responder: Arc<dyn ChallengeResponder>,
    ) -> Self {
        self.challenge_responder = Some(responder);
        self
    }

    /// Realize the logical initial INVITE on THIS run's wire — mirrors e2e-core
    /// `InfraRuntime::outgoing_invite`. `callees` is the ordered candidate list
    /// (primary first; every current scenario passes `["bob"]`). It (1) routes
    /// through the SUT ingress ([`Self::via`]), (2) applies the resolved
    /// [`CoreIdentity`] From/To/R-URI overrides (the e2e Test case's `core` on
    /// the load surface), (3) applies the per-call run identity — the
    /// correlation stamp + the emergency marker, which stay orthogonal to
    /// routing AND supersede the authored core where they collide (a To-user
    /// correlation stamp overrides an authored `to`: correlation is
    /// load-bearing demux infrastructure) — then (4) applies the layout's
    /// [`EgressRewrite`](crate::egress::EgressRewrite), which has the final say
    /// (e.g. an `X-Api-Call` destination pin / AOR R-URI on the real cluster).
    pub fn outgoing_invite<'b>(&self, callees: &[&str], inv: Invite<'b>) -> Invite<'b> {
        let inv = self.apply_identity(self.apply_core(inv.through(self.via)));
        let targets: Vec<CalleeTarget> = callees.iter().map(|r| self.callee(r)).collect();
        self.egress.rewrite_for(&targets).apply(inv)
    }

    /// Fold the resolved binding's core From/To/R-URI overrides into the INVITE
    /// (mirrors the `input.core` fold-in of `InfraRuntime::outgoing_invite`).
    fn apply_core<'b>(&self, mut inv: Invite<'b>) -> Invite<'b> {
        if let Some(from) = &self.core.from {
            inv = inv.from(from);
        }
        if let Some(to) = &self.core.to {
            inv = inv.to(to);
        }
        if let Some(ruri) = &self.core.ruri {
            inv = inv.ruri(ruri);
        }
        inv
    }

    /// The per-call identity: the correlation stamp + the emergency marker.
    /// Deliberately separate from the egress rewrite — correlation is a run
    /// strategy, not topology.
    fn apply_identity<'b>(&self, inv: Invite<'b>) -> Invite<'b> {
        let mut inv = match &self.stamp {
            CorrelationStamp::Header { name, value } => inv.with_header(name, value),
            // The token IS the To user-part; the host part stays the callee's
            // address (cosmetic — the SUT routes on the R-URI / its config).
            CorrelationStamp::ToUser => {
                inv.to(format!("sip:{}@{}", self.token, self.bob.addr()))
            }
        };
        if self.emergency {
            // Emergency namespace marker the b2bua's overload brake never sheds.
            inv = inv.with_header("Resource-Priority", "esnet.0");
        }
        inv
    }

    /// The `X-Api-Call` REFER-authorization header value for a blind transfer to
    /// charlie under `refer_key` — [`ApiCall::refer`] over the seam-resolved
    /// charlie target ([`Self::callee`]), exactly what the e2e
    /// `transfer-refer-media` shape emits. `refer_key` is per-run SUT auth data
    /// (the scenario input), not topology — hence a parameter, not an env field.
    /// `None` if no charlie leg is bound.
    pub fn refer_authorization(&self, refer_key: &str) -> Option<String> {
        let target = self.refer_target()?;
        Some(
            ApiCall::refer(refer_key, target.addr.ip().to_string(), target.addr.port())
                .to_header(),
        )
    }

    /// Charlie (the transfer target) resolved through the same egress seam as
    /// any callee. `None` if no charlie leg is bound.
    pub fn refer_target(&self) -> Option<CalleeTarget> {
        self.charlie?;
        Some(self.callee("charlie"))
    }

    /// The `<sip:…>` Refer-To addressing charlie **through the egress seam** (its
    /// host part is the policy-resolved URI's: the registered AOR domain or
    /// charlie's address). The user-part names the transfer **role** (`charlie`),
    /// because the SUT-originated C-leg copies the Refer-To onto its Request-URI
    /// (only the R-URI — the C-leg's To is inherited from the A-leg): that makes
    /// the transfer leg arrive `sip:charlie@…`, **prefix-routable** to charlie's
    /// receiver when it shares the callee socket with bob/bob2 (the load mux's
    /// `prefix_leg_picker`). Correlation is orthogonal and unaffected: the token
    /// rides the relayed header (header stamp) or the A-leg-inherited To-user
    /// (To-user stamp), never this user-part. `None` if no charlie.
    pub fn refer_to(&self) -> Option<String> {
        let target = self.refer_target()?;
        // Splice the transfer role into the policy-resolved URI
        // (`sip:charlie@<rest>`), keeping the topology-correct host part.
        let user = target.role.as_str();
        let rest = target
            .uri
            .split_once('@')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| target.addr.to_string());
        Some(format!("<sip:{user}@{rest}>"))
    }
}

/// Per-call timing recorder. Holds the call's start instant, the named
/// **checkpoints** ("keywords") a scenario marks at points whose latency we want
/// to track (e.g. `time_to_200`), and the **phase markers** a scenario stamps as
/// it advances (connected, re-invited, transferred, post-PRACK, …) so a NOK
/// sample can say WHICH phase was live and the chaos correlation can be
/// phase-aware. `Mutex` (not `RefCell`) so it is `Sync` — the `async-trait`
/// scenario future borrows `&CallCtx` across awaits.
pub struct CallCtx {
    t0: Instant,
    checkpoints: Mutex<Vec<(&'static str, Duration)>>,
    phases: Mutex<Vec<(&'static str, Instant)>>,
    /// Whether the caller received the `18x` ringing provisional before the answer.
    /// `None` until the call reaches the ring→answer step; `Some(true)` if the 18x
    /// arrived, `Some(false)` if it was legitimately lost (a NON-PRACK provisional
    /// is best-effort, not a call failure — the driver aggregates the rate into a
    /// cross-call gate instead of failing the individual call).
    ringing: Mutex<Option<bool>>,
    /// Message-anchor collection — the load surface's analogue of
    /// [`Harness::tag_anchor`](crate::Harness::tag_anchor) (ADR-0019). OFF by
    /// default: [`anchor`](Self::anchor) is then a single relaxed atomic load
    /// (~free on the unsampled load majority and in the functional leak gate).
    /// The load driver enables it on a SAMPLED (recording) call so the e2e
    /// check engine can resolve `<agent>.<anchor>` over the recorded trace.
    collect_anchors: AtomicBool,
    anchors: Mutex<Vec<AnchorTag>>,
}

impl CallCtx {
    pub fn new() -> Self {
        Self {
            t0: Instant::now(),
            checkpoints: Mutex::new(Vec::new()),
            phases: Mutex::new(Vec::new()),
            ringing: Mutex::new(None),
            collect_anchors: AtomicBool::new(false),
            anchors: Mutex::new(Vec::new()),
        }
    }

    /// Turn message-anchor collection ON for this call (see the field docs).
    /// The load driver calls this exactly when the call is sampled (recording),
    /// so anchors always have a recorded trace to resolve against.
    pub fn enable_anchor_collection(&self) {
        self.collect_anchors.store(true, Ordering::Relaxed);
    }

    /// Label a message `agent` just RECEIVED with a canonical anchor name
    /// (ADR-0019) — the load-surface analogue of `Harness::tag_anchor` /
    /// e2e-core's `InfraRuntime::anchor`. A no-op (one relaxed atomic load,
    /// `keys` never converted) unless anchor collection was enabled. `name` is
    /// `'static` to keep the vocabulary fixed, like phases/checkpoints.
    pub fn anchor(&self, agent: &Agent, name: &'static str, keys: impl Into<AnchorKeys>) {
        self.push_anchor(agent, name, keys, false);
    }

    /// Label a message `agent` just SENT — for a request whose only receiver is
    /// the external SUT (the REFER anchor). See [`AnchorTag::sent`].
    pub fn anchor_sent(&self, agent: &Agent, name: &'static str, keys: impl Into<AnchorKeys>) {
        self.push_anchor(agent, name, keys, true);
    }

    fn push_anchor(
        &self,
        agent: &Agent,
        name: &'static str,
        keys: impl Into<AnchorKeys>,
        sent: bool,
    ) {
        if !self.collect_anchors.load(Ordering::Relaxed) {
            return;
        }
        self.anchors.lock().unwrap().push(AnchorTag {
            agent: agent.name().to_string(),
            anchor: name.to_string(),
            agent_addr: agent.addr(),
            keys: keys.into(),
            sent,
        });
    }

    /// Drain the collected `(agent, anchor)` labels, in tag order (empty unless
    /// [`enable_anchor_collection`](Self::enable_anchor_collection) was called).
    pub fn take_anchors(&self) -> Vec<AnchorTag> {
        std::mem::take(&mut *self.anchors.lock().unwrap())
    }

    pub fn checkpoint(&self, name: &'static str) {
        self.checkpoints.lock().unwrap().push((name, self.t0.elapsed()));
    }

    /// The instant the call started (its first INVITE) — the lower bound of the
    /// lifetime window the chaos correlation classifies against.
    pub fn start_instant(&self) -> Instant {
        self.t0
    }

    /// Mark that the call reached a named lifecycle phase (e.g. `"connected"`,
    /// `"reinvited"`, `"transferred"`, `"pracked"`), stamping the instant. Cheap
    /// and unconditional; the driver folds these into the NOK sample banner and
    /// uses them for phase-aware chaos correlation. `name` is `'static` to keep
    /// cardinality bounded (a fixed phase vocabulary, like checkpoints).
    pub fn phase(&self, name: &'static str) {
        self.phases.lock().unwrap().push((name, Instant::now()));
    }

    /// The recorded phase markers `(name, instant)`, in order reached.
    pub fn phases(&self) -> Vec<(&'static str, Instant)> {
        self.phases.lock().unwrap().clone()
    }

    /// Record whether this call's caller saw its `18x` ringing provisional
    /// (`true`) or it was lost (`false`) — a non-PRACK provisional is best-effort,
    /// so a miss is EXPECTED, not a failure. The driver folds this into the
    /// cross-call `loadgen_ringing_{expected,received}_total` gate (>99%).
    pub fn mark_ringing(&self, received: bool) {
        *self.ringing.lock().unwrap() = Some(received);
    }

    /// The recorded ringing outcome: `None` if the call never reached the answer
    /// step, else `Some(received)`.
    pub fn ringing(&self) -> Option<bool> {
        *self.ringing.lock().unwrap()
    }

    pub fn elapsed(&self) -> Duration {
        self.t0.elapsed()
    }

    pub fn take_checkpoints(&self) -> Vec<(&'static str, Duration)> {
        std::mem::take(&mut self.checkpoints.lock().unwrap())
    }
}

impl Default for CallCtx {
    fn default() -> Self {
        Self::new()
    }
}
