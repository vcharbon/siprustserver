//! Per-call context shared by the load driver and the in-process functional
//! leak gate: the bound agents + correlation/routing config a scenario operates
//! on ([`CallEnv`]), and the timing/checkpoint recorder ([`CallCtx`]).
//!
//! This is SUT-agnostic: a scenario built against [`CallEnv`] runs identically
//! whether the agents were bound by the load `AgentBinder` (mux network, real
//! cluster) or by a functional [`Harness`](crate::Harness) (simulated network,
//! in-process `B2buaCore`). The only coupling is the `scenario_harness` agent
//! DSL; the correlation header is carried as a plain header NAME (the load mux
//! demuxes on it; a functional run simply relays it like any other header).

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{Agent, Invite};

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
    /// The header NAME the per-call correlation token is carried in (a single
    /// transparent header the SUT relays onto every originated leg, so bob and
    /// charlie share the one token). The load mux demuxes on `(socket, token)`;
    /// a functional run just relays it.
    pub correlation_header: String,
    /// The call's correlation token — stamped on alice's INVITE and (via the
    /// SUT's header relay) carried onto every downstream leg.
    pub token: String,
    /// Emergency call: stamps `Resource-Priority: esnet.0` on the INVITE so the
    /// SUT force-admits it (never shed by the Tier-3 / panic-ELU overload gate).
    pub emergency: bool,
    /// Optional `X-Api-Call` destination pin (our-b2bua routing adapter): the
    /// static `uas`/`refer` socket the SUT should send the callee leg to. `None`
    /// when the SUT routes the callee by its own (static) config.
    pub route_pin: Option<SocketAddr>,
    pub refer_pin: Option<SocketAddr>,
    /// The `X-Api-Call` `refer_key` the SUT's REFER backend authorizes.
    pub refer_key: String,
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
    /// a [`Harness`](crate::Harness), routing through `via` (the bound SUT). No
    /// `X-Api-Call` pin (the functional B2BUA routes the callee by its own config),
    /// and **realistic non-zero timing** by default so the dwell between
    /// 180→200, around the re-INVITE, and before the BYE is actually exercised.
    /// Under `#[tokio::test(start_paused)]` those sleeps auto-advance, so they cost
    /// ~zero wall-clock while still aging the dialog on the SUT.
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
        Self {
            alice,
            bob,
            charlie,
            via,
            correlation_header: correlation_header.into(),
            token: token.into(),
            emergency: false,
            route_pin: None,
            refer_pin: None,
            refer_key: String::new(),
            // Realistic dwell defaults (free under a paused clock).
            options_hold: Duration::from_secs(2),
            options_cadence: Duration::from_secs(1),
            ring_delay: Duration::from_secs(2),
            talk_time: Duration::from_secs(3),
            reinvite_gap: Duration::from_secs(1),
            long_hold: Duration::from_secs(2),
        }
    }

    /// Apply the per-call correlation token (and optional routing pin) to the
    /// initial INVITE so every leg of this call can be matched back to it.
    pub fn prepare_invite<'b>(&self, inv: Invite<'b>) -> Invite<'b> {
        let mut inv = inv.with_header(&self.correlation_header, &self.token);
        if self.emergency {
            // Emergency namespace marker the b2bua's overload brake never sheds.
            inv = inv.with_header("Resource-Priority", "esnet.0");
        }
        if let Some(pin) = self.route_pin {
            inv = inv.with_header("X-Api-Call", &api_pin(pin));
        }
        inv
    }

    /// The `X-Api-Call` REFER-authorization JSON pinning the transfer to the
    /// `refer` endpoint (our-b2bua adapter). `None` if no charlie leg or no pin.
    pub fn refer_api_call(&self) -> Option<String> {
        let pin = self.refer_pin?;
        Some(format!(
            r#"{{"refer_key":"{}","destination":{{"host":"{}","port":{}}}}}"#,
            self.refer_key,
            pin.ip(),
            pin.port()
        ))
    }

    /// A `<sip:transfer@host:port>` Refer-To pointing at charlie's **address**
    /// (so the SUT routes the transfer there). Correlation rides the relayed
    /// header, not this URI, so the user-part is cosmetic. `None` if no charlie.
    pub fn refer_to(&self) -> Option<String> {
        let c = self.charlie?;
        Some(format!("<sip:transfer@{}>", c.addr()))
    }
}

fn api_pin(addr: SocketAddr) -> String {
    format!(
        r#"{{"destination":{{"host":"{}","port":{}}}}}"#,
        addr.ip(),
        addr.port()
    )
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
}

impl CallCtx {
    pub fn new() -> Self {
        Self { t0: Instant::now(), checkpoints: Mutex::new(Vec::new()), phases: Mutex::new(Vec::new()) }
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
