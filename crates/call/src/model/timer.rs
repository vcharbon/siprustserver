//! Serializable timer intents on the replicated call body: [`TimerType`] (the
//! closed union of core timers + the service-owned extension point) and
//! [`TimerEntry`]. Live scheduling/firing is NOT here — it rides the b2bua
//! timer driver; the list-replace helper is
//! [`crate::helpers::replace_timer_by_id`].

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

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
    /// `call.timers` ledger, so unlike the sip-txn `INVITE_INITIAL_TIMEOUT`
    /// backstop (which dies with a crashed node's transactions) it survives
    /// crash → reclaim and still reaps a stuck-in-setup call.
    SetupTimeout,
    GlobalDuration,
    LimiterRefresh,
    Keepalive,
    KeepaliveTimeout,
    /// RFC 3261 §13.3.1.4 — periodic retransmit of the a-leg INVITE **2xx** while
    /// the caller's ACK is missing (re-armed each fire; cancelled on the a-leg
    /// ACK). Paired with [`AckTimeout`](Self::AckTimeout), which bounds the window.
    AckRetransmit,
    /// RFC 3261 §13.3.1.4 — the a-leg 2xx-without-ACK **give-up** deadline (RFC's
    /// 64·T1). Single-shot, armed when the a-leg 2xx is relayed at dialog
    /// confirmation, cancelled on the a-leg ACK; on expiry the B2BUA BYEs the
    /// just-created a-leg dialog AND tears down the b-leg (the answered-call leak
    /// fix). Distinct from [`AckRetransmit`](Self::AckRetransmit), the on-wire
    /// re-send cadence.
    AckTimeout,
    /// RFC 3261 §13.3.1.4 (in-dialog) — periodic retransmit of the a-leg
    /// **re-INVITE** 2xx while the originator's ACK is missing (re-armed each
    /// fire; cancelled on the a-leg ACK matching the re-INVITE CSeq). The
    /// re-INVITE twin of [`AckRetransmit`](Self::AckRetransmit); distinct so it
    /// never collides with the initial-INVITE watchdog and re-sends the cached
    /// re-INVITE 2xx (not the initial 2xx). Paired with
    /// [`ReinviteAckTimeout`](Self::ReinviteAckTimeout).
    ReinviteAckRetransmit,
    /// RFC 3261 §13.3.1.4 (in-dialog) — the a-leg re-INVITE 2xx-without-ACK
    /// **give-up** deadline (64·T1). Single-shot, armed when the re-INVITE 2xx
    /// is relayed to the originator, cancelled on the a-leg ACK; on expiry the
    /// B2BUA tears the call down (a permanently-lost re-INVITE ACK must never
    /// retransmit forever). The re-INVITE twin of [`AckTimeout`](Self::AckTimeout).
    ReinviteAckTimeout,
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
            TimerType::AckRetransmit => f.write_str("AckRetransmit"),
            TimerType::AckTimeout => f.write_str("AckTimeout"),
            TimerType::ReinviteAckRetransmit => f.write_str("ReinviteAckRetransmit"),
            TimerType::ReinviteAckTimeout => f.write_str("ReinviteAckTimeout"),
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
