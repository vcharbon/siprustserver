//! What the driver applies to each call: the settings every call shares, and the
//! robustness knobs resolved per scenario.

use std::net::SocketAddr;
use std::time::Duration;

use scenario_harness::EgressPolicy;

/// Static per-call routing config (shared `Arc` across all calls).
pub struct CallConfig {
    /// The address the initial INVITE routes through (SUT / VIP).
    pub via: SocketAddr,
    /// How this run's layout realizes a logical INVITE on the wire — the
    /// environment axis shared with the e2e framework (`EndpointConfig.egress`):
    /// [`EgressPolicy::Transparent`] when the SUT routes the callee itself,
    /// [`EgressPolicy::ApiCallPin`] to pin the b-leg back to the static `uas`
    /// endpoint (`--route-pin-to-uas`).
    pub egress: EgressPolicy,
    pub options_hold: Duration,
    pub options_cadence: Duration,
    /// Realistic ring time (callee 180→200 dwell). `0` = answer immediately.
    pub ring_delay: Duration,
    /// Post-connect talk time before BYE (basic call). `0` = hang up immediately.
    pub talk_time: Duration,
    /// Spacing held before and after a re-INVITE. `0` = back-to-back.
    pub reinvite_gap: Duration,
    /// Total hold of a long recorded call, split either side of its OPTIONS ping.
    pub long_hold: Duration,
    /// After a *failed* call's a-leg is torn down, how long to drain-and-200 the
    /// in-process callee legs so the SUT closes its relayed b-leg promptly.
    pub teardown_quiesce: Duration,
    /// How long a call waits on one step, the caller's wait for its final among
    /// them. `None` keeps each plan's ceiling, else the standard 32 s.
    pub final_wait: Option<Duration>,
}

/// Robustness knobs applied per call, resolved per scenario (a global default
/// overridden by any per-scenario entry). Both default off: an un-tuned run
/// drops and retransmits nothing extra.
#[derive(Clone, Copy, Debug, Default)]
pub struct CallTuning {
    /// Simulated packet-drop probability on this call's mux legs (0 = off). Each
    /// datagram is independently dropped; the SUT (and, when enabled,
    /// auto-retransmit) is what recovers the call.
    pub drop_rate: f64,
    /// Whether the harness auto-retransmits per real SIP timers (Timer A/E for
    /// requests, 2xx-until-ACK for answers) on this call — so a rare drop is
    /// recovered instead of failing the call.
    pub retransmit: bool,
    /// Optional DETERMINISTIC targeted drop (test knob): discard the nth
    /// distinct inbound request of a given CSeq method, once or permanently —
    /// see [`crate::mux::TargetedDrop`]. Default off.
    pub drop_nth: Option<crate::mux::TargetedDrop>,
}
