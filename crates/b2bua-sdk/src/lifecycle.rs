//! The call's lifecycle events: an attempt opened or ended, a leg ended. The
//! engine emits them and delivers each, in the turn that caused it and after
//! the rule that claimed the turn, to every service whose machine is active
//! ([`crate::model::MatchKind::Lifecycle`]). A service's handler may arm or
//! cancel its own service timers, move its own machine's cursor and write
//! call ext; nothing else.

use call::{AttemptCause, AttemptId, AttemptOutcome, ByeDisposition, Deadline, ExtMap};

/// What of a routing answer an attempt's services read: its label and the
/// service slices it states.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteAnswer {
    pub label: Option<String>,
    pub service_ext: ExtMap,
}

/// The attempt a destination leg's [`crate::model::RuleAction::CreateLeg`]
/// opens: why, the routing answer it dials, and the ring deadline its legs
/// run from their first 18x.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptOpening {
    pub cause: AttemptCause,
    pub route: RouteAnswer,
    pub no_answer: Deadline,
}

impl AttemptOpening {
    /// An attempt for `cause` dialling `route`, its legs ringing under no
    /// deadline.
    pub fn new(cause: AttemptCause, route: RouteAnswer) -> Self {
        Self { cause, route, no_answer: Deadline::Off }
    }

    /// The same attempt, its legs ringing for `no_answer`.
    pub fn ringing(self, no_answer: Deadline) -> Self {
        Self { no_answer, ..self }
    }
}

/// One lifecycle event of the call.
#[derive(Debug, Clone, PartialEq)]
pub enum LifecycleEvent {
    /// `attempt` opened to dial `route`.
    AttemptStarted { attempt: AttemptId, route: RouteAnswer },
    /// `attempt` ended with `outcome`; the timers it scoped are retired.
    AttemptEnded { attempt: AttemptId, outcome: AttemptOutcome },
    /// `leg` ended: it reads terminated, `cause` saying how where its facts
    /// state it.
    LegEnded { leg: String, cause: Option<ByeDisposition> },
}

impl LifecycleEvent {
    /// The short name the trace and the logs give the event.
    pub fn kind(&self) -> &'static str {
        match self {
            LifecycleEvent::AttemptStarted { .. } => "attempt-started",
            LifecycleEvent::AttemptEnded { .. } => "attempt-ended",
            LifecycleEvent::LegEnded { .. } => "leg-ended",
        }
    }
}
