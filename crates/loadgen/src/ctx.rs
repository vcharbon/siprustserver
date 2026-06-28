//! Per-call context: the bound agents + correlation/routing config a scenario
//! operates on ([`CallEnv`]), and the timing/checkpoint recorder ([`CallCtx`]).

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use scenario_harness::{Agent, Invite};

use crate::mux::{Correlation, Source};

/// Everything a [`LoadScenario`](crate::scenarios::LoadScenario) needs to drive
/// one call: the agents the runner bound on the mux + the correlation/routing
/// knobs. Built fresh per call (so the per-call tokens are unique).
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
    /// How the per-call correlation token is carried through the SUT to the
    /// callee leg (a transparent header, or the To URI user-part).
    pub correlation: Correlation,
    /// The callee (bob) leg's correlation token.
    pub callee_token: String,
    /// The transfer-target (charlie) leg's correlation token.
    pub refer_token: String,
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
}

impl CallEnv<'_> {
    /// Apply the per-call correlation token (and optional routing pin) to the
    /// initial INVITE so the callee leg can be matched back to this call.
    pub fn prepare_invite<'b>(&self, inv: Invite<'b>) -> Invite<'b> {
        let mut inv = embed_token(inv, &self.correlation, &self.callee_token, self.bob);
        if let Some(pin) = self.route_pin {
            inv = inv.with_header("X-Api-Call", &api_pin(pin));
        }
        inv
    }

    /// The `X-Api-Call` REFER-authorization JSON pinning the transfer to the
    /// `refer` endpoint (our-b2bua adapter). `None` if no charlie leg.
    pub fn refer_api_call(&self) -> Option<String> {
        let pin = self.refer_pin?;
        Some(format!(
            r#"{{"refer_key":"{}","destination":{{"host":"{}","port":{}}}}}"#,
            self.refer_key,
            pin.ip(),
            pin.port()
        ))
    }

    /// A `<sip:<refer_token>@host:port>` Refer-To carrying charlie's correlation
    /// token in the user-part (so the transfer INVITE correlates). `None` if no
    /// charlie leg.
    pub fn refer_to(&self) -> Option<String> {
        let c = self.charlie?;
        Some(format!("<sip:{}@{}>", self.refer_token, c.addr()))
    }
}

/// Embed `token` into an INVITE in the correlation's primary channel.
fn embed_token<'b>(inv: Invite<'b>, c: &Correlation, token: &str, bob: &Agent) -> Invite<'b> {
    match c.primary() {
        Source::Header(name) => inv.with_header(name, token),
        Source::ToUser => inv.to(format!("sip:{token}@{}", bob.addr().ip())),
        Source::RuriUser => inv.ruri(format!("sip:{token}@{}", bob.addr())),
    }
}

fn api_pin(addr: SocketAddr) -> String {
    format!(
        r#"{{"destination":{{"host":"{}","port":{}}}}}"#,
        addr.ip(),
        addr.port()
    )
}

/// Per-call timing recorder. Holds the call's start instant and the named
/// **checkpoints** ("keywords") a scenario marks at points whose latency we want
/// to track (e.g. `time_to_200`). `Mutex` (not `RefCell`) so it is `Sync` — the
/// `async-trait` scenario future borrows `&CallCtx` across awaits.
pub struct CallCtx {
    t0: Instant,
    checkpoints: Mutex<Vec<(&'static str, Duration)>>,
}

impl CallCtx {
    pub fn new() -> Self {
        Self { t0: Instant::now(), checkpoints: Mutex::new(Vec::new()) }
    }

    pub fn checkpoint(&self, name: &'static str) {
        self.checkpoints.lock().unwrap().push((name, self.t0.elapsed()));
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
