//! Serializable timer intents on the replicated call body: [`TimerType`] (the
//! closed union of core timers + the service-owned extension point), the
//! [`TimerScope`] a timer guards, the [`TimerKey`] that names one timer of a
//! call, and the [`TimerEntry`] the ledger holds. Live scheduling/firing is
//! NOT here — it rides the b2bua timer driver; the ledger writers are in
//! [`crate::helpers::timer`].

use std::borrow::Cow;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use super::attempt::AttemptId;
use super::obligation::Obligation;
use super::sm::MachineId;

/// Union of known timer types. Closed for the CORE-claimed variants; the one
/// open extension point is [`TimerType::Service`] — a service-owned watchdog
/// whose semantics core never interprets. A type names what a timer does; the
/// [`TimerScope`] of its [`TimerKey`] names what it guards.
///
/// Equality is the type's identity; the hash reads the fields that identify
/// a timer (a [`TimerType::FailureAnswer`]'s `unanswered` payload is carried,
/// not hashed).
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
    /// The next rung of the ladder repeating the retained emission its
    /// obligation scope is owed for (ADR-0032 X4): the un-ACKed 2xx (RFC 3261
    /// §13.3.1.4) or the un-PRACKed reliable provisional (RFC 3262 §3). The
    /// framework re-sends the retained datagram and re-arms the next rung
    /// itself — no rule sees a rung. Retired with the obligation: by the
    /// discharging ACK or PRACK, by the give-up, or with the leg, transaction
    /// or call the emission belonged to.
    Rung,
    /// The give-up deadline of its obligation scope's ladder — the one ladder event a
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
    RepeatGiveUp,
    /// Safety-net timer scheduled when entering "terminating" state.
    TerminatingTimeout,
    /// REFER subscription expiry (RFC 3515).
    ReferSubscriptionExpiry,
    /// Per re-INVITE answer watchdog during REFER-driven blind transfer.
    ReferReinviteAnswer,
    /// Overall REFER safety timer covering the full transfer state machine.
    ReferOverallSafety,
    /// The bound on an established call's reroute: the replacement leg's dial,
    /// the realign and the old leg's release.
    RerouteGuard,
    /// The deadline of the answer to the adaptation HTTP request a service
    /// sent off the call's turn, named by its `correlation_id`: the request's
    /// budget plus a margin. In the ledger while the answer is awaited; its
    /// expiry is the answer read as lost (ADR-0039).
    ServiceHttpAnswer {
        correlation_id: String,
    },
    /// The deadline of the answer to a service's replacement of the call's
    /// admission set, named by its `correlation_id` and numbered `change`:
    /// the limiter's admit budget plus a margin (ADR-0039).
    ServiceAdmitAnswer {
        correlation_id: String,
        change: u64,
    },
    /// The deadline of the answer to a `/call/failure` consult the core sent
    /// off the call's turn, named by the first change number the consult
    /// reserved: the failover chain's budget plus a margin. In the ledger
    /// while the answer is awaited; its expiry is the consult's fold read
    /// unanswered, the `terminate` payload `unanswered` (ADR-0039).
    FailureAnswer {
        change: u64,
        unanswered: serde_json::Value,
    },
    /// A **service-owned** per-call timer (ADR-0016): `service_id` is the owning
    /// callflow service's [`MachineId`], `key` a service-chosen discriminator
    /// (e.g. `"deadline18x"`). Core schedules / cancels / replicates / restores it
    /// like any other timer but attaches NO semantics — only the owning
    /// service's rules match its firing (exact `(service_id, key)` via a
    /// `timer_type` match column, or per-service via the `service_timers`
    /// wildcard). Two timers with different keys are distinct timers;
    /// re-arming the same `(service_id, key)` under the same scope replaces
    /// the previous instance.
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

    /// A service-owned timer with a runtime-computed key.
    pub fn service_owned(service_id: MachineId, key: String) -> Self {
        TimerType::Service { service_id, key: Cow::Owned(key) }
    }
}

impl Hash for TimerType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            TimerType::ServiceHttpAnswer { correlation_id } => correlation_id.hash(state),
            TimerType::ServiceAdmitAnswer { correlation_id, change } => {
                correlation_id.hash(state);
                change.hash(state);
            }
            TimerType::FailureAnswer { change, .. } => change.hash(state),
            TimerType::Service { service_id, key } => {
                service_id.hash(state);
                key.hash(state);
            }
            _ => {}
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
            TimerType::Rung => f.write_str("Rung"),
            TimerType::RepeatGiveUp => f.write_str("RepeatGiveUp"),
            TimerType::TerminatingTimeout => f.write_str("TerminatingTimeout"),
            TimerType::ReferSubscriptionExpiry => f.write_str("ReferSubscriptionExpiry"),
            TimerType::ReferReinviteAnswer => f.write_str("ReferReinviteAnswer"),
            TimerType::ReferOverallSafety => f.write_str("ReferOverallSafety"),
            TimerType::RerouteGuard => f.write_str("RerouteGuard"),
            TimerType::ServiceHttpAnswer { correlation_id } => {
                write!(f, "ServiceHttpAnswer:{correlation_id}")
            }
            TimerType::ServiceAdmitAnswer { correlation_id, .. } => {
                write!(f, "ServiceAdmitAnswer:{correlation_id}")
            }
            TimerType::FailureAnswer { change, .. } => write!(f, "FailureAnswer:{change}"),
            TimerType::Service { service_id, key } => {
                write!(f, "Service:{}:{}", service_id.as_str(), key)
            }
        }
    }
}

/// What a timer guards: it lives as long as that scope does, and leaving the
/// scope retires it.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerScope {
    /// The call: retired when the call ends or its termination begins.
    Call,
    /// One attempt, by id: retired when the attempt ends.
    Attempt(AttemptId),
    /// One leg, by id: retired when the leg ends (cancelled, destroyed or
    /// terminated).
    Leg(String),
    /// One retransmission obligation (ADR-0032 X4): retired with its ladder.
    Obligation(Obligation),
}

impl std::fmt::Debug for TimerScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimerScope::Call => f.write_str("call"),
            TimerScope::Attempt(id) => write!(f, "{id:?}"),
            TimerScope::Leg(leg) => write!(f, "leg {leg}"),
            TimerScope::Obligation(o) => write!(f, "{o:?}"),
        }
    }
}

/// One timer of a call: its type and the scope it guards. A call holds at
/// most one armed timer per key; arming a key again replaces it.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimerKey {
    #[serde(rename = "type")]
    pub timer_type: TimerType,
    pub scope: TimerScope,
}

impl TimerKey {
    /// `timer_type` guarding the call.
    pub const fn call(timer_type: TimerType) -> Self {
        TimerKey { timer_type, scope: TimerScope::Call }
    }

    /// `timer_type` guarding attempt `id`.
    pub const fn attempt(timer_type: TimerType, id: AttemptId) -> Self {
        TimerKey { timer_type, scope: TimerScope::Attempt(id) }
    }

    /// `timer_type` guarding leg `leg_id`.
    pub fn leg(timer_type: TimerType, leg_id: impl Into<String>) -> Self {
        TimerKey { timer_type, scope: TimerScope::Leg(leg_id.into()) }
    }

    /// `timer_type` guarding the retransmission `obligation`.
    pub fn obligation(timer_type: TimerType, obligation: Obligation) -> Self {
        TimerKey { timer_type, scope: TimerScope::Obligation(obligation) }
    }

    /// The leg the timer guards, for a leg-scoped timer.
    pub fn leg_id(&self) -> Option<&str> {
        match &self.scope {
            TimerScope::Leg(leg) => Some(leg),
            _ => None,
        }
    }

    /// The attempt the timer guards, for an attempt-scoped timer.
    pub fn attempt_id(&self) -> Option<AttemptId> {
        match self.scope {
            TimerScope::Attempt(id) => Some(id),
            _ => None,
        }
    }

    /// The obligation the timer guards, for an obligation-scoped timer.
    pub fn obligation_of(&self) -> Option<&Obligation> {
        match &self.scope {
            TimerScope::Obligation(o) => Some(o),
            _ => None,
        }
    }
}

impl std::fmt::Debug for TimerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} on {:?}", self.timer_type, self.scope)
    }
}

/// An armed timer on the replicated ledger: its key, the generation it was
/// armed under (the call's counter, [`crate::Call::timer_generation`]), and
/// its absolute deadline. A fire names key and generation; one the ledger no
/// longer holds is stale.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerEntry {
    pub key: TimerKey,
    pub generation: u64,
    /// Epoch ms — absolute deadline.
    pub fire_at: i64,
}

impl TimerEntry {
    /// The timer's type.
    pub fn timer_type(&self) -> &TimerType {
        &self.key.timer_type
    }

    /// The leg the timer guards, for a leg-scoped timer.
    pub fn leg_id(&self) -> Option<&str> {
        self.key.leg_id()
    }
}
