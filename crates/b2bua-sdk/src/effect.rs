//! The side effects a machine-bound rule declares it may emit ([`Effect`])
//! and the category every action belongs to ([`EffectKind`]), ADR-0016 X9.

use call::{TimerScopeKind, TimerType};
use sip_message::Method;

/// The category every action ([`crate::model::RuleAction`]) belongs to
/// (ADR-0016 X9). [`crate::model::RuleAction::effect_kind`] is a **total** map
/// onto this — the compiler's exhaustiveness check forces each present and future action into exactly one category, so a
/// machine-bound rule's declared [`Effect`]s can be verified against what its
/// handler actually emits (`emitted ⊆ declared`, compared by this kind alone).
///
/// Three kinds are **tracked** (declared by a service rule, rendered on the
/// diagram): [`Self::LegMessage`], [`Self::CallLifecycleCommand`],
/// [`Self::GuardTimer`]. The other two are **not** side effects and are
/// auto-allowed (never declared, never checked): [`Self::CursorMove`] is the
/// transition itself (`SetState`) or machine deactivation (`ClearState`),
/// already drawn as the edge / terminal; [`Self::Bookkeeping`] is a local data
/// write or async kick with no wire output of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectKind {
    LegMessage,
    CallLifecycleCommand,
    GuardTimer,
    CursorMove,
    Bookkeeping,
}

impl EffectKind {
    /// Is this a **tracked** side effect a machine-bound rule must declare? The
    /// cursor-move + bookkeeping kinds are auto-allowed (the executor's effect
    /// drift-check skips them).
    pub fn is_tracked(self) -> bool {
        matches!(
            self,
            EffectKind::LegMessage | EffectKind::CallLifecycleCommand | EffectKind::GuardTimer
        )
    }
}

/// A tracked side effect a machine-bound rule declares it may emit (ADR-0016 X9).
/// Categorised by the **attribution principle** — *who authors the message* —
/// into the three [`EffectKind`]s: a **leg message** (its four wire forms below),
/// a **call-lifecycle command**, or a **guard timer**. Listed in the rule's
/// `effects` (required by `sm_rule!`) and rendered on the generated diagram; the
/// executor's drift-check compares only [`Self::kind`], so the typed payloads are
/// labels for the diagram, not part of the check (a handler targeting a
/// dynamically-named leg still satisfies a `LegMessage`-kind declaration). Every
/// variant carries a free, unenforced `label`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// **Leg message** — a request the service rule **originates** toward a leg
    /// (the MRF INVITE, an MSCML `INFO`, a re-INVITE to A/C, a rule-driven
    /// BYE/CANCEL/ACK). `method` is the canonical [`Method`]; the rule owns the
    /// semantic payload, core fills the mechanical SIP layer.
    Originate { method: Method, label: &'static str },
    /// **Leg message** — a transparent **relay** of the inbound message onward
    /// (peer leg / named leg / a-leg failure); the method is whatever arrived.
    Relay { label: &'static str },
    /// **Leg message** — a final **response** to a leg's server transaction.
    Respond { status: u16, label: &'static str },
    /// **Leg message** — an unreliable **provisional** `1xx` (RFC 3262 early media).
    Provisional { status: u16, label: &'static str },
    /// **Call-lifecycle command** — the one synchronous service → global hop (X3):
    /// the call-lifecycle subset of the action union (`BeginTermination`/
    /// `TerminateCall`/`Merge`/`Split`). The global machine owns any wire messages
    /// it then generates.
    LifecycleCommand { label: &'static str },
    /// **Guard timer** — a watchdog timer the rule arms or cancels: its type
    /// (a service timer: its owning service) and the kind of scope its key
    /// names.
    GuardTimer { timer: TimerType, scope: TimerScopeKind, label: &'static str },
}

impl Effect {
    /// The free, unenforced diagram label every variant carries.
    pub fn label(&self) -> &'static str {
        match self {
            Effect::Originate { label, .. }
            | Effect::Relay { label }
            | Effect::Respond { label, .. }
            | Effect::Provisional { label, .. }
            | Effect::LifecycleCommand { label }
            | Effect::GuardTimer { label, .. } => label,
        }
    }

    /// The category this declared effect contributes to the drift-check.
    pub fn kind(&self) -> EffectKind {
        match self {
            Effect::Originate { .. }
            | Effect::Relay { .. }
            | Effect::Respond { .. }
            | Effect::Provisional { .. } => EffectKind::LegMessage,
            Effect::LifecycleCommand { .. } => EffectKind::CallLifecycleCommand,
            Effect::GuardTimer { .. } => EffectKind::GuardTimer,
        }
    }
}
