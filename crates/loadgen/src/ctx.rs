//! Per-call context: the bound agents + correlation/routing config a scenario
//! operates on ([`CallEnv`]), and the timing/checkpoint recorder ([`CallCtx`]).

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use scenario_harness::{Agent, Invite};

use crate::mux::Correlation;

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
    /// How the per-call correlation token is carried through the SUT (a single
    /// transparent header). The SUT relays it onto every originated leg, so
    /// bob and charlie share this one token.
    pub correlation: Correlation,
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
}

impl CallEnv<'_> {
    /// Apply the per-call correlation token (and optional routing pin) to the
    /// initial INVITE so every leg of this call can be matched back to it.
    pub fn prepare_invite<'b>(&self, inv: Invite<'b>) -> Invite<'b> {
        let mut inv = inv.with_header(self.correlation.header_name(), &self.token);
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

    /// A `<sip:transfer@host:port>` Refer-To pointing at charlie's **address**
    /// (so the SUT routes the transfer there). Correlation rides the relayed
    /// `X-Loadgen-Id` header, not this URI, so the user-part is cosmetic. `None`
    /// if no charlie leg.
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
