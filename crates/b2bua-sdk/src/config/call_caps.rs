//! The per-call caps (ADR-0057 §2, §3, §8): what one call's record may hold
//! and what one turn may do. Each cap is a failsafe well above any valid call;
//! none can be switched off, and every one is raised by its `B2BUA_CAP_*`
//! variable.
//!
//! | cap | variable | bounds | kind |
//! |---|---|---|---|
//! | [`Cap::Legs`] | `B2BUA_CAP_LEGS` (+ `_PER_HOUR`) | legs dialled over the call's life | peer |
//! | [`Cap::Attempts`] | `B2BUA_CAP_ATTEMPTS` (+ `_PER_HOUR`) | routes tried, refused admissions included | defect |
//! | [`Cap::Timers`] | `B2BUA_CAP_TIMERS` | timer keys held at once | defect |
//! | [`Cap::Requests`] | `B2BUA_CAP_REQUESTS` | request records and service requests in flight | peer |
//! | [`Cap::CdrEvents`] | `B2BUA_CAP_CDR_EVENTS` (+ `_PER_HOUR`) | CDR events | peer |
//! | [`Cap::Dialogs`] | `B2BUA_CAP_DIALOGS` | dialogs of one leg | peer |
//! | [`Cap::HeaderUpdates`] | `B2BUA_CAP_HEADER_UPDATES` | header statements stored | defect |
//! | [`Cap::ExtBytes`] | `B2BUA_CAP_EXT_BYTES` | one service's ext value, JSON-encoded | defect |
//! | [`Cap::ActionsPerTurn`] | `B2BUA_CAP_ACTIONS_PER_TURN` | actions one turn returns | defect |
//!
//! `B2BUA_CAP_SERVICE_HTTP_TIMEOUT_MS` clamps a service's HTTP timeout; it
//! trips nothing.
//!
//! The upstream defaults are the heaviest shape of the scenario catalogue
//! times the factor ADR-0057 §9 sets by what the cap costs, each stated
//! beside its value in [`CallCaps::DEFAULT`]; a deployment sets its own from
//! measurement.

/// One per-call cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cap {
    Legs,
    Attempts,
    Timers,
    Requests,
    CdrEvents,
    Dialogs,
    HeaderUpdates,
    ExtBytes,
    ActionsPerTurn,
}

/// Who can reach a cap: a peer's traffic, or only a defect in a service, the
/// decision service or the B2BUA. Both end the call the same way; only a
/// defect cap alerts (ADR-0057 §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CapKind {
    Peer,
    Defect,
}

impl CapKind {
    /// Every kind, in exposition order.
    pub const ALL: [CapKind; 2] = [CapKind::Peer, CapKind::Defect];

    /// The metric label value.
    pub const fn label(self) -> &'static str {
        match self {
            CapKind::Peer => "peer",
            CapKind::Defect => "defect",
        }
    }
}

impl Cap {
    /// Every cap, in exposition order.
    pub const ALL: [Cap; 9] = [
        Cap::Legs,
        Cap::Attempts,
        Cap::Timers,
        Cap::Requests,
        Cap::CdrEvents,
        Cap::Dialogs,
        Cap::HeaderUpdates,
        Cap::ExtBytes,
        Cap::ActionsPerTurn,
    ];

    /// The cap's name: its metric label value, its log field and its CDR
    /// reason's suffix.
    pub const fn label(self) -> &'static str {
        match self {
            Cap::Legs => "legs",
            Cap::Attempts => "attempts",
            Cap::Timers => "timers",
            Cap::Requests => "requests",
            Cap::CdrEvents => "cdr-events",
            Cap::Dialogs => "dialogs",
            Cap::HeaderUpdates => "header-updates",
            Cap::ExtBytes => "ext-bytes",
            Cap::ActionsPerTurn => "actions-per-turn",
        }
    }

    /// The caps on what grows with a call's age, in [`Cap::ALL`] order.
    pub const AGED: [Cap; 3] = [Cap::Legs, Cap::Attempts, Cap::CdrEvents];

    /// The cap's position in [`Cap::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The cap's position in [`Cap::AGED`]; `None` for a cap on what a call
    /// holds at once.
    pub const fn aged_index(self) -> Option<usize> {
        match self {
            Cap::Legs => Some(0),
            Cap::Attempts => Some(1),
            Cap::CdrEvents => Some(2),
            _ => None,
        }
    }

    /// The cap named `label`.
    pub fn of_label(label: &str) -> Option<Cap> {
        Cap::ALL.into_iter().find(|c| c.label() == label)
    }

    /// The reason a tripped call's CDR records.
    pub const fn cdr_reason(self) -> &'static str {
        match self {
            Cap::Legs => "cap-legs",
            Cap::Attempts => "cap-attempts",
            Cap::Timers => "cap-timers",
            Cap::Requests => "cap-requests",
            Cap::CdrEvents => "cap-cdr-events",
            Cap::Dialogs => "cap-dialogs",
            Cap::HeaderUpdates => "cap-header-updates",
            Cap::ExtBytes => "cap-ext-bytes",
            Cap::ActionsPerTurn => "cap-actions-per-turn",
        }
    }

    /// Who can reach the cap.
    pub const fn kind(self) -> CapKind {
        match self {
            Cap::Legs | Cap::Requests | Cap::CdrEvents | Cap::Dialogs => CapKind::Peer,
            Cap::Attempts
            | Cap::Timers
            | Cap::HeaderUpdates
            | Cap::ExtBytes
            | Cap::ActionsPerTurn => CapKind::Defect,
        }
    }

    /// The runner variable stating the cap (its base, for a cap that grows
    /// with the call's age).
    pub const fn variable(self) -> &'static str {
        match self {
            Cap::Legs => "B2BUA_CAP_LEGS",
            Cap::Attempts => "B2BUA_CAP_ATTEMPTS",
            Cap::Timers => "B2BUA_CAP_TIMERS",
            Cap::Requests => "B2BUA_CAP_REQUESTS",
            Cap::CdrEvents => "B2BUA_CAP_CDR_EVENTS",
            Cap::Dialogs => "B2BUA_CAP_DIALOGS",
            Cap::HeaderUpdates => "B2BUA_CAP_HEADER_UPDATES",
            Cap::ExtBytes => "B2BUA_CAP_EXT_BYTES",
            Cap::ActionsPerTurn => "B2BUA_CAP_ACTIONS_PER_TURN",
        }
    }
}

/// A cap on what grows with a call's age: `base`, plus `per_hour` for each
/// started hour of the call's granted maximum duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgeCap {
    pub base: u64,
    pub per_hour: u64,
}

impl AgeCap {
    /// The cap of a call granted `granted_ms` (`0`: none granted yet).
    pub fn at(self, granted_ms: i64) -> u64 {
        self.base.saturating_add(self.per_hour.saturating_mul(Self::started_hours(granted_ms)))
    }

    /// The started hours of a grant of `granted_ms`; `0` for none or a
    /// negative one.
    pub fn started_hours(granted_ms: i64) -> u64 {
        const HOUR_MS: u64 = 3_600_000;
        u64::try_from(granted_ms).unwrap_or(0).div_ceil(HOUR_MS)
    }
}

/// The per-call caps a worker enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallCaps {
    pub legs: AgeCap,
    pub attempts: AgeCap,
    pub cdr_events: AgeCap,
    pub timers: u64,
    pub requests: u64,
    pub dialogs: u64,
    pub header_updates: u64,
    pub ext_bytes: u64,
    pub actions_per_turn: u64,
    /// The longest timeout a service's HTTP request runs under, in ms; a
    /// longer one, the port's default included, is clamped to it.
    pub service_http_timeout_ms: u64,
}

impl CallCaps {
    /// The upstream defaults: the heaviest catalogue shape times the factor
    /// of its cap's cost (ADR-0057 §9).
    pub const DEFAULT: CallCaps = CallCaps {
        // × 100. A setup dials at most the decision service's reroute budget
        // (10 routes) plus a media leg and a transfer target: 12. An answered
        // call adds about 6 an hour (release reroutes, transfers).
        legs: AgeCap { base: 1_200, per_hour: 600 },
        // × 1 000 of the 12 routes and 6 an hour, above the floor the
        // startup check holds: every leg may follow MAX_LIMITER_FAILOVER (5)
        // refused admissions, the legs' cap times 6.
        attempts: AgeCap { base: 12_000, per_hour: 6_000 },
        // × 20. About 2 events per attempt over a 10-route setup, then about
        // 10 an hour.
        cdr_events: AgeCap { base: 400, per_hour: 200 },
        // × 20. About 20 at once: the call's setup, duration, keepalive and
        // refresh timers, a no-answer per ringing leg, a ladder per dialog, an
        // answer deadline per request in flight.
        timers: 400,
        // × 20. About 20 at once: relays both ways, keepalive probes, queued
        // NOTIFYs, service requests.
        requests: 400,
        // × 20. A forking callee shows about 5 early dialogs.
        dialogs: 100,
        // × 20. A decision states about 50 header lines over its scopes.
        header_updates: 1_000,
        // × 20. A service's state stays near 6 KiB.
        ext_bytes: 122_880,
        // × 500 of about 200 actions, the initial turn's longest failover
        // chain with every service's start; above the teardown of a call at
        // its base leg cap, one action per unresolved leg and two more.
        actions_per_turn: 100_000,
        // A held request's answer deadline (Timer F less T2, 28 s) less the
        // 2 s answer margin (ADR-0039): the longest a service's answer can
        // take and still answer the request it holds.
        service_http_timeout_ms: 26_000,
    };

    /// The cap `cap` sets on a call granted `granted_ms`.
    pub fn limit(&self, cap: Cap, granted_ms: i64) -> u64 {
        match cap {
            Cap::Legs => self.legs.at(granted_ms),
            Cap::Attempts => self.attempts.at(granted_ms),
            Cap::CdrEvents => self.cdr_events.at(granted_ms),
            Cap::Timers => self.timers,
            Cap::Requests => self.requests,
            Cap::Dialogs => self.dialogs,
            Cap::HeaderUpdates => self.header_updates,
            Cap::ExtBytes => self.ext_bytes,
            Cap::ActionsPerTurn => self.actions_per_turn,
        }
    }
}

impl Default for CallCaps {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_age_cap_adds_its_rate_per_started_hour_of_the_grant() {
        let cap = AgeCap { base: 10, per_hour: 4 };
        assert_eq!(cap.at(0), 10, "nothing granted yet: the base");
        assert_eq!(cap.at(1), 14, "a started hour counts whole");
        assert_eq!(cap.at(3_600_000), 14);
        assert_eq!(cap.at(3_600_001), 18);
        assert_eq!(cap.at(-5), 10, "a negative grant reads as none");
    }

    #[test]
    fn every_cap_knows_its_positions() {
        for (at, cap) in Cap::ALL.into_iter().enumerate() {
            assert_eq!(cap.index(), at);
        }
        for (at, cap) in Cap::AGED.into_iter().enumerate() {
            assert_eq!(cap.aged_index(), Some(at));
        }
        let aged = Cap::ALL.into_iter().filter(|c| c.aged_index().is_some()).count();
        assert_eq!(aged, Cap::AGED.len());
    }

    #[test]
    fn every_cap_names_itself_back() {
        for cap in Cap::ALL {
            assert_eq!(Cap::of_label(cap.label()), Some(cap));
        }
    }
}
