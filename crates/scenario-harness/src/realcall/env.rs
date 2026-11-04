//! Per-call context shared by the load driver and the in-process functional
//! leak gate: the bound agents + correlation/routing config a scenario operates
//! on ([`CallEnv`]); its recorder is [`CallCtx`](super::CallCtx).
//!
//! This is SUT-agnostic: a scenario built against [`CallEnv`] runs identically
//! whether the agents were bound by the load `AgentBinder` (mux network, real
//! cluster) or by a functional [`Harness`](crate::Harness) (simulated network,
//! in-process `B2buaCore`). The only coupling is the `scenario_harness` agent
//! DSL; correlation is carried as a data-only [`CorrelationStamp`] (the load
//! mux's strategy owns how the token is extracted back; a functional run simply
//! carries the stamp like any other INVITE content).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_message::header::{HeaderValue, ReferTo, Uri};
use sip_message::sip_str::SipStr;

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
    /// The token IS the calling party's From URI user, which the resolved core
    /// identity already carries: no header and no To override. Requires a SUT
    /// that keeps the From URI user on every originated leg.
    FromUser,
    /// The token IS the INVITE's Call-ID. Requires a SUT whose originated legs
    /// carry a Call-ID derived from it by a fixed rule.
    CallId,
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

/// Everything an [`ActorScenario`](crate::actor::ActorScenario) needs to drive
/// one call: the agents (bound on the mux or a functional harness) + the
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
    /// SUT's panic-ELU and CPS bucket rungs admit it.
    pub emergency: bool,
    /// The final the run declares its system under test refuses a call with
    /// under overload (e.g. `Some(503)`, RFC 3261 §21.5.4). When set, that final
    /// to a non-emergency call's establishing INVITE is an accepted ending
    /// ([`Branch::refused_overload`](crate::actor::Branch::refused_overload));
    /// `None` (the default) accepts no refusal the scenario does not declare.
    pub overload_reject: Option<u16>,
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
            // The functional gate uses the plain relayed-header stamp
            // (value = the bare token).
            stamp: CorrelationStamp::Header {
                name: correlation_header.into(),
                value: token.clone(),
            },
            token,
            emergency: false,
            overload_reject: None,
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
    /// first (a named-`LegSpec` role such as `"mrf"`), then the fixed
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
    pub fn with_challenge_responder(mut self, responder: Arc<dyn ChallengeResponder>) -> Self {
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
    ///
    /// Delegates to [`Self::invite_plan`] so the borrowed (linear) and owned
    /// (actor) surfaces can never drift.
    pub fn outgoing_invite<'b>(&self, callees: &[&str], inv: Invite<'b>) -> Invite<'b> {
        self.invite_plan(callees).apply(inv)
    }

    /// The OWNED, data-only realization of [`Self::outgoing_invite`] — the same
    /// fold-in (via → core → identity → egress rewrite), captured as replayable
    /// data so a consumer that outlives the borrowed `CallEnv` (the actor
    /// harness's goal cursor, which drives the INVITE at goal time from owned
    /// state) can apply it later. Everything is eagerly resolved here: the plan
    /// is `Send + 'static`.
    pub fn invite_plan(&self, callees: &[&str]) -> InvitePlan {
        self.invite_plan_with(callees, None)
    }

    /// [`Self::invite_plan`] with a per-route NO-ANSWER ring timer: on a
    /// pinned layout the `routes` failover plan arms `no_answer_ms` on
    /// every hop, so the SUT reroutes on ring-timeout as well as on reject
    /// (`EgressPolicy::rewrite_with_no_answer`). Other layouts ignore the knob.
    pub fn invite_plan_no_answer(&self, callees: &[&str], no_answer_ms: i64) -> InvitePlan {
        self.invite_plan_with(callees, Some(no_answer_ms))
    }

    fn invite_plan_with(&self, callees: &[&str], no_answer_ms: Option<i64>) -> InvitePlan {
        // (3) the per-call identity — the correlation stamp + emergency marker.
        // A To-user stamp supersedes the authored core `to` (`apply_identity`
        // applies AFTER `apply_core` on the builder; eager form: override here).
        let mut headers = Vec::new();
        let mut to = self.core.to.clone();
        let mut call_id = None;
        match &self.stamp {
            CorrelationStamp::Header { name, value } => {
                headers.push((name.clone(), value.clone()));
            }
            CorrelationStamp::ToUser => {
                to = Some(format!("sip:{}@{}", self.token, self.bob.addr()));
            }
            CorrelationStamp::FromUser => {}
            CorrelationStamp::CallId => call_id = Some(self.token.clone()),
        }
        if self.emergency {
            headers.push(("Resource-Priority".to_string(), "esnet.0".to_string()));
        }
        let targets: Vec<CalleeTarget> = callees.iter().map(|r| self.callee(r)).collect();
        InvitePlan {
            via: self.via,
            from: self.core.from.clone(),
            to,
            ruri: self.core.ruri.clone(),
            call_id,
            headers,
            rewrite: self.egress.rewrite_with_no_answer(&targets, no_answer_ms),
        }
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
            ApiCall::refer(refer_key, target.addr.ip().to_string(), target.addr.port()).to_header(),
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
        // Splice the transfer role into the policy-resolved URI, keeping the
        // topology-correct host part. A URI no reader accepts falls back to
        // charlie's own socket.
        let uri = Uri::parse(&SipStr::owned(&target.uri)).unwrap_or_else(|_| {
            Uri::sip(target.addr.ip().to_string()).with_port(target.addr.port())
        });
        Some(ReferTo::from_uri(uri.with_user(target.role.as_str())).to_wire())
    }
}

/// The owned, data-only realization of one logical initial INVITE on a run's
/// wire — everything [`CallEnv::outgoing_invite`] folds in (SUT ingress route,
/// resolved core identity, correlation stamp, emergency marker, layout egress
/// rewrite), eagerly resolved so it is `Send + 'static`. Built by
/// [`CallEnv::invite_plan`]; applied onto an [`Invite`] builder at send time.
#[derive(Debug, Clone)]
pub struct InvitePlan {
    /// The SUT ingress the INVITE routes through (`Invite::through`).
    pub via: SocketAddr,
    /// Resolved From override (the authored core identity), if any.
    pub from: Option<String>,
    /// Resolved To override — the authored core `to`, superseded by a To-user
    /// correlation stamp (correlation is load-bearing demux infrastructure).
    pub to: Option<String>,
    /// Resolved R-URI override (the authored core), if any — the egress
    /// rewrite's own R-URI still has the final say.
    pub ruri: Option<String>,
    /// The Call-ID the dialog is created under (a Call-ID correlation stamp);
    /// `None` lets the caller mint one.
    pub call_id: Option<String>,
    /// Extra headers, in fold-in order: the correlation header stamp, then the
    /// emergency `Resource-Priority` marker.
    pub headers: Vec<(String, String)>,
    /// The layout's egress rewrite — applied LAST (final say on R-URI/headers).
    pub rewrite: crate::egress::EgressRewrite,
}

impl InvitePlan {
    /// Add an extra header to fold onto the INVITE (before the egress rewrite) —
    /// the owned-plan analogue of an `Invite::with_header`. The 100rel flows use
    /// it to advertise `Supported: 100rel` (RFC 3262 §3) from the actor lane,
    /// where the caller drives the INVITE from owned plan data at goal time.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// [`with_header`](Self::with_header) preset for `Supported: 100rel`.
    pub fn with_supported_100rel(self) -> Self {
        self.with_header("Supported", "100rel")
    }

    /// Replay the plan onto an INVITE builder (what
    /// [`CallEnv::outgoing_invite`] applies), in op order: `through(via)` → core
    /// From/To/R-URI → identity headers → the egress rewrite last.
    pub fn apply<'b>(&self, inv: Invite<'b>) -> Invite<'b> {
        let mut inv = inv.through(self.via);
        if let Some(from) = &self.from {
            inv = inv.from(from);
        }
        if let Some(to) = &self.to {
            inv = inv.to(to);
        }
        if let Some(ruri) = &self.ruri {
            inv = inv.ruri(ruri);
        }
        if let Some(call_id) = &self.call_id {
            inv = inv.with_call_id(call_id);
        }
        for (name, value) in &self.headers {
            inv = inv.with_header(name, value);
        }
        self.rewrite.clone().apply(inv)
    }
}
