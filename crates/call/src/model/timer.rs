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
/// Equality and the hash read the fields that identify a timer: a
/// [`TimerType::FailureAnswer`]'s `unanswered` payload is carried, no part of
/// its identity.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerType {
    NoAnswer,
    /// The call's setup deadline: armed at route time under
    /// [`TimerScope::Setup`], untouched by reroutes (each reroute gets its own
    /// per-leg `NoAnswer`; this caps the *whole* setup). It rides the call
    /// record's ledger, so a call whose transactions are gone is still ended
    /// at the deadline.
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
    /// discharging ACK or PRACK, by a 2xx's give-up, or with the leg,
    /// transaction or call the emission belonged to.
    Rung,
    /// The give-up deadline of a 2xx's ladder — the one ladder event a rule
    /// sees: the deployment's ACK deadline (Timer L where it states none).
    /// Armed once with the first rung, so a re-emission cannot push the
    /// deadline out, and in the ledger for as long as the retained emission
    /// is; the framework scrubs the ladder when it fires and a CORE rule
    /// (which a service may re-author) says what the silence means. The
    /// session ends whatever the rule decides (RFC 3261 §13.3.1.4) — a service
    /// authors the teardown's shape, never whether it happens. A reliable
    /// provisional's ladder arms none: its §3 reject is its responder's.
    RepeatGiveUp,
    /// Safety-net timer scheduled when entering "terminating" state.
    TerminatingTimeout,
    /// REFER subscription expiry (RFC 3515).
    ReferSubscriptionExpiry,
    /// Per re-INVITE answer watchdog during REFER-driven blind transfer.
    ReferReinviteAnswer,
    /// Overall REFER safety timer covering the full transfer state machine.
    ReferOverallSafety,
    /// The earliest end of a wait of this stack's requests held behind its own
    /// NOTIFY on a dialog ([`crate::OwnNotify::ANSWER_CAP_MS`] after that
    /// NOTIFY left): they leave though it is unanswered. Call-scoped, armed
    /// exactly while a request is held; it outlives the start of the
    /// termination, whose BYE may be the request held.
    NotifyAnswerCap,
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
    /// The deadline of the answer to a release event's consult, named by the
    /// change number its turn reserved: the decision budget plus the admit
    /// budget of the route it may answer, plus a margin. In the ledger while
    /// the answer is awaited; its expiry is the release event again
    /// (ADR-0039).
    ReleaseAnswer {
        change: u64,
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

impl PartialEq for TimerType {
    fn eq(&self, other: &Self) -> bool {
        use TimerType::*;
        match self {
            NoAnswer => matches!(other, NoAnswer),
            SetupTimeout => matches!(other, SetupTimeout),
            GlobalDuration => matches!(other, GlobalDuration),
            LimiterRefresh => matches!(other, LimiterRefresh),
            Keepalive => matches!(other, Keepalive),
            KeepaliveTimeout => matches!(other, KeepaliveTimeout),
            Rung => matches!(other, Rung),
            RepeatGiveUp => matches!(other, RepeatGiveUp),
            TerminatingTimeout => matches!(other, TerminatingTimeout),
            ReferSubscriptionExpiry => matches!(other, ReferSubscriptionExpiry),
            ReferReinviteAnswer => matches!(other, ReferReinviteAnswer),
            ReferOverallSafety => matches!(other, ReferOverallSafety),
            NotifyAnswerCap => matches!(other, NotifyAnswerCap),
            RerouteGuard => matches!(other, RerouteGuard),
            ServiceHttpAnswer { correlation_id: a } => {
                matches!(other, ServiceHttpAnswer { correlation_id: b } if a == b)
            }
            ServiceAdmitAnswer { correlation_id: a, change: x } => {
                matches!(other, ServiceAdmitAnswer { correlation_id: b, change: y } if a == b && x == y)
            }
            FailureAnswer { change: a, .. } => {
                matches!(other, FailureAnswer { change: b, .. } if a == b)
            }
            ReleaseAnswer { change: a } => matches!(other, ReleaseAnswer { change: b } if a == b),
            Service { service_id: a, key: k } => {
                matches!(other, Service { service_id: b, key: l } if a == b && k == l)
            }
        }
    }
}

impl Eq for TimerType {}

impl Hash for TimerType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            TimerType::ServiceHttpAnswer { correlation_id } => correlation_id.hash(state),
            TimerType::ServiceAdmitAnswer { correlation_id, change } => {
                correlation_id.hash(state);
                change.hash(state);
            }
            TimerType::FailureAnswer { change, .. } | TimerType::ReleaseAnswer { change } => {
                change.hash(state)
            }
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
            TimerType::NotifyAnswerCap => f.write_str("NotifyAnswerCap"),
            TimerType::RerouteGuard => f.write_str("RerouteGuard"),
            TimerType::ServiceHttpAnswer { correlation_id } => {
                write!(f, "ServiceHttpAnswer:{correlation_id}")
            }
            TimerType::ServiceAdmitAnswer { correlation_id, .. } => {
                write!(f, "ServiceAdmitAnswer:{correlation_id}")
            }
            TimerType::FailureAnswer { change, .. } => write!(f, "FailureAnswer:{change}"),
            TimerType::ReleaseAnswer { change } => write!(f, "ReleaseAnswer:{change}"),
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
    /// The call's setup: over while any leg holds a confirmed dialog (a
    /// media-server leg's included) and once the call's termination began. A
    /// fire is read against it as it stands at the fire, so a timer of this
    /// scope is retired by the rule that answers or by the termination, never
    /// because a leg's dialog was confirmed for a while.
    Setup,
}

impl std::fmt::Debug for TimerScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimerScope::Call => f.write_str("call"),
            TimerScope::Setup => f.write_str("setup"),
            TimerScope::Attempt(id) => write!(f, "{id:?}"),
            TimerScope::Leg(leg) => write!(f, "leg {leg}"),
            TimerScope::Obligation(o) => write!(f, "{o:?}"),
        }
    }
}

/// The kind of scope a timer guards, its id aside: what a rule declares of
/// the timers it arms or cancels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimerScopeKind {
    Call,
    Attempt,
    Leg,
    Obligation,
    Setup,
}

impl TimerScope {
    /// The kind of this scope.
    pub fn kind(&self) -> TimerScopeKind {
        match self {
            TimerScope::Call => TimerScopeKind::Call,
            TimerScope::Attempt(_) => TimerScopeKind::Attempt,
            TimerScope::Leg(_) => TimerScopeKind::Leg,
            TimerScope::Obligation(_) => TimerScopeKind::Obligation,
            TimerScope::Setup => TimerScopeKind::Setup,
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

    /// `timer_type` guarding the call's setup.
    pub const fn setup(timer_type: TimerType) -> Self {
        TimerKey { timer_type, scope: TimerScope::Setup }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure consult's deadline is named by its change number: the lost
    /// answer it carries is no part of its identity.
    #[test]
    fn a_failure_answer_key_is_its_change_number() {
        let key = |unanswered| TimerKey::call(TimerType::FailureAnswer { change: 4, unanswered });
        assert_eq!(key(serde_json::json!({ "status": 486 })), key(serde_json::Value::Null));
        assert_ne!(
            key(serde_json::Value::Null),
            TimerKey::call(TimerType::FailureAnswer {
                change: 5,
                unanswered: serde_json::Value::Null
            })
        );
    }
}
