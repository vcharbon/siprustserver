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
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::egress::{ApiCall, CalleeTarget, EgressPolicy};
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

/// Everything a [`RealCallScenario`](super::RealCallScenario) needs to drive one
/// call: the agents (bound on the mux or a functional harness) + the
/// correlation/routing knobs + the realistic-timing dwells. Built fresh per call
/// (so the per-call token is unique).
pub struct CallEnv<'a> {
    /// The UAC originating the call (routes through [`via`](Self::via)).
    pub alice: &'a Agent,
    /// The downstream UAS the SUT routes the callee leg to.
    pub bob: &'a Agent,
    /// The transfer target leg (REFER scenario only).
    pub charlie: Option<&'a Agent>,
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
    /// How THIS run's layout realizes a logical INVITE on its wire — the
    /// environment-axis seam shared with the e2e framework (same
    /// [`EgressPolicy`] the Infra shapes declare). The callee resolver is
    /// [`Self::callee`] (role → the bound agent's socket, URI'd by the policy);
    /// scenarios never branch on the policy — they call
    /// [`Self::outgoing_invite`], which consults it. Replaces the hand-rolled
    /// `X-Api-Call` route/refer pins.
    pub egress: EgressPolicy,
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
            charlie,
            via,
            // The functional gate keeps the historic plain relayed-header stamp
            // (value = the bare token).
            stamp: CorrelationStamp::Header {
                name: correlation_header.into(),
                value: token.clone(),
            },
            token,
            emergency: false,
            egress: EgressPolicy::Transparent,
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
        let addr = match role {
            "bob" => self.bob.addr(),
            "charlie" => {
                self.charlie.expect("CallEnv: callee role \"charlie\" is not bound").addr()
            }
            other => panic!("CallEnv has no agent for callee role {other:?}"),
        };
        CalleeTarget { role: role.to_string(), uri: self.egress.callee_uri(role, addr), addr }
    }

    /// Realize the logical initial INVITE on THIS run's wire — mirrors e2e-core
    /// `InfraRuntime::outgoing_invite`. `callees` is the ordered candidate list
    /// (primary first; every current scenario passes `["bob"]`). It (1) routes
    /// through the SUT ingress ([`Self::via`]), (2) applies the per-call
    /// identity — the correlation stamp + the emergency marker, which stay
    /// orthogonal to routing (the load analogue of the e2e Test case's `core`
    /// From/To/R-URI overrides) — then (3) applies the layout's
    /// [`EgressRewrite`](crate::egress::EgressRewrite), which has the final say
    /// (e.g. an `X-Api-Call` destination pin on the real cluster).
    pub fn outgoing_invite<'b>(&self, callees: &[&str], inv: Invite<'b>) -> Invite<'b> {
        let inv = self.apply_identity(inv.through(self.via));
        let targets: Vec<CalleeTarget> = callees.iter().map(|r| self.callee(r)).collect();
        self.egress.rewrite_for(&targets).apply(inv)
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

    /// The `<sip:…>` Refer-To addressing charlie **through the egress seam**
    /// (its host part is the policy-resolved URI's: the registered AOR domain or
    /// charlie's address). The user-part is chosen by the correlation strategy:
    /// cosmetic (`transfer`) with a header stamp, the TOKEN with the To-user
    /// stamp — the SUT's transfer INVITE derives its To/R-URI from this URI,
    /// which is how the C leg correlates. `None` if no charlie.
    pub fn refer_to(&self) -> Option<String> {
        let target = self.refer_target()?;
        let user = match &self.stamp {
            CorrelationStamp::ToUser => self.token.as_str(),
            CorrelationStamp::Header { .. } => "transfer",
        };
        // Splice the correlation user-part into the policy-resolved URI
        // (`sip:charlie@<rest>` → `sip:<user>@<rest>`), keeping the topology-
        // correct host part.
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
}

impl CallCtx {
    pub fn new() -> Self {
        Self {
            t0: Instant::now(),
            checkpoints: Mutex::new(Vec::new()),
            phases: Mutex::new(Vec::new()),
            ringing: Mutex::new(None),
        }
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
