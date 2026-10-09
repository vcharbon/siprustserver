//! The **accepted endings** a scenario declares: each one a named [`Branch`]
//! whose [`Ending`] is a predicate over the call's final observed state. A
//! call that tears down and settles succeeds under the first branch whose
//! ending holds; one whose ending matches none fails (see [`super::verdict`]).
//!
//! The set is `'static` and `Copy`, so declaring it costs nothing per call. A
//! scenario whose accepted endings depend on the call's context (emergency,
//! overload) picks its set in [`ActorScenario::build`](super::ActorScenario::build),
//! which sees the [`CallEnv`]. One context-dependent branch comes from the run,
//! not the scenario: [`Branch::refused_overload`], present only when the run
//! declares the final its system under test refuses overload with.

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
    /// The callee's 2xx crossed the caller's CANCEL (RFC 3261 §9.2): her
    /// INVITE drew a 2xx before her CANCEL drew its 200, and no non-2xx final.
    /// A server that ignores the CANCEL — no final to it, or a 2xx after its
    /// 200 — shows neither order and is no crossing.
    AnsweredAcrossCancel,
    /// The callee's 2xx reached the caller before her CANCEL left: she sent
    /// none, her INVITE drew a 2xx and no non-2xx final.
    AnsweredBeforeCancel,
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
    /// The callee's 2xx crossed the caller's CANCEL: the CANCEL had no effect
    /// (RFC 3261 §9.2), so the caller ACKed the 2xx and hung up with a BYE
    /// (§15).
    pub const ANSWERED_DESPITE_CANCEL: Branch =
        Branch::new("answered_despite_cancel", Ending::AnsweredAcrossCancel);
    /// The callee's 2xx reached the caller before she abandoned: she sent no
    /// CANCEL and hung up with a BYE (§15).
    pub const ANSWERED_BEFORE_CANCEL: Branch =
        Branch::new("answered_before_cancel", Ending::AnsweredBeforeCancel);
    pub const fn new(name: &'static str, ending: Ending) -> Self {
        Branch { name, ending }
    }

    /// The callee (or the server) refused the call with `code`.
    pub const fn rejected(code: u16) -> Self {
        Branch::new("rejected", Ending::Refused(code))
    }

    /// The server refused the call with `code`, the final the run declares its
    /// system under test answers overload with ([`CallEnv::overload_reject`]).
    /// Added to every call that is no emergency call (RFC 4412
    /// `Resource-Priority`) of such a run, unless the call opts out
    /// ([`Expect::without_overload_refusal`]).
    pub const fn refused_overload(code: u16) -> Self {
        Branch::new("refused_overload", Ending::Refused(code))
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
            Ending::AnsweredAcrossCancel => leg.is_some_and(|l| {
                let facts = l.responses();
                let answered = facts
                    .iter()
                    .position(|f| (200..300).contains(&f.status) && f.cseq_method == "INVITE");
                let cancel_final =
                    facts.iter().position(|f| f.status >= 200 && f.cseq_method == "CANCEL");
                l.first_final().is_none()
                    && matches!((answered, cancel_final), (Some(a), Some(c))
                        if a < c && facts[c].status == 200)
            }),
            Ending::AnsweredBeforeCancel => leg.is_some_and(|l| {
                !l.cancel_sent()
                    && l.first_final().is_none()
                    && l.responses()
                        .iter()
                        .any(|f| (200..300).contains(&f.status) && f.cseq_method == "INVITE")
            }),
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
/// then), plus [`Branch::refused_overload`] when the run declares an overload
/// refusal, the call is no emergency call and has not opted out
/// ([`in_context`](Self::in_context)). Nothing else is accepted.
#[derive(Debug, Clone, Copy)]
pub struct Expect {
    declared: &'static [Branch],
    built: Option<Branch>,
    /// Whether the call takes the run's overload refusal (the opt-out clears it).
    overload_refusal: bool,
    /// The run's overload-refusal final, once [`in_context`](Self::in_context)
    /// resolved it for this call.
    overload_reject: Option<u16>,
}

impl Expect {
    /// No accepted ending: the runner of a bare [`CallPlan`](super::CallPlan).
    pub const NONE: Expect =
        Expect { declared: &[], built: None, overload_refusal: false, overload_reject: None };

    /// The happy path alone: the call connects and tears down.
    pub const fn answered() -> Self {
        Expect::of(&[Branch::ANSWERED])
    }

    /// This one branch, plus the run's overload refusal if it declares one.
    pub const fn one(branch: Branch) -> Self {
        Expect { declared: &[], built: Some(branch), overload_refusal: true, overload_reject: None }
    }

    /// These branches, tried in order, plus the run's overload refusal if it
    /// declares one.
    pub const fn of(declared: &'static [Branch]) -> Self {
        Expect { declared, built: None, overload_refusal: true, overload_reject: None }
    }

    /// Also accept `branch`, tried after the declared list. One such branch per
    /// set: a second is a scenario bug and panics.
    pub const fn and(self, branch: Branch) -> Self {
        assert!(self.built.is_none(), "an accepted-ending set holds one run-time branch");
        Expect { built: Some(branch), ..self }
    }

    /// Opt out of the run's [`Branch::refused_overload`]: the run's overload
    /// refusal fails this call like any undeclared final.
    pub const fn without_overload_refusal(self) -> Self {
        Expect { overload_refusal: false, overload_reject: None, ..self }
    }

    /// This set as it applies to one call: the run's overload refusal
    /// ([`CallEnv::overload_reject`]) joins it only for a call that is no
    /// emergency call and has not opted out.
    pub fn in_context(self, env: &CallEnv<'_>) -> Self {
        let takes_it = self.overload_refusal && !env.emergency;
        Expect { overload_reject: env.overload_reject.filter(|_| takes_it), ..self }
    }

    /// Every branch this set accepts, in the order they are tried.
    pub fn branches(&self) -> impl Iterator<Item = Branch> + '_ {
        let overload = self.overload_reject.map(Branch::refused_overload);
        self.declared.iter().copied().chain(self.built).chain(overload)
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
