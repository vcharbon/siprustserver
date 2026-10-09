//! The **accepted endings** a scenario declares: each one a named [`Branch`]
//! whose [`Ending`] is a predicate over the call's final observed state. A
//! call that tears down and settles succeeds under the first branch whose
//! ending holds; one whose ending matches none fails (see [`super::verdict`]).
//!
//! The set is `'static` and `Copy`, so declaring it costs nothing per call. A
//! scenario whose accepted endings depend on the call's context (emergency,
//! overload) picks its set in [`ActorScenario::build`](super::ActorScenario::build),
//! which sees the [`CallEnv`]. One context-dependent branch is the harness
//! default: [`Branch::REFUSED_OVERLOAD`].

use super::state::StateInner;
use crate::realcall::CallEnv;

/// What a call's final observed state must show for a [`Branch`] to hold.
/// `Refused`/`EarlyMediaRefused` are read on the caller's establishing INVITE,
/// so the final is one received before the call connected.
#[derive(Debug, Clone, Copy)]
pub enum Ending {
    /// The caller's establishing INVITE drew no non-2xx final: the dialog
    /// confirmed, and the call tore down.
    Answered,
    /// The caller's establishing INVITE drew this non-2xx final.
    Refused(u16),
    /// The caller received a provisional carrying SDP (early media), then its
    /// establishing INVITE drew this non-2xx final.
    EarlyMediaRefused(u16),
    /// A scenario-specific predicate over the final observed state.
    Observed(fn(&StateInner) -> bool),
}

/// One accepted ending: a bounded name (the `branch` label) and its [`Ending`].
#[derive(Debug, Clone, Copy)]
pub struct Branch {
    pub name: &'static str,
    pub ending: Ending,
}

impl Branch {
    /// The call connected and tore down.
    pub const ANSWERED: Branch = Branch::new("answered", Ending::Answered);
    /// The caller CANCELled its pending INVITE (RFC 3261 §9.1), which drew
    /// `487 Request Terminated`.
    pub const ABANDONED: Branch = Branch::new("abandoned", Ending::Refused(487));
    /// A `503 Service Unavailable` to the caller's establishing INVITE (RFC 3261
    /// §21.5.4, the server is overloaded). The harness adds it to every call
    /// that is no emergency call (RFC 4412 `Resource-Priority`) in a run that
    /// drives overload on purpose ([`CallEnv::overload_expected`]), unless the
    /// call opts out ([`Expect::without_overload_refusal`]).
    pub const REFUSED_OVERLOAD: Branch = Branch::new("refused_overload", Ending::Refused(503));

    pub const fn new(name: &'static str, ending: Ending) -> Self {
        Branch { name, ending }
    }

    /// The callee (or the server) refused the call with `code`.
    pub const fn rejected(code: u16) -> Self {
        Branch::new("rejected", Ending::Refused(code))
    }

    /// The caller heard early media, then the call was refused with `code`.
    pub const fn early_media_rejected(code: u16) -> Self {
        Branch::new("early_media_rejected", Ending::EarlyMediaRefused(code))
    }

    /// Whether this branch's ending holds on the final state, `caller` being the
    /// originating leg.
    fn holds(&self, s: &StateInner, caller: &str) -> bool {
        let leg = s.leg_ref(caller);
        match self.ending {
            Ending::Answered => leg.is_some_and(|l| l.first_final().is_none()),
            Ending::Refused(code) => leg.is_some_and(|l| l.saw_final(code)),
            Ending::EarlyMediaRefused(code) => {
                leg.is_some_and(|l| l.saw_early_media() && l.saw_final(code))
            }
            Ending::Observed(pred) => pred(s),
        }
    }

    /// Whether this branch ends the call on the caller's establishing-INVITE
    /// final `code`.
    fn ends_on_final(&self, code: u16) -> bool {
        matches!(self.ending, Ending::Refused(c) | Ending::EarlyMediaRefused(c) if c == code)
    }
}

/// The set of endings a call accepts: the scenario's declared branches (a
/// `'static` list plus at most one built at run time, for a status known only
/// then), plus the harness's [`Branch::REFUSED_OVERLOAD`] unless the call opted
/// out or its context rules it out ([`in_context`](Self::in_context)).
#[derive(Debug, Clone, Copy)]
pub struct Expect {
    declared: &'static [Branch],
    built: Option<Branch>,
    overload_refusal: bool,
}

impl Expect {
    /// No accepted ending: the runner of a bare [`CallPlan`](super::CallPlan).
    pub const NONE: Expect = Expect { declared: &[], built: None, overload_refusal: false };

    /// The happy path alone: the call connects and tears down.
    pub const fn answered() -> Self {
        Expect::of(&[Branch::ANSWERED])
    }

    /// This one branch, plus the harness's overload refusal.
    pub const fn one(branch: Branch) -> Self {
        Expect { declared: &[], built: Some(branch), overload_refusal: true }
    }

    /// These branches, tried in order, plus the harness's overload refusal.
    pub const fn of(declared: &'static [Branch]) -> Self {
        Expect { declared, built: None, overload_refusal: true }
    }

    /// Also accept `branch`, tried after the declared list. One such branch per
    /// set: a second is a scenario bug and panics.
    pub const fn and(self, branch: Branch) -> Self {
        assert!(self.built.is_none(), "an accepted-ending set holds one run-time branch");
        Expect { built: Some(branch), ..self }
    }

    /// Opt out of the harness's [`Branch::REFUSED_OVERLOAD`]: a 503 fails this
    /// call even in a run that expects overload.
    pub const fn without_overload_refusal(self) -> Self {
        Expect { overload_refusal: false, ..self }
    }

    /// This set as it applies to one call: the overload refusal stays only for a
    /// call that is no emergency call in a run that expects overload.
    pub fn in_context(self, env: &CallEnv<'_>) -> Self {
        Expect {
            overload_refusal: self.overload_refusal && !env.emergency && env.overload_expected,
            ..self
        }
    }

    /// Every branch this set accepts, in the order they are tried.
    pub fn branches(&self) -> impl Iterator<Item = &Branch> {
        let overload = self.overload_refusal.then_some(&Branch::REFUSED_OVERLOAD);
        self.declared.iter().chain(self.built.as_ref()).chain(overload)
    }

    /// Whether a non-2xx final `code` on the caller's establishing INVITE is an
    /// ending this call accepts — the caller then ends its call cleanly rather
    /// than failing it.
    pub fn accepts_final(&self, code: u16) -> bool {
        self.branches().any(|b| b.ends_on_final(code))
    }

    /// The first accepted branch the final state shows, `caller` being the
    /// originating leg.
    pub fn branch_of(&self, s: &StateInner, caller: &str) -> Option<&'static str> {
        self.branches().find(|b| b.holds(s, caller)).map(|b| b.name)
    }
}
