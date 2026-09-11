//! Serializable timer intents on the replicated call body: [`TimerType`] (the
//! closed union of core timers + the service-owned extension point) and
//! [`TimerEntry`]. Live scheduling/firing is NOT here — it rides the b2bua
//! timer driver; the list-replace helper is
//! [`crate::helpers::replace_timer_by_id`].

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::obligation::Obligation;
use super::sm::MachineId;

/// Union of known timer types. Closed for the CORE-claimed variants; the one
/// open extension point is [`TimerType::Service`] — a service-owned watchdog
/// whose semantics core never interprets.
///
/// **`Debug` is the persisted timer-id recipe** (see [`TimerType::timer_id`]):
/// `ActionExecutor::schedule` mints the replicated `TimerEntry.id` from
/// `format!("{timer_type:?}")` (+ `":{leg_id}"`), and every cancel site mints
/// from the same recipe. The manual impl below keeps the unit variants byte-
/// identical to the derive output while giving `Service` a compact
/// `Service:<service_id>:<key>` form, so the recipe covers it without drift.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerType {
    NoAnswer,
    /// Call-level a-leg setup deadline: armed at route time, cancelled at
    /// answer, untouched by reroutes (each reroute gets its own per-leg
    /// `NoAnswer`; this caps the *whole* setup). Rides the replicated
    /// `call.timers` ledger, so unlike the configured sip-txn initial-INVITE
    /// bound (which dies with a crashed node's transactions) it survives
    /// crash → reclaim and still reaps a stuck-in-setup call.
    SetupTimeout,
    GlobalDuration,
    LimiterRefresh,
    Keepalive,
    KeepaliveTimeout,
    /// The next rung of the ladder repeating the retained emission
    /// `obligation` is owed for (ADR-0029 X4): the un-ACKed 2xx (RFC 3261
    /// §13.3.1.4) or the un-PRACKed reliable provisional (RFC 3262 §3). The
    /// framework re-sends the retained datagram and re-arms the next rung
    /// itself — no rule sees a rung. Retired with the obligation: by the
    /// discharging ACK or PRACK, by the give-up, or with the leg, transaction
    /// or call the emission belonged to.
    Rung {
        obligation: Obligation,
    },
    /// The give-up deadline of `obligation`'s ladder — the one ladder event a
    /// rule sees: 64·T1 for a reliable provisional, the deployment's ACK
    /// deadline for a 2xx (Timer L where it states none). Armed once with the
    /// first rung, so a re-emission cannot push the deadline out, and in the
    /// ledger for as long as the retained emission is; the framework scrubs
    /// the ladder when it fires and a CORE rule (which a service may
    /// re-author) says what the silence means. For a 2xx the session ends
    /// whatever the rule decides (RFC 3261 §13.3.1.4) — a service authors the
    /// teardown's shape, never whether it happens; a reliable provisional's is
    /// the rule's alone — a teardown, an in-dialog reject, or nothing (RFC
    /// 3262 §3).
    RepeatGiveUp {
        obligation: Obligation,
    },
    /// Safety-net timer scheduled when entering "terminating" state.
    TerminatingTimeout,
    /// REFER subscription expiry (RFC 3515).
    ReferSubscriptionExpiry,
    /// Per re-INVITE answer watchdog during REFER-driven blind transfer.
    ReferReinviteAnswer,
    /// Overall REFER safety timer covering the full transfer state machine.
    ReferOverallSafety,
    /// A **service-owned** per-call timer (ADR-0016): `service_id` is the owning
    /// callflow service's [`MachineId`], `key` a service-chosen discriminator
    /// (e.g. `"timer18x"`). Core schedules / cancels / replicates / restores it
    /// like any other timer but attaches NO semantics — only the owning
    /// service's rules match its firing (exact `(service_id, key)` via a
    /// `timer_type` match column, or per-service via the `service_timers`
    /// wildcard). Identity: two timers with different keys are distinct timers
    /// (distinct persisted ids); re-scheduling the same `(service_id, key)`
    /// [+ leg] supersedes the previous instance (same id → ledger replace +
    /// driver epoch bump). Neither `service_id` nor `key` should contain `:`
    /// (the id-recipe separator).
    ///
    /// `Cow<'static, str>` for the same reason as [`MachineId`]: rule
    /// declarations use compile-time literals ([`TimerType::service`], usable
    /// in `const`), while a replicated `TimerEntry` deserialises into an owned
    /// string.
    Service {
        service_id: MachineId,
        key: Cow<'static, str>,
    },
}

impl TimerType {
    /// A service-owned timer from compile-time literals (usable in `const` —
    /// e.g. an `Effect::GuardTimer` declaration or a static match column).
    pub const fn service(service_id: MachineId, key: &'static str) -> Self {
        TimerType::Service { service_id, key: Cow::Borrowed(key) }
    }

    /// A service-owned timer with a runtime-computed key (e.g. per-leg).
    pub fn service_owned(service_id: MachineId, key: String) -> Self {
        TimerType::Service { service_id, key: Cow::Owned(key) }
    }

    /// The canonical persisted timer id — the ONE recipe every schedule *and*
    /// cancel site must mint from (drift = a cancel that silently misses):
    /// `{self:?}` for a call-level timer, `{self:?}:{leg_id}` for a per-leg one.
    pub fn timer_id(&self, leg_id: Option<&str>) -> String {
        match leg_id {
            Some(l) => format!("{self:?}:{l}"),
            None => format!("{self:?}"),
        }
    }
}

impl std::fmt::Debug for TimerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimerType::NoAnswer => f.write_str("NoAnswer"),
            TimerType::SetupTimeout => f.write_str("SetupTimeout"),
            TimerType::GlobalDuration => f.write_str("GlobalDuration"),
            TimerType::LimiterRefresh => f.write_str("LimiterRefresh"),
            TimerType::Keepalive => f.write_str("Keepalive"),
            TimerType::KeepaliveTimeout => f.write_str("KeepaliveTimeout"),
            TimerType::Rung { obligation } => write!(f, "Rung:{obligation:?}"),
            TimerType::RepeatGiveUp { obligation } => write!(f, "RepeatGiveUp:{obligation:?}"),
            TimerType::TerminatingTimeout => f.write_str("TerminatingTimeout"),
            TimerType::ReferSubscriptionExpiry => f.write_str("ReferSubscriptionExpiry"),
            TimerType::ReferReinviteAnswer => f.write_str("ReferReinviteAnswer"),
            TimerType::ReferOverallSafety => f.write_str("ReferOverallSafety"),
            TimerType::Service { service_id, key } => {
                write!(f, "Service:{}:{}", service_id.as_str(), key)
            }
        }
    }
}

/// A serializable timer intent (the live fiber lives in the deferred
/// `TimerService` slice).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub timer_type: TimerType,
    /// Epoch ms — absolute deadline.
    pub fire_at: i64,
    /// `None` = call-level timer.
    pub leg_id: Option<String>,
}
